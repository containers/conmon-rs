use std::fmt::Display;

/// Library errors.
#[derive(thiserror::Error, Debug)]
#[error("libsystemd error: {msg}")]
pub struct SdError {
    pub(crate) kind: ErrorKind,
    pub(crate) msg: String,
}

impl From<&str> for SdError {
    fn from(arg: &str) -> Self {
        Self {
            kind: ErrorKind::Generic,
            msg: arg.to_string(),
        }
    }
}

impl From<String> for SdError {
    fn from(arg: String) -> Self {
        Self {
            kind: ErrorKind::Generic,
            msg: arg,
        }
    }
}

/// Markers for recoverable error kinds.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ErrorKind {
    Generic,
    SysusersUnknownType,
}

/// Context is similar to anyhow::Context, in that it provides a mechanism internally to adapt
/// errors from systemd into SdError, while providing additional context in a readable manner.
pub(crate) trait Context<T, E> {
    /// Prepend the error with context.
    fn context<C>(self, context: C) -> Result<T, SdError>
    where
        C: Display + Send + Sync + 'static;

    /// Prepend the error with context that is lazily evaluated.
    fn with_context<C, F>(self, f: F) -> Result<T, SdError>
    where
        C: Display + Send + Sync + 'static,
        F: FnOnce() -> C;
}

impl<T, E> Context<T, E> for Result<T, E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn context<C>(self, context: C) -> Result<T, SdError>
    where
        C: Display + Send + Sync + 'static,
    {
        self.map_err(|e| format!("{}: {}", context, e).into())
    }

    fn with_context<C, F>(self, context: F) -> Result<T, SdError>
    where
        C: Display + Send + Sync + 'static,
        F: FnOnce() -> C,
    {
        self.map_err(|e| format!("{}: {}", context(), e).into())
    }
}

impl<T> Context<T, core::convert::Infallible> for Option<T> {
    fn context<C>(self, context: C) -> Result<T, SdError>
    where
        C: Display + Send + Sync + 'static,
    {
        self.ok_or_else(|| format!("{}", context).into())
    }

    fn with_context<C, F>(self, context: F) -> Result<T, SdError>
    where
        C: Display + Send + Sync + 'static,
        F: FnOnce() -> C,
    {
        self.ok_or_else(|| format!("{}", context()).into())
    }
}
