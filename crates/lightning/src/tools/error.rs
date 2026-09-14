use std::io;
use std::fmt;

#[derive(Debug)]
pub enum ToolError {
    NotFound{path: String},
    PathOutsideProject{path: String},
    InvalidArguments{message: String},
    //ExecutionFailed{command: String, exit_code: i32, stderr: String},
    //Timeout{command: String, duration: i32},
    Io{message: String},
    Other(io::Error),
}

impl std::error::Error for ToolError {}

impl From<io::Error> for ToolError {
    fn from(err: io::Error) -> Self {
        ToolError::Other(err)
    }
}

impl From<serde_json::Error> for ToolError {
    fn from(err: serde_json::Error) -> Self {
        ToolError::InvalidArguments{
            message: err.to_string()
        }
    }
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToolError::NotFound {path} => {
                write!(f, "The file path: {path} is not found")
            }
            ToolError::PathOutsideProject { path } => {
                write!(f, "The file path: {path} is outside the working directory. You do not have access to this")
            }
            ToolError::Io { message } => {
                write!(f, "I/O error occurred: {message}")
            }
            ToolError::InvalidArguments { message } => {
                write!(f, "Invalid tool arguments or tool name used: {message}")
            }
            ToolError::Other(err) => {
                write!(f, "Some error occurred: {}", err)
            }
        }
    }
}

