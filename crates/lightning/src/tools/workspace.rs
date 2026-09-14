use std::path::{Path, PathBuf};
use crate::tools::ToolError;
use std::fs;
use std::io::ErrorKind;

pub struct Workspace {
    root: PathBuf
}

impl Workspace {
    pub fn new(root: impl AsRef<Path>) -> Result<Self, ToolError>{
        let canonical_root = fs::canonicalize(root.as_ref())?;
        Ok(Workspace { root: canonical_root })
    }

    pub fn resolve(&self, target_path: impl AsRef<Path>) -> Result<PathBuf, ToolError> {
    let target = target_path.as_ref();

    let canonical_target = fs::canonicalize(self.root.join(target)).map_err(
        |err| match err.kind() {
            ErrorKind::PermissionDenied => ToolError::Io {
                message: format!("Permission denied accessing: {}", target.display()),
            },
            ErrorKind::NotFound => ToolError::NotFound {
                path: target.display().to_string(),
            },
            _ => ToolError::Other(err),
        }
    )?;

    if !canonical_target.starts_with(&self.root) {
        Err(ToolError::PathOutsideProject {
            path: target.display().to_string(),
        })
    } else {
        Ok(canonical_target)
    }

    }

}

