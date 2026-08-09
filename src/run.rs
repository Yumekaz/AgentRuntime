//! Domain types for durable runs and deterministic steps.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RunStatus {
    Created,
    Running,
    Succeeded,
}

impl RunStatus {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
        }
    }
}

impl TryFrom<&str> for RunStatus {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "created" => Ok(Self::Created),
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            other => Err(format!("unknown run status `{other}`")),
        }
    }
}

impl fmt::Display for RunStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StepDefinition {
    pub(crate) index: usize,
    pub(crate) id: String,
}

impl StepDefinition {
    pub(crate) fn sequence(count: usize) -> Vec<Self> {
        (0..count)
            .map(|index| Self {
                index,
                id: format!("step-{:03}", index + 1),
            })
            .collect()
    }
}

pub(crate) fn new_run_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before the Unix epoch");
    format!(
        "run-{}-{}-{}",
        now.as_secs(),
        now.subsec_nanos(),
        std::process::id()
    )
}
