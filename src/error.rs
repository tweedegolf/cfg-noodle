//! Error types used for the list
use crate::storage_node::State;

/// General error that is not specific to the flash implementation
#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Error {
    /// Deserializing node from the buffer failed
    Deserialization,
    /// Serializing a node into the buffer failed
    Serialization,
    /// The key hash already exists in the list
    DuplicateKey,
    /// Recoverable error to tell the caller that the list needs reading first.
    NeedsRead,
    /// Node is in a state that is invalid at the particular point of operation.
    /// Contains a tuple of the node key hash and the state that was deemed invalid.
    InvalidState(&'static str, State),
}

#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
/// Errors during loading from and storing to flash.
///
/// Sometimes specific to the flash implementation.
pub enum LoadStoreError<T> {
    /// Needs Initial read processed
    NeedsFirstRead,
    /// Writing to flash has failed. Contains the error returned by the storage impl.
    FlashWrite(T),
    /// Reading from flash has failed. Contains the error returned by the storage impl.
    FlashRead(T),
    /// Value read back from the flash during verification did not match serialized list node.
    WriteVerificationFailed,
    /// Application-level error that occurred during list operations.
    AppError(Error),
}

impl<T> From<Error> for LoadStoreError<T> {
    fn from(value: Error) -> Self {
        Self::AppError(value)
    }
}
