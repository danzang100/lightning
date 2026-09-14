use std::fs;
use std::path::Path;
use std::io;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::tools::ToolOutput;
use crate::tools::error::ToolError;
use crate::tools::workspace;

#[derive(Deserialize, Debug, JsonSchema)]
/// This function returns the contents of the folder path specified. Default is workspace root, and any paths outside the workspace directory are strictly forbidden.
pub struct ListDirArgs { 
    /// `dir_path` parameter handles the path of the folder to run the list directory tool on. Default is workspace root. 
    pub dir_path: String,  
}

pub fn list_dir(work_space: &workspace::Workspace, args: ListDirArgs) -> Result<ToolOutput, ToolError> {
    let target:&Path = args.dir_path.as_ref();
    let canonical_target = work_space.resolve(target)?;

    let mut entries = fs::read_dir(&canonical_target)?
        .map(|res| res.map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect::<Result<Vec<_>, io::Error>>()?;

    entries.sort();

    let mut folder_contents= String::new();

    for entry in &entries {
        folder_contents += entry;
        folder_contents += "\n";
    }

    Ok(ToolOutput {
        content: folder_contents,
        metadata: json!({
            "item_count": entries.len()
        }),
    })
}