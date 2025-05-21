#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Deserialization failed!")]
    Deserialization,
    #[error("Duplicate key!")]
    DuplicateKey
}
