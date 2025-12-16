//! Flash storage wrapper for sequential storage operations.
//!
//! This module provides a `Flash` struct that wraps a MultiwriteNorFlash device
//! and exposes async methods for queue-like operations on persistent storage.
use core::{num::NonZeroU32, ops::Deref};

use embedded_storage_async::nor_flash::{ErrorType, MultiwriteNorFlash};
use sequential_storage::{
    cache::{CacheImpl, NoCache},
    queue::{QueueConfig, QueueIterator, QueueIteratorEntry, QueueStorage},
};

use crate::{
    Elem, NdlDataStorage, NdlElemIter, NdlElemIterNode, SerData, consts, logging::MaybeDefmtFormat,
};

/// Owns a flash and the range reserved for the `StorageList`
pub struct Flash<T: MultiwriteNorFlash, C: CacheImpl> {
    queue: QueueStorage<T, C>,
}

/// An iterator over a [`Flash`]
pub struct FlashIter<'flash, T: MultiwriteNorFlash, C: CacheImpl> {
    iter: QueueIterator<'flash, T, C>,
}

/// A single position in the flash queue iterator
///
/// This represents a well-decoded element, where `half` contains the decoded
/// contents that correspond with `qit`.
pub struct FlashNode<'flash, 'iter, 'buf, T: MultiwriteNorFlash, C: CacheImpl> {
    half: Option<HalfElem>,
    qit: QueueIteratorEntry<'flash, 'buf, 'iter, T, C>,
}

/// A partially decoded element
///
/// This is a helper type that mimics [`Elem`], so we don't need to re-decode it
/// on every access. HalfElem is called that because it's a "half parsed [`Elem`]"
///
/// If we parse out good start/end data, we just store that so we don't have to keep reloading it
/// If it's a data elem, we just check that the discriminant is right, but we don't store it,
/// we just remember that the slice of data that we have is a data slice, and we use that
/// when we create the real [`Elem`].
///
/// This is a trick to not re-parse the raw data every time we call `.data()`.
#[derive(Clone, Copy)]
enum HalfElem {
    Start { seq_no: NonZeroU32 },
    Data,
    End { seq_no: NonZeroU32, calc_crc: u32 },
}

/// We only support v0 start
const V0_START: u8 = consts::ELEM_VERSION_V0 | consts::ELEM_DISCRIMINANT_START;

/// We only support v0 data
const V0_DATA: u8 = consts::ELEM_VERSION_V0 | consts::ELEM_DISCRIMINANT_DATA;

/// We only support v0 end
const V0_END: u8 = consts::ELEM_VERSION_V0 | consts::ELEM_DISCRIMINANT_END;

// ---- impl Flash ----

impl<T: MultiwriteNorFlash, C: CacheImpl> Flash<T, C> {
    /// Creates a new Flash instance with the given flash device and address range.
    ///
    /// # Arguments
    /// * `flash` - The MultiwriteNorFlash device to use for storage operations
    /// * `range` - The address range within the flash device reserved for this storage
    /// * `cache` - the cache to use with this flash access
    ///
    /// # Panic
    ///
    /// Panics if the range is not valid for the flash. To avoid panics, use [Self::try_new].
    pub const fn new(flash: T, range: core::ops::Range<u32>, cache: C) -> Self {
        Self {
            queue: QueueStorage::new(flash, QueueConfig::new(range), cache),
        }
    }

    /// Creates a new Flash instance with the given flash device and address range.
    ///
    /// # Arguments
    /// * `flash` - The MultiwriteNorFlash device to use for storage operations
    /// * `range` - The address range within the flash device reserved for this storage
    /// * `cache` - the cache to use with this flash access
    pub fn try_new(flash: T, range: core::ops::Range<u32>, cache: C) -> Option<Self> {
        let config = QueueConfig::try_new(range)?;
        Some(Self {
            queue: QueueStorage::new(flash, config, cache),
        })
    }

    /// Returns a mutable reference to the underlying flash device.
    pub fn flash(&mut self) -> &mut T {
        self.queue.flash()
    }

