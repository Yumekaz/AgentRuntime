//! Small reference workflows built on the durable executor.

use crate::audit::sha256_hex;
use crate::exec::{self, ExecutionError};
use crate::model::{AgentPlan, ModelProvider, ModelRequest, PlanAction};
use crate::run::{StepDefinition, new_run_id};
use crate::sandbox::{Policy, ToolRouter};
use crate::store::{Store, ToolStepSpec};
use std::path::Path;

pub(crate) struct RepoFixResult {
    pub(crate) run_id: String,
    pub(crate) output: String,
}

pub(crate) fn repo_fix(
    store_path: &Path,
    workspace: &Path,
    relative_path: &str,
    find: &str,
    replace: &str,
) -> Result<RepoFixResult, ExecutionError> {
    repo_fix_with_options(store_path, workspace, relative_path, find, replace, None, 0)
}

pub(crate) fn repo_fix_with_options(
    store_path: &Path,
    workspace: &Path,
    relative_path: &str,
    find: &str,
    replace: &str,
    requested_run_id: Option<String>,
    pause_ms: u64,
) -> Result<RepoFixResult, ExecutionError> {
    if find.is_empty() {
        return Err(ExecutionError::GateFailed(
            "repo-fix requires non-empty search text".to_owned(),
        ));
    }

    let policy = Policy::workspace(workspace)?;
    let router = ToolRouter::new(policy);
    let source = router.read_file(relative_path)?;
    if source.matches(find).count() != 1 {
        return Err(ExecutionError::GateFailed(format!(
            "repo-fix expected exactly one match in `{relative_path}`"
        )));
    }
    let replacement = source.replacen(find, replace, 1);
    let workspace_root = std::fs::canonicalize(workspace)
        .map_err(crate::sandbox::SandboxError::from)?
        .to_string_lossy()
        .into_owned();

    let run_id = requested_run_id.unwrap_or_else(new_run_id);
    let definitions = StepDefinition::sequence(4);
    let store = Store::open(store_path)?;
    store.create_run(&run_id, &definitions)?;
    store.record_event(
        &run_id,
        "agent.created",
        None,
        &format!(
            "agent=repo-fix path={} before_sha256={} after_sha256={}",
            relative_path,
            sha256_hex(source.as_bytes()),
            sha256_hex(replacement.as_bytes())
        ),
    )?;

    let specs = [
        ToolStepSpec {
            idempotency_key: exec::idempotency_key(&run_id, 0),
            workspace_root: workspace_root.clone(),
            tool_name: "read_file".to_owned(),
            path: relative_path.to_owned(),
            contents: None,
            read_only: true,
            denied_tool: None,
        },
        ToolStepSpec {
            idempotency_key: exec::idempotency_key(&run_id, 1),
            workspace_root: workspace_root.clone(),
            tool_name: "write_file".to_owned(),
            path: relative_path.to_owned(),
            contents: Some(replacement.clone()),
            read_only: false,
            denied_tool: None,
        },
        ToolStepSpec {
            idempotency_key: exec::idempotency_key(&run_id, 2),
            workspace_root: workspace_root.clone(),
            tool_name: "read_file".to_owned(),
            path: relative_path.to_owned(),
            contents: None,
            read_only: true,
            denied_tool: None,
        },
        ToolStepSpec {
            idempotency_key: exec::idempotency_key(&run_id, 3),
            workspace_root,
            tool_name: "gate_contains".to_owned(),
            path: relative_path.to_owned(),
            contents: Some(replace.to_owned()),
            read_only: true,
            denied_tool: None,
        },
    ];
    for (index, spec) in specs.iter().enumerate() {
        store.configure_tool_step(&run_id, index, spec)?;
    }

    exec::resume_run_with_pause(&store, &run_id, pause_ms)?;
    Ok(RepoFixResult {
        run_id,
        output: replacement,
    })
}

