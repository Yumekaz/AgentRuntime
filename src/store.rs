//! Durable SQLite persistence for runs, steps, and checkpoints.

use crate::run::{RunStatus, StepDefinition};
use rusqlite::{Connection, OptionalExtension, params};
use std::fmt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub(crate) enum StoreError {
    Io(std::io::Error),
    Sql(rusqlite::Error),
    InvalidStatus(String),
    RunNotFound(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Sql(error) => write!(formatter, "database error: {error}"),
            Self::InvalidStatus(error) => write!(formatter, "{error}"),
            Self::RunNotFound(run_id) => write!(formatter, "run `{run_id}` was not found"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<std::io::Error> for StoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sql(error)
    }
}

#[derive(Debug)]
pub(crate) struct StoredRun {
    pub(crate) run_id: String,
    pub(crate) status: RunStatus,
    pub(crate) current_step: usize,
    pub(crate) total_steps: usize,
}

#[derive(Debug)]
pub(crate) struct StoredStep {
    pub(crate) index: usize,
    pub(crate) id: String,
    pub(crate) completed: bool,
    pub(crate) output: Option<String>,
}

#[derive(Debug)]
pub(crate) struct StoredEvent {
    pub(crate) sequence: i64,
    pub(crate) event_type: String,
    pub(crate) step_index: Option<usize>,
    pub(crate) payload: String,
}

#[derive(Clone, Debug)]
pub(crate) struct ToolStepSpec {
    pub(crate) workspace_root: String,
    pub(crate) tool_name: String,
    pub(crate) path: String,
    pub(crate) contents: Option<String>,
    pub(crate) read_only: bool,
    pub(crate) denied_tool: Option<String>,
}

pub(crate) struct Store {
    connection: Connection,
}

impl Store {
    pub(crate) fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }

        let connection = Connection::open(path)?;
        let store = Self { connection };
        store.initialize()?;
        Ok(store)
    }

    pub(crate) fn create_run(
        &self,
        run_id: &str,
        steps: &[StepDefinition],
    ) -> Result<(), StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        let now = timestamp();

        transaction.execute(
            "INSERT INTO runs (run_id, status, current_step, created_at, updated_at)
             VALUES (?1, ?2, 0, ?3, ?3)",
            params![run_id, RunStatus::Created.as_str(), now],
        )?;

        for step in steps {
            transaction.execute(
                "INSERT INTO steps (run_id, step_index, step_id, status)
                 VALUES (?1, ?2, ?3, 'pending')",
                params![run_id, step.index as i64, step.id],
            )?;
        }

        append_event(&transaction, run_id, "run.created", None, "")?;

        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn configure_tool_step(
        &self,
        run_id: &str,
        step_index: usize,
        spec: &ToolStepSpec,
    ) -> Result<(), StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        let step_exists: Option<i64> = transaction
            .query_row(
                "SELECT step_index FROM steps WHERE run_id = ?1 AND step_index = ?2",
                params![run_id, step_index as i64],
                |row| row.get(0),
            )
            .optional()?;
        if step_exists.is_none() {
            return Err(StoreError::RunNotFound(format!(
                "step {step_index} in run `{run_id}`"
            )));
        }

        transaction.execute(
            "INSERT OR REPLACE INTO tool_steps
             (run_id, step_index, workspace_root, tool_name, path, contents, read_only, denied_tool)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                run_id,
                step_index as i64,
                spec.workspace_root,
                spec.tool_name,
                spec.path,
                spec.contents,
                spec.read_only as i64,
                spec.denied_tool,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn mark_running(&self, run_id: &str) -> Result<(), StoreError> {
        let previous_status: Option<String> = self
            .connection
            .query_row(
                "SELECT status FROM runs WHERE run_id = ?1",
                params![run_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(previous_status) = previous_status else {
            return Err(StoreError::RunNotFound(run_id.to_owned()));
        };
        let event_type = if previous_status == RunStatus::Created.as_str() {
            "run.started"
        } else {
            "run.resumed"
        };

        let transaction = self.connection.unchecked_transaction()?;
        transaction.execute(
            "UPDATE runs SET status = 'running', updated_at = ?1 WHERE run_id = ?2",
            params![timestamp(), run_id],
        )?;
        append_event(&transaction, run_id, event_type, None, "")?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn complete_step(
        &self,
        run_id: &str,
        step_index: usize,
        output: &str,
    ) -> Result<(), StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        let changed = transaction.execute(
            "UPDATE steps
             SET status = 'completed', output = ?1
             WHERE run_id = ?2 AND step_index = ?3 AND status != 'completed'",
            params![output, run_id, step_index as i64],
        )?;

        let newly_completed = changed > 0;
        if !newly_completed {
            let exists: Option<String> = transaction
                .query_row(
                    "SELECT status FROM steps WHERE run_id = ?1 AND step_index = ?2",
                    params![run_id, step_index as i64],
                    |row| row.get(0),
                )
                .optional()?;
            if exists.as_deref() != Some("completed") {
                return Err(StoreError::RunNotFound(format!(
                    "step {step_index} in run `{run_id}`"
                )));
            }
        }

        transaction.execute(
            "UPDATE runs
             SET current_step = CASE WHEN current_step < ?1 THEN ?1 ELSE current_step END,
                 updated_at = ?2
             WHERE run_id = ?3",
            params![(step_index + 1) as i64, timestamp(), run_id],
        )?;
        if newly_completed {
            append_event(
                &transaction,
                run_id,
                "step.completed",
                Some(step_index),
                output,
            )?;
            append_event(
                &transaction,
                run_id,
                "checkpoint.saved",
                Some(step_index),
                &format!("frontier={}", step_index + 1),
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn finish_run(&self, run_id: &str) -> Result<(), StoreError> {
        let existing: Option<String> = self
            .connection
            .query_row(
                "SELECT status FROM runs WHERE run_id = ?1",
                params![run_id],
                |row| row.get(0),
            )
            .optional()?;
        if existing.is_none() {
            return Err(StoreError::RunNotFound(run_id.to_owned()));
        }
        if existing.as_deref() == Some(RunStatus::Succeeded.as_str()) {
            return Ok(());
        }

        let transaction = self.connection.unchecked_transaction()?;
        transaction.execute(
            "UPDATE runs SET status = 'succeeded', updated_at = ?1 WHERE run_id = ?2",
            params![timestamp(), run_id],
        )?;
        append_event(
            &transaction,
            run_id,
            "run.finished",
            None,
            "status=succeeded",
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn record_event(
        &self,
        run_id: &str,
        event_type: &str,
        step_index: Option<usize>,
        payload: &str,
    ) -> Result<(), StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        let exists: Option<String> = transaction
            .query_row(
                "SELECT run_id FROM runs WHERE run_id = ?1",
                params![run_id],
                |row| row.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Err(StoreError::RunNotFound(run_id.to_owned()));
        }
        append_event(&transaction, run_id, event_type, step_index, payload)?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn load_run(&self, run_id: &str) -> Result<StoredRun, StoreError> {
        let row = self
            .connection
            .query_row(
                "SELECT run_id, status, current_step,
                        (SELECT COUNT(*) FROM steps WHERE steps.run_id = runs.run_id)
                 FROM runs WHERE run_id = ?1",
                params![run_id],
                |row| {
                    let status: String = row.get(1)?;
                    Ok((
                        row.get::<_, String>(0)?,
                        status,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;

        let Some((run_id, status, current_step, total_steps)) = row else {
            return Err(StoreError::RunNotFound(run_id.to_owned()));
        };

        Ok(StoredRun {
            run_id,
            status: RunStatus::try_from(status.as_str()).map_err(StoreError::InvalidStatus)?,
            current_step: current_step as usize,
            total_steps: total_steps as usize,
        })
    }

    pub(crate) fn load_steps(&self, run_id: &str) -> Result<Vec<StoredStep>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT step_index, step_id, status, output
             FROM steps WHERE run_id = ?1 ORDER BY step_index",
        )?;
        let rows = statement.query_map(params![run_id], |row| {
            let status: String = row.get(2)?;
            Ok(StoredStep {
                index: row.get::<_, i64>(0)? as usize,
                id: row.get(1)?,
                completed: status == "completed",
                output: row.get(3)?,
            })
        })?;

        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    pub(crate) fn load_tool_step(
        &self,
        run_id: &str,
        step_index: usize,
    ) -> Result<Option<ToolStepSpec>, StoreError> {
        self.connection
            .query_row(
                "SELECT workspace_root, tool_name, path, contents, read_only, denied_tool
                 FROM tool_steps WHERE run_id = ?1 AND step_index = ?2",
                params![run_id, step_index as i64],
                |row| {
                    Ok(ToolStepSpec {
                        workspace_root: row.get(0)?,
                        tool_name: row.get(1)?,
                        path: row.get(2)?,
                        contents: row.get(3)?,
                        read_only: row.get::<_, i64>(4)? != 0,
                        denied_tool: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sql)
    }

    pub(crate) fn load_events(&self, run_id: &str) -> Result<Vec<StoredEvent>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT sequence, event_type, step_index, payload
             FROM events WHERE run_id = ?1 ORDER BY sequence",
        )?;
        let rows = statement.query_map(params![run_id], |row| {
            Ok(StoredEvent {
                sequence: row.get(0)?,
                event_type: row.get(1)?,
                step_index: row.get::<_, Option<i64>>(2)?.map(|value| value as usize),
                payload: row.get(3)?,
            })
        })?;

        rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::Sql)
    }

    fn initialize(&self) -> Result<(), StoreError> {
        self.connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS runs (
                 run_id TEXT PRIMARY KEY,
                 status TEXT NOT NULL,
                 current_step INTEGER NOT NULL,
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS steps (
                 run_id TEXT NOT NULL,
                 step_index INTEGER NOT NULL,
                 step_id TEXT NOT NULL,
                 status TEXT NOT NULL,
                 output TEXT,
                 PRIMARY KEY (run_id, step_index),
                 UNIQUE (run_id, step_id),
                 FOREIGN KEY (run_id) REFERENCES runs(run_id) ON DELETE CASCADE
             );
             CREATE TABLE IF NOT EXISTS events (
                 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id TEXT NOT NULL,
                 event_type TEXT NOT NULL,
                 step_index INTEGER,
                 payload TEXT NOT NULL,
                 occurred_at INTEGER NOT NULL,
                 FOREIGN KEY (run_id) REFERENCES runs(run_id) ON DELETE CASCADE
             );
             CREATE TABLE IF NOT EXISTS tool_steps (
                 run_id TEXT NOT NULL,
                 step_index INTEGER NOT NULL,
                 workspace_root TEXT NOT NULL,
                 tool_name TEXT NOT NULL,
                 path TEXT NOT NULL,
                 contents TEXT,
                 read_only INTEGER NOT NULL,
                 denied_tool TEXT,
                 PRIMARY KEY (run_id, step_index),
                 FOREIGN KEY (run_id) REFERENCES runs(run_id) ON DELETE CASCADE
             );",
        )?;
        Ok(())
    }
}

fn append_event(
    transaction: &rusqlite::Transaction<'_>,
    run_id: &str,
    event_type: &str,
    step_index: Option<usize>,
    payload: &str,
) -> Result<(), rusqlite::Error> {
    transaction.execute(
        "INSERT INTO events (run_id, event_type, step_index, payload, occurred_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            run_id,
            event_type,
            step_index.map(|value| value as i64),
            payload,
            timestamp()
        ],
    )?;
    Ok(())
}

fn timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before the Unix epoch")
        .as_secs() as i64
}
