use alloc::boxed::Box;
use core::convert::Infallible;

/// An error that can be classified.
///
/// Classifying an error into a [`kind`](Self::kind) lets callers respond to it programmatically
/// without knowing the concrete error type. The kind is usually an enum with each variant being a
/// high-level classification that may be acted on.
pub trait Error<K>: core::error::Error {
    /// Returns the kind of this error.
    fn kind(&self) -> K;
}

impl<K> Error<K> for Infallible {
    fn kind(&self) -> K {
        #[expect(
            clippy::uninhabited_references,
            reason = "a reference to Infallible can never be constructed, so the dereference is unreachable"
        )]
        match *self {}
    }
}

impl<E, K> Error<K> for Box<E>
where
    E: Error<K>,
{
    fn kind(&self) -> K {
        (**self).kind()
    }
}