pub(crate) fn repo_fix_from_model<P: ModelProvider>(
    store_path: &Path,
    workspace: &Path,
    provider: &P,
    request: &ModelRequest,
    requested_run_id: Option<String>,
    pause_ms: u64,
) -> Result<RepoFixResult, ExecutionError> {
    Policy::workspace(workspace)?;
    let run_id = requested_run_id.unwrap_or_else(new_run_id);
    let planning_definitions = StepDefinition::sequence(1);
    let store = Store::open(store_path)?;
    store.create_run(&run_id, &planning_definitions)?;
    store.record_event(
        &run_id,
        "agent.created",
        None,
        &format!("agent=repo-fix-model model={}", request.model),
    )?;

    let plan_text = exec::execute_llm_step_in_run(
        &store,
        &run_id,
        &planning_definitions[0],
        provider,
        request,
    )?;
    let parsed_plan = match AgentPlan::parse(&plan_text) {
        Ok(plan) => plan,
        Err(error) => {
            store.record_event(&run_id, "agent.plan_rejected", Some(0), &error.to_string())?;
            return Err(error.into());
        }
    };
    let plan = match RepoFixPlan::from_agent_plan(parsed_plan) {
        Ok(plan) => plan,
        Err(error) => {
            store.record_event(&run_id, "agent.plan_rejected", Some(0), &error.to_string())?;
            return Err(error);
        }
    };
    let router = ToolRouter::new(Policy::workspace(workspace)?);
    for validation in [
        router.validate_read(&plan.path),
        router.validate_write(&plan.path, &plan.contents),
        router.validate_read(&plan.path),
    ] {
        if let Err(error) = validation {
            store.record_event(&run_id, "agent.plan_rejected", Some(0), &error.to_string())?;
            return Err(error.into());
        }
    }
    store.record_event(
        &run_id,
        "agent.plan",
        Some(0),
        &format!(
            "summary={} plan_sha256={}",
            plan.summary,
            sha256_hex(plan_text.as_bytes())
        ),
    )?;
    store.record_event(
        &run_id,
        "agent.plan_validated",
        Some(0),
        &format!(
            "tools=read_file,write_file,read_file,gate_contains path={}",
            plan.path
        ),
    )?;

    let definitions = StepDefinition::sequence(5);
    store.append_steps(&run_id, &definitions[1..])?;
    let workspace_root = std::fs::canonicalize(workspace)
        .map_err(crate::sandbox::SandboxError::from)?
        .to_string_lossy()
        .into_owned();
    let specs = [
        ToolStepSpec {
            idempotency_key: exec::idempotency_key(&run_id, 1),
            workspace_root: workspace_root.clone(),
            tool_name: "read_file".to_owned(),
            path: plan.path.clone(),
            contents: None,
            read_only: true,
            denied_tool: None,
        },
        ToolStepSpec {
            idempotency_key: exec::idempotency_key(&run_id, 2),
            workspace_root: workspace_root.clone(),
            tool_name: "write_file".to_owned(),
            path: plan.path.clone(),
            contents: Some(plan.contents.clone()),
            read_only: false,
            denied_tool: None,
        },
        ToolStepSpec {
            idempotency_key: exec::idempotency_key(&run_id, 3),
            workspace_root: workspace_root.clone(),
            tool_name: "read_file".to_owned(),
            path: plan.path.clone(),
            contents: None,
            read_only: true,
            denied_tool: None,
        },
        ToolStepSpec {
            idempotency_key: exec::idempotency_key(&run_id, 4),
            workspace_root,
            tool_name: "gate_contains".to_owned(),
            path: plan.path.clone(),
            contents: Some(plan.expected.clone()),
            read_only: true,
            denied_tool: None,
        },
    ];
    for (offset, spec) in specs.iter().enumerate() {
        store.configure_tool_step(&run_id, offset + 1, spec)?;
    }
    exec::resume_run_with_pause(&store, &run_id, pause_ms)?;
    Ok(RepoFixResult {
        run_id,
        output: plan.summary,
    })
}

struct RepoFixPlan {
    summary: String,
    path: String,
    contents: String,
    expected: String,
}

