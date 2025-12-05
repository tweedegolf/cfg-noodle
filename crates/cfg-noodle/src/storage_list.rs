//! # Intrusive Linked List for Configuration Storage
//!
//! This module implements the [`StorageList`] type, which serves as an "anchor"
//! to attach storage nodes onto.
//!
//! Users will typically create a [`StorageList`], and then primarily interact with
//! [`StorageListNode`](crate::StorageListNode) and [`StorageListNodeHandle`](crate::StorageListNodeHandle)

use crate::{
    Crc32, Elem, NdlDataStorage, NdlElemIter, NdlElemIterNode, SerData,
    error::{Error, LoadStoreError},
    logging::{debug, error, info, warn},
    storage_node::{Node, NodeHeader, State},
};
use cordyceps::{List, list::IterRaw};
use core::{
    num::NonZeroU32,
    ops::RangeInclusive,
    ptr::{NonNull, addr_of},
    sync::atomic::Ordering,
};
use maitake_sync::{Mutex, WaitQueue};
use minicbor::encode::write::{Cursor, EndOfSlice};
use mutex_traits::{ConstInit, ScopedRawMutex};

/// "Global anchor" of all storage items.
///
/// It serves as the meeting point between two conceptual pieces:
///
/// 1. The [`StorageListNode`](crate::StorageListNode)s, which each store one configuration item
/// 2. The worker task that owns the external flash, and serves the
///    role of loading data FROM flash (and putting it in the Nodes),
///    as well as the role of deciding when to write data TO flash
///    (retrieving it from each node).
pub struct StorageList<R: ScopedRawMutex, const KEPT_RECORDS: usize> {
    /// The type parameter `R` allows us to be generic over kinds of mutex
    /// impls, allowing for use of a `std` mutex on `std`, and for things
    /// like `CriticalSectionRawMutex` or `ThreadModeRawMutex` on no-std
    /// targets.
    ///
    /// This mutex MUST be locked whenever you:
    ///
    /// 1. Want to append an item to the linked list, e.g. with `StorageListNode`.
    ///    You'd also need to lock it to REMOVE something from the list, but
    ///    we'll probably not support that in this library, at least for now.
    /// 2. You want to interact with ANY node that is in the list, REGARDLESS
    ///    of whether you get to a node "directly" or by iterating through
    ///    the linked list. To repeat: you MUST hold the mutex the ENTIRE time
    ///    there is a live reference to a `Node<T>`, mutable or immutable!
    ///    THIS IS EXTREMELY LOAD BEARING TO SOUNDNESS.
    ///
    /// Note that this is the first level of trickery, EVERY node can actually
    /// hold a different type T, so at a top level, we ONLY store a list of the
    /// Header, which is the first field in `Node<T>`, which is repr-C, so we
    /// can cast this back to a `Node<T>` as needed
    pub(crate) inner: Mutex<StorageListInner<KEPT_RECORDS>, R>,
    /// Notifies the worker task that nodes need to be read from flash.
    /// Woken when a new node is attached and requires data to be loaded.
    pub(crate) needs_read: WaitQueue,
    /// Notifies the worker task that nodes have pending writes to flash.
    /// Woken when a node's data is modified and needs to be persisted.
    pub(crate) needs_write: WaitQueue,
    /// Notifies waiting nodes that the read process has completed.
    /// Woken after `process_reads` finishes loading data from flash.
    pub(crate) reading_done: WaitQueue,
}

/// Counter values from a [`StorageList::process_reads()`] operation
#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[non_exhaustive]
pub struct ProcessReadCounters {
    /// Total bytes read from storage, including any re-reads if necessary
    pub total_bytes_read: usize,
    /// Total data bytes used by data of the current valid write record,
    /// used to populate the list.
    pub current_bytes_read: usize,
}

/// Counter values from a [`StorageList::process_writes()`] operation
#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[non_exhaustive]
pub struct ProcessWriteCounters {
    /// Total bytes read from storage, including any re-reads if necessary
    pub total_bytes_read: usize,
    /// Total data bytes written as part of this operation
    pub current_bytes_written: usize,
}

/// Counter values from a [`StorageList::process_garbage()`] operation
#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[non_exhaustive]
pub struct ProcessGarbageCounters {
    /// Total bytes read from storage, including any re-reads if necessary
    pub total_bytes_read: usize,
    /// Bytes read AFTER collection occurred. May be zero if no collection
    /// was necessary.
    pub post_collection_bytes_read: usize,
    /// Total data bytes popped as part of this operation
    pub total_bytes_popped: usize,
}

/// The functional core of a [`StorageList`]
///
/// This type contains any pieces that require the mutex to be locked to access.
/// This means that holding an `&mut StorageListInner` means that you have exclusive
/// access to the entire list and all nodes attached to it.
pub(crate) struct StorageListInner<const KEPT_RECORDS: usize> {
    pub(crate) list: List<NodeHeader>,
    seq_state: SeqState<KEPT_RECORDS>,
}

/// The metadata of a verified "Write Record"
///
/// This records the existence of a valid Write Record, that had a well formed
/// Start, Data (repeated), End sequence of elements, AND had a valid CRC.
///
/// This is used to remember both the sequence number and iterator-range of this
/// valid WriteRecord, so we can cache that discovery.
#[derive(Clone, Debug, PartialEq)]
struct GoodWriteRecord {
    /// The sequence number of the valid Write Record
    seq: NonZeroU32,
    /// The range of storage iterator nodes that this Write Record resides in
    range: RangeInclusive<usize>,
}

/// The current cache state of the StorageList
///
/// This serves as a cache that is loaded on the initial scan of the external
/// storage. It retains what our next sequence number should be, and what we consider
/// the three most recent, valid, Write Records to be, and where they reside on the
/// external flash.
///
/// This is populated when we perform the first `process_read`s, updated during
/// during each successful `process_write`, and invalidated and re-calculated
/// during each successful `process_garbage` call.
#[derive(Clone)]
struct SeqState<const KEPT_RECORDS: usize> {
    /// Next Sequence Number. Is None if we have not performed the initial scan of the storage
    next_seq: Option<NonZeroU32>,
    /// The most recent, valid, Write Records (as sorted by their sequence numbers)
    ///
    /// We retain multiple for the purpose of garbage collection: we don't necessarily want to
    /// invalidate ALL old records, in the off chance that our LATEST written record becomes
    /// corrupt. Users can set this to just one though if they don't care about redundancy
    last_records: [Option<GoodWriteRecord>; KEPT_RECORDS],
    /// Do we need to perform a garbage collection pass?
    ///
    /// This is set after each call to process_write, and
    needs_gc: bool,
}

/// Information regarding a successful Record Write
struct WriteReport {
    /// The sequence number used for writing the record
    seq: NonZeroU32,
    /// The CRC of data contained in this record
    crc: u32,
    /// The count of data items contained in this record
    ct: usize,
    /// The total number of bytes written for this record
    bytes_written: usize,
}

// --------------------------------------------------------------------------
// impl StorageList
// --------------------------------------------------------------------------

impl<R: ScopedRawMutex + ConstInit, const KEPT_RECORDS: usize> StorageList<R, KEPT_RECORDS> {
    /// const constructor to make a new empty list. Intended to be used
    /// to create a static.
    ///
    /// Will panic if `KEPT_RECORDS` is 0.
    ///
    /// ```
    /// # use cfg_noodle::StorageList;
    /// # use mutex::raw_impls::cs::CriticalSectionRawMutex;
    /// static GLOBAL_LIST: StorageList<CriticalSectionRawMutex, 3> = StorageList::new();
    /// ```
    pub const fn new() -> Self {
        Self::try_new().expect("StorageList must keep at least one record")
    }

    /// const constructor to make a new empty list. Intended to be used
    /// to create a static.
    ///
    /// Will return `None` if `KEPT_RECORDS` is 0.
    ///
    /// ```
    /// # use cfg_noodle::StorageList;
    /// # use mutex::raw_impls::cs::CriticalSectionRawMutex;
    /// static GLOBAL_LIST: StorageList<CriticalSectionRawMutex, 3> = StorageList::try_new().unwrap();
    /// ```
    pub const fn try_new() -> Option<Self> {
        if KEPT_RECORDS == 0 {
            None
        } else {
            Some(Self {
                inner: Mutex::new_with_raw_mutex(
                    StorageListInner {
                        list: List::new(),
                        seq_state: SeqState::new(),
                    },
                    R::INIT,
                ),
                needs_read: WaitQueue::new(),
                needs_write: WaitQueue::new(),
                reading_done: WaitQueue::new(),
            })
        }
    }
}

impl<R: ScopedRawMutex + ConstInit, const KEPT_RECORDS: usize> Default
    for StorageList<R, KEPT_RECORDS>
{
    /// this only exists to shut up the clippy lint about impl'ing default
    fn default() -> Self {
        Self::new()
    }
}

/// These are public methods for the `StorageList`. They currently are intended to be
/// used by the "storage worker task", that decides when we actually want to
/// interact with the flash.
impl<R: ScopedRawMutex, const KEPT_RECORDS: usize> StorageList<R, KEPT_RECORDS> {
    /// Process any nodes that are requesting flash data
    ///
    /// This function will traverse the list in order to attempt to fill any [`StorageListNode`]s that
    /// are still in the [`State::Initial`](crate::State) state, meaning they are waiting to see if the
    /// flash contains the data they are interested or not.
    ///
    /// The need for this is signalled by calling [`Self::needs_read()`] and awaiting the returned [`WaitQueue`]
    /// signal. You may choose to "debounce" this and wait for some time after the signal is noted, in order
    /// to batch up as many nodes as possible to reduce the amount of loading from external storage.
    ///
    /// Reads are ONLY necessary after new nodes are attached with [`StorageListNode::attach()`], which will
    /// only happen for some amount of time after startup.
    ///
    /// This function will hold an async mutex for the duration of access, inhibiting other operations such
    /// as [`Self::process_writes()`]. Nodes that have already had their data populated will not have read
    /// access inhibited, though calls to [`StorageListNodeHandle::write()`] WILL not resolve until this
    /// operation completes.
    ///
    /// [`StorageListNode`]: crate::StorageListNode
    /// [`StorageListNode::attach()`]: crate::StorageListNode::attach
    /// [`StorageListNodeHandle`]: crate::StorageListNodeHandle
    /// [`StorageListNodeHandle::write()`]: crate::StorageListNodeHandle::write
    ///
    /// ## Params:
    ///
    /// - `storage`: a [`NdlDataStorage`] object containing the necessary information to call [`NdlDataStorage::push`]
    /// - `buf`: a scratch-buffer of length `S::MAX_ELEM_SIZE`.
    ///
    pub async fn process_reads<S: NdlDataStorage>(
        &'static self,
        storage: &mut S,
        buf: &mut [u8],
    ) -> Result<ProcessReadCounters, LoadStoreError<S::Error>> {
        info!("Start process_reads");

        if buf.len() != S::MAX_ELEM_SIZE {
            warn!(
                "mismatch in size of buffer provided ({}), vs storage's MAX_ELEM_SIZE ({}). Maximum item size will be limited to {}",
                buf.len(),
                S::MAX_ELEM_SIZE,
                core::cmp::min(buf.len(), S::MAX_ELEM_SIZE)
            );
        }

        // Lock the list, remember, if we're touching nodes, we need to have the list
        // locked the entire time!
        let mut inner = self.inner.lock().await;
        debug!("process_reads locked list");

        // Have we already determined the latest valid write record?
        let mut total_bytes_read = 0usize;
        let mut current_bytes_read = 0usize;
        let res = inner
            .get_or_populate_latest(storage, buf, &mut total_bytes_read)
            .await?;

        // If we have something: populate all present items
        if let Some(latest) = res {
            inner
                .extract_all(
                    latest,
                    storage,
                    buf,
                    &mut total_bytes_read,
                    &mut current_bytes_read,
                )
                .await?;
        }

        // Now, we either have NOTHING, or we have populated all items that DO exist.
        // Mark any nodes that are STILL in the INITIAL state as nonresident.
        inner.mark_initial_nonresident();

        debug!("Reading done. Waking all.");
        self.reading_done.wake_all();
        Ok(ProcessReadCounters {
            total_bytes_read,
            current_bytes_read,
        })
    }

    /// Process writes to flash.
    ///
    /// If any of the nodes attached to the list has pending writes, this function writes the
    /// ENTIRE list to flash. This method:
    ///
    /// 1. Locks the mutex
    /// 2. Iterates through each node currently linked in the list
    /// 3. Serializes each node and writes it to flash
    /// 4. Verifies the written data matches what was intended to be written
    /// 5. On success, marks all nodes as no longer having pending changes
    /// 6. Releases the mutex
    ///
    /// The need for this call can be determined by calling [`Self::needs_write()`], and awaiting
    /// the returned [`WaitQueue`]. You may choose to debounce or delay calling `process_writes`
    /// in order to reduce the number of writes/erase to the flash, however data is not "synced"
    /// to disk until a call to `process_writes` completes successfully.
    ///
    /// This method WILL NOT succeed until after the first call to `process_read`.
    ///
    /// ## Arguments:
    ///
    /// * `storage`: a [`NdlDataStorage`] backend that the list should be written to
    /// * `read_buf`: a scratch buffer used to store data read from flash during verification.
    ///   Must be able to hold any serialized node.
    /// * `buf`: a scratch-buffer of length `S::MAX_ELEM_SIZE`.
    pub async fn process_writes<S: NdlDataStorage>(
        &'static self,
        storage: &mut S,
        buf: &mut [u8],
    ) -> Result<ProcessWriteCounters, LoadStoreError<S::Error>> {
        debug!("Start process_writes");

        if buf.len() != S::MAX_ELEM_SIZE {
            warn!(
                "mismatch in size of buffer provided ({}), vs storage's MAX_ELEM_SIZE ({}). Maximum item size will be limited to {}",
                buf.len(),
                S::MAX_ELEM_SIZE,
                core::cmp::min(buf.len(), S::MAX_ELEM_SIZE)
            );
        }

        // Lock the list, remember, if we're touching nodes, we need to have the list
        // locked the entire time!
        let mut inner = self.inner.lock().await;
        debug!("process_writes locked list");

        let Some(next_seq) = inner.seq_state.next_seq else {
            return Err(LoadStoreError::AppError(Error::NeedsFirstRead));
        };

        if inner.seq_state.needs_gc {
            return Err(LoadStoreError::AppError(Error::NeedsGarbageCollect));
        }

        let needs_writing = inner.needs_writing()?;

        // If the list is unchanged, there is no need to write it to flash!
        if !needs_writing {
            debug!("List does not need writing. Returning.");
            return Ok(ProcessWriteCounters {
                total_bytes_read: 0,
                current_bytes_written: 0,
            });
        }

        debug!("Attempt write_to_flash");

        // TODO: Retries have to be handled in the OUTER context, so we can
        // start over from the beginning.
        let rpt = inner.write_to_flash(buf, storage, next_seq).await?;

        // Read back all items in the list any verify the data on the flash
        // actually is what we wanted to write.
        debug!("Verifying seq {}", rpt.seq);
        let mut total_bytes_read = 0;
        let range = verify_list_in_flash(storage, &rpt, buf, &mut total_bytes_read).await?;
        debug!(
            "Verified new write at pos {}..={}",
            range.start(),
            range.end()
        );
        inner.seq_state.insert_good(GoodWriteRecord {
            seq: next_seq,
            range,
        });

        // The write must have succeeded. So mark all nodes accordingly.
        for hdrref in inner.list.iter() {
            let state = hdrref.state.load(Ordering::Acquire);
            let state = State::from_u8(state);
            let update = match state {
                // We DO NOT update any Initial nodes
                State::Initial => false,
                // We DO NOT update any NonResident nodes
                State::NonResident => false,
                // technically we could skip updating (we're already in this state),
                // but we will update anyway
                State::ValidNoWriteNeeded => true,
                // We DO update needs write nodes
                State::NeedsWrite => true,
            };
            if update {
                // Mark the store as complete
                // Rule 4.1.3: We can move from NeedsWrite -> ValidNoWriteNeeded because we hold the lock
                hdrref
                    .state
                    .store(State::ValidNoWriteNeeded.into_u8(), Ordering::Release);
            }
        }

        Ok(ProcessWriteCounters {
            total_bytes_read,
            current_bytes_written: rpt.bytes_written,
        })
    }

    /// Process garbage collection
    ///
    /// Note: this process may take a while, but does NOT hold the mutex for the entire
    /// time. However, attaches and writes will not be processed until after completion.
    ///
    /// Although we don't HOLD the mutex for the entire time, by nature of the fact
    /// that we have exclusive access to the storage medium, we can be fairly confident
    /// it would not be possible to have some other function like `process_reads` or
    /// `process_writes` called, even while we are not holding the mutex.
    pub async fn process_garbage<S: NdlDataStorage>(
        &'static self,
        storage: &mut S,
        buf: &mut [u8],
    ) -> Result<ProcessGarbageCounters, LoadStoreError<S::Error>> {
        // Take the mutex for a short time to copy out the current seq_state.
        let cur_seq_state = {
            let mut guard = self.inner.lock().await;
            if !guard.seq_state.needs_gc {
                debug!("No garbage collect needed, returning Ok");
                return Ok(ProcessGarbageCounters {
                    total_bytes_read: 0,
                    post_collection_bytes_read: 0,
                    total_bytes_popped: 0,
                });
            }

            // Take the seq_state to inhibit any other reads/writes until
            // we successfully complete, AND that a re-index will be required
            // even if we early-return here
            core::mem::replace(&mut guard.seq_state, SeqState::new())
        };

        // Helper function that iterates over each present GoodWriteRecord,
        // and returns whether ANY of the good records contain this iterator-index
        //
        // TODO: ensure none of the good items are overlapping?
        let last_records_contain = |n: usize| {
            cur_seq_state
                .last_records
                .iter()
                .filter_map(Option::as_ref)
                .any(|r| r.range.contains(&n))
        };

        // Setup counters
        let mut total_bytes_read = 0;
        let mut total_bytes_popped = 0;

        // Create an iterator
        let mut elems = storage
            .iter_elems()
            .await
            .map_err(LoadStoreError::FlashRead)?;

        // Begin iterating, keeping track of our index in the iterator
        let mut idx = 0;
        loop {
            let this_idx = idx;
            let next = elems.next(buf).await.map_err(LoadStoreError::FlashRead)?;
            let Some(next) = next else {
                // End of list reached
                break;
            };
            idx += 1;
            let used = next.len();
            total_bytes_read += used;

            // If it doesn't decode: yeet
            // If it's not in a good range: yeet
            if next.data().is_none() || !last_records_contain(this_idx) {
                total_bytes_popped += used;
                next.invalidate()
                    .await
                    .map_err(LoadStoreError::FlashWrite)?;
                continue;
            }
        }
        drop(elems);

        // todo: It's possible we could avoid a full rescan, and just update the positions
        // of the ranges based on what elements we have popped. That would require extensive
        // testing to make sure we don't accidentally drift from the correct items, so instead
        // we just reset the seq_state and re-perform the initial scan procedure.
        //
        // This will hold the mutex for a bit, but HOPEFULLY not for too long, as we only perform
        // reads here, never writes or erases.
        let mut post_collection_bytes_read = 0;
        let mut inner = self.inner.lock().await;
        inner
            .get_or_populate_latest(storage, buf, &mut post_collection_bytes_read)
            .await?;
        inner.seq_state.needs_gc = false;
        total_bytes_read += post_collection_bytes_read;

        Ok(ProcessGarbageCounters {
            total_bytes_read,
            post_collection_bytes_read,
            total_bytes_popped,
        })
    }

    /// Returns a reference to the wait queue that signals when nodes need to be read from flash.
    /// This queue is woken when new nodes are attached and require data to be loaded.
    pub async fn needs_read(&self) {
        // No need to check for errors, we never close the WaitQueue
        let _ = self.needs_read.wait().await;
    }

    /// Returns a reference to the wait queue that signals when nodes have pending writes to flash.
    /// This queue is woken when node data is modified and needs to be persisted.
    pub async fn needs_write(&self) {
        // No need to check for errors, we never close the WaitQueue
        let _ = self.needs_write.wait().await;
    }

    /// Manually reset the `cfg-noodle` cache
    ///
    /// This should not normally be necessary, and has some important caveats:
    ///
    /// 1. The next `process_reads` will be slow (requires re-building cache, like on a typical
    ///    first `process_reads`), and a `process_reads` MUST be completed before calling
    ///    `process_writes`/`process_garbage`, similar to after a clean startup.
    /// 2. This method does NOT reset the cache of our storage, for example if `sequential-storage`
    ///    is used with a key/pointer cache. You still must manually perform this action separately.
    /// 3. This method does NOT reset any of the [`StorageNode`]s. If they have previously loaded
    ///    some value, they will retain that value moving forward.
    ///
    /// This method is intended for the EXTREMELY RARE case where:
    ///
    /// 1. `process_reads` succeeds at startup
    /// 2. Some later failure/corruption of the underlying storage makes it necessary to wipe
    ///    or reset the underlying storage method
    /// 3. We need to "fix the plane while it is flying", and try to get cfg-noodle back into
    ///    a reasonable state.
    ///
    /// In this dangerous case, you probably want to do something like the following from the
    /// I/O worker task:
    ///
    /// 1. Notice the state is bad (failed writes? some other signs?)
    /// 2. Do whatever reset/full wipe/reformat of the external storage
    /// 3. (if necessary), reset the cache of the external storage (e.g. s-s's storage cache)
    /// 4. Call this method
    /// 5. Call `process_reads()` to re-build the cache
    /// 6. Call `process_garbage()`, if necessary (probably not if we have an empty storage
    ///    medium!)
    /// 7. Call `process_writes()` to persist the current state of in-memory data back to the
    ///    external flash
    ///
    /// This method does not perform any I/O.
    pub async fn reset_cache(&self) {
        warn!("manually resetting cache!");
        let mut inner = self.inner.lock().await;
        inner.seq_state = SeqState::new();
    }
}

// --------------------------------------------------------------------------
// impl StorageListInner
// --------------------------------------------------------------------------

impl<const KEPT_RECORDS: usize> StorageListInner<KEPT_RECORDS> {
    /// Find a `NodeHeader` with the given `key` in the storage list.
    ///
    /// This function searches through the intrusive linked list of storage nodes to find
    /// a node with a matching key.
    ///
    /// # Arguments
    ///
    /// * `list` - A mutable guard to the locked storage list
    /// * `key` - The key to search for
    ///
    /// # Returns
    ///
    /// * `Some(NonNull<NodeHeader>)` - A non-null pointer to the matching node header
    /// * `None` - If no node with the given key exists in the list
    pub(crate) fn find_node(&mut self, key: &str) -> Option<NonNull<NodeHeader>> {
        // SAFETY:
        // - Rule 1: NodeHeader access is ALWAYS shared
        // - Rule 8: StorageListInner is only usable with the mutex locked
        //
        // NOTE: although we could use `iter` for the search, we need to return a pointer
        // with full node provenance, so we use iter_raw instead.
        self.list
            .iter_raw()
            .find(|item| unsafe { item.as_ref() }.key == key)
    }

    /// Gets the highest seen item, performing an initial read if not yet performed
    ///
    /// Returns:
    ///
    /// - `Ok(Some(rpt))` - The most recent valid Write Record (as a [`GoodWriteRecord`])
    /// - `Ok(None)` - The storage contains NO valid write records
    /// - `Err(e)` - An error while accessing storage
    async fn get_or_populate_latest<S: NdlDataStorage>(
        &mut self,
        s: &mut S,
        buf: &mut [u8],
        bytes_read: &mut usize,
    ) -> Result<Option<GoodWriteRecord>, LoadStoreError<S::Error>> {
        // If we've already scanned, we know the highest number (or that there is none)
        if self.seq_state.initial_scan_completed() {
            return Ok(self.seq_state.highest_seen());
        }

        // We have NOT hydrated, do so now.
        //
        // Start by getting an iterator, and setting up some tracking state
        let mut iter = s.iter_elems().await.map_err(LoadStoreError::FlashRead)?;
        let mut current: Option<(NonZeroU32, usize)> = None;
        let mut crc = Crc32::new();
        let mut ctr = 0;

        // Loop over all elements in the storage...
        loop {
            let item = match iter.next(buf).await {
                Ok(Some(item)) => item,
                // end of list
                Ok(None) => break,
                // flash error
                Err(e) => return Err(LoadStoreError::FlashRead(e)),
            };
            let idx = ctr;
            ctr += 1;

            // Increment "data seen" counter
            *bytes_read += item.len();

            // What kind of data is this?
            match item.data() {
                None => {
                    // badly decoded data - if we're in the middle of a maybe-good
                    // record, invalidate it.
                    current = None;
                    continue;
                }
                // A start node: discard any current state, and start reading this
                // new node. If we had some other partial data, it is lost.
                Some(Elem::Start { seq_no }) => {
                    current = Some((seq_no, idx));
                    crc = Crc32::new();
                }
                // A data element: If we've seen a start, then CRC the data. Otherwise,
                // just ignore it and keep going until we see a start.
                Some(Elem::Data { data }) => {
                    if current.is_none() {
                        continue;
                    } else {
                        crc.update(data.key_val());
                    }
                }
                // An end element: If we've seen a start, then finish the CRC calcs so
                // far, and see if it matches what we expect. If it all checks out,
                // record it as a good item. This doesn't matter what order we visit
                // Write records, we'll just remember the top three.
                Some(Elem::End { seq_no, calc_crc }) => {
                    let Some((seq, start_idx)) = current.take() else {
                        continue;
                    };
                    let check_crc = crc.finalize();
                    crc = Crc32::new();
                    if seq != seq_no {
                        continue;
                    }
                    if calc_crc != check_crc {
                        continue;
                    }
                    debug!(
                        "Good found seq: {}, range {}..={}",
                        u32::from(seq_no),
                        start_idx,
                        idx
                    );
                    self.seq_state.insert_good(GoodWriteRecord {
                        seq: seq_no,
                        range: start_idx..=idx,
                    });
                }
            }
        }

        let highest = self.seq_state.highest_seen();
        if highest.is_none() {
            self.seq_state.next_seq = NonZeroU32::new(1);
        }

        Ok(highest)
    }

    /// Extract all data for all `State::Initial` nodes from the given sequence number
    ///
    /// Attempts to iterate over all data nodes in the current highest Write Record,
    /// and if a node matching that key exists and is in the Initial state, deserialize
    /// data into it.
    async fn extract_all<S: NdlDataStorage>(
        &mut self,
        latest: GoodWriteRecord,
        storage: &mut S,
        buf: &mut [u8],
        total_bytes_read: &mut usize,
        current_bytes_read: &mut usize,
    ) -> Result<(), LoadStoreError<S::Error>> {
        let mut queue_iter = storage
            .iter_elems()
            .await
            .map_err(LoadStoreError::FlashRead)?;

        // skip items before the start
        //
        // NOTE: We do NOT use `skip_to_seq`, because we have not necessarily
        // performed gc yet, so there may be invalid starts that we could falsely
        // match on here. Instead, ONLY trust the GoodWriteRecord indexes, and validate
        // that we end up at the sequence number that we expect.
        for _ in 0..*latest.range.start() {
            match queue_iter.next(buf).await {
                Ok(Some(item)) => {
                    *total_bytes_read += item.len();
                }
                Ok(None) => return Err(LoadStoreError::AppError(Error::InconsistentFlash)),
                Err(e) => return Err(LoadStoreError::FlashRead(e)),
            }
        }

        // We need the next item to exist...
        let Ok(Some(item)) = queue_iter.next(buf).await else {
            return Err(LoadStoreError::AppError(Error::InconsistentFlash));
        };
        // update the counter
        *total_bytes_read += item.len();
        // ...and it needs to be a Start record...
        let Some(Elem::Start { seq_no }) = item.data() else {
            return Err(LoadStoreError::AppError(Error::InconsistentFlash));
        };
        // ...and it needs to be the start record we expect
        if latest.seq != seq_no {
            return Err(LoadStoreError::AppError(Error::InconsistentFlash));
        }
        drop(item);

        // We should now be at the start of data elements.
        loop {
            let res = queue_iter.next(buf).await;
            let item = match res {
                // Got an item
                Ok(Some(item)) => item,
                // end of list - we did NOT hit a good End record!
                Ok(None) => return Err(LoadStoreError::AppError(Error::InconsistentFlash)),
                // flash error
                Err(e) => return Err(LoadStoreError::FlashRead(e)),
            };
            let item_len = item.len();
            *total_bytes_read += item_len;

            let data = match item.data() {
                // Got data: great!
                Some(Elem::Data { data }) => data,
                // Got end: if it's the one we expect, great!
                Some(Elem::End {
                    seq_no,
                    calc_crc: _,
                }) if seq_no == latest.seq => {
                    break;
                }
                // If we reached a malformed element, OR a new start, OR an end for the wrong
                // sequence number, that is bad, because this is SUPPOSED to be a pre-validated
                // write record. Something has gone very wrong.
                None | Some(Elem::Start { .. }) | Some(Elem::End { .. }) => {
                    warn!(
                        "Reached an unexpected item when extracting a pre-verified Write Record!"
                    );
                    return Err(LoadStoreError::AppError(Error::InconsistentFlash));
                }
            };
            *current_bytes_read += item_len;

            let Ok(kvpair) = data.kv_pair() else {
                warn!("Failed to extract data on extract_all!");
                continue;
            };

            debug!(
                "Extracted metadata: key {:?}, payload: {:?}",
                kvpair.key, kvpair.body,
            );
            let Some(node_header) = self.find_node(kvpair.key) else {
                // No node found? This could happen if the node was "late"
                // to the initial hydration
                debug!("Skipping key {:?}", kvpair.key);
                continue;
            };

            let header_meta = {
                // SAFETY: Rule 1: NodeHeader access is ALWAYS shared
                let node_header = unsafe { node_header.as_ref() };
                match State::from_u8(node_header.state.load(Ordering::Acquire)) {
                    State::Initial => Some(node_header.vtable),
                    State::NonResident => None,
                    State::ValidNoWriteNeeded => None,
                    State::NeedsWrite => None,
                }
            };
            if let Some(vtable) = header_meta {
                // Make a node pointer from a header pointer.
                let hdrptr: NonNull<NodeHeader> = node_header;
                let nodeptr: NonNull<Node<()>> = hdrptr.cast();

                // Call the deserialization function
                //
                // SAFETY:
                // - Rule 3.1: The node is part of this list
                // - Rule 3.1.1: The mutex is locked
                // - Rule 3.1.2.1: We checked the node is in the Initial state
                let res = unsafe { (vtable.deserialize)(nodeptr, kvpair.body) };

                // SAFETY: Rule 1: NodeHeader access is ALWAYS shared
                let hdrref = unsafe { hdrptr.as_ref() };

                // SAFETY:
                // - Rule 4.1: We are holding the mutex
                // - Rule 4.1.1 or Rule 4.1.2: We are moving from Initial to ValidNoWriteNeeded/NonResident
                if res.is_ok() {
                    // If it went okay, let the node know that it has been hydrated with data
                    //
                    hdrref
                        .state
                        .store(State::ValidNoWriteNeeded.into_u8(), Ordering::Release);
                } else {
                    // If there WAS a key, but the deser failed, this means that either the data
                    // was corrupted, or there was a breaking schema change. Either way, we can't
                    // particularly recover from this. We might want to log this, but exposing
                    // the difference between this and "no data found" to the node probably won't
                    // actually be useful.
                    warn!(
                        "Key {:?} exists and was wanted, but deserialization failed",
                        kvpair.key
                    );
                    hdrref
                        .state
                        .store(State::NonResident.into_u8(), Ordering::Release);
                }
            }
        }
        Ok(())
    }

    /// Writes all nodes in the storage list to flash memory.
    ///
    /// This function iterates through all nodes in the storage list, serializes each node's
    /// data into the provided buffer, and writes it to flash.
    /// Each node is written as a separate flash item containing its key and serialized payload.
    ///
    /// # Arguments
    /// * `buf` - A scratch buffer used for serialization (must be large enough for the largest node)
    /// * `storage` - The flash storage device to write to
    /// * `seq_no` - The sequence number to use when writing
    ///
    /// # Returns
    /// * `Ok(())` - If all nodes were successfully serialized and written to flash
    /// * `Err(StoreError::FlashWrite)` - If a flash write operation failed
    /// * `Err(StoreError::AppError(Error::Serialization))` - If node serialization failed
    async fn write_to_flash<S: NdlDataStorage>(
        &mut self,
        buf: &mut [u8],
        storage: &mut S,
        seq_no: NonZeroU32,
    ) -> Result<WriteReport, LoadStoreError<S::Error>> {
        // SAFETY: Rule 8: We can traverse the list with the mutex locked
        let iter = StaticRawIter {
            iter: self.list.iter_raw(),
        };
        let mut check_crc = Crc32::new();
        let mut bytes_written = 0usize;

        let used = storage
            .push(&Elem::Start { seq_no })
            .await
            .map_err(LoadStoreError::FlashWrite)?;
        bytes_written += used;
        let mut ctr = 0;

        for hdrptr in iter {
            // First: is this a node that is VALID for serialization?
            let state = {
                // SAFETY: Rule 1: NodeHeader access is ALWAYS shared
                let state_ref = unsafe { &*addr_of!((*hdrptr.ptr.as_ptr()).state) };
                let state = state_ref.load(Ordering::Acquire);
                State::from_u8(state)
            };

            match state {
                // State is initial: we can't write this
                State::Initial => continue,
                // State is nonresident: we can't write this
                State::NonResident => continue,
                // all other states: we can write this
                State::ValidNoWriteNeeded => {}
                State::NeedsWrite => {}
            }

            // Attempt to serialize: split off first item to reserve space for element
            // discriminant
            let (_first, rest) = buf.split_first_mut().ok_or(Error::Serialization)?;

            // SAFETY:
            // - Rule 7.2.1: Node is not in Initial/NonResident state
            // - Rule 7.2.2: the mutex is held
            let used = unsafe { serialize_node(hdrptr.ptr, rest)? };

            let used = &mut buf[..(used + 1)];

            debug!(
                "Pushing to flash: {:?}, addr {:x}",
                used,
                used.as_ptr() as usize
            );
            let serdat =
                SerData::new(used).ok_or(LoadStoreError::AppError(Error::Serialization))?;
            check_crc.update(serdat.key_val());
            // Try writing to flash
            let used = storage
                .push(&Elem::Data { data: serdat })
                .await
                .map_err(LoadStoreError::FlashWrite)?;
            bytes_written += used;
            ctr += 1;
        }
        let crc = check_crc.finalize();

        let used = storage
            .push(&Elem::End {
                seq_no,
                calc_crc: crc,
            })
            .await
            .map_err(LoadStoreError::FlashWrite)?;
        bytes_written += used;

        Ok(WriteReport {
            seq: seq_no,
            crc,
            ct: ctr,
            bytes_written,
        })
    }

    /// Iterates over all nodes, and changes Initial nodes to NonResident
    fn mark_initial_nonresident(&mut self) {
        // Set nodes in initial states to non resident
        for hdrref in self.list.iter() {
            if matches!(
                State::from_u8(hdrref.state.load(Ordering::Acquire)),
                State::Initial
            ) {
                // Initial -> NonResident is always a safe transition for a node
                hdrref
                    .state
                    .store(State::NonResident.into_u8(), Ordering::Release);
            }
        }
    }

    fn needs_writing(&mut self) -> Result<bool, Error> {
        // Set to true if any node in the list needs writing
        let mut needs_writing = false;

        // Iterate over all list nodes and check if any node needs writing or
        // is in an invalid state.
        for header in self.list.iter() {
            // The node may be spinning to populate itself right now, so use acquire ordering
            match State::from_u8(header.state.load(Ordering::Acquire)) {
                // If no write is needed, we obviously won't write.
                State::ValidNoWriteNeeded => {}
                // The node has been changed (including default write-backs) but not
                // written to flash yet.
                //
                // NOTE: We don't want to early return so we can detect nodes in invalid
                // states
                State::NeedsWrite => needs_writing = true,
                // A node may be in `Initial` state, if it has been attached but
                // `process_reads` hasn't run, yet.
                State::Initial => {
                    return Err(Error::NeedsFirstRead);
                }
                // A node may be NonResident if we are quickly doing a write after an
                // initial read, but the storage node has not yet had a chance to
                // initialize itself with a default value. Just skip.
                State::NonResident => {}
            }
        }

        // If NONE of the items need writing, BUT we also have no data on disk, then
        // we should perform a write-back. In MOST cases, this isn't necessary, but
        // if some kind of tricky reset has happened, e.g. `reset_cache()` has been
        // called, we should check if we need to write-back.
        if !needs_writing {
            needs_writing |= self.seq_state.highest_seen().is_none();
        }

        Ok(needs_writing)
    }
}

// --------------------------------------------------------------------------
// impl SeqState
// --------------------------------------------------------------------------

impl<const KEPT_RECORDS: usize> SeqState<KEPT_RECORDS> {
    const fn new() -> Self {
        Self {
            next_seq: None,
            last_records: [const { None }; KEPT_RECORDS],
            needs_gc: false,
        }
    }

    /// Have we hydrated from the external flash?
    #[inline]
    fn initial_scan_completed(&self) -> bool {
        self.next_seq.is_some()
    }

    /// What is the highest sequence number we've seen in external flash, if any?
    #[inline]
    fn highest_seen(&self) -> Option<GoodWriteRecord> {
        if let Some(last) = self.last_records.last()
            && let Some(seen) = last.as_ref()
        {
            return Some(seen.clone());
        }
        None
    }

