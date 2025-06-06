//! Queue trait implementation for sequential-storage
use super::{Queue, QueueIter};
use embedded_storage_async::nor_flash::MultiwriteNorFlash;
use sequential_storage::{
    Error as SeqStorError,
    cache::CacheImpl,
    queue::{self, QueueIterator},
};

/// Queue wrapper for use with sequential-storage
pub struct SeqStorQueue<T, C>
where
    T: MultiwriteNorFlash,
    C: CacheImpl,
{
    flash: T,
    range: core::ops::Range<u32>,
    cache: C,
}

impl<T, C> SeqStorQueue<T, C>
where
    T: MultiwriteNorFlash,
    C: CacheImpl,
{
    /// Creates a new Queue instance with the given flash device and address range.
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

impl<T, C> Queue for SeqStorQueue<T, C>
where
    T: MultiwriteNorFlash,
    C: CacheImpl,
{
    type Error = SeqStorError<T::Error>;

    /// Pushes data to the sequential storage queue.
    async fn push_entry(&mut self, data: &[u8]) -> Result<(), SeqStorError<T::Error>> {
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
    async fn iter_entries(
        &mut self,
    ) -> Result<impl QueueIter<SeqStorError<T::Error>>, SeqStorError<T::Error>> {
        Ok(SeqStorQueueIter {
            inner: queue::iter(&mut self.flash, self.range.clone(), &mut self.cache).await?,
        })
    }

    /// Pops data from the sequential storage queue.
    async fn pop_entry<'a>(
        &mut self,
        data: &'a mut [u8],
    ) -> Result<Option<&'a mut [u8]>, SeqStorError<T::Error>> {
        queue::pop(&mut self.flash, self.range.clone(), &mut self.cache, data).await
    }
    /// Peeks at data from the sequential storage queue.
    async fn peek_entry<'a>(
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
        // Get next item from iterator and map the `QueueIteratorEntry`
        // in the Ok(Some(_)) to &[]
        self.inner
            .next(buf)
            .await
            .map(|opt| opt.map(|data| &*data.into_buf()))
    }
}
