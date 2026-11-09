use core::{error::Error, fmt};

/// An error formatted with its sources.
pub struct ErrorChain<'a>(pub &'a dyn Error);

impl fmt::Display for ErrorChain<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)?;
        let mut source = self.0.source();

        while let Some(error) = source {
            write!(f, "\n  caused by: {error}")?;
            source = error.source();
        }

        Ok(())
    }
}