    /// Insert a known-good sequence number
    fn insert_good(&mut self, rec: GoodWriteRecord) {
        let lowest = self
            .last_records
            .iter_mut()
            .min_by_key(|rec| match rec {
                Some(rec) => rec.seq.get(),
                None => 0,
            })
            .expect("This is a non-0-len array, not a slice. We can safely unwrap");

        if let Some(lowest) = lowest
            && lowest.seq > rec.seq
        {
            // We have newer records already
            return;
        }

        *lowest = Some(rec);

        self.last_records
            .sort_unstable_by_key(|n| n.as_ref().map(|i| i.seq));

        // Should be impossible to not have something, just prevent panic branches
        let next = self
            .last_records
            .last()
            .expect("This is a non-0-len array, not a slice. We can safely unwrap")
            .as_ref()
            .map(|n| n.seq)
            .unwrap_or(const { NonZeroU32::new(1).unwrap() });
        // Note: this could overflow, but it would take 2^32 writes, which is
        // likely much more than the durability of reasonable flash parts (100k cycles)
        self.next_seq = next.checked_add(1);
        // Note: this is an overly cautious heuristic, if we are either doing
        // first pass, OR if we just inserted something, assume we need GC.
        self.needs_gc = true;
    }
}

// --------------------------------------------------------------------------
// Helper structs and functions
// --------------------------------------------------------------------------

/// Wrapper for [`IterRaw`] that implements `Send`.
///
/// This WOULD be addressed by https://github.com/hawkw/mycelium/pull/536, which
/// makes IterRaw impl Send, however we still need to wrap the yielded NonNulls,
/// which are not Send. Therefore, we will keep this structure mostly for the
/// ability to wrap the Iterator impl to return Send-implementing [`SendPtr`]s
/// instead of `NonNull`s.
struct StaticRawIter<'a> {
    iter: IterRaw<'a, NodeHeader>,
}

/// SAFETY: The contained IterRaw is only valid for the lifetime of the List it
/// comes from, which can only be obtained by holding the mutex. This means that we
/// have exclusive access, and all nodes must be 'static. Therefore, it is
/// sound to Send both the iterator, and the wrapped NonNulls it returns.
///
/// - Rule 1: Nodes are always shared
/// - Rule 5: The nodes may not be attached/detached while we hold the mutex
unsafe impl Send for StaticRawIter<'_> {}

impl Iterator for StaticRawIter<'_> {
    type Item = SendPtr;

    fn next(&mut self) -> Option<SendPtr> {
        self.iter.next().map(|ptr| SendPtr { ptr })
    }
}

/// Wrapper for [`NonNull<NodeHeader>`] that implements `Send`.
///
/// This is necessary because we iterate over the IterRaw in async context,
/// and for testing this means that futures need to be Send. Since the IterRaw
/// yields `NonNull<T>`s, the iterated nodes are not Send. This adapter is
/// sound because for as long as we have the IterRaw live, the mutex must remain
/// locked.
struct SendPtr {
    ptr: NonNull<NodeHeader>,
}

/// SAFETY: The contained NodeHeader ptr is only valid for the lifetime of the List it
/// comes from, which can only be obtained by holding the mutex. This means that we
/// have exclusive access, and all nodes must be 'static. Therefore, it is
/// sound to Send the wrapped NonNull.
///
/// - Rule 1: Nodes are always shared
/// - Rule 5: The nodes may not be attached/detached while we hold the mutex
unsafe impl Send for SendPtr {}

/// Serialize a storage node by writing its key and payload data
/// into the provided buffer.
///
/// # Format
///
/// The key is serialized as a CBOR `&str`, followed by the CBOR serialized payload `T`.
///
/// # Arguments
///
/// * `headerptr` - A non-null pointer to the NodeHeader to serialize
/// * `buf` - The buffer to write the serialized data into
///
/// # Returns
///
/// * `Ok(usize)` - The total number of bytes written to the buffer
/// * `Err(Error::Serialization)` - If serialization fails (e.g., buffer too small, serialization error)
///
/// # Safety
///
/// The caller must ensure that:
///
/// - `headerptr` points to a valid NodeHeader
/// - Rule 7.2.1 :The Node is NOT in the Initial/NonResident state
/// - Rule 7.2.2: The mutex MUST be held for the duration of this operation
unsafe fn serialize_node(headerptr: NonNull<NodeHeader>, buf: &mut [u8]) -> Result<usize, Error> {
    let (vtable, key) = {
        // SAFETY: Rule 1: NodeHeader access is ALWAYS shared
        let node = unsafe { headerptr.as_ref() };
        (node.vtable, node.key)
    };

    debug!("serializing node with key <{:?}>", key,);

    let mut cursor = Cursor::new(&mut *buf);
    let res: Result<(), minicbor::encode::Error<EndOfSlice>> = minicbor::encode(key, &mut cursor);
    let Ok(()) = res else {
        error!("Key could not be serialized to the buffer.");
        return Err(Error::Serialization);
    };
    let used = cursor.position();
    let Some(remain) = buf.get_mut(used..) else {
        error!("Serialized key consumed the entire buffer");
        return Err(Error::Serialization);
    };

    let nodeptr: NonNull<Node<()>> = headerptr.cast();

    // Serialize payload into buffer
    //
    // SAFETY: Caller has guaranteed compliance with:
    // - Rule 7.2.1 :The Node is NOT in the Initial/NonResident state
    // - Rule 7.2.2: The mutex MUST be held for the duration of this operation
    let res = unsafe { (vtable.serialize)(nodeptr, remain) };

    match res {
        Ok(len) => Ok(len + used),
        Err(_) => {
            error!("Node key+payload too large for serializing into the buffer.");
            Err(Error::Serialization)
        }
    }
}

/// Verifies that the storage list was correctly written to flash memory.
///
/// This function compares checks the CRC of the given Write Record to ensure it was
/// stored to flash correctly
///
/// # Arguments
///
/// * `storage` - The flash storage device and address range to read from
/// * `rpt` - Metadata about the write that just completed
/// * `buf` - Buffer used for reading back nodes
///
/// # Returns
///
/// * `Ok(())` - If all nodes match their corresponding flash entries
/// * `Err(LoadError::WriteVerificationFailed)` - If any node doesn't match its flash entry
/// * `Err(LoadError::FlashRead)` - If reading from flash fails
///
async fn verify_list_in_flash<S: NdlDataStorage>(
    storage: &mut S,
    rpt: &WriteReport,
    buf: &mut [u8],
    total_bytes_read: &mut usize,
) -> Result<RangeInclusive<usize>, LoadStoreError<S::Error>> {
    // Create a `QueueIterator`
    let mut total_items_seen_ctr = 0;
    let mut queue_iter = storage
        .iter_elems()
        .await
        .map_err(LoadStoreError::FlashRead)?;

    let seq = rpt.seq;

    // Fast forward until we reach the expected start
    loop {
        match queue_iter.next(buf).await {
            Ok(Some(item)) => {
                total_items_seen_ctr += 1;
                *total_bytes_read += item.len();
                if matches!(item.data(), Some(Elem::Start { seq_no }) if seq_no == seq) {
                    // Found it!
                    break;
                }
            }
            Ok(None) => return Err(LoadStoreError::AppError(Error::InconsistentFlash)),
            Err(e) => return Err(LoadStoreError::FlashRead(e)),
        }
    }
    // We just saw the start counter, its position is 1 back from the # of items seen
    let start_pos = total_items_seen_ctr - 1;

    let mut crc = Crc32::new();
    let mut data_items_consumed_ctr = 0;
    loop {
        let item = match queue_iter.next(buf).await {
            Ok(Some(i)) => {
                debug!("visiting {:?} in verify", i.data());
                *total_bytes_read += i.len();
                i
            }
            Ok(None) => {
                debug!("Surprising end of list");
                return Err(LoadStoreError::WriteVerificationFailed);
            }
            Err(e) => return Err(LoadStoreError::FlashRead(e)),
        };
        total_items_seen_ctr += 1;

        let data = item.data();
        match data {
            None => {
                debug!("Surprising invalid element");
                return Err(LoadStoreError::WriteVerificationFailed);
            }
            Some(Elem::Start { .. }) => {
                debug!("Surprising start of list");
                return Err(LoadStoreError::WriteVerificationFailed);
            }
            Some(Elem::Data { data }) => {
                crc.update(data.key_val());
                data_items_consumed_ctr += 1;
            }
            Some(Elem::End { seq_no, calc_crc }) => {
                let check_crc = crc.finalize();
                let seq_chk = seq_no == rpt.seq;
                let crc_chk1 = check_crc == rpt.crc;
                let crc_chk2 = calc_crc == rpt.crc;
                let ctr_chk = data_items_consumed_ctr == rpt.ct;
                let good = seq_chk && crc_chk1 && crc_chk2 && ctr_chk;
                debug!(
                    "check: {}, {}, {}, {} => {}",
                    seq_chk, crc_chk1, crc_chk2, ctr_chk, good
                );
                if good {
                    return Ok(RangeInclusive::new(start_pos, total_items_seen_ctr));
                } else {
                    return Err(LoadStoreError::WriteVerificationFailed);
                }
            }
        }
    }
}