    #[cfg(any(test, feature = "std"))]
    pub(crate) fn queue(&mut self) -> &mut QueueStorage<T, C> {
        &mut self.queue
    }
}

impl<T: MultiwriteNorFlash, C: CacheImpl> NdlDataStorage for Flash<T, C>
where
    <T as ErrorType>::Error: MaybeDefmtFormat,
{
    type Iter<'this>
        = FlashIter<'this, T, C>
    where
        Self: 'this;

    type Error = sequential_storage::Error<<T as ErrorType>::Error>;

    async fn iter_elems<'this>(
        &'this mut self,
    ) -> Result<Self::Iter<'this>, <Self::Iter<'this> as NdlElemIter>::Error> {
        Ok(FlashIter {
            iter: self.queue.iter().await?,
        })
    }

    async fn push(&mut self, data: &Elem<'_>) -> Result<usize, Self::Error> {
        // scratch buffer used if this is start/end
        let mut buf = [0u8; 9];
        let used = match data {
            Elem::Start { seq_no } => {
                let buf = &mut buf[..5];
                buf[0] = V0_START;
                buf[1..5].copy_from_slice(&seq_no.get().to_le_bytes());
                buf
            }
            Elem::Data { data } => data.hdr_key_val,
            Elem::End { seq_no, calc_crc } => {
                buf[0] = V0_END;
                buf[1..5].copy_from_slice(&seq_no.get().to_le_bytes());
                buf[5..9].copy_from_slice(&calc_crc.to_le_bytes());
                buf.as_slice()
            }
        };

        // Push data to the underlying queue
        self.queue.push(used, false).await?;

        Ok(size_with_overhead::<T>(used.len()))
    }

    const MAX_ELEM_SIZE: usize = const {
        // We start from the (min) erase size for the flash
        let baseline = T::ERASE_SIZE;

        // Each sector has some metadata: two words
        let sector_metadata = 2 * crate::max(T::WRITE_SIZE, T::READ_SIZE);

        // Each pushed item has some metadata
        let item_metadata = QueueStorage::<T, C>::item_overhead_size() as usize;

        // The max size is the baseline minus any overhead.
        baseline - sector_metadata - item_metadata
    };
}

// ---- impl FlashIter ----

impl<'flash, T: MultiwriteNorFlash, C: CacheImpl> NdlElemIter for FlashIter<'flash, T, C> {
    type Item<'this, 'buf>
        = FlashNode<'flash, 'this, 'buf, T, C>
    where
        Self: 'this,
        Self: 'buf;

    type Error = sequential_storage::Error<<T as ErrorType>::Error>;

    async fn next<'iter, 'buf>(
        &'iter mut self,
        buf: &'buf mut [u8],
    ) -> Result<Option<Self::Item<'iter, 'buf>>, Self::Error>
    where
        Self: 'buf,
        Self: 'iter,
    {
        // Attempt to get the next item
        let nxt: Option<QueueIteratorEntry<'flash, 'buf, 'iter, T, C>> =
            self.iter.next(buf).await?;

        // No data? all done.
        let Some(nxt) = nxt else { return Ok(None) };

        let flno = FlashNode {
            half: HalfElem::from_bytes(&nxt),
            qit: nxt,
        };

        Ok(Some(flno))
    }
}

// ---- impl FlashIterNode ----

impl<T: MultiwriteNorFlash, C: CacheImpl> NdlElemIterNode for FlashNode<'_, '_, '_, T, C> {
    type Error = sequential_storage::Error<<T as ErrorType>::Error>;

    fn data(&self) -> Option<Elem<'_>> {
        let half = self.half.as_ref()?;
        Some(match *half {
            HalfElem::Start { seq_no } => Elem::Start { seq_no },
            HalfElem::Data => Elem::Data {
                // NOTE: IF HalfElem decoded successfully (and we have a Some(HalfElem)), then the
                // SerData creation must ALWAYS be valid (they check the same header).
                data: SerData::from_existing(self.qit.deref())?,
            },
            HalfElem::End { seq_no, calc_crc } => Elem::End { seq_no, calc_crc },
        })
    }

    async fn invalidate(self) -> Result<(), Self::Error> {
        self.qit.pop().await?;
        Ok(())
    }

    fn len(&self) -> usize {
        size_with_overhead::<T>(self.qit.deref().len())
    }
}

