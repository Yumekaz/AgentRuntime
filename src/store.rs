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

        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn mark_running(&self, run_id: &str) -> Result<(), StoreError> {
        let changed = self.connection.execute(
            "UPDATE runs SET status = 'running', updated_at = ?1 WHERE run_id = ?2",
            params![timestamp(), run_id],
        )?;
        if changed == 0 {
            return Err(StoreError::RunNotFound(run_id.to_owned()));
        }
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

        if changed == 0 {
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
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn finish_run(&self, run_id: &str) -> Result<(), StoreError> {
        let changed = self.connection.execute(
            "UPDATE runs SET status = 'succeeded', updated_at = ?1 WHERE run_id = ?2",
            params![timestamp(), run_id],
        )?;
        if changed == 0 {
            return Err(StoreError::RunNotFound(run_id.to_owned()));
        }
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
             );",
        )?;
        Ok(())
    }
}

fn timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before the Unix epoch")
        .as_secs() as i64
}