#[cfg(test)]
mod test {
    #![allow(clippy::unwrap_used)]

    extern crate std;

    use crate::StorageListNode;
    use crate::test_utils::{
        TestStorage, get_mock_flash, worker_task_seq_sto, worker_task_tst_sto,
        worker_task_tst_sto_custom,
    };

    use super::*;

    use core::time::Duration;
    use minicbor::{CborLen, Decode, Encode};
    use mutex::raw_impls::cs::CriticalSectionRawMutex;
    use std::sync::Arc;
    use test_log::test;
    use tokio::task::LocalSet;
    use tokio::time::sleep;

    #[test]
    fn seq_sort() {
        const fn maker(seq: u32, range: RangeInclusive<usize>) -> GoodWriteRecord {
            GoodWriteRecord {
                seq: NonZeroU32::new(seq).unwrap(),
                range,
            }
        }

        const ONE: GoodWriteRecord = maker(1, 10..=15);
        const TWO: GoodWriteRecord = maker(2, 20..=25);
        const THREE: GoodWriteRecord = maker(3, 30..=35);
        const FOUR: GoodWriteRecord = maker(4, 40..=45);

        let mut s = SeqState::new();
        assert_eq!(s.last_records, [None, None, None]);
        s.insert_good(ONE);
        assert_eq!(s.last_records, [None, None, Some(ONE)]);
        s.insert_good(TWO);
        assert_eq!(s.last_records, [None, Some(ONE), Some(TWO)]);
        s.insert_good(THREE);
        assert_eq!(s.last_records, [Some(ONE), Some(TWO), Some(THREE)]);
        s.insert_good(FOUR);
        assert_eq!(s.last_records, [Some(TWO), Some(THREE), Some(FOUR)]);
        s.insert_good(ONE);
        assert_eq!(s.last_records, [Some(TWO), Some(THREE), Some(FOUR)]);
    }

    #[derive(Debug, Encode, Decode, Clone, PartialEq, CborLen)]
    struct PositronConfig {
        #[n(0)]
        up: u8,
        #[n(1)]
        down: u16,
        #[n(2)]
        strange: u32,
    }

    impl Default for PositronConfig {
        fn default() -> Self {
            Self {
                up: 10,
                down: 20,
                strange: 103,
            }
        }
    }

    /// Test that we can handle multiple nodes using sequential-storage
    #[test(tokio::test)]
    async fn test_two_configs_seq_sto() {
        static GLOBAL_LIST: StorageList<CriticalSectionRawMutex, 3> = StorageList::new();
        static POSITRON_CONFIG1: StorageListNode<PositronConfig> =
            StorageListNode::new("positron/config1");
        static POSITRON_CONFIG2: StorageListNode<PositronConfig> =
            StorageListNode::new("positron/config2");

        let flash = get_mock_flash();

        let local = LocalSet::new();
        local
            .run_until(async move {
                info!("Spawn worker_task");
                let worker_task =
                    tokio::task::spawn_local(worker_task_seq_sto(&GLOBAL_LIST, flash));

                // Obtain a handle for the first config
                let mut config_handle = match POSITRON_CONFIG1.attach(&GLOBAL_LIST).await {
                    Ok(ch) => ch,
                    Err(_) => panic!("Could not attach config 1 to list"),
                };

                // Obtain a handle for the second config. This should _not_ error!
                if POSITRON_CONFIG2.attach(&GLOBAL_LIST).await.is_err() {
                    panic!("Could not attach config 2 to list");
                }

                // Load data for the first handle
                let data: PositronConfig = config_handle.load();
                info!("T3 Got {data:?}");

                // Write a new config to first handle
                let new_config = PositronConfig {
                    up: 15,
                    down: 25,
                    strange: 108,
                };
                config_handle
                    .write(&new_config)
                    .await
                    .expect("Writing config to node should not fail");

                // Give the worker_task some time to process the write
                sleep(Duration::from_millis(100)).await;

                // Assert that the loaded value equals the written value
                assert_eq!(config_handle.load(), new_config);

                // Wait for the worker task to finish
                let _ = tokio::time::timeout(Duration::from_millis(100), worker_task).await;
            })
            .await;
    }

    /// Test that we can handle multiple nodes using the TestStorage backend
    #[test(tokio::test)]
    async fn test_two_configs_tst_sto() {
        static GLOBAL_LIST: StorageList<CriticalSectionRawMutex, 3> = StorageList::new();
        static POSITRON_CONFIG1: StorageListNode<PositronConfig> =
            StorageListNode::new("positron/config1");
        static POSITRON_CONFIG2: StorageListNode<PositronConfig> =
            StorageListNode::new("positron/config2");

        let local = LocalSet::new();
        local
            .run_until(async move {
                let stopper = Arc::new(WaitQueue::new());
                info!("Spawn worker_task");
                let worker_task =
                    tokio::task::spawn_local(worker_task_tst_sto(&GLOBAL_LIST, stopper.clone()));

                // Obtain a handle for the first config
                let mut config_handle = match POSITRON_CONFIG1.attach(&GLOBAL_LIST).await {
                    Ok(ch) => ch,
                    Err(_) => panic!("Could not attach config 1 to list"),
                };

                // Obtain a handle for the second config. This should _not_ error!
                let Ok(_ch2) = POSITRON_CONFIG2.attach(&GLOBAL_LIST).await else {
                    panic!("Could not attach config 2 to list");
                };

                // Load data for the first handle
                let data: PositronConfig = config_handle.load();
                info!("T3 Got {data:?}");

                // Write a new config to first handle
                let new_config = PositronConfig {
                    up: 15,
                    down: 25,
                    strange: 108,
                };
                config_handle
                    .write(&new_config)
                    .await
                    .expect("Writing config to node should not fail");

                // Give the worker_task some time to process the write
                sleep(Duration::from_millis(100)).await;

                // Assert that the loaded value equals the written value
                assert_eq!(config_handle.load(), new_config);

                // Wait for the worker task to finish
                stopper.close();
                let report = tokio::time::timeout(Duration::from_millis(100), worker_task)
                    .await
                    .unwrap()
                    .unwrap();
                report.assert_no_errs();
            })
            .await;
    }

