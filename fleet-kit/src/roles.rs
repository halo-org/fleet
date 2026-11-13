use alloc::{boxed::Box, rc::Rc, sync::Arc, vec::Vec};
use core::fmt;
use fleet_core::{Role, RoleName};

/// The version of a stored role.
///
/// Versions are unique within a store, so a deleted and recreated role has a new version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RoleVersion(u128);

/// A role with its version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRole {
    /// The role.
    pub role: Role,
    /// The version of the role.
    pub version: RoleVersion,
}

/// Provides versioned access to stored roles.
///
/// Agents read roles through [`fleet_core::Roles`], so edits apply from their next lookup.
///
/// # Idempotency
///
/// Mutations called repeatedly with the same idempotency key and arguments perform their side
/// effects once. After a mutation succeeds, matching retries return success even if the role has
/// since changed. Reusing a key with different arguments returns an
/// [`IdempotencyConflict`](RoleStoreErrorKind::IdempotencyConflict) error.
pub trait RoleStore {
    /// The error type returned by this provider.
    type Error: fleet_core::Error<RoleStoreErrorKind> + 'static;

    /// Lists the stored roles by name.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider fails to read the roles.
    fn roles(&self) -> impl Future<Output = Result<Vec<StoredRole>, Self::Error>> + Send;

    /// Returns a stored role.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider fails to perform the lookup.
    fn role(
        &self,
        name: &RoleName,
    ) -> impl Future<Output = Result<Option<StoredRole>, Self::Error>> + Send;

    /// Creates or replaces a role if its version matches.
    ///
    /// An `expected` version of `None` creates the role only if it does not exist. Storing an
    /// identical role keeps its version.
    ///
    /// # Errors
    ///
    /// Returns errors with the following error kinds:
    ///
    /// - [`PreconditionFailed`](RoleStoreErrorKind::PreconditionFailed) when the role's version
    ///   does not match `expected`.
    /// - [`IdempotencyConflict`](RoleStoreErrorKind::IdempotencyConflict) when the key was used
    ///   with different arguments.
    /// - [`InvalidRole`](RoleStoreErrorKind::InvalidRole) when the provider does not accept the
    ///   role.
    /// - [`Other`](RoleStoreErrorKind::Other) when there are any other provider specific errors.
    fn put_role(
        &self,
        idempotency_key: &str,
        role: &Role,
        expected: Option<RoleVersion>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// The kind of error returned by a role store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RoleStoreErrorKind {
    /// The role's version does not match the expected version.
    PreconditionFailed,
    /// The idempotency key was used with different arguments.
    IdempotencyConflict,
    /// The provider does not accept the role.
    InvalidRole,
    /// An error not covered by any other kind.
    Other,
}

macro_rules! impl_role_store_for_pointer {
    ($t:ty) => {
        #[warn(clippy::missing_trait_methods)]
        impl<T: RoleStore + ?Sized> RoleStore for $t {
            type Error = T::Error;

            fn roles(&self) -> impl Future<Output = Result<Vec<StoredRole>, Self::Error>> + Send {
                (**self).roles()
            }

            fn role(
                &self,
                name: &RoleName,
            ) -> impl Future<Output = Result<Option<StoredRole>, Self::Error>> + Send {
                (**self).role(name)
            }

            fn put_role(
                &self,
                idempotency_key: &str,
                role: &Role,
                expected: Option<RoleVersion>,
            ) -> impl Future<Output = Result<(), Self::Error>> + Send {
                (**self).put_role(idempotency_key, role, expected)
            }
        }
    };
}

impl_role_store_for_pointer!(&T);
impl_role_store_for_pointer!(&mut T);
impl_role_store_for_pointer!(Box<T>);
impl_role_store_for_pointer!(Rc<T>);
impl_role_store_for_pointer!(Arc<T>);

impl From<u128> for RoleVersion {
    fn from(value: u128) -> Self {
        Self(value)
    }
}

impl From<RoleVersion> for u128 {
    fn from(value: RoleVersion) -> Self {
        value.0
    }
}

impl fmt::Display for RoleStoreErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PreconditionFailed => f.write_str("role changed"),
            Self::IdempotencyConflict => f.write_str("idempotency conflict"),
            Self::InvalidRole => f.write_str("invalid role"),
            Self::Other => f.write_str("other error"),
        }
    }
}
