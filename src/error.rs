//! Error types used for the list
use embedded_storage_async::nor_flash::NorFlashError;
use sequential_storage::Error as SecStorError;

use crate::intrusive::{KEY_LEN, State};

/// General error that is not specific to the flash implementation
#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// Deserializing node from the buffer failed
    #[error("Deserialization failed!")]
    Deserialization,
    /// Serializing a node into the buffer failed
    #[error("Serialization failed! Buffer too small.")]
    Serialization,
    /// Old version of the node was found. Returned when the node does not have the latest
    /// counter value.
    #[error("Only an old instance of this StorageListNode was found")]
    OldData,
    /// The key hash already exists in the list
    #[error("Duplicate key(-hash)!")]
    DuplicateKey,
    /// Node is in a state that is invalid at the particular point of operation.
    /// Contains a tuple of the node key hash and the state that was deemed invalid.
    #[error("Invalid Node State! Key {:?}, {:?}", .0, .1)]
    InvalidState([u8; KEY_LEN], State),
}


// TODO @James: I have created this error because it uses the generic <F>. Adding generics
// to the generall error struct was super awkward. Is there any better way of wrapping errors
// from other crates when they require generics?

#[derive(thiserror::Error, Debug)]
/// Errors during loading from and storing to flash.
/// 
/// Sometimes specific to the flash implementation.
pub enum LoadStoreError<F: NorFlashError> {
    /// Writing to flash has failed. Contains the error returned by sequential storage.
    #[error("Writing to flash has failed: {0:?}")]
    FlashWrite(SecStorError<F>),
    /// Reading from flash has failed. Contains the error returned by sequential storage.
    #[error("Reading from flash has failed: {0:?}")]
    FlashRead(SecStorError<F>),
    /// Value read back from the flash during verification did not match serialized list node.
    #[error("Value written to flash does not match serialized list node.")]
    WriteVerificationFailed,
    /// Recoverable error to tell the caller that the list needs reading first.
    #[error(
        "List needs to be read first because some node is in Initial state and cannot be written."
    )]
    NeedsRead,
    /// Application-level error that occurred during list operations.
    #[error(transparent)]
    AppError(#[from] Error),
}
