use rusqlite::{Connection, OptionalExtension, params};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

fn temporary_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!("agentrt-process-{label}-{}", std::process::id()))
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

    let connection = Connection::open(&database).expect("database opens");
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

    drop(connection);
    std::fs::remove_file(&database).expect("database removes");
    std::fs::remove_dir_all(&workspace).expect("workspace removes");
}

fn wait_for_tool_result(database: &PathBuf) -> String {
    for _ in 0..150 {
        if let Ok(connection) = Connection::open(database) {
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
                let count: i64 = connection
                    .query_row(
                        "SELECT COUNT(*) FROM events WHERE run_id = ?1 AND event_type = 'tool.result'",
                        params![run_id],
                        |row| row.get(0),
                    )
                    .expect("event lookup succeeds");
                if count > 0 {
                    return run_id;
                }
            }
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("tool result was not persisted before timeout");
}
