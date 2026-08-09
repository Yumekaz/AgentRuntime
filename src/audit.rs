//! Exportable audit evidence for a completed or interrupted run.

use crate::store::{Store, StoreError};
use sha2::{Digest, Sha256};
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) enum AuditError {
    Io(std::io::Error),
    Store(StoreError),
    DestinationNotEmpty(PathBuf),
}

impl fmt::Display for AuditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::Store(error) => write!(formatter, "{error}"),
            Self::DestinationNotEmpty(path) => {
                write!(
                    formatter,
                    "audit destination is not empty: {}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for AuditError {}

impl From<std::io::Error> for AuditError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<StoreError> for AuditError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

pub(crate) fn export(store: &Store, run_id: &str, destination: &Path) -> Result<(), AuditError> {
    prepare_destination(destination)?;

    let run = store.load_run(run_id)?;
    let events = store.load_events(run_id)?;

    let run_json = format!(
        "{{\n  \"run_id\": {},\n  \"status\": {},\n  \"current_step\": {},\n  \"total_steps\": {}\n}}\n",
        json_string(&run.run_id),
        json_string(run.status.as_str()),
        run.current_step,
        run.total_steps
    );
    let events_jsonl = events
        .iter()
        .map(|event| {
            format!(
                "{{\"sequence\":{},\"type\":{},\"step_index\":{},\"payload\":{}}}\n",
                event.sequence,
                json_string(&event.event_type),
                event
                    .step_index
                    .map(|index| index.to_string())
                    .unwrap_or_else(|| "null".to_owned()),
                json_string(&event.payload)
            )
        })
        .collect::<String>();

    let run_path = destination.join("run.json");
    let events_path = destination.join("events.jsonl");
    std::fs::write(&run_path, run_json.as_bytes())?;
    std::fs::write(&events_path, events_jsonl.as_bytes())?;

    let manifest = format!(
        "{}  run.json\n{}  events.jsonl\n",
        sha256_hex(run_json.as_bytes()),
        sha256_hex(events_jsonl.as_bytes())
    );
    std::fs::write(destination.join("MANIFEST"), manifest)?;
    Ok(())
}

fn prepare_destination(destination: &Path) -> Result<(), AuditError> {
    if destination.exists() {
        if !destination.is_dir()
            || std::fs::read_dir(destination)?
                .next()
                .transpose()?
                .is_some()
        {
            return Err(AuditError::DestinationNotEmpty(destination.to_owned()));
        }
    } else {
        std::fs::create_dir_all(destination)?;
    }
    Ok(())
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn json_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() => {
                escaped.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => escaped.push(character),
        }
    }
    escaped.push('"');
    escaped
}

#[cfg(test)]
mod tests {
    use super::export;
    use crate::exec::execute;
    use crate::run::{StepDefinition, new_run_id};
    use crate::store::Store;
    use std::path::PathBuf;

    fn temporary_path(suffix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("agentrt-{}-{suffix}", new_run_id()))
    }

    #[test]
    fn export_contains_reconstructable_files_and_hashes() {
        let database = temporary_path("audit.db");
        let bundle = temporary_path("bundle");
        let run_id = new_run_id();
        let definitions = StepDefinition::sequence(2);

        {
            let store = Store::open(&database).expect("store opens");
            store
                .create_run(&run_id, &definitions)
                .expect("run creates");
            execute(&store, &run_id, &definitions, None).expect("run succeeds");
            export(&store, &run_id, &bundle).expect("bundle exports");
        }

        let run_json = std::fs::read_to_string(bundle.join("run.json")).expect("run metadata");
        let events = std::fs::read_to_string(bundle.join("events.jsonl")).expect("event stream");
        let manifest = std::fs::read_to_string(bundle.join("MANIFEST")).expect("manifest");
        assert!(run_json.contains(&run_id));
        assert_eq!(events.lines().count(), 7);
        assert!(manifest.contains("  run.json\n"));
        assert!(manifest.contains("  events.jsonl\n"));

        std::fs::remove_file(database).expect("database removes");
        std::fs::remove_dir_all(bundle).expect("bundle removes");
    }
}
