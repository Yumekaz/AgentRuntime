//! Deterministic step execution and recovery coordination.

use crate::run::StepDefinition;
use crate::store::{Store, StoreError};
use std::fmt;

#[derive(Debug)]
pub(crate) enum ExecutionError {
    Store(StoreError),
    SimulatedCrash { run_id: String, completed: usize },
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "{error}"),
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
    use super::{ExecutionError, execute};
    use crate::run::{StepDefinition, new_run_id};
    use crate::store::Store;
    use std::path::PathBuf;

    fn temporary_store_path() -> PathBuf {
        std::env::temp_dir().join(format!("agentrt-{}.db", new_run_id()))
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
        }

        std::fs::remove_file(path).expect("temporary database removes");
    }
}
