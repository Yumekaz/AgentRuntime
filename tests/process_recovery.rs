use rusqlite::{Connection, OptionalExtension, params};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

fn temporary_path(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "agentrt-process-{label}-{}-{nonce}",
        std::process::id()
    ))
}

#[test]
fn killed_tool_process_resumes_from_persisted_spec() {
    let database = temporary_path("recovery.db");
    let workspace = temporary_path("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace creates");

    let mut child = Command::new(env!("CARGO_BIN_EXE_agentrt"))
        .args([
            "tool",
            "write",
            "--workspace",
            workspace.to_str().expect("workspace is UTF-8"),
            "--path",
            "output.txt",
            "--contents",
            "durable",
            "--store",
            database.to_str().expect("database is UTF-8"),
            "--pause-ms",
            "3000",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("tool process starts");

    let run_id = wait_for_tool_result(&database);
    child.kill().expect("tool process kills");
    let _ = child.wait().expect("tool process waits");

    let resumed = Command::new(env!("CARGO_BIN_EXE_agentrt"))
        .args([
            "resume",
            "--store",
            database.to_str().expect("database is UTF-8"),
            "--run-id",
            &run_id,
        ])
        .output()
        .expect("resume process starts");
    assert!(resumed.status.success(), "resume failed: {resumed:?}");

    let connection = open_connection(&database).expect("database opens");
    let status: String = connection
        .query_row(
            "SELECT status FROM runs WHERE run_id = ?1",
            params![run_id],
            |row| row.get(0),
        )
        .expect("run status loads");
    assert_eq!(status, "succeeded");

    let invocations: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM events WHERE run_id = ?1 AND event_type = 'tool.invoked'",
            params![run_id],
            |row| row.get(0),
        )
        .expect("invocation count loads");
    let checkpoints: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM events WHERE run_id = ?1 AND event_type = 'checkpoint.saved'",
            params![run_id],
            |row| row.get(0),
        )
        .expect("checkpoint count loads");
    let deduplicated: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM events WHERE run_id = ?1 AND event_type = 'tool.deduplicated'",
            params![run_id],
            |row| row.get(0),
        )
        .expect("deduplication count loads");
    assert_eq!(invocations, 1);
    assert_eq!(deduplicated, 1);
    assert_eq!(checkpoints, 1);
    assert_eq!(
        std::fs::read_to_string(workspace.join("output.txt")).expect("output reads"),
        "durable"
    );

    // The runner is disposable. Avoid deleting SQLite files immediately after
    // the child process exits because Windows file scanners can still hold them.
    drop(connection);
}

#[test]
fn killed_reference_agent_resumes_at_each_tool_checkpoint() {
    for checkpoint in 0..3 {
        let database = temporary_path(&format!("agent-{checkpoint}.db"));
        let workspace = temporary_path(&format!("agent-{checkpoint}-workspace"));
        std::fs::create_dir_all(&workspace).expect("workspace creates");
        std::fs::write(workspace.join("fixture.txt"), "status=broken\n").expect("fixture writes");
        let run_id = format!("agent-recovery-{checkpoint}-{}", std::process::id());

        let mut child = Command::new(env!("CARGO_BIN_EXE_agentrt"))
            .args([
                "agent",
                "repo-fix",
                "--workspace",
                workspace.to_str().expect("workspace is UTF-8"),
                "--path",
                "fixture.txt",
                "--find",
                "status=broken",
                "--replace",
                "status=fixed",
                "--store",
                database.to_str().expect("database is UTF-8"),
                "--run-id",
                &run_id,
                "--pause-ms",
                "3000",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("agent process starts");

        wait_for_tool_result_at(&database, &run_id, checkpoint);
        child.kill().expect("agent process kills");
        let _ = child.wait().expect("agent process waits");

        let resumed = Command::new(env!("CARGO_BIN_EXE_agentrt"))
            .args([
                "resume",
                "--store",
                database.to_str().expect("database is UTF-8"),
                "--run-id",
                &run_id,
            ])
            .output()
            .expect("resume process starts");
        assert!(resumed.status.success(), "resume failed: {resumed:?}");

        let connection = open_connection(&database).expect("database opens");
        let status: String = connection
            .query_row(
                "SELECT status FROM runs WHERE run_id = ?1",
                params![run_id],
                |row| row.get(0),
            )
            .expect("run status loads");
        assert_eq!(status, "succeeded");

        let invocations: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM events WHERE run_id = ?1 AND event_type = 'tool.invoked'",
                params![run_id],
                |row| row.get(0),
            )
            .expect("invocation count loads");
        assert_eq!(invocations, 3, "checkpoint={checkpoint}");

        let deduplicated: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM events WHERE run_id = ?1 AND event_type = 'tool.deduplicated' AND step_index = ?2",
                params![run_id, checkpoint as i64],
                |row| row.get(0),
            )
            .expect("deduplication count loads");
        assert_eq!(deduplicated, 1, "checkpoint={checkpoint}");

        let checkpoints: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM events WHERE run_id = ?1 AND event_type = 'checkpoint.saved'",
                params![run_id],
                |row| row.get(0),
            )
            .expect("checkpoint count loads");
        assert_eq!(checkpoints, 4, "checkpoint={checkpoint}");
        assert_eq!(
            std::fs::read_to_string(workspace.join("fixture.txt")).expect("fixture reads"),
            "status=fixed\n",
            "checkpoint={checkpoint}"
        );
        assert_event_exists(&connection, &run_id, "gate.evaluated");

        drop(connection);
    }
}

fn wait_for_tool_result(database: &PathBuf) -> String {
    for _ in 0..150 {
        if let Ok(connection) = open_connection(database) {
            let run_id: Option<String> = match connection
                .query_row("SELECT run_id FROM runs LIMIT 1", [], |row| row.get(0))
                .optional()
            {
                Ok(run_id) => run_id,
                Err(_) => {
                    thread::sleep(Duration::from_millis(20));
                    continue;
                }
            };
            if let Some(run_id) = run_id {
                let count: Result<i64, _> = connection.query_row(
                    "SELECT COUNT(*) FROM events WHERE run_id = ?1 AND event_type = 'tool.result'",
                    params![run_id],
                    |row| row.get(0),
                );
                if let Ok(count) = count {
                    if count > 0 {
                        return run_id;
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("tool result was not persisted before timeout");
}

fn wait_for_tool_result_at(database: &PathBuf, run_id: &str, step_index: usize) {
    for _ in 0..600 {
        if let Ok(connection) = open_connection(database) {
            let count: Result<i64, _> = connection.query_row(
                "SELECT COUNT(*) FROM events WHERE run_id = ?1 AND event_type = 'tool.result' AND step_index = ?2",
                params![run_id, step_index as i64],
                |row| row.get(0),
            );
            if matches!(count, Ok(value) if value > 0) {
                return;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("tool result for step {step_index} was not persisted before timeout");
}

fn assert_event_exists(connection: &Connection, run_id: &str, event_type: &str) {
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM events WHERE run_id = ?1 AND event_type = ?2",
            params![run_id, event_type],
            |row| row.get(0),
        )
        .expect("event count loads");
    assert!(count > 0, "event type `{event_type}` was not recorded");
}

fn open_connection(database: &PathBuf) -> rusqlite::Result<Connection> {
    let connection = Connection::open(database)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    Ok(connection)
}
