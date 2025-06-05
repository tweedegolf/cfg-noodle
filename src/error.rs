//! Error types used for the list
use sequential_storage::Error as SecStorError;

use crate::intrusive::{KEY_LEN, State};

/// General error that is not specific to the flash implementation
#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Error {
    /// Deserializing node from the buffer failed
    Deserialization,
    /// Serializing a node into the buffer failed
    Serialization,
    /// Old version of the node was found. Returned when the node does not have the latest
    /// counter value.
    OldData,
    /// The key hash already exists in the list
    DuplicateKey,
    /// Node is in a state that is invalid at the particular point of operation.
    /// Contains a tuple of the node key hash and the state that was deemed invalid.
    InvalidState([u8; KEY_LEN], State),
}

// TODO @James: I have created this error because it uses the generic <F>. Adding generics
// to the generall error struct was super awkward. Is there any better way of wrapping errors
// from other crates when they require generics?

#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
/// Errors during loading from and storing to flash.
///
/// Sometimes specific to the flash implementation.
pub enum LoadStoreError<F> {
    /// Writing to flash has failed. Contains the error returned by sequential storage.
    FlashWrite(SecStorError<F>),
    /// Reading from flash has failed. Contains the error returned by sequential storage.
    FlashRead(SecStorError<F>),
    /// Value read back from the flash during verification did not match serialized list node.
    WriteVerificationFailed,
    /// Recoverable error to tell the caller that the list needs reading first.
    NeedsRead,
    /// Application-level error that occurred during list operations.
    AppError(Error),
}
