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

/// Owns a flash and the range reserved for the `StorageList`
pub struct Flash<T: MultiwriteNorFlash, C: CacheImpl> {
    flash: T,
    range: core::ops::Range<u32>,
    cache: C,
}

impl<T: MultiwriteNorFlash, C: CacheImpl> Flash<T, C> {
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

    /// Pushes data to the sequential storage queue.
    pub async fn push(&mut self, data: &[u8]) -> Result<(), SeqStorError<T::Error>> {
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
    pub async fn iter(&mut self) -> Result<QueueIterator<T, C>, SeqStorError<T::Error>> {
        queue::iter(&mut self.flash, self.range.clone(), &mut self.cache).await
    }

    /// Pops data from the sequential storage queue.
    pub async fn pop<'a>(
        &mut self,
        data: &'a mut [u8],
    ) -> Result<Option<&'a mut [u8]>, SeqStorError<T::Error>> {
        queue::pop(&mut self.flash, self.range.clone(), &mut self.cache, data).await
    }
    /// Peeks at data from the sequential storage queue.
    pub async fn peek<'a>(
        &mut self,
        data: &'a mut [u8],
    ) -> Result<Option<&'a mut [u8]>, SeqStorError<T::Error>> {
        queue::peek(&mut self.flash, self.range.clone(), &mut self.cache, data).await
    }
}

/// A single element stored in flash
#[derive(Debug, PartialEq)]
pub enum Elem<'a> {
    Start {
        /// The "write record" sequence number
        seq_no: u32,
    },
    Data {
        /// Contains the serialized key and value for the current data element
        data: &'a [u8],
    },
    End {
        /// The "write record" sequence number. Must match `Elem::Start { seq_no }`
        /// to properly end a "write record".
        seq_no: u32,
        /// The CRC32 of ALL `Elem::Data { data }` fields, in the other they appear
        /// in the FIFO queue.
        calc_crc: u32,
    },
}

/// A single element yielded from a NdlElemIter implementation
pub trait NdlElemIterNode {
    /// Error encountered while invalidating an element
    ///
    /// TODO: make this a concrete type? some kind of InvalidateErrorKind bound?
    type InvalidateError;

    /// Returns the present element.
    ///
    /// Note: this is infallible. Errors in encoding should be detected when calling
    /// `NdlElemIter::next()`, and elements with malformed data should not be yielded.
    fn data(&self) -> Elem<'_>;

    /// Invalidate the element.
    ///
    /// If this operation succeeds, the current element should NEVER be returned from
    /// future calls to `NdlElemIter::next()`. Implementors are free to decide how this
    /// is done. Invalidating an element MAY require time-expensive work, such as a write
    /// or erase (for example if all nodes of a flash sector have now been invalidated),
    /// so this should not be called in time-sensitive code.
    ///
    /// This method MUST be cancellation-safe, but in the case of cancellation, may
    /// require time-expensive recovery, so cancellation of this method should be
    /// avoided in the normal case. If this method is cancelled, the element may or
    /// may not be invalidated, however other currently-valid data MUST NOT be lost.
    async fn invalidate(self) -> Result<(), Self::InvalidateError>;
}

/// An iterator over `Elem`s stored in the queue.
pub trait NdlElemIter {
    /// Items yielded by this iterator
    type Item: NdlElemIterNode;
    /// The error returned when next/skip_to_seq or NdlDataStorage::iter_elems fails
    ///
    /// TODO: make this a concrete type? some kind of ErrorKind bound?
    type Error;

    /// Obtain the next item, in oldest-to-newest order.
    ///
    /// This method MUST be cancellation safe, however cancellation of this function
    /// may require re-creation of the iterator (e.g. the iterator may return a
    /// latched Error of some kind after cancellation). Cancellation MUST NOT lead
    /// to data loss.
    async fn next(&mut self) -> Result<Option<Self::Item>, Self::Error>;

    /// Fast-forwards the iterator to the Elem::Start item with the given seq_no.
    /// Returns an error if not found. If Err is returned, the iterator
    /// is exhausted. If Ok is returned, the next call to `next` will succeed
    /// and return an NdlElemIterNode that produces `Elem::Start { seq_no }`.
    ///
    /// This method MUST be cancellation safe, however cancellation of this function
    /// may require re-creation of the iterator (e.g. the iterator may return a
    /// latched Error of some kind after cancellation). Cancellation MUST NOT lead
    /// to data loss.
    async fn skip_to_seq(&mut self, seq_no: u32) -> Result<(), Self::Error>;
}

/// A storage backend representing a FIFO queue of elements
pub trait NdlDataStorage {
    /// The type of iterator returned by this implementation
    type Iter: NdlElemIter;
    /// The error returned when pushing fails
    ///
    /// TODO: make this a concrete type? some kind of PushErrorKind bound?
    type PushError;

    /// Returns an iterator over all elements, back to front.
    ///
    /// This method MUST be cancellation safe, and cancellation MUST NOT lead to
    /// data loss.
    async fn iter_elems(&mut self) -> Result<Self::Iter, <Self::Iter as NdlElemIter>::Error>;

    /// Insert an element at the FRONT of the list.
    ///
    /// This method MUST be cancellation safe, however if cancelled, it is not
    /// specified whether the item has been successfully written or not.
    /// Cancellation MUST NOT lead to data loss, other than the element currently
    /// being written.
    async fn push(&mut self, data: &Elem<'_>) -> Result<(), Self::PushError>;
}
