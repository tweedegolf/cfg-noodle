//! Configuration management
#![cfg_attr(not(any(test, doctest, feature = "std")), no_std)]
#![warn(missing_docs)]
#![deny(clippy::unwrap_used)]
#![allow(async_fn_in_trait)]

pub mod error;
pub mod flash;

#[cfg(any(test, feature = "std"))]
#[doc(hidden)]
pub mod test_utils;

mod storage_list;
mod storage_node;

use core::num::NonZeroU32;

#[doc(inline)]
pub use storage_list::StorageList;

#[doc(inline)]
pub use storage_node::{State, StorageListNode, StorageListNodeHandle};

#[allow(unused)]
pub(crate) mod logging {
    #[cfg(feature = "std")]
    pub use log::*;

    #[cfg(feature = "defmt")]
    pub use defmt::*;

    #[cfg(all(feature = "std", feature = "defmt"))]
    compile_error!("Cannot enable both 'std' and 'defmt' features simultaneously");

    /// No-op macros when no logging feature is enabled
    #[cfg(not(any(feature = "std", feature = "defmt")))]
    macro_rules! trace {
        ($($arg:tt)*) => {};
    }

    #[cfg(not(any(feature = "std", feature = "defmt")))]
    macro_rules! debug {
        ($($arg:tt)*) => {};
    }

    #[cfg(not(any(feature = "std", feature = "defmt")))]
    macro_rules! info {
        ($($arg:tt)*) => {};
    }

    #[cfg(not(any(feature = "std", feature = "defmt")))]
    macro_rules! log_warn {
        ($($arg:tt)*) => {};
    }

    #[cfg(not(any(feature = "std", feature = "defmt")))]
    macro_rules! error {
        ($($arg:tt)*) => {};
    }
    #[cfg(not(any(feature = "std", feature = "defmt")))]
    pub(crate) use {debug, error, info, log_warn as warn, trace};
}

mod consts {
    /// Discriminant used to mark Start elements on disk
    pub(crate) const ELEM_START: u8 = 0;
    /// Discriminant used to mark Data elements on disk
    pub(crate) const ELEM_DATA: u8 = 1;
    /// Discriminant used to mark End elements on disk
    pub(crate) const ELEM_END: u8 = 2;
}

/// Serialized Data Element
///
/// Includes header, key, and value
#[derive(Debug, PartialEq)]
pub struct SerData<'a> {
    hdr_key_val: &'a [u8],
}

impl<'a> SerData<'a> {
    /// Create a new Serialized Data Element.
    ///
    /// `data[0]` MUST be the header position, with key+val starting
    /// at `data[1]`. The header will be overwritten with the data discriminant
    ///
    /// Returns None if the slice is empty.
    pub fn new(data: &'a mut [u8]) -> Option<Self> {
        let f = data.first_mut()?;
        *f = consts::ELEM_DATA;

        Some(Self { hdr_key_val: data })
    }

    /// Create a Serialized Data Element from an existing slice. The
    /// discriminant will NOT be written.
    ///
    /// Returns None if the slice is empty.
    pub fn from_existing(data: &'a [u8]) -> Option<Self> {
        if data.is_empty() {
            None
        } else {
            Some(Self { hdr_key_val: data })
        }
    }

    /// Obtain the header
    pub fn hdr(&self) -> u8 {
        // SAFETY: We checked the slice is not empty
        unsafe { *self.hdr_key_val.get_unchecked(0) }
    }

    /// Get the key+val portion of the SerData
    ///
    /// Will return an empty slice if the slice was originally empty
    pub fn key_val(&self) -> &[u8] {
        self.hdr_key_val.get(1..).unwrap_or(&[])
    }
}

/// A single element stored in flash
#[derive(Debug, PartialEq)]
pub enum Elem<'a> {
    /// Start element
    Start {
        /// The "write record" sequence number
        seq_no: NonZeroU32,
    },
    /// Data element
    Data {
        /// Contains the serialized key and value for the current data element
        data: SerData<'a>,
    },
    /// End element
    End {
        /// The "write record" sequence number. Must match `Elem::Start { seq_no }`
        /// to properly end a "write record".
        seq_no: NonZeroU32,
        /// The CRC32 of ALL `Elem::Data { data }` fields, in the other they appear
        /// in the FIFO queue.
        calc_crc: u32,
    },
}

/// A storage backend representing a FIFO queue of elements
pub trait NdlDataStorage {
    /// The type of iterator returned by this implementation
    type Iter<'this>: NdlElemIter<Error = Self::Error>
    where
        Self: 'this;
    /// The error returned when pushing fails
    type Error;

