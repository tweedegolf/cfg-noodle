use mutex::ScopedRawMutex;

use crate::intrusive::{KEY_LEN, State, StorageListNodeHandle};

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Deserialization failed!")]
    Deserialization,
    #[error("Serialization failed! Buffer too small.")]
    Serialization,
    #[error("Only an old instance of this StorageListNode was found")]
    OldData,
    #[error("Duplicate key!")]
    DuplicateKey,
    #[error("Invalid Node State! Key {:?}, {:?}", 0.0, 0.0)]
    InvalidState(([u8; KEY_LEN], State)),
}
