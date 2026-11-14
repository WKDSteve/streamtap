//! The sink's errors at its CLI boundary (RS-05): a closed enum, each
//! variant with its own exit code.

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use crate::proc::{Pipe, Tool};

#[derive(Debug)]
pub enum SinkError {
    /// Bad command line.
    Usage(String),
    /// A file or socket operation failed.
    Io {
        what: String,
        path: Option<PathBuf>,
        source: io::Error,
    },
    /// The peer broke the wire protocol (bad handshake, malformed record).
    Protocol(String),
    /// The capture holds nothing that can be muxed.
    Capture(String),
    /// The tool is not on `PATH`.
    ToolMissing { tool: Tool },
    /// The tool exists but could not be started or waited for.
    ToolSpawn { tool: Tool, source: io::Error },
    /// The tool ran past its deadline and was killed.
    ToolTimeout { tool: Tool, after: Duration },
    /// The tool wrote more than the pipe limit and was killed.
    ToolOutputLimit {
        tool: Tool,
        pipe: Pipe,
        limit: usize,
    },
    /// The tool exited unsuccessfully; `stderr` is its bounded tail.
    ToolFailed {
        tool: Tool,
        status: String,
        stderr: String,
    },
}

impl SinkError {
    /// The process exit code of this error (documented in the README).
    pub fn exit_code(&self) -> i32 {
        match self {
            SinkError::Io { .. } | SinkError::Protocol(_) => 1,
            SinkError::Usage(_) => 2,
            SinkError::Capture(_) => 3,
            SinkError::ToolMissing { .. } | SinkError::ToolSpawn { .. } => 4,
            SinkError::ToolTimeout { .. } | SinkError::ToolOutputLimit { .. } => 5,
            SinkError::ToolFailed { .. } => 6,
        }
    }

    pub fn io(what: impl Into<String>, path: Option<PathBuf>, source: io::Error) -> Self {
        SinkError::Io {
            what: what.into(),
            path,
            source,
        }
    }
}

impl fmt::Display for SinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SinkError::Usage(m) | SinkError::Capture(m) | SinkError::Protocol(m) => f.write_str(m),
            SinkError::Io {
                what,
                path: Some(p),
                source,
            } => write!(f, "{what} {}: {source}", p.display()),
            SinkError::Io {
                what,
                path: None,
                source,
            } => write!(f, "{what}: {source}"),
            SinkError::ToolMissing { tool } => write!(
                f,
                "{tool} not found on PATH; install ffmpeg (it ships ffprobe), e.g. `scoop install ffmpeg`"
            ),
            SinkError::ToolSpawn { tool, source } => write!(f, "cannot run {tool}: {source}"),
            SinkError::ToolTimeout { tool, after } => {
                write!(f, "{tool} did not finish within {} s; killed", after.as_secs())
            }
            SinkError::ToolOutputLimit { tool, pipe, limit } => write!(
                f,
                "{tool} wrote more than {} KiB to {pipe}; killed",
                limit / 1024
            ),
            SinkError::ToolFailed {
                tool,
                status,
                stderr,
            } => {
                write!(f, "{tool} failed ({status})")?;
                if !stderr.is_empty() {
                    write!(f, ":\n{stderr}")?;
                }
                Ok(())
            }
        }
    }
}

/// A message that is not in the wire format is a protocol error of its connection.
impl From<streamtap_core::wire::WireError> for SinkError {
    fn from(e: streamtap_core::wire::WireError) -> Self {
        SinkError::Protocol(e.0)
    }
}

impl std::error::Error for SinkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SinkError::Io { source, .. } | SinkError::ToolSpawn { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant, with its exit code: one code per failure class, none
    /// shared with success or with another class.
    #[test]
    fn every_variant_has_its_own_exit_code() {
        let io_err = || io::Error::other("x");
        let cases = [
            (SinkError::io("read", None, io_err()), 1),
            (SinkError::Protocol("p".into()), 1),
            (SinkError::Usage("u".into()), 2),
            (SinkError::Capture("c".into()), 3),
            (SinkError::ToolMissing { tool: Tool::Ffmpeg }, 4),
            (
                SinkError::ToolSpawn {
                    tool: Tool::Ffprobe,
                    source: io_err(),
                },
                4,
            ),
            (
                SinkError::ToolTimeout {
                    tool: Tool::Ffmpeg,
                    after: Duration::from_secs(3),
                },
                5,
            ),
            (
                SinkError::ToolOutputLimit {
                    tool: Tool::Ffmpeg,
                    pipe: Pipe::Stdout,
                    limit: 1024,
                },
                5,
            ),
            (
                SinkError::ToolFailed {
                    tool: Tool::Ffmpeg,
                    status: "exit code 1".into(),
                    stderr: String::new(),
                },
                6,
            ),
        ];
        for (err, code) in &cases {
            assert_eq!(err.exit_code(), *code, "{err:?}");
            assert_ne!(err.to_string(), "");
        }
    }

    #[test]
    fn a_missing_tool_says_how_to_get_it() {
        let msg = SinkError::ToolMissing { tool: Tool::Ffmpeg }.to_string();
        assert!(msg.starts_with("ffmpeg not found on PATH"), "{msg}");
        assert!(msg.contains("install"), "{msg}");
    }
}