    /// Returns an iterator over all elements, back to front.
    ///
    /// This method MUST be cancellation safe, and cancellation MUST NOT lead to
    /// data loss.
    async fn iter_elems<'this>(
        &'this mut self,
    ) -> Result<Self::Iter<'this>, <Self::Iter<'this> as NdlElemIter>::Error>;

    /// Insert an element at the FRONT of the list.
    ///
    /// This method MUST be cancellation safe, however if cancelled, it is not
    /// specified whether the item has been successfully written or not.
    /// Cancellation MUST NOT lead to data loss, other than the element currently
    /// being written.
    async fn push(&mut self, data: &Elem<'_>) -> Result<(), Self::Error>;
}

/// An iterator over `Elem`s stored in the queue.
pub trait NdlElemIter {
    /// Items yielded by this iterator
    type Item<'this, 'buf>: NdlElemIterNode<Error = Self::Error>
    where
        Self: 'this,
        Self: 'buf;
    /// The error returned when next/skip_to_seq or NdlDataStorage::iter_elems fails
    type Error;

    /// Obtain the next item, in oldest-to-newest order.
    ///
    /// For lifetime reason, this returns an awkward `Option<Option<Item>>`. Return
    /// values mean:
    ///
    /// - `Ok(Some(Some(item)))` - An item exists and was properly decoded as an `Elem`
    /// - `Ok(Some(None))` - An item exists, BUT it cannot be properly decoded as an `Elem`
    /// - `Ok(None)` - No item exists, but no flash error occurred
    /// - `Err(e)` - An error occurred when decoding
    ///
    /// This method should normally be used with the [`step!()`] and [`skip_to_seq!()`]
    /// macros for convenience.
    ///
    /// This method MUST be cancellation safe, however cancellation of this function
    /// may require re-creation of the iterator (e.g. the iterator may return a
    /// latched Error of some kind after cancellation). Cancellation MUST NOT lead
    /// to data loss.
    async fn next<'iter, 'buf>(
        &'iter mut self,
        buf: &'buf mut [u8],
    ) -> Result<Option<Option<Self::Item<'iter, 'buf>>>, Self::Error>
    where
        Self: 'buf,
        Self: 'iter;
}

/// A single element yielded from a NdlElemIter implementation
pub trait NdlElemIterNode {
    /// Error encountered while invalidating an element
    type Error;

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
    async fn invalidate(self) -> Result<(), Self::Error>;
}

/// Result used by [`step`] and [`skip_to_seq`] macros
pub type StepResult<T, E> = Result<T, StepErr<E>>;

/// An error occurred while calling [`step`] or [`skip_to_seq`] macros
#[derive(Debug, PartialEq)]
pub enum StepErr<E> {
    /// End of list reached
    EndOfList,
    /// A flash error occurred
    FlashError(E),
}

/// Advance the iterator one step
///
/// Skips nodes that do not properly decode as an Elem.
#[macro_export]
macro_rules! step {
    ($iter:ident, $buf:ident) => {
        loop {
            let res = $iter.next($buf).await;
            match res {
                // Got at item
                Ok(Some(Some(item))) => break StepResult::Ok(item),
                // Got a bad item, continue
                Ok(Some(None)) => {}
                // end of list
                Ok(None) => break Err($crate::StepErr::EndOfList),
                // flash error
                Err(e) => break Err($crate::StepErr::FlashError(e)),
            }
        }
    };
}

/// Fast-forwards the iterator to the Elem::Start item with the given seq_no.
/// Returns an error if not found. If Err is returned, the iterator
/// is exhausted. If Ok is returned, `Elem::Start { seq_no }` has been consumed.
#[macro_export]
macro_rules! skip_to_seq {
    ($iter:ident, $buf:ident, $seq:ident) => {
        loop {
            let res = $iter.next($buf).await;
            let item = match res {
                // Got at item
                Ok(Some(Some(item))) => item,
                // Got a bad item, continue
                Ok(Some(None)) => continue,
                // end of list
                Ok(None) => break Err($crate::StepErr::EndOfList),
                // flash error
                Err(e) => break Err($crate::StepErr::FlashError(e)),
            };
            if !matches!(item.data(), Elem::Start { seq_no } if seq_no == $seq) {
                continue;
            }
            break StepResult::Ok(());
        }
    };
}

use crc::{CRC_32_CKSUM, Crc, Digest, NoTable};

/// CRC32 implementation
///
/// Currently uses [`CRC_32_CKSUM`] from the [`crc`] crate with no table.
///
/// Wrapped for semver reasons
pub struct Crc32(Digest<'static, u32, NoTable>);

impl Crc32 {
    const CRC: Crc<u32, NoTable> = Crc::<u32, NoTable>::new(&CRC_32_CKSUM);

    /// Create new initial CRC digest
    pub const fn new() -> Self {
        Self(Self::CRC.digest())
    }

    /// Update the CRC with data
    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data)
    }

    /// Finalize the CRC, producing the output
    pub fn finalize(self) -> u32 {
        self.0.finalize()
    }
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}