    /// This test will write a config to the flash first and then read it back to check
    /// whether reading an item from flash works.
    #[test(tokio::test)]
    async fn test_load_existing() {
        static GLOBAL_LIST: StorageList<CriticalSectionRawMutex, 3> = StorageList::new();
        static POSITRON_CONFIG: StorageListNode<PositronConfig> =
            StorageListNode::new("positron/config");

        let local = LocalSet::new();
        local
            .run_until(async move {
                // First, write our custom config
                let mut flash = TestStorage::default();
                let mut wr = flash.start_write_record(NonZeroU32::new(1).unwrap());

                // Serialize the custom_config config so we can write it to our flash
                let custom_config = PositronConfig {
                    up: 1,
                    down: 22,
                    strange: 333,
                };
                assert_ne!(custom_config, PositronConfig::default());
                wr.add_data_elem("positron/config", &custom_config);
                wr.end_write_record();

                let stopper = Arc::new(WaitQueue::new());
                info!("Spawn worker_task");
                let worker_task = tokio::task::spawn_local(worker_task_tst_sto_custom(
                    &GLOBAL_LIST,
                    stopper.clone(),
                    flash,
                ));

                // Obtain a handle for the config. It should match the custom_config.
                // This should _not_ error!
                let expecting_already_present = match POSITRON_CONFIG.attach(&GLOBAL_LIST).await {
                    Ok(ch) => ch,
                    Err(_) => panic!("Could not attach config to list"),
                };

                assert_eq!(
                    custom_config,
                    expecting_already_present.load(),
                    "Key should already be present"
                );

                // Wait for the worker task to finish
                stopper.close();
                let report = tokio::time::timeout(Duration::from_secs(2), worker_task)
                    .await
                    .unwrap()
                    .unwrap();
                report.assert_no_errs();
            })
            .await;
    }

    /// Test that attaching duplicate keys causes a panic
    #[test(tokio::test)]
    async fn test_duplicate_key() {
        static GLOBAL_LIST: StorageList<CriticalSectionRawMutex, 3> = StorageList::new();
        static POSITRON_CONFIG1: StorageListNode<PositronConfig> =
            StorageListNode::new("positron/config");
        static POSITRON_CONFIG2: StorageListNode<PositronConfig> =
            StorageListNode::new("positron/config");

        let local = LocalSet::new();
        local
            .run_until(async move {
                let stopper = Arc::new(WaitQueue::new());
                info!("Spawn worker_task");
                // We still need the worker task so we can fulfill reads
                let _worker_task =
                    tokio::task::spawn_local(worker_task_tst_sto(&GLOBAL_LIST, stopper.clone()));

                // Obtain a handle for the first config
                let _config_handle = match POSITRON_CONFIG1.attach(&GLOBAL_LIST).await {
                    Ok(ch) => ch,
                    Err(_) => panic!("Could not attach config 1 to list"),
                };

                // Obtain a handle for the second config. It has the same key as the first.
                let config_handle = POSITRON_CONFIG2.attach(&GLOBAL_LIST).await;
                assert_eq!(
                    config_handle.expect_err("Duplicate key did not cause an error"),
                    Error::DuplicateKey,
                );
            })
            .await;
    }

    /// Test creating two handles from the same node fails
    #[test(tokio::test)]
    async fn test_duplicate_handle() {
        static GLOBAL_LIST: StorageList<CriticalSectionRawMutex, 3> = StorageList::new();
        static POSITRON_CONFIG1: StorageListNode<PositronConfig> =
            StorageListNode::new("positron/config");

        let local = LocalSet::new();
        local
            .run_until(async move {
                let stopper = Arc::new(WaitQueue::new());
                info!("Spawn worker_task");
                // We still need the worker task so we can fulfill reads
                let _worker_task =
                    tokio::task::spawn_local(worker_task_tst_sto(&GLOBAL_LIST, stopper.clone()));

                // Obtain a handle for the config
                let _config_handle = match POSITRON_CONFIG1.attach(&GLOBAL_LIST).await {
                    Ok(ch) => ch,
                    Err(_) => panic!("Could not attach config 1 to list"),
                };

                // Try to re-obtain a handle for the same config.
                let config_handle = POSITRON_CONFIG1.attach(&GLOBAL_LIST).await;
                assert_eq!(
                    config_handle.expect_err("Duplicate handle did not cause an error"),
                    Error::AlreadyTaken,
                );
            })
            .await;
    }

    /// Test reattaching a node fails before dropping the handle and works after dropping the handle
    #[test(tokio::test)]
    async fn test_reattach_handle() {
        static GLOBAL_LIST: StorageList<CriticalSectionRawMutex, 3> = StorageList::new();
        static POSITRON_CONFIG1: StorageListNode<PositronConfig> =
            StorageListNode::new("positron/config");

        let local = LocalSet::new();
        local
            .run_until(async move {
                let stopper = Arc::new(WaitQueue::new());
                info!("Spawn worker_task");
                // We still need the worker task so we can fulfill reads
                let _worker_task =
                    tokio::task::spawn_local(worker_task_tst_sto(&GLOBAL_LIST, stopper.clone()));

                // Obtain a handle for the config
                let _config_handle = match POSITRON_CONFIG1.attach(&GLOBAL_LIST).await {
                    Ok(ch) => ch,
                    Err(_) => panic!("Could not attach config 1 to list"),
                };

                // Try to re-obtain a handle for the same config on the same list.
                let config_handle = POSITRON_CONFIG1.attach(&GLOBAL_LIST).await;
                assert_eq!(
                    config_handle.expect_err("Duplicate handle did not cause an error"),
                    Error::AlreadyTaken,
                );

                // explicitly drop handle
                drop(_config_handle);

                // Re-attach allowed
                let _config_handle = match POSITRON_CONFIG1.attach(&GLOBAL_LIST).await {
                    Ok(ch) => ch,
                    Err(_) => panic!("Could not re-attach config 1 to list"),
                };
            })
            .await;
    }

    /// Test creating two handles from the same node fails
    #[test(tokio::test)]
    async fn test_cant_reattach_without_detach_handle() {
        static GLOBAL_LIST1: StorageList<CriticalSectionRawMutex, 3> = StorageList::new();
        static GLOBAL_LIST2: StorageList<CriticalSectionRawMutex, 3> = StorageList::new();
        static POSITRON_CONFIG1: StorageListNode<PositronConfig> =
            StorageListNode::new("positron/config");

        let local = LocalSet::new();
        local
            .run_until(async move {
                let stopper = Arc::new(WaitQueue::new());
                info!("Spawn worker_task");
                // We still need the worker task so we can fulfill reads
                let _worker_task1 =
                    tokio::task::spawn_local(worker_task_tst_sto(&GLOBAL_LIST1, stopper.clone()));
                // We still need the worker task so we can fulfill reads
                let _worker_task2 =
                    tokio::task::spawn_local(worker_task_tst_sto(&GLOBAL_LIST2, stopper.clone()));

                // Obtain a handle for the config
                let _config_handle = match POSITRON_CONFIG1.attach(&GLOBAL_LIST1).await {
                    Ok(ch) => ch,
                    Err(_) => panic!("Could not attach config 1 to list"),
                };

                // Try to re-obtain a handle for the same config on the same list.
                let config_handle = POSITRON_CONFIG1.attach(&GLOBAL_LIST1).await;
                assert_eq!(
                    config_handle.expect_err("Duplicate handle did not cause an error"),
                    Error::AlreadyTaken,
                );

                // Try to attach to a different list
                let config_handle = POSITRON_CONFIG1.attach(&GLOBAL_LIST2).await;
                assert_eq!(
                    config_handle.expect_err("Attach on second list shouldn't work"),
                    Error::NodeInOtherList,
                );

                // explicitly drop handle
                drop(_config_handle);

                // Try to attach to a different list
                let config_handle = POSITRON_CONFIG1.attach(&GLOBAL_LIST2).await;
                assert_eq!(
                    config_handle.expect_err("Attach on second list shouldn't work"),
                    Error::NodeInOtherList,
                );

                // Detach from wrong list doesn't work
                let res = POSITRON_CONFIG1.detach(&GLOBAL_LIST2).await;
                assert_eq!(
                    res.expect_err("Detach from wrong list shouldn't work"),
                    Error::NodeInOtherList,
                );

                // Detach from list
                let _: () = POSITRON_CONFIG1
                    .detach(&GLOBAL_LIST1)
                    .await
                    .expect("Detach should succeed");

                // Try to attach to a different list
                let _config_handle = POSITRON_CONFIG1
                    .attach(&GLOBAL_LIST2)
                    .await
                    .expect("Attach after detach should work");
            })
            .await;
    }
}
