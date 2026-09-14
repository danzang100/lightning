use serde::Serialize;

pub mod read_file;  
pub mod list_dir;
pub mod error;
pub mod registry;
pub mod workspace;

#[derive(Serialize)]
pub struct ToolOutput {
    pub content: String,
    pub metadata: serde_json::Value,       
}

pub use read_file::read_file;
pub use list_dir::list_dir;
pub use error::ToolError;
