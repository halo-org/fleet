use fleet_core::JsonObject;
use serde::{Deserialize as _, Serialize, de::DeserializeOwned};

/// A typed entry in an extensions object.
pub trait Extension: Serialize + DeserializeOwned {
    /// The key of this entry.
    const KEY: &'static str;

    /// Reads this entry. A missing or null entry is `None`.
    ///
    /// # Errors
    ///
    /// Returns an error if the entry does not match this type.
    fn get(extensions: &JsonObject) -> Result<Option<Self>, serde_json::Error> {
        extensions
            .get(Self::KEY)
            .map_or(Ok(None), Option::<Self>::deserialize)
    }

    /// Writes this entry.
    ///
    /// # Errors
    ///
    /// Returns an error if this value cannot be represented as JSON.
    fn insert(self, extensions: &mut JsonObject) -> Result<(), serde_json::Error> {
        extensions.insert(Self::KEY.into(), serde_json::to_value(self)?);
        Ok(())
    }
}