// ---- impl HalfElem ----

impl HalfElem {
    fn from_bytes(data: &[u8]) -> Option<Self> {
        let (first, rest) = data.split_first()?;
        match *first {
            V0_START => {
                if rest.len() != 4 {
                    return None;
                }
                let mut bytes = [0u8; 4];
                bytes.copy_from_slice(rest);
                Some(HalfElem::Start {
                    seq_no: NonZeroU32::new(u32::from_le_bytes(bytes))?,
                })
            }
            V0_DATA => {
                if rest.is_empty() {
                    None
                } else {
                    Some(HalfElem::Data)
                }
            }
            V0_END => {
                if rest.len() != 8 {
                    return None;
                }
                let mut seq_bytes = [0u8; 4];
                seq_bytes.copy_from_slice(&rest[..4]);
                let mut crc_bytes = [0u8; 4];
                crc_bytes.copy_from_slice(&rest[4..8]);
                Some(HalfElem::End {
                    seq_no: NonZeroU32::new(u32::from_le_bytes(seq_bytes))?,
                    calc_crc: u32::from_le_bytes(crc_bytes),
                })
            }
            _ => None,
        }
    }
}

// Helper functions

/// Calculate the size this item would take for the given flash in a sequential-storage queue
#[inline]
fn size_with_overhead<T>(len: usize) -> usize
where
    T: MultiwriteNorFlash,
{
    // Items pushed to the queue have overhead
    let baseline_overhead = QueueStorage::<T, NoCache>::item_overhead_size() as usize;
    // Items pushed to the queue require some alignment padding
    let len_roundup = len.next_multiple_of(crate::max(T::WRITE_SIZE, T::READ_SIZE));

    baseline_overhead + len_roundup
}

/// An implementation of an empty flash
///
/// On reads, it is always empty, any writes to it are ignored.
/// See also [`worker_task::no_op_worker_task`](`crate::worker_task::no_op_worker_task`) for a worker that uses it.
pub mod empty {
    use crate::{Elem, NdlDataStorage, NdlElemIter, NdlElemIterNode};

    /// A Flash that always reads as empty and ignores all writes
    ///
    /// It can be used for testing and evaluation.
    pub struct AlwaysEmptyFlash;

    impl NdlDataStorage for AlwaysEmptyFlash {
        type Iter<'this> = AlwaysEmptyIter;
        type Error = core::convert::Infallible;

        async fn iter_elems<'this>(
            &'this mut self,
        ) -> Result<Self::Iter<'this>, <Self::Iter<'this> as NdlElemIter>::Error> {
            Ok(AlwaysEmptyIter)
        }

        async fn push(&mut self, _data: &Elem<'_>) -> Result<usize, Self::Error> {
            Ok(0)
        }

        const MAX_ELEM_SIZE: usize = 0;
    }

    /// An implementation of [AlwaysEmptyIter] that always returns no elements.
    pub struct AlwaysEmptyIter;

    impl NdlElemIter for AlwaysEmptyIter {
        type Item<'a, 'b> = NeverNode;
        type Error = core::convert::Infallible;

        async fn next<'iter, 'buf>(
            &'iter mut self,
            _buf: &'buf mut [u8],
        ) -> Result<Option<Self::Item<'iter, 'buf>>, Self::Error>
        where
            Self: 'buf,
            Self: 'iter,
        {
            Ok(None)
        }
    }

    /// An implementation of [NdlElemIterNode] that can never be constructed.
    ///
    /// This is useful for testing with [AlwaysEmptyFlash].
    pub enum NeverNode {}

    impl NdlElemIterNode for NeverNode {
        type Error = core::convert::Infallible;

        fn data(&self) -> Option<Elem<'_>> {
            match *self {}
        }

        fn len(&self) -> usize {
            match *self {}
        }

        async fn invalidate(self) -> Result<(), Self::Error> {
            match self {}
        }
    }
}
