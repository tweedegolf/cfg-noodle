use embedded_storage_async::nor_flash::NorFlashError;
use sequential_storage::Error as SecStorError;

use crate::intrusive::{KEY_LEN, State};

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
    #[error("Invalid Node State! Key {:?}, {:?}", .0, .1)]
    InvalidState([u8; KEY_LEN], State),
}

#[derive(thiserror::Error, Debug)]
pub enum LoadStoreError<F: NorFlashError> {
    #[error("Writing to flash has failed.")]
    FlashWrite(SecStorError<F>),
    #[error("Reading from flash has failed.")]
    FlashRead(SecStorError<F>),
    #[error("Value written to flash does not match serialized list node.")]
    WriteVerificationFailed,
    #[error(transparent)]
    AppError(#[from] Error)
}
