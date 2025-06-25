//! Configuration management
#![cfg_attr(not(any(test, doctest, feature = "std")), no_std)]
#![warn(missing_docs)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::undocumented_unsafe_blocks)]
#![allow(async_fn_in_trait)]

pub mod error;
pub mod flash;
pub mod safety_guide;
pub mod worker_task;

pub use mutex_traits;

#[cfg(any(test, feature = "std"))]
#[doc(hidden)]
pub mod test_utils;

mod storage_list;
mod storage_node;

use core::num::NonZeroU32;

use minicbor::{CborLen, Encode, len_with};
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
        ($s:literal $(, $x:expr)* $(,)?) => {
            {
            let _ = ($( & $x ),*);
            }
        };
    }

    #[cfg(not(any(feature = "std", feature = "defmt")))]
    macro_rules! debug {
        ($s:literal $(, $x:expr)* $(,)?) => {
            {
            let _ = ($( & $x ),*);
            }
        };
    }

    #[cfg(not(any(feature = "std", feature = "defmt")))]
    macro_rules! info {
        ($s:literal $(, $x:expr)* $(,)?) => {
            {
            let _ = ($( & $x ),*);
            }
        };
    }

    #[cfg(not(any(feature = "std", feature = "defmt")))]
    macro_rules! log_warn {
        ($s:literal $(, $x:expr)* $(,)?) => {
            {
            let _ = ($( & $x ),*);
            }
        };
    }

    #[cfg(not(any(feature = "std", feature = "defmt")))]
    macro_rules! error {
        ($s:literal $(, $x:expr)* $(,)?) => {{
            let _ = ($( & $x ),*);
        }
        };
    }

    #[cfg(not(any(feature = "std", feature = "defmt")))]
    pub(crate) use {debug, error, info, log_warn as warn, trace};

    /// A marker trait that requires `T: defmt::Format` when the `defmt` feature is enabled
    #[cfg(not(feature = "defmt"))]
    pub trait MaybeDefmtFormat {}

    /// A marker trait that requires `T: defmt::Format` when the `defmt` feature is enabled
    #[cfg(feature = "defmt")]
    pub trait MaybeDefmtFormat: defmt::Format {}

    #[cfg(not(feature = "defmt"))]
    impl<T> MaybeDefmtFormat for T {}

    #[cfg(feature = "defmt")]
    impl<T: defmt::Format> MaybeDefmtFormat for T {}
}

/// Const helper to compute the maximum of two usize values
const fn max(a: usize, b: usize) -> usize {
    if a > b { a } else { b }
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
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
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
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
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
    type Error: MaybeDefmtFormat;

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

    /// Return the maximum size of an `Elem` that may be stored in the list in bytes.
    ///
    /// This includes a one byte element header, the CBOR-serialized key, and the
    /// CBOR-serialized value.
    const MAX_ELEM_SIZE: usize;

    /// Checks whether the size of the `key` and `node` fit into the maximium
    /// element size of this `NdlDataStorage`.
    ///
    /// This function can be used to check if writing the node to flash is
    /// possible before the node is serialized at a later stage.
    fn check_node_size<T>(key: &str, node: T) -> bool
    where
        T: CborLen<()> + Encode<()>,
    {
        Self::MAX_ELEM_SIZE >= 1 + len_with(key, &mut ()) + len_with(node, &mut ())
    }
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
    /// The item returned MAY not be a valid Element, however access is still provided
    /// to allow invalidation of this node when relevant.
    ///
    /// This method returns:
    ///
    /// - `Ok(Some(item))`: There is an item here, but it may or may not contain a valid
    ///   Elem when [`NdlElemIterNode::data()`] is called.
    /// - `Ok(None)`: The end of the iterator has been reached successfully
    /// - `Err(e)`: An error occurred while reading from the storage
    ///
    /// This method MUST be cancellation safe, however cancellation of this function
    /// may require re-creation of the iterator (e.g. the iterator may return a
    /// latched Error of some kind after cancellation). Cancellation MUST NOT lead
    /// to data loss.
    async fn next<'iter, 'buf>(
        &'iter mut self,
        buf: &'buf mut [u8],
    ) -> Result<Option<Self::Item<'iter, 'buf>>, Self::Error>
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
    /// If the contained item is NOT a valid element, `None` is returned here.
    /// This means that the storage did not consider this item invalid, however we
    /// are unable to decode it as a valid [`Elem`].
    fn data(&self) -> Option<Elem<'_>>;

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
#[macro_export]
macro_rules! step {
    ($iter:ident, $buf:ident) => {
        loop {
            let res = $iter.next($buf).await;
            match res {
                // Got an item (good or bad)
                Ok(Some(item)) => break StepResult::Ok(item),
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
///
/// On success, it returns the number of items consumed, including the Start item
#[macro_export]
macro_rules! skip_to_seq {
    ($iter:ident, $buf:ident, $seq:ident) => {
        {
            let mut ctr = 0;
            loop {
                let res = $iter.next($buf).await;
                ctr += 1;
                let item = match res {
                    // Got an item
                    Ok(Some(item)) => item,
                    // end of list
                    Ok(None) => break Err($crate::StepErr::EndOfList),
                    // flash error
                    Err(e) => break Err($crate::StepErr::FlashError(e)),
                };
                if !matches!(item.data(), Some(Elem::Start { seq_no }) if seq_no == $seq) {
                    continue;
                }
                break StepResult::Ok(((), ctr));
            }
        }
    };
}

use crc::{CRC_32_CKSUM, Crc, Digest, NoTable};

use crate::logging::MaybeDefmtFormat;

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
