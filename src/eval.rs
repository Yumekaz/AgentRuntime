//! Evaluation cases and reports that exercise the same runtime as production runs.

use crate::agent;
use crate::gate;
use crate::model::{FakeProvider, Message, ModelRequest};
use crate::run::new_run_id;
use crate::sandbox::{Policy, ToolRouter};
use std::path::{Path, PathBuf};

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct EvalReport {
    pub(crate) total: usize,
    pub(crate) passed: usize,
    pub(crate) failures: Vec<String>,
}

impl EvalReport {
    pub(crate) fn succeeded(&self) -> bool {
        self.failures.is_empty() && self.passed == self.total
    }
}

impl std::fmt::Display for EvalReport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(formatter, "evals={}/{} passed", self.passed, self.total)?;
        for failure in &self.failures {
            writeln!(formatter, "FAIL {failure}")?;
        }
        Ok(())
    }
}

pub(crate) fn run_suite(break_regression: bool) -> EvalReport {
    let mut report = EvalReport {
        total: 6,
        passed: 0,
        failures: Vec::new(),
    };

    run_case(&mut report, "repo-fix applies one replacement", || {
        let workspace = fixture_workspace(
            "repo-fix",
            include_str!("../fixtures/evals/repo-fix/input.txt"),
        )?;
        let store = temporary_store("repo-fix");
        let result = agent::repo_fix(
            &store,
            &workspace,
            "input.txt",
            "status=broken",
            "status=fixed",
        )
        .map_err(|error| error.to_string())?;
        let output = std::fs::read_to_string(workspace.join("input.txt"))
            .map_err(|error| error.to_string())?;
        cleanup(&workspace, &store);
        if output != "status=fixed\n" || !result.output.contains("status=fixed") {
            return Err("repo-fix did not produce the expected file".to_owned());
        }
        Ok(())
    });

    run_case(&mut report, "repo-fix rejects ambiguous edits", || {
        let workspace = fixture_workspace(
            "ambiguous",
            include_str!("../fixtures/evals/ambiguous/input.txt"),
        )?;
        let store = temporary_store("ambiguous");
        let result = agent::repo_fix(
            &store,
            &workspace,
            "input.txt",
            "status=broken",
            "status=fixed",
        );
        let rejected = matches!(result, Err(crate::exec::ExecutionError::GateFailed(_)));
        cleanup(&workspace, &store);
        if rejected {
            Ok(())
        } else {
            Err("ambiguous edit was accepted".to_owned())
        }
    });

    run_case(&mut report, "sandbox rejects traversal", || {
        let workspace = fixture_workspace(
            "traversal",
            include_str!("../fixtures/evals/traversal/input.txt"),
        )?;
        let policy = Policy::workspace(&workspace).map_err(|error| error.to_string())?;
        let router = ToolRouter::new(policy);
        let rejected = router.read_file("../outside.txt").is_err();
        cleanup(&workspace, &PathBuf::new());
        if rejected {
            Ok(())
        } else {
            Err("traversal escaped the workspace policy".to_owned())
        }
    });

    run_case(&mut report, "read-only policy denies writes", || {
        let workspace = fixture_workspace(
            "read-only",
            include_str!("../fixtures/evals/read-only/input.txt"),
        )?;
        let policy = Policy::read_only(&workspace).map_err(|error| error.to_string())?;
        let router = ToolRouter::new(policy);
        let rejected = router.write_file("input.txt", "tampered").is_err();
        cleanup(&workspace, &PathBuf::new());
        if rejected {
            Ok(())
        } else {
            Err("read-only policy allowed a write".to_owned())
        }
    });

    run_case(
        &mut report,
        "gate detects the intentional regression",
        || {
            let workspace = fixture_workspace(
                "regression",
                include_str!("../fixtures/evals/regression/input.txt"),
            )?;
            let expected = if break_regression {
                "status=broken"
            } else {
                "status=fixed"
            };
            let result = gate::file_contains(&workspace.to_string_lossy(), "input.txt", expected);
            cleanup(&workspace, &PathBuf::new());
            if result.passed {
                Ok(())
            } else {
                Err(result.evidence)
            }
        },
    );

    run_case(&mut report, "model plan is executed through gates", || {
        let workspace = fixture_workspace(
            "model-plan",
            include_str!("../fixtures/evals/repo-fix/input.txt"),
        )?;
        let store = temporary_store("model-plan");
        let response = r#"{
            "version": 1,
            "summary": "model repaired fixture",
            "actions": [
                {"kind": "read", "path": "input.txt"},
                {"kind": "write", "path": "input.txt", "contents": "status=fixed\n"},
                {"kind": "read", "path": "input.txt"},
                {"kind": "gate_contains", "path": "input.txt", "expected": "status=fixed"}
            ]
        }"#;
        let provider = FakeProvider::new(response);
        let request = ModelRequest {
            model: "fake-agent-planner".to_owned(),
            messages: vec![Message::user("repair fixture")],
            temperature: 0.0,
        };
        let result = agent::repo_fix_from_model(&store, &workspace, &provider, &request, None, 0)
            .map_err(|error| error.to_string())?;
        let output = std::fs::read_to_string(workspace.join("input.txt"))
            .map_err(|error| error.to_string())?;
        cleanup(&workspace, &store);
        if output == "status=fixed\n" && result.output == "model repaired fixture" {
            Ok(())
        } else {
            Err("model plan did not produce the expected repair".to_owned())
        }
    });

    report
}

fn run_case<F>(report: &mut EvalReport, name: &str, case: F)
where
    F: FnOnce() -> Result<(), String>,
{
    match case() {
        Ok(()) => report.passed += 1,
        Err(error) => report.failures.push(format!("{name}: {error}")),
    }
}

fn fixture_workspace(name: &str, contents: &str) -> Result<PathBuf, String> {
    let path = std::env::temp_dir().join(format!("agentrt-eval-{name}-{}", new_run_id()));
    std::fs::create_dir_all(&path).map_err(|error| error.to_string())?;
    std::fs::write(path.join("input.txt"), contents).map_err(|error| error.to_string())?;
    Ok(path)
}

fn temporary_store(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("agentrt-eval-{name}-{}.db", new_run_id()))
}

fn cleanup(workspace: &Path, store: &Path) {
    let _ = std::fs::remove_dir_all(workspace);
    if !store.as_os_str().is_empty() {
        let _ = std::fs::remove_file(store);
    }
}

#[cfg(test)]
mod tests {
    use super::run_suite;

    #[test]
    fn default_eval_suite_passes() {
        let report = run_suite(false);
        assert!(report.succeeded(), "{report}");
        assert_eq!(report.total, 6);
    }

    #[test]
    fn intentional_regression_fails_the_suite() {
        let report = run_suite(true);
        assert!(!report.succeeded());
        assert_eq!(report.passed, 5);
    }
}
