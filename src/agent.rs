//! Small reference workflows built on the durable executor.

use crate::audit::sha256_hex;
use crate::exec::{self, ExecutionError};
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

    let run_id = new_run_id();
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

    exec::resume_run(&store, &run_id)?;
    Ok(RepoFixResult {
        run_id,
        output: replacement,
    })
}

#[cfg(test)]
mod tests {
    use super::repo_fix;
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
        std::fs::write(workspace.join("fixture.txt"), "status=broken\n")
            .expect("fixture writes");

        let result = repo_fix(
            &database,
            &workspace,
            "fixture.txt",
            "status=broken",
            "status=fixed",
        )
        .expect("repo fix succeeds");
        assert_eq!(std::fs::read_to_string(workspace.join("fixture.txt")).unwrap(), "status=fixed\n");

        let store = Store::open(&database).expect("store reopens");
        let run = store.load_run(&result.run_id).expect("run loads");
        assert_eq!(run.status.as_str(), "succeeded");
        assert_eq!(run.current_step, 4);
        assert!(store.load_steps(&result.run_id).unwrap().iter().all(|step| step.completed));
        let events = store.load_events(&result.run_id).expect("events load");
        assert!(events.iter().any(|event| event.event_type == "agent.created"));
        assert_eq!(
            events.iter().filter(|event| event.event_type == "tool.invoked").count(),
            3
        );
        assert!(events.iter().any(|event| event.event_type == "gate.evaluated"));
        assert!(events.iter().any(|event| event.event_type == "run.finished"));

        drop(store);
        std::fs::remove_file(database).expect("database removes");
        std::fs::remove_dir_all(workspace).expect("workspace removes");
    }
}