impl RepoFixPlan {
    fn from_agent_plan(plan: AgentPlan) -> Result<Self, ExecutionError> {
        if plan.actions.len() != 4 {
            return Err(ExecutionError::Plan(
                "repo-fix model plan must contain read, write, read, and gate actions".to_owned(),
            ));
        }
        let [
            PlanAction::Read { path: read_path },
            PlanAction::Write {
                path: write_path,
                contents,
            },
            PlanAction::Read { path: verify_path },
            PlanAction::GateContains {
                path: gate_path,
                expected,
            },
        ] = plan.actions.as_slice()
        else {
            return Err(ExecutionError::Plan(
                "repo-fix model plan has an unsafe or incomplete action sequence".to_owned(),
            ));
        };
        if read_path != write_path || read_path != verify_path || read_path != gate_path {
            return Err(ExecutionError::Plan(
                "repo-fix actions must target one file".to_owned(),
            ));
        }
        Ok(Self {
            summary: plan.summary,
            path: read_path.clone(),
            contents: contents.clone(),
            expected: expected.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{repo_fix, repo_fix_from_model};
    use crate::exec::ExecutionError;
    use crate::model::{FakeProvider, Message, ModelRequest};
    use crate::run::new_run_id;
    use crate::store::Store;
    use std::path::PathBuf;

    fn temporary_store_path() -> PathBuf {
        std::env::temp_dir().join(format!("agentrt-agent-{}.db", new_run_id()))
    }

    fn temporary_workspace() -> PathBuf {
        std::env::temp_dir().join(format!("agentrt-agent-workspace-{}", new_run_id()))
    }

    #[test]
    fn repo_fix_persists_read_write_verify_and_gate_steps() {
        let database = temporary_store_path();
        let workspace = temporary_workspace();
        std::fs::create_dir_all(&workspace).expect("workspace creates");
        std::fs::write(workspace.join("fixture.txt"), "status=broken\n").expect("fixture writes");

        let result = repo_fix(
            &database,
            &workspace,
            "fixture.txt",
            "status=broken",
            "status=fixed",
        )
        .expect("repo fix succeeds");
        assert_eq!(
            std::fs::read_to_string(workspace.join("fixture.txt")).unwrap(),
            "status=fixed\n"
        );

        let store = Store::open(&database).expect("store reopens");
        let run = store.load_run(&result.run_id).expect("run loads");
        assert_eq!(run.status.as_str(), "succeeded");
        assert_eq!(run.current_step, 4);
        assert!(
            store
                .load_steps(&result.run_id)
                .unwrap()
                .iter()
                .all(|step| step.completed)
        );
        let events = store.load_events(&result.run_id).expect("events load");
        assert!(
            events
                .iter()
                .any(|event| event.event_type == "agent.created")
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "tool.invoked")
                .count(),
            3
        );
        assert!(
            events
                .iter()
                .any(|event| event.event_type == "gate.evaluated")
        );
        assert!(
            events
                .iter()
                .any(|event| event.event_type == "run.finished")
        );

        drop(store);
        std::fs::remove_file(database).expect("database removes");
        std::fs::remove_dir_all(workspace).expect("workspace removes");
    }

    #[test]
    fn model_plan_drives_audited_durable_workflow() {
        let database = temporary_store_path();
        let workspace = temporary_workspace();
        std::fs::create_dir_all(&workspace).expect("workspace creates");
        std::fs::write(workspace.join("fixture.txt"), "status=broken\n").expect("fixture writes");
        let plan = r#"{
            "version": 1,
            "summary": "repair fixture from model plan",
            "actions": [
                {"kind": "read", "path": "fixture.txt"},
                {"kind": "write", "path": "fixture.txt", "contents": "status=fixed\n"},
                {"kind": "read", "path": "fixture.txt"},
                {"kind": "gate_contains", "path": "fixture.txt", "expected": "status=fixed"}
            ]
        }"#;
        let provider = FakeProvider::new(plan);
        let request = ModelRequest {
            model: "fake-agent-planner".to_owned(),
            messages: vec![Message::user("repair the fixture")],
            temperature: 0.0,
        };

        let result = repo_fix_from_model(&database, &workspace, &provider, &request, None, 0)
            .expect("model workflow succeeds");
        assert_eq!(result.output, "repair fixture from model plan");
        assert_eq!(
            std::fs::read_to_string(workspace.join("fixture.txt")).unwrap(),
            "status=fixed\n"
        );

        let store = Store::open(&database).expect("store reopens");
        let run = store.load_run(&result.run_id).expect("run loads");
        assert_eq!(run.status.as_str(), "succeeded");
        assert_eq!(run.current_step, 5);
        let events = store.load_events(&result.run_id).expect("events load");
        assert!(events.iter().any(|event| event.event_type == "llm.request"));
        assert!(
            events
                .iter()
                .any(|event| event.event_type == "llm.response")
        );
        assert!(events.iter().any(|event| event.event_type == "agent.plan"));
        assert!(
            events
                .iter()
                .any(|event| event.event_type == "agent.plan_validated")
        );
        assert!(
            events
                .iter()
                .any(|event| event.event_type == "run.plan_expanded")
        );
        assert!(
            events
                .iter()
                .any(|event| event.event_type == "gate.evaluated")
        );

        drop(store);
        std::fs::remove_file(database).expect("database removes");
        std::fs::remove_dir_all(workspace).expect("workspace removes");
    }

    #[test]
    fn malformed_model_plan_is_rejected_and_audited() {
        let database = temporary_store_path();
        let workspace = temporary_workspace();
        std::fs::create_dir_all(&workspace).expect("workspace creates");
        std::fs::write(workspace.join("fixture.txt"), "status=broken\n").expect("fixture writes");
        let provider = FakeProvider::new("not-json");
        let request = ModelRequest {
            model: "fake-agent-planner".to_owned(),
            messages: vec![Message::user("repair the fixture")],
            temperature: 0.0,
        };
        let run_id = format!("model-reject-{}", new_run_id());

        let result = repo_fix_from_model(
            &database,
            &workspace,
            &provider,
            &request,
            Some(run_id.clone()),
            0,
        );
        assert!(matches!(result, Err(ExecutionError::Plan(_))));
        assert_eq!(
            std::fs::read_to_string(workspace.join("fixture.txt")).unwrap(),
            "status=broken\n"
        );
        let store = Store::open(&database).expect("store reopens");
        let events = store.load_events(&run_id).expect("events load");
        assert!(
            events
                .iter()
                .any(|event| event.event_type == "agent.plan_rejected")
        );

        drop(store);
        std::fs::remove_file(database).expect("database removes");
        std::fs::remove_dir_all(workspace).expect("workspace removes");
    }
}
