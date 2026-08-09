//! Deterministic step execution and recovery coordination.

use crate::audit::sha256_hex;
use crate::run::StepDefinition;
use crate::sandbox::{SandboxError, ToolRouter};
use crate::store::{Store, StoreError};
use std::fmt;

#[derive(Debug)]
pub(crate) enum ExecutionError {
    Store(StoreError),
    Sandbox(SandboxError),
    SimulatedCrash { run_id: String, completed: usize },
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "{error}"),
            Self::Sandbox(error) => write!(formatter, "{error}"),
            Self::SimulatedCrash { run_id, completed } => write!(
                formatter,
                "simulated process crash after {completed} completed step(s) in run `{run_id}`"
            ),
        }
    }
}

impl std::error::Error for ExecutionError {}

impl From<StoreError> for ExecutionError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<SandboxError> for ExecutionError {
    fn from(error: SandboxError) -> Self {
        Self::Sandbox(error)
    }
}

pub(crate) enum ToolAction {
    ReadFile(String),
    WriteFile { path: String, contents: String },
    ListDir(String),
}

impl ToolAction {
    fn name(&self) -> &'static str {
        match self {
            Self::ReadFile(_) => "read_file",
            Self::WriteFile { .. } => "write_file",
            Self::ListDir(_) => "list_dir",
        }
    }

    fn arguments(&self) -> String {
        match self {
            Self::ReadFile(path) | Self::ListDir(path) => path.clone(),
            Self::WriteFile { path, contents } => format!("{path}\n{contents}"),
        }
    }
}

pub(crate) fn execute_tool_step(
    store: &Store,
    run_id: &str,
    definition: &StepDefinition,
    router: &ToolRouter,
    action: &ToolAction,
) -> Result<String, ExecutionError> {
    store.mark_running(run_id)?;
    let argument_hash = sha256_hex(action.arguments().as_bytes());
    store.record_event(
        run_id,
        "tool.invoked",
        Some(definition.index),
        &format!("name={} args_sha256={argument_hash}", action.name()),
    )?;

    let result = match action {
        ToolAction::ReadFile(path) => router.read_file(path),
        ToolAction::WriteFile { path, contents } => router
            .write_file(path, contents)
            .map(|()| "written".to_owned()),
        ToolAction::ListDir(path) => router.list_dir(path).map(|entries| entries.join("\n")),
    };

    let output = match result {
        Ok(output) => output,
        Err(error) => {
            if error.is_denied() {
                store.record_event(
                    run_id,
                    "sandbox.denied",
                    Some(definition.index),
                    &error.to_string(),
                )?;
            } else {
                store.record_event(
                    run_id,
                    "tool.result",
                    Some(definition.index),
                    &format!("name={} status=error", action.name()),
                )?;
            }
            return Err(error.into());
        }
    };

    store.record_event(
        run_id,
        "tool.result",
        Some(definition.index),
        &format!(
            "name={} status=ok result_sha256={}",
            action.name(),
            sha256_hex(output.as_bytes())
        ),
    )?;
    store.complete_step(run_id, definition.index, &output)?;
    store.finish_run(run_id)?;
    Ok(output)
}

