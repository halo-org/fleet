use core::{error::Error, fmt};

/// An authentication failure.
#[derive(Debug)]
pub enum AuthenticationError {
    /// The connection or method does not exist.
    NotFound,
    /// The request or provider settings are invalid.
    Invalid(&'static str),
    /// The connection needs authorization.
    Unauthorized,
    /// The authorization attempt expired or was cancelled.
    Cancelled,
    /// The authorization server rejected a request.
    Rejected(u16),
    /// The callback listener could not be started.
    Callback(std::io::Error),
    /// An authentication operation failed.
    Other(&'static str, Box<dyn Error + Send + Sync>),
}

impl AuthenticationError {
    pub fn source(message: &'static str, error: impl Error + Send + Sync + 'static) -> Self {
        Self::Other(message, Box::new(error))
    }
}

impl fmt::Display for AuthenticationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("connection or sign-in method not found"),
            Self::Invalid(message) | Self::Other(message, _) => f.write_str(message),
            Self::Unauthorized => f.write_str("connection needs authorization"),
            Self::Cancelled => f.write_str("sign-in expired or was cancelled"),
            Self::Rejected(status) => write!(f, "authorization server returned HTTP {status}"),
            Self::Callback(_) => f.write_str("could not start sign-in callback listener"),
        }
    }
}

impl Error for AuthenticationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Callback(error) => Some(error),
            Self::Other(_, error) => Some(error.as_ref()),
            _ => None,
        }
    }
}
