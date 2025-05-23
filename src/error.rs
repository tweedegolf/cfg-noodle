use crate::intrusive::KEY_LEN;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Deserialization failed!")]
    Deserialization,
    #[error("Serialization failed!")]
    Serialization,
    #[error("Duplicate key!")]
    DuplicateKey,
    #[error("Invalid Node State! Key {0:?}")]
    InvalidState([u8; KEY_LEN])
}