pub(crate) fn execute(
    store: &Store,
    run_id: &str,
    definitions: &[StepDefinition],
    crash_after: Option<usize>,
) -> Result<(), ExecutionError> {
    store.mark_running(run_id)?;
    let stored_steps = store.load_steps(run_id)?;
    let mut completed_now = 0;

    for definition in definitions {
        let stored = stored_steps
            .iter()
            .find(|step| step.index == definition.index && step.id == definition.id)
            .expect("every definition must have a stored step");
        if stored.completed {
            assert!(
                stored.output.is_some(),
                "completed steps must retain their deterministic output"
            );
            continue;
        }

        let output = format!("completed {}", stored.id);
        store.complete_step(run_id, definition.index, &output)?;
        completed_now += 1;

        if crash_after == Some(completed_now) {
            return Err(ExecutionError::SimulatedCrash {
                run_id: run_id.to_owned(),
                completed: completed_now,
            });
        }
    }

    store.finish_run(run_id)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ExecutionError, ToolAction, execute, execute_tool_step};
    use crate::run::{StepDefinition, new_run_id};
    use crate::sandbox::{Policy, ToolRouter};
    use crate::store::Store;
    use std::path::PathBuf;

    fn temporary_store_path() -> PathBuf {
        std::env::temp_dir().join(format!("agentrt-{}.db", new_run_id()))
    }

    fn temporary_workspace() -> PathBuf {
        std::env::temp_dir().join(format!("agentrt-workspace-{}", new_run_id()))
    }

    #[test]
    fn crash_resume_skips_completed_steps() {
        let path = temporary_store_path();
        let run_id = new_run_id();
        let definitions = StepDefinition::sequence(4);

        {
            let store = Store::open(&path).expect("store opens");
            store
                .create_run(&run_id, &definitions)
                .expect("run creates");
            let result = execute(&store, &run_id, &definitions, Some(2));
            assert!(matches!(
                result,
                Err(ExecutionError::SimulatedCrash { completed: 2, .. })
            ));
            let run = store.load_run(&run_id).expect("run loads");
            assert_eq!(run.current_step, 2);
            assert_eq!(run.status.as_str(), "running");
            let events = store.load_events(&run_id).expect("events load");
            assert_eq!(events.len(), 6);
            assert_eq!(events[0].event_type, "run.created");
            assert_eq!(events[1].event_type, "run.started");
            assert_eq!(events[2].event_type, "step.completed");
            assert_eq!(events[3].event_type, "checkpoint.saved");
            assert_eq!(events[5].event_type, "checkpoint.saved");
        }

        {
            let store = Store::open(&path).expect("store reopens");
            execute(&store, &run_id, &definitions, None).expect("run resumes");
            let run = store.load_run(&run_id).expect("run loads after resume");
            assert_eq!(run.current_step, 4);
            assert_eq!(run.status.as_str(), "succeeded");
            let steps = store.load_steps(&run_id).expect("steps load");
            assert!(steps.iter().all(|step| step.completed));
            assert!(steps.iter().all(|step| step.output.is_some()));
            let events = store
                .load_events(&run_id)
                .expect("events load after resume");
            assert_eq!(events.len(), 12);
            assert_eq!(events[6].event_type, "run.resumed");
            assert_eq!(events[11].event_type, "run.finished");
            assert!(
                events
                    .windows(2)
                    .all(|pair| pair[0].sequence < pair[1].sequence)
            );
        }

        std::fs::remove_file(path).expect("temporary database removes");
    }

    #[test]
    fn tool_step_records_result_before_checkpoint() {
        let database = temporary_store_path();
        let workspace = temporary_workspace();
        let run_id = new_run_id();
        let definitions = StepDefinition::sequence(1);
        std::fs::create_dir_all(&workspace).expect("workspace creates");
        std::fs::write(workspace.join("input.txt"), "hello").expect("input writes");

        let store = Store::open(&database).expect("store opens");
        store
            .create_run(&run_id, &definitions)
            .expect("run creates");
        let router = ToolRouter::new(Policy::workspace(&workspace).expect("policy creates"));
        execute_tool_step(
            &store,
            &run_id,
            &definitions[0],
            &router,
            &ToolAction::ReadFile("input.txt".to_owned()),
        )
        .expect("tool step succeeds");

        let events = store.load_events(&run_id).expect("events load");
        let event_types = events
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            event_types,
            vec![
                "run.created",
                "run.started",
                "tool.invoked",
                "tool.result",
                "step.completed",
                "checkpoint.saved",
                "run.finished"
            ]
        );

        drop(store);
        std::fs::remove_file(database).expect("database removes");
        std::fs::remove_dir_all(workspace).expect("workspace removes");
    }

    #[test]
    fn denied_tool_action_is_audited_without_checkpointing() {
        let database = temporary_store_path();
        let workspace = temporary_workspace();
        let run_id = new_run_id();
        let definitions = StepDefinition::sequence(1);
        std::fs::create_dir_all(&workspace).expect("workspace creates");

        let store = Store::open(&database).expect("store opens");
        store
            .create_run(&run_id, &definitions)
            .expect("run creates");
        let router = ToolRouter::new(Policy::read_only(&workspace).expect("policy creates"));
        let result = execute_tool_step(
            &store,
            &run_id,
            &definitions[0],
            &router,
            &ToolAction::WriteFile {
                path: "blocked.txt".to_owned(),
                contents: "nope".to_owned(),
            },
        );
        assert!(matches!(result, Err(ExecutionError::Sandbox(_))));

        let run = store.load_run(&run_id).expect("run loads");
        assert_eq!(run.current_step, 0);
        assert_eq!(run.status.as_str(), "running");
        let events = store.load_events(&run_id).expect("events load");
        assert_eq!(events[2].event_type, "tool.invoked");
        assert_eq!(events[3].event_type, "sandbox.denied");

        drop(store);
        std::fs::remove_file(database).expect("database removes");
        std::fs::remove_dir_all(workspace).expect("workspace removes");
    }
}
