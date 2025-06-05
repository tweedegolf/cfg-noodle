//! Flash storage wrapper for sequential storage operations.
//!
//! This module provides a `Flash` struct that wraps a MultiwriteNorFlash device
//! and exposes async methods for queue-like operations on persistent storage.

#[cfg(feature = "sequential-storage")]
pub mod sequential_storage_backend;

/// Simple iterator providing a `next` function to iterate over the elements in the queue.
pub trait QueueIter<E> {
    /// Gets the next element from the iterator.
    /// Returns `None` when no more elements are left in the iterator.
    fn next<'a>(
        &'a mut self,
        buf: &'a mut [u8],
    ) -> impl Future<Output = Result<Option<&'a [u8]>, E>>;
}

/// Flash interface used by the configuration storage
pub trait Flash {
    /// Error type for flash operations.
    type Error: core::fmt::Debug;
    /// Pushes data to the flash storage.
    fn push(&mut self, data: &[u8]) -> impl Future<Output = Result<(), Self::Error>>;
    /// Returns an iterator over the flash storage.
    fn iter(&mut self) -> impl Future<Output = Result<impl QueueIter<Self::Error>, Self::Error>>;
    /// Pops data from the flash storage.
    fn pop<'a>(
        &mut self,
        data: &'a mut [u8],
    ) -> impl Future<Output = Result<Option<&'a mut [u8]>, Self::Error>>;
    /// Peeks at data from the flash storage without removing it.
    fn peek<'a>(
        &mut self,
        data: &'a mut [u8],
    ) -> impl Future<Output = Result<Option<&'a mut [u8]>, Self::Error>>;
}
