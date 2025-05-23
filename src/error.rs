use crate::intrusive::{State, KEY_LEN};

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Deserialization failed!")]
    Deserialization,
    #[error("Serialization failed! Buffer too small.")]
    Serialization,
    #[error("Duplicate key!")]
    DuplicateKey,
    #[error("Invalid Node State! Key {:?}, {:?}", 0.0, 0.0)]
    InvalidState(([u8; KEY_LEN], State))
}
