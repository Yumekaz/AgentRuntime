//! Deterministic gates that turn workspace facts into explicit pass/fail results.

use crate::sandbox::{Policy, ToolRouter};
use std::fmt;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GateResult {
    pub(crate) name: String,
    pub(crate) passed: bool,
    pub(crate) evidence: String,
}

impl GateResult {
    fn pass(name: impl Into<String>, evidence: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            passed: true,
            evidence: evidence.into(),
        }
    }

    fn fail(name: impl Into<String>, evidence: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            passed: false,
            evidence: evidence.into(),
        }
    }
}

impl fmt::Display for GateResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} {}: {}",
            if self.passed { "PASS" } else { "FAIL" },
            self.name,
            self.evidence
        )
    }
}

pub(crate) fn file_exists(workspace: &str, relative_path: &str) -> GateResult {
    let name = format!("file_exists:{relative_path}");
    let policy = match Policy::read_only(workspace) {
        Ok(policy) => policy,
        Err(error) => return GateResult::fail(name, error.to_string()),
    };
    let router = ToolRouter::new(policy);
    match router.list_dir(relative_path) {
        Ok(_) => GateResult::pass(name, "path exists and is a directory"),
        Err(_) => match router.read_file(relative_path) {
            Ok(_) => GateResult::pass(name, "path exists and is a file"),
            Err(error) => GateResult::fail(name, error.to_string()),
        },
    }
}

pub(crate) fn file_contains(workspace: &str, relative_path: &str, expected: &str) -> GateResult {
    let name = format!("file_contains:{relative_path}");
    let policy = match Policy::read_only(workspace) {
        Ok(policy) => policy,
        Err(error) => return GateResult::fail(name, error.to_string()),
    };
    let router = ToolRouter::new(policy);
    match router.read_file(relative_path) {
        Ok(contents) if contents.contains(expected) => {
            GateResult::pass(name, format!("found expected text `{expected}`"))
        }
        Ok(_) => GateResult::fail(name, format!("expected text `{expected}` was absent")),
        Err(error) => GateResult::fail(name, error.to_string()),
    }
}

pub(crate) fn evaluate_all(results: &[GateResult]) -> bool {
    results.iter().all(|result| result.passed)
}

#[cfg(test)]
mod tests {
    use super::{evaluate_all, file_contains, file_exists};
    use crate::run::new_run_id;
    use std::path::PathBuf;

    fn workspace() -> PathBuf {
        std::env::temp_dir().join(format!("agentrt-gate-{}", new_run_id()))
    }

    #[test]
    fn gates_are_deterministic_and_fail_closed() {
        let root = workspace();
        std::fs::create_dir_all(&root).expect("workspace creates");
        std::fs::write(root.join("result.txt"), "fixed=true\n").expect("fixture writes");
        let root_string = root.to_string_lossy();

        let exists = file_exists(&root_string, "result.txt");
        let contains = file_contains(&root_string, "result.txt", "fixed=true");
        let broken = file_contains(&root_string, "result.txt", "fixed=false");
        assert!(exists.passed);
        assert!(contains.passed);
        assert!(!broken.passed);
        assert!(evaluate_all(&[exists, contains]));
        assert!(!evaluate_all(&[broken]));

        std::fs::remove_dir_all(root).expect("workspace removes");
    }
}
