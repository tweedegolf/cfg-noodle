//! Flash storage wrapper for sequential storage operations.
//!
//! This module provides a `Flash` struct that wraps a MultiwriteNorFlash device
//! and exposes async methods for queue-like operations on persistent storage.
use core::{num::NonZeroU32, ops::Deref};

use embedded_storage_async::nor_flash::{ErrorType, MultiwriteNorFlash};
use sequential_storage::{
    cache::CacheImpl,
    queue::{self, QueueIterator, QueueIteratorEntry},
};

use crate::{Elem, NdlDataStorage, NdlElemIter, NdlElemIterNode, SerData, consts};

/// Owns a flash and the range reserved for the `StorageList`
pub struct Flash<T: MultiwriteNorFlash, C: CacheImpl> {
    flash: T,
    range: core::ops::Range<u32>,
    cache: C,
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
/// on every access.
#[derive(Clone, Copy)]
enum HalfElem {
    Start { seq_no: NonZeroU32 },
    Data,
    End { seq_no: NonZeroU32, calc_crc: u32 },
}

// ---- impl Flash ----

impl<T: MultiwriteNorFlash, C: CacheImpl> Flash<T, C> {
    /// Creates a new Flash instance with the given flash device and address range.
    ///
    /// # Arguments
    /// * `flash` - The MultiwriteNorFlash device to use for storage operations
    /// * `range` - The address range within the flash device reserved for this storage
    /// * `cache` - the cache to use with this flash access
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

impl<T: MultiwriteNorFlash + 'static, C: CacheImpl + 'static> NdlDataStorage for Flash<T, C> {
    type Iter<'this>
        = FlashIter<'this, T, C>
    where
        Self: 'this;

    type Error = sequential_storage::Error<<T as ErrorType>::Error>;

    async fn iter_elems<'this>(
        &'this mut self,
    ) -> Result<Self::Iter<'this>, <Self::Iter<'this> as NdlElemIter>::Error> {
        Ok(FlashIter {
            iter: queue::iter(&mut self.flash, self.range.clone(), &mut self.cache).await?,
        })
    }

    async fn push(&mut self, data: &Elem<'_>) -> Result<(), Self::Error> {
        // scratch buffer used if this is start/end
        let mut buf = [0u8; 9];
        let used = match data {
            Elem::Start { seq_no } => {
                let buf = &mut buf[..5];
                buf[0] = consts::ELEM_START;
                buf[1..5].copy_from_slice(&seq_no.get().to_le_bytes());
                buf
            }
            Elem::Data { data } => data.hdr_key_val,
            Elem::End { seq_no, calc_crc } => {
                buf[0] = consts::ELEM_END;
                buf[1..5].copy_from_slice(&seq_no.get().to_le_bytes());
                buf[5..9].copy_from_slice(&calc_crc.to_le_bytes());
                buf.as_slice()
            }
        };

        // Push data to the underlying queue
        queue::push(
            &mut self.flash,
            self.range.clone(),
            &mut self.cache,
            used,
            false,
        )
        .await
    }
}

// ---- impl FlashIter ----

impl<'flash, T: MultiwriteNorFlash + 'static, C: CacheImpl + 'static> NdlElemIter
    for FlashIter<'flash, T, C>
{
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
}

// ---- impl HalfElem ----

impl HalfElem {
    fn from_bytes(data: &[u8]) -> Option<Self> {
        let (first, rest) = data.split_first()?;
        match *first {
            consts::ELEM_START => {
                if rest.len() != 4 {
                    return None;
                }
                let mut bytes = [0u8; 4];
                bytes.copy_from_slice(rest);
                Some(HalfElem::Start {
                    seq_no: NonZeroU32::new(u32::from_le_bytes(bytes))?,
                })
            }
            consts::ELEM_DATA => {
                if rest.is_empty() {
                    None
                } else {
                    Some(HalfElem::Data)
                }
            }
            consts::ELEM_END => {
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
