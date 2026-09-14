use std::fs;
use std::path::Path;
use crate::tools::ToolOutput;
use crate::tools::error::ToolError;
use crate::tools::workspace;
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, Debug, JsonSchema)]
/// This function returns the contents of the file specified. Default is workspace root directory, and relative paths to the workspace must be provided and any paths outside the workspace directory are strictly forbidden.
pub struct ReadFileArgs{ 
    /// `file_path` parameter handles the file path to run the read file tool on. All paths must be relative to the workspace root directory. 
    pub file_path: String
}

pub fn read_file(work_space: &workspace::Workspace, args: ReadFileArgs) -> Result<ToolOutput, ToolError> {
    let target: &Path = args.file_path.as_ref(); 
    let canonical_target = work_space.resolve(target)?;

    let raw_contents = fs::read_to_string(&canonical_target)
    .map_err(|err| ToolError::Io{
        message: format!("Failed reading {}: {err}", target.display()),
    });

    let content_str = raw_contents?;
    let line_count = content_str.lines().count();

    Ok(ToolOutput {
        content: content_str,
        metadata: serde_json::json!({
            "line_count": line_count
        })
    })
}