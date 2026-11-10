//! The final status of a session, as the client gets it.

use super::{SUCCESS_STATUS, Version};
use anyhow::{Context, Result};
use serde::Serialize;
use std::fmt;

/// What a session runs, which the status of its failure names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Operation {
    Exec,
    Attach,
}

impl Operation {
    /// What k8s.io/cri-streaming puts before the error of a session.
    fn context(self) -> &'static str {
        match self {
            Self::Exec => "error executing command in container",
            Self::Attach => "error attaching to container",
        }
    }
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Exec => "exec",
            Self::Attach => "attach",
        })
    }
}

/// The final status of a session.
#[derive(Debug, PartialEq)]
pub(crate) enum Status {
    Success,
    ExitCode(i32),

    /// The session itself failed, for example a command which could not be
    /// started. The message is the one of the internal error, see
    /// [`Status::failed`].
    Error(String),
}

impl Status {
    /// The status of a command which exited with `code`. A negative one
    /// tells that it could not be waited for, clients only take exit codes
    /// from 1 to 255.
    pub(crate) fn exited(code: i32) -> Self {
        match code {
            0 => Self::Success,
            1.. => Self::ExitCode(code),
            _ => Self::failed(
                Operation::Exec,
                format_args!("unable to get the exit code of the command ({code})"),
            ),
        }
    }

    /// The failure of a session, worded like k8s.io/cri-streaming does.
    pub(crate) fn failed(operation: Operation, error: impl fmt::Display) -> Self {
        Self::Error(format!(
            "Internal error occurred: {}: {error}",
            operation.context()
        ))
    }

    /// The status on the error stream, none for a success before v4.
    pub(super) fn encode(&self, version: Version) -> Result<Option<Vec<u8>>> {
        if version.json_status() {
            return match self {
                Self::Success => Ok(Some(SUCCESS_STATUS.to_vec())),
                Self::ExitCode(code) => Self::json(&ErrorMessage::new(code)).map(Some),
                Self::Error(message) => {
                    Self::json(&ErrorMessage::internal(message.clone())).map(Some)
                }
            };
        }
        Ok(match self {
            Self::Success => None,
            Self::ExitCode(code) => Some(
                format!("command terminated with non-zero exit code: exit status {code}")
                    .into_bytes(),
            ),
            Self::Error(message) => Some(message.clone().into_bytes()),
        })
    }

    fn json(status: &impl Serialize) -> Result<Vec<u8>> {
        serde_json::to_vec(status).context("serialize status message")
    }
}

/// The status of a failed session in JSON, the fields in the order of
/// k8s.io/cri-streaming.
#[derive(Debug, Serialize)]
pub(super) struct ErrorMessage {
    status: &'static str,
    message: String,
    reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<ErrorDetails>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<u16>,
}

impl ErrorMessage {
    /// The status of a command which exited with the code, its message ends
    /// with the exit error like the text status before v4.
    pub(super) fn new<T>(exit_code: T) -> Self
    where
        T: fmt::Display,
    {
        Self {
            status: "Failure",
            message: format!("command terminated with non-zero exit code: exit status {exit_code}"),
            reason: "NonZeroExitCode",
            details: Some(ErrorDetails {
                causes: vec![ErrorCause {
                    reason: "ExitCode",
                    message: exit_code.to_string(),
                }],
            }),
            code: None,
        }
    }

    /// An error of the session itself, for example a command which could not
    /// be started, see [`Status::failed`].
    fn internal(message: String) -> Self {
        Self {
            status: "Failure",
            message,
            reason: "InternalError",
            details: None,
            code: Some(500),
        }
    }
}

/// Error details for the ErrorMessage.
#[derive(Debug, Serialize)]
struct ErrorDetails {
    causes: Vec<ErrorCause>,
}

/// Error cause for the ErrorDetails.
#[derive(Debug, Serialize)]
struct ErrorCause {
    reason: &'static str,
    message: String,
}
