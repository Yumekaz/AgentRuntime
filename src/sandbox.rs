//! Typed filesystem tools and constrained execution boundaries.

use std::collections::HashSet;
use std::fmt;
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum Tool {
    ReadFile,
    WriteFile,
    ListDir,
}

impl Tool {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::ReadFile => "read_file",
            Self::WriteFile => "write_file",
            Self::ListDir => "list_dir",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "read_file" => Some(Self::ReadFile),
            "write_file" => Some(Self::WriteFile),
            "list_dir" => Some(Self::ListDir),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(crate) enum SandboxError {
    InvalidRoot(PathBuf),
    Denied { rule: String, attempted: PathBuf },
    Io(std::io::Error),
}

impl fmt::Display for SandboxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRoot(root) => {
                write!(formatter, "workspace root is invalid: {}", root.display())
            }
            Self::Denied { rule, attempted } => write!(
                formatter,
                "sandbox denied `{}`: {}",
                attempted.display(),
                rule
            ),
            Self::Io(error) => write!(formatter, "sandbox I/O error: {error}"),
        }
    }
}

impl std::error::Error for SandboxError {}

impl From<std::io::Error> for SandboxError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Policy {
    root: PathBuf,
    allowed_tools: HashSet<Tool>,
    allow_write: bool,
}

impl Policy {
    pub(crate) fn workspace(root: impl AsRef<Path>) -> Result<Self, SandboxError> {
        let root = std::fs::canonicalize(root.as_ref())?;
        if !root.is_dir() {
            return Err(SandboxError::InvalidRoot(root));
        }

        Ok(Self {
            root,
            allowed_tools: HashSet::from([Tool::ReadFile, Tool::WriteFile, Tool::ListDir]),
            allow_write: true,
        })
    }

    pub(crate) fn read_only(root: impl AsRef<Path>) -> Result<Self, SandboxError> {
        let mut policy = Self::workspace(root)?;
        policy.allow_write = false;
        policy.allowed_tools.remove(&Tool::WriteFile);
        Ok(policy)
    }

    pub(crate) fn deny_tool(mut self, tool: Tool) -> Self {
        self.allowed_tools.remove(&tool);
        self
    }
}

pub(crate) struct ToolRouter {
    policy: Policy,
}

impl ToolRouter {
    pub(crate) fn new(policy: Policy) -> Self {
        Self { policy }
    }

    pub(crate) fn read_file(&self, relative_path: &str) -> Result<String, SandboxError> {
        self.require_tool(Tool::ReadFile, relative_path)?;
        let path = self.resolve_existing(relative_path)?;
        Ok(std::fs::read_to_string(path)?)
    }

    pub(crate) fn write_file(
        &self,
        relative_path: &str,
        contents: &str,
    ) -> Result<(), SandboxError> {
        self.require_tool(Tool::WriteFile, relative_path)?;
        if !self.policy.allow_write {
            return Err(SandboxError::Denied {
                rule: "writes are disabled by policy".to_owned(),
                attempted: PathBuf::from(relative_path),
            });
        }

        let path = self.resolve_for_write(relative_path)?;
        std::fs::write(path, contents)?;
        Ok(())
    }

    pub(crate) fn list_dir(&self, relative_path: &str) -> Result<Vec<String>, SandboxError> {
        self.require_tool(Tool::ListDir, relative_path)?;
        let path = self.resolve_existing(relative_path)?;
        let mut entries = std::fs::read_dir(path)?
            .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
            .collect::<Result<Vec<_>, _>>()?;
        entries.sort();
        Ok(entries)
    }

    fn require_tool(&self, tool: Tool, attempted: &str) -> Result<(), SandboxError> {
        if self.policy.allowed_tools.contains(&tool) {
            Ok(())
        } else {
            Err(SandboxError::Denied {
                rule: format!("tool `{}` is not allowed", tool.name()),
                attempted: PathBuf::from(attempted),
            })
        }
    }

