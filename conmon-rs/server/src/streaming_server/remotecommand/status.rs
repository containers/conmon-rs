//! The final status of a session, as the client gets it.

use super::{SUCCESS_STATUS, Version};
use anyhow::{Context, Result};
use serde::Serialize;

/// The final status of a session.
#[derive(Debug, PartialEq)]
pub enum Status {
    Success,
    ExitCode(i32),

    /// The session itself failed, for example a command which could not be
    /// started.
    Error(String),
}

impl Status {
    /// The status of a command which exited with `code`. A negative one
    /// tells that it could not be waited for, clients only take exit codes
    /// from 1 to 255.
    pub fn exited(code: i32) -> Self {
        match code {
            0 => Self::Success,
            1.. => Self::ExitCode(code),
            _ => Self::Error(format!(
                "unable to get the exit code of the command ({code})"
            )),
        }
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

#[derive(Debug, Serialize)]
/// Error message type used for exec in case that the command fails.
pub struct ErrorMessage {
    status: &'static str,
    reason: &'static str,
    details: ErrorDetails,
    message: String,
}

impl ErrorMessage {
    pub fn new<T>(exit_code: T) -> Self
    where
        T: ToString,
    {
        Self {
            status: "Failure",
            reason: "NonZeroExitCode",
            details: ErrorDetails {
                causes: vec![ErrorCause {
                    reason: "ExitCode",
                    message: exit_code.to_string(),
                }],
            },
            message: "command terminated with non-zero exit code".into(),
        }
    }

    /// An error of the session itself, for example a command which could not
    /// be started.
    pub fn internal(message: String) -> Self {
        Self {
            status: "Failure",
            reason: "InternalError",
            details: ErrorDetails { causes: vec![] },
            message,
        }
    }
}

#[derive(Debug, Serialize)]
/// Error details for the ErrorMessage.
struct ErrorDetails {
    causes: Vec<ErrorCause>,
}

#[derive(Debug, Serialize)]
/// Error cause for the ErrorDetails.
struct ErrorCause {
    reason: &'static str,
    message: String,
}
