use core::fmt;

/// An injected simulation error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RandomError;

impl fmt::Display for RandomError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("random error")
    }
}

impl core::error::Error for RandomError {}
