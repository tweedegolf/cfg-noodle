//! Flash storage wrapper for sequential storage operations.
//!
//! This module provides a `Flash` struct that wraps a MultiwriteNorFlash device
//! and exposes async methods for queue-like operations on persistent storage.

use embedded_storage_async::nor_flash::MultiwriteNorFlash;
use sequential_storage::{
    Error as SeqStorError,
    cache::CacheImpl,
    queue::{self, QueueIterator},
};

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

/// Owns a flash and the range reserved for the `StorageList`
pub struct SeqStorFlash<T, C>
where
    T: MultiwriteNorFlash,
    C: CacheImpl,
{
    flash: T,
    range: core::ops::Range<u32>,
    cache: C,
}

impl<T, C> SeqStorFlash<T, C>
where
    T: MultiwriteNorFlash,
    C: CacheImpl,
{
    /// Creates a new Flash instance with the given flash device and address range.
    ///
    /// # Arguments
    /// * `flash` - The MultiwriteNorFlash device to use for storage operations
    /// * `range` - The address range within the flash device reserved for this storage
    pub fn new(flash: T, range: core::ops::Range<u32>, cache: C) -> Self {
        Self {
            flash,
            range,
            cache,
        }
    }
    /// Returns a mutable reference to the underlying flash device.
    pub fn flash(&mut self) -> &mut T {
        &mut self.flash
    }
}
impl<T, C> Flash for SeqStorFlash<T, C>
where
    T: MultiwriteNorFlash,
    C: CacheImpl,
{
    type Error = SeqStorError<T::Error>;

    /// Pushes data to the sequential storage queue.
    async fn push(&mut self, data: &[u8]) -> Result<(), SeqStorError<T::Error>> {
        queue::push(
            &mut self.flash,
            self.range.clone(),
            &mut self.cache,
            data,
            false,
        )
        .await
    }

    /// Creates an iterator over the sequential storage queue.
    async fn iter(
        &mut self,
    ) -> Result<impl QueueIter<SeqStorError<T::Error>>, SeqStorError<T::Error>> {
        Ok(SeqStorQueueIter {
            inner: queue::iter(&mut self.flash, self.range.clone(), &mut self.cache).await?,
        })
    }

    /// Pops data from the sequential storage queue.
    async fn pop<'a>(
        &mut self,
        data: &'a mut [u8],
    ) -> Result<Option<&'a mut [u8]>, SeqStorError<T::Error>> {
        queue::pop(&mut self.flash, self.range.clone(), &mut self.cache, data).await
    }
    /// Peeks at data from the sequential storage queue.
    async fn peek<'a>(
        &mut self,
        data: &'a mut [u8],
    ) -> Result<Option<&'a mut [u8]>, SeqStorError<T::Error>> {
        queue::peek(&mut self.flash, self.range.clone(), &mut self.cache, data).await
    }
}

struct SeqStorQueueIter<'a, T: MultiwriteNorFlash, C: CacheImpl> {
    inner: QueueIterator<'a, T, C>,
}

impl<T: MultiwriteNorFlash, C: CacheImpl> QueueIter<SeqStorError<T::Error>>
    for SeqStorQueueIter<'_, T, C>
{
    async fn next<'a>(
        &'a mut self,
        buf: &'a mut [u8],
    ) -> Result<Option<&'a [u8]>, SeqStorError<T::Error>> {
        Ok(self
            .inner
            .next(buf)
            .await?
            .map(|data| data.into_buf())
            .map(|data| &*data))
    }
}