    fn resolve_existing(&self, relative_path: &str) -> Result<PathBuf, SandboxError> {
        validate_relative(relative_path)?;
        let candidate = self.policy.root.join(relative_path);
        let resolved = std::fs::canonicalize(&candidate)?;
        self.ensure_inside_root(&resolved, relative_path)?;
        Ok(resolved)
    }

    fn resolve_for_write(&self, relative_path: &str) -> Result<PathBuf, SandboxError> {
        validate_relative(relative_path)?;
        let candidate = self.policy.root.join(relative_path);
        let Some(file_name) = candidate.file_name() else {
            return Err(SandboxError::Denied {
                rule: "write target must name a file".to_owned(),
                attempted: candidate,
            });
        };
        let parent = candidate.parent().ok_or_else(|| SandboxError::Denied {
            rule: "write target has no valid parent".to_owned(),
            attempted: candidate.clone(),
        })?;
        let resolved_parent = std::fs::canonicalize(parent)?;
        self.ensure_inside_root(&resolved_parent, relative_path)?;
        Ok(resolved_parent.join(file_name))
    }

    fn ensure_inside_root(&self, resolved: &Path, attempted: &str) -> Result<(), SandboxError> {
        if resolved.starts_with(&self.policy.root) {
            Ok(())
        } else {
            Err(SandboxError::Denied {
                rule: "path escapes workspace root".to_owned(),
                attempted: PathBuf::from(attempted),
            })
        }
    }
}

fn validate_relative(relative_path: &str) -> Result<(), SandboxError> {
    let path = Path::new(relative_path);
    if path.as_os_str().is_empty()
        || path.components().any(|component| {
            matches!(
                component,
                Component::Prefix(_) | Component::RootDir | Component::ParentDir
            )
        })
    {
        return Err(SandboxError::Denied {
            rule: "only relative paths without `..` are allowed".to_owned(),
            attempted: path.to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Policy, SandboxError, Tool, ToolRouter};
    use crate::run::new_run_id;
    use std::path::PathBuf;

    fn temporary_root() -> PathBuf {
        std::env::temp_dir().join(format!("agentrt-sandbox-{}", new_run_id()))
    }

    #[test]
    fn legal_filesystem_tools_work_inside_workspace() {
        let root = temporary_root();
        std::fs::create_dir_all(&root).expect("workspace creates");
        std::fs::write(root.join("input.txt"), "hello").expect("fixture writes");

        let router = ToolRouter::new(Policy::workspace(&root).expect("policy creates"));
        assert_eq!(router.read_file("input.txt").expect("file reads"), "hello");
        router
            .write_file("output.txt", "safe")
            .expect("file writes");
        assert_eq!(
            router.list_dir(".").expect("directory lists"),
            vec!["input.txt", "output.txt"]
        );

        std::fs::remove_dir_all(root).expect("workspace removes");
    }

    #[test]
    fn traversal_and_denied_writes_are_blocked() {
        let root = temporary_root();
        std::fs::create_dir_all(&root).expect("workspace creates");
        let outside = root.parent().unwrap().join(format!(
            "{}-outside.txt",
            root.file_name().unwrap().to_string_lossy()
        ));
        std::fs::write(&outside, "must remain").expect("outside fixture writes");

        let router = ToolRouter::new(Policy::read_only(&root).expect("policy creates"));
        assert!(matches!(
            router.read_file("../outside.txt"),
            Err(SandboxError::Denied { .. })
        ));
        assert!(matches!(
            router.write_file("blocked.txt", "nope"),
            Err(SandboxError::Denied { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(&outside).expect("outside reads"),
            "must remain"
        );

        let denied_router = ToolRouter::new(
            Policy::workspace(&root)
                .expect("policy creates")
                .deny_tool(Tool::ReadFile),
        );
        assert!(matches!(
            denied_router.read_file("../outside.txt"),
            Err(SandboxError::Denied { .. })
        ));

        std::fs::remove_file(outside).expect("outside fixture removes");
        std::fs::remove_dir_all(root).expect("workspace removes");
    }
}
