//! # Intrusive Linked List for Configuration Storage
//!
//! This module implements the [`StorageList`] type, which serves as an "anchor"
//! to attach storage nodes onto.
//!
//! Users will typically create a [`StorageList`], and then primarily interact with
//! [`StorageListNode`](crate::StorageListNode) and [`StorageListNodeHandle`](crate::StorageListNodeHandle)

use crate::{
    Crc32, Elem, NdlDataStorage, NdlElemIter, NdlElemIterNode, SerData, StepErr, StepResult,
    error::{Error, LoadStoreError},
    logging::{debug, info},
    skip_to_seq, step,
    storage_node::{Node, NodeHeader, State},
};
use cordyceps::{List, list::IterRaw};
use core::{num::NonZeroU32, ptr::NonNull};
use maitake_sync::{Mutex, WaitQueue};
use minicbor::{
    encode::write::{Cursor, EndOfSlice},
    len_with,
};
use mutex::{ConstInit, ScopedRawMutex};

/// "Global anchor" of all storage items.
///
/// It serves as the meeting point between two conceptual pieces:
///
/// 1. The [`StorageListNode`](crate::StorageListNode)s, which each store one configuration item
/// 2. The worker task that owns the external flash, and serves the
///    role of loading data FROM flash (and putting it in the Nodes),
///    as well as the role of deciding when to write data TO flash
///    (retrieving it from each node).
pub struct StorageList<R: ScopedRawMutex> {
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
    pub(crate) inner: Mutex<StorageListInner, R>,
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

/// The functional core of a [`StorageList`]
///
/// This type contains any pieces that require the mutex to be locked to access.
/// This means that holding an `&mut StorageListInner` means that you have exclusive
/// access to the entire list and all nodes attached to it.
pub(crate) struct StorageListInner {
    pub(crate) list: List<NodeHeader>,
    seq_state: SeqState,
}

#[derive(Default)]
struct SeqState {
    /// Next Sequence Number. Is None if we have not hydrated
    next_seq: Option<NonZeroU32>,
    /// The last three valid sequence numbers, in sorted order
    last_three: [Option<NonZeroU32>; 3],
}

struct WriteReport {
    seq: NonZeroU32,
    crc: u32,
    ct: usize,
}

// --------------------------------------------------------------------------
// impl StorageList
// --------------------------------------------------------------------------

impl<R: ScopedRawMutex + ConstInit> StorageList<R> {
    /// const constructor to make a new empty list. Intended to be used
    /// to create a static.
    ///
    /// ```
    /// # use cfg_noodle::StorageList;
    /// # use mutex::raw_impls::cs::CriticalSectionRawMutex;
    /// static GLOBAL_LIST: StorageList<CriticalSectionRawMutex> = StorageList::new();
    /// ```
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new_with_raw_mutex(
                StorageListInner {
                    list: List::new(),
                    seq_state: SeqState {
                        next_seq: None,
                        last_three: [None; 3],
                    },
                },
                R::INIT,
            ),
            needs_read: WaitQueue::new(),
            needs_write: WaitQueue::new(),
            reading_done: WaitQueue::new(),
        }
    }
}

impl<R: ScopedRawMutex + ConstInit> Default for StorageList<R> {
    /// this only exists to shut up the clippy lint about impl'ing default
    fn default() -> Self {
        Self::new()
    }
}

/// These are public methods for the `StorageList`. They currently are intended to be
/// used by the "storage worker task", that decides when we actually want to
/// interact with the flash.
impl<R: ScopedRawMutex> StorageList<R> {
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
    /// - `buf`: a scratch-buffer that must be large enough to hold the largest serialized node.
    ///   In practice, a buffer as large as the flash's page size is enough.
    ///
    pub async fn process_reads<S: NdlDataStorage>(
        &'static self,
        storage: &mut S,
        buf: &mut [u8],
    ) -> Result<(), LoadStoreError<S::Error>> {
        info!("Start process_reads");
        // Lock the list, remember, if we're touching nodes, we need to have the list
        // locked the entire time!
        let mut inner = self.inner.lock().await;
        debug!("process_reads locked list");

        // Have we already determined the latest valid write record?
        let res = inner.get_or_populate_latest(storage, buf).await?;

        // If we have something: populate all present items
        if let Some(latest) = res {
            inner.extract_all(latest, storage, buf).await?;
        }

        // Now, we either have NOTHING, or we have populated all items that DO exist.
        // Mark any nodes that are STILL in the INITIAL state as nonresident.
        inner.mark_initial_nonresident();

        debug!("Reading done. Waking all.");
        self.reading_done.wake_all();
        Ok(())
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
    /// * `buf`: a scratch buffer used to serialize nodes for writing.  Must be able to hold
    ///   any serialized node. In practice, a buffer as large as the flash's page size is
    ///   sufficient.
    pub async fn process_writes<S: NdlDataStorage>(
        &'static self,
        storage: &mut S,
        buf: &mut [u8],
    ) -> Result<(), LoadStoreError<S::Error>> {
        debug!("Start process_writes");

        // Lock the list, remember, if we're touching nodes, we need to have the list
        // locked the entire time!
        let mut inner = self.inner.lock().await;
        debug!("process_writes locked list");

        let Some(next_seq) = inner.seq_state.next_seq else {
            return Err(LoadStoreError::NeedsFirstRead);
        };

        let needs_writing = inner.needs_writing()?;

        // If the list is unchanged, there is no need to write it to flash!
        if !needs_writing {
            debug!("List does not need writing. Returning.");
            return Ok(());
        }

        debug!("Attempt write_to_flash");

        // TODO: Retries have to be handled in the OUTER context, so we can
        // start over from the beginning.
        let rpt = inner.write_to_flash(buf, storage, next_seq).await?;

        // Read back all items in the list any verify the data on the flash
        // actually is what we wanted to write.
        verify_list_in_flash(storage, rpt, buf).await?;
        inner.seq_state.insert_good(next_seq);

        // The write must have succeeded. So mark all nodes accordingly.
        for hdrmut in inner.list.iter_mut() {
            // Mark the store as complete, wake the node if it cares about state
            // changes.
            //
            // SAFETY: We hold the lock to the list, so no other thread can modify
            // the node while we are modifying it.
            unsafe { hdrmut.set_state(State::ValidNoWriteNeeded) }
        }

        Ok(())
    }

    /// Returns a reference to the wait queue that signals when nodes need to be read from flash.
    /// This queue is woken when new nodes are attached and require data to be loaded.
    pub fn needs_read(&self) -> &WaitQueue {
        &self.needs_read
    }

    /// Returns a reference to the wait queue that signals when nodes have pending writes to flash.
    /// This queue is woken when node data is modified and needs to be persisted.
    pub fn needs_write(&self) -> &WaitQueue {
        &self.needs_write
    }
}

// --------------------------------------------------------------------------
// impl StorageListInner
// --------------------------------------------------------------------------

impl StorageListInner {
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
        // SAFETY: We have exclusive access to the contents of the list.
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
    /// - `Ok(Some(seq))` - This is the highest valid seen sequence number
    /// - `Ok(None)` - The storage contains NO valid write records
    /// - `Err(e)` - An error while accessing storage
    async fn get_or_populate_latest<S: NdlDataStorage>(
        &mut self,
        s: &mut S,
        buf: &mut [u8],
    ) -> Result<Option<NonZeroU32>, LoadStoreError<S::Error>> {
        // If we've already hydrated, we know the highest number (or that there is none)
        if self.seq_state.has_hydrated() {
            return Ok(self.seq_state.highest_seen());
        }

        // We have NOT hydrated, do so now.
        //
        // Start by getting an iterator, and setting up some tracking state
        let mut iter = s.iter_elems().await.map_err(LoadStoreError::FlashRead)?;
        let mut current: Option<NonZeroU32> = None;
        let mut crc = Crc32::new();

        // Loop over all elements in the storage...
        'outer: loop {
            // Loop until we find a valid item. If we reach the end of the list
            // or a read error, bail.
            let item = loop {
                let res = iter.next(buf).await;
                match res {
                    // Got at item
                    Ok(Some(Some(item))) => break item,
                    // Got a bad item, continue
                    Ok(Some(None)) => {}
                    // end of list
                    Ok(None) => break 'outer,
                    // flash error
                    Err(e) => return Err(LoadStoreError::FlashRead(e)),
                };
            };

            // What kind of data is this?
            match item.data() {
                // A start node: discard any current state, and start reading this
                // new node. If we had some other partial data, it is lost.
                Elem::Start { seq_no } => {
                    current = Some(seq_no);
                    crc = Crc32::new();
                }
                // A data element: If we've seen a start, then CRC the data. Otherwise,
                // just ignore it and keep going until we see a start.
                Elem::Data { data } => {
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
                Elem::End { seq_no, calc_crc } => {
                    let Some(seq) = current.take() else {
                        continue;
                    };
                    let check_crc = crc.finalize();
                    crc = Crc32::new();
                    if seq != seq_no {
                        continue;
                    }
                    // todo: real crc
                    if calc_crc != check_crc {
                        continue;
                    }
                    self.seq_state.insert_good(seq_no);
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
        latest: NonZeroU32,
        storage: &mut S,
        buf: &mut [u8],
    ) -> Result<(), LoadStoreError<S::Error>> {
        let mut queue_iter = storage
            .iter_elems()
            .await
            .map_err(LoadStoreError::FlashRead)?;

        let skipres = skip_to_seq!(queue_iter, buf, latest);
        match skipres {
            Ok(()) => {}
            // This state should be impossible: we already determined which seq_no to skip to
            Err(StepErr::EndOfList) => unreachable!("external flash inconsistent"),
            Err(StepErr::FlashError(e)) => return Err(LoadStoreError::FlashRead(e)),
        };

        'outer: loop {
            let item = loop {
                let res = queue_iter.next(buf).await;
                match res {
                    // Got at item
                    Ok(Some(Some(item))) => break item,
                    // Got a bad item, continue
                    Ok(Some(None)) => {}
                    // end of list
                    Ok(None) => break 'outer,
                    // flash error
                    Err(e) => return Err(LoadStoreError::FlashRead(e)),
                };
            };

            let data = match item.data() {
                Elem::Start { .. } | Elem::End { .. } => break,
                Elem::Data { data } => data,
            };

            let Ok(kvpair) = extract(data.key_val()) else {
                continue;
            };

            debug!(
                "Extracted metadata: key {:?}, payload: {:?}",
                kvpair.key, kvpair.body,
            );
            let Some(node_header) = self.find_node(kvpair.key) else {
                // No node found?
                continue;
            };

            // SAFETY: We hold the lock, we are allowed to gain exclusive mut access
            // to the contents of the node. We COPY OUT the vtable for later use,
            // which is important because we might throw away our `node` ptr shortly.
            let header_meta = {
                let node_header = unsafe { node_header.as_ref() };
                match node_header.state {
                    State::Initial => Some(node_header.vtable),
                    State::NonResident => None,
                    State::DefaultUnwritten => None,
                    State::ValidNoWriteNeeded => None,
                    State::NeedsWrite => None,
                }
            };
            if let Some(vtable) = header_meta {
                // Make a node pointer from a header pointer. This *consumes* the `Pin<&mut NodeHeader>`, meaning
                // we are free to later re-invent other mutable ptrs/refs, AS LONG AS we still treat the data
                // as pinned.
                let mut hdrptr: NonNull<NodeHeader> = node_header;
                let nodeptr: NonNull<Node<()>> = hdrptr.cast();

                // Call the deserialization function
                let res = (vtable.deserialize)(nodeptr, kvpair.body);

                // SAFETY: We can re-magic a reference to the header, because the NonNull<Node<()>>
                // does not have a live reference anymore
                let hdrmut = unsafe { hdrptr.as_mut() };

                if res.is_ok() {
                    // If it went okay, let the node know that it has been hydrated with data
                    hdrmut.state = State::ValidNoWriteNeeded;
                } else {
                    // If there WAS a key, but the deser failed, this means that either the data
                    // was corrupted, or there was a breaking schema change. Either way, we can't
                    // particularly recover from this. We might want to log this, but exposing
                    // the difference between this and "no data found" to the node probably won't
                    // actually be useful.
                    //
                    // todo: add logs? some kind of asserts?
                    hdrmut.state = State::NonResident;
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
        let iter = StaticRawIter {
            iter: self.list.iter_raw(),
        };
        let mut check_crc = Crc32::new();
        storage
            .push(&Elem::Start { seq_no })
            .await
            .map_err(LoadStoreError::FlashWrite)?;
        let mut ctr = 0;

        for hdrptr in iter {
            // Attempt to serialize: split off first item to reserve space for element
            // discriminant
            let (_first, rest) = buf.split_first_mut().ok_or(Error::Serialization)?;

            let res = serialize_node(hdrptr.ptr, rest);

            let Ok(used) = res else {
                // TODO @James: This comment is still from your first version. I am
                // unsure how up to date this is and whether or not this becomes a
                // problem with sequential storage.

                // I'm honestly not sure what we should do if serialization failed.
                // If it was a simple "out of space" because we have a batch of writes
                // already, we might want to try again later. If it failed for some
                // other reason, or it fails even with an empty page (e.g. serializes
                // to more than 1k or 4k or something), there's really not much to be
                // done, other than log.
                // For now, we just return an error and let the caller decide what to do.
                return Err(LoadStoreError::AppError(Error::Serialization));
            };

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
            storage
                .push(&Elem::Data { data: serdat })
                .await
                .map_err(LoadStoreError::FlashWrite)?;
            ctr += 1;
        }
        let crc = check_crc.finalize();

        storage
            .push(&Elem::End {
                seq_no,
                calc_crc: crc,
            })
            .await
            .map_err(LoadStoreError::FlashWrite)?;

        Ok(WriteReport {
            seq: seq_no,
            crc,
            ct: ctr,
        })
    }

    /// Iterates over all nodes, and changes Initial nodes to NonResident
    fn mark_initial_nonresident(&mut self) {
        // Set nodes in initial states to non resident
        for hdrmut in self.list.iter_mut() {
            if matches!(hdrmut.state, State::Initial) {
                // SAFETY: Initial -> NonResident is always a safe transition for a node
                unsafe {
                    hdrmut.set_state(State::NonResident);
                }
            }
        }
    }

    fn needs_writing(&mut self) -> Result<bool, Error> {
        // Set to true if any node in the list needs writing
        let mut needs_writing = false;

        // Iterate over all list nodes and check if any node needs writing or
        // is in an invalid state.
        for hdrptr in self.list.iter_raw() {
            let header = unsafe { hdrptr.as_ref() };

            match header.state {
                // If no write is needed, we obviously won't write.
                State::ValidNoWriteNeeded => {}
                // If the node hasn't been written to flash yet and we initialized it
                // with a Default, we now write it to flash.
                //
                // NOTE: We don't want to early return so we can detect nodes in invalid
                // states
                State::DefaultUnwritten | State::NeedsWrite => needs_writing = true,
                // TODO: A node may be in `Initial` state, if it has been attached but
                // `process_reads` hasn't run, yet. Should we return early here and trigger
                // another process_reads?
                State::Initial => {
                    return Err(Error::NeedsRead);
                }
                // A node may be NonResident if we are quickly doing a write after an
                // initial read, but the storage node has not yet had a chance to
                // initialize itself with a default value. Just skip.
                State::NonResident => {}
            }
        }

        Ok(needs_writing)
    }
}

// --------------------------------------------------------------------------
// impl SeqState
// --------------------------------------------------------------------------

impl SeqState {
    /// Have we hydrated from the external flash?
    #[inline]
    fn has_hydrated(&self) -> bool {
        self.next_seq.is_some()
    }

    /// What is the highest sequence number we've seen in external flash, if any?
    #[inline]
    fn highest_seen(&self) -> Option<NonZeroU32> {
        self.last_three.last().copied()?
    }

    /// Insert a known-good sequence number
    fn insert_good(&mut self, seq: NonZeroU32) {
        // todo: smarter than this
        let mut last_four = [
            self.last_three[0],
            self.last_three[1],
            self.last_three[2],
            Some(seq),
        ];
        last_four.sort_unstable();
        self.last_three.copy_from_slice(&last_four[1..]);

        // Should be impossible to not have something, just prevent panic branches
        let next = self.last_three[2].unwrap_or(const { NonZeroU32::new(1).unwrap() });
        // Note: this could overflow, but it would take 2^32 writes, which is
        // likely much more than the durability of reasonable flash parts (100k cycles)
        self.next_seq = next.checked_add(1);
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

/// ## Safety
/// The contained IterRaw is only valid for the lifetime of the List it comes
/// from, which can only be obtained by holding the mutex. This means that we
/// have exclusive access, and all nodes must be 'static. Therefore, it is
/// sound to Send both the iterator, and the wrapped NonNulls it returns.
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
///
/// ## Safety
/// This must only be used when the List mutex is locked and Node and Anchor
/// live &'static.
struct SendPtr {
    ptr: NonNull<NodeHeader>,
}
unsafe impl Send for SendPtr {}

struct KvPair<'a> {
    key: &'a str,
    body: &'a [u8],
}

/// Get the `key` bytes from a list item in the flash.
///
/// Used to extract the key from a `QueueIteratorItem`.
fn extract(item: &[u8]) -> Result<KvPair<'_>, Error> {
    let Ok(key) = minicbor::decode::<&str>(item) else {
        return Err(Error::Deserialization);
    };
    let len = len_with(key, &mut ());
    let Some(remain) = item.get(len..) else {
        return Err(Error::Deserialization);
    };
    Ok(KvPair { key, body: remain })
}

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
/// - `headerptr` points to a valid NodeHeader
/// - The storage list mutex is held during the entire operation
/// - The buffer is large enough to hold the serialized data
pub(crate) fn serialize_node(
    headerptr: NonNull<NodeHeader>,
    buf: &mut [u8],
) -> Result<usize, Error> {
    let (vtable, key) = {
        let node = unsafe { headerptr.as_ref() };
        (node.vtable, node.key)
    };

    debug!("serializing node with key <{:?}>", key,);

    let mut cursor = Cursor::new(&mut *buf);
    let res: Result<(), minicbor::encode::Error<EndOfSlice>> = minicbor::encode(key, &mut cursor);
    let Ok(()) = res else {
        return Err(Error::Serialization);
    };
    let used = cursor.position();
    let Some(remain) = buf.get_mut(used..) else {
        return Err(Error::Serialization);
    };

    let nodeptr: NonNull<Node<()>> = headerptr.cast();

    // Serialize payload into buffer
    (vtable.serialize)(nodeptr, remain)
        .map(|len| len + used)
        .map_err(|_| Error::Serialization)
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
/// # Safety
///
/// The caller must hold the storage list mutex (enforced by the `MutexGuard` parameter)
/// to ensure exclusive access during verification.
async fn verify_list_in_flash<S: NdlDataStorage>(
    storage: &mut S,
    rpt: WriteReport,
    buf: &mut [u8],
) -> Result<(), LoadStoreError<S::Error>> {
    // Create a `QueueIterator`
    let mut queue_iter = storage
        .iter_elems()
        .await
        .map_err(LoadStoreError::FlashRead)?;

    let seq = rpt.seq;
    let skipres = skip_to_seq!(queue_iter, buf, seq);
    match skipres {
        Ok(()) => {}
        // This state should be impossible: we already determined which seq_no to skip to
        Err(StepErr::EndOfList) => unreachable!("external flash inconsistent"),
        Err(StepErr::FlashError(e)) => return Err(LoadStoreError::FlashRead(e)),
    };

    let mut crc = Crc32::new();
    let mut ctr = 0;
    loop {
        let res = step!(queue_iter, buf);
        let item = match res {
            Ok(i) => {
                debug!("visiting {:?} in verify", i.data());
                i
            }
            Err(StepErr::EndOfList) => {
                debug!("Surprising end of list");
                return Err(LoadStoreError::WriteVerificationFailed);
            }
            Err(StepErr::FlashError(e)) => return Err(LoadStoreError::FlashRead(e)),
        };

        let data = item.data();
        match data {
            Elem::Start { .. } => {
                debug!("Surprising start of list");
                return Err(LoadStoreError::WriteVerificationFailed);
            }
            Elem::Data { data } => {
                crc.update(data.key_val());
                ctr += 1;
            }
            Elem::End { seq_no, calc_crc } => {
                let check_crc = crc.finalize();
                let seq_chk = seq_no == rpt.seq;
                let crc_chk1 = check_crc == rpt.crc;
                let crc_chk2 = calc_crc == rpt.crc;
                let ctr_chk = ctr == rpt.ct;
                let good = seq_chk && crc_chk1 && crc_chk2 && ctr_chk;
                debug!(
                    "check: {}, {}, {}, {} => {}",
                    seq_chk, crc_chk1, crc_chk2, ctr_chk, good
                );
                if good {
                    return Ok(());
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
        const ONE: NonZeroU32 = NonZeroU32::new(1).unwrap();
        const TWO: NonZeroU32 = NonZeroU32::new(2).unwrap();
        const THREE: NonZeroU32 = NonZeroU32::new(3).unwrap();
        const FOUR: NonZeroU32 = NonZeroU32::new(4).unwrap();
        let mut s = SeqState::default();
        assert_eq!(s.last_three, [None, None, None]);
        s.insert_good(ONE);
        assert_eq!(s.last_three, [None, None, Some(ONE)]);
        s.insert_good(TWO);
        assert_eq!(s.last_three, [None, Some(ONE), Some(TWO)]);
        s.insert_good(THREE);
        assert_eq!(s.last_three, [Some(ONE), Some(TWO), Some(THREE)]);
        s.insert_good(FOUR);
        assert_eq!(s.last_three, [Some(TWO), Some(THREE), Some(FOUR)]);
        s.insert_good(ONE);
        assert_eq!(s.last_three, [Some(TWO), Some(THREE), Some(FOUR)]);
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
        static GLOBAL_LIST: StorageList<CriticalSectionRawMutex> = StorageList::new();
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
                let config_handle = match POSITRON_CONFIG1.attach(&GLOBAL_LIST).await {
                    Ok(ch) => ch,
                    Err(_) => panic!("Could not attach config 1 to list"),
                };

                // Obtain a handle for the second config. This should _not_ error!
                if POSITRON_CONFIG2.attach(&GLOBAL_LIST).await.is_err() {
                    panic!("Could not attach config 2 to list");
                }

                // Load data for the first handle
                let data: PositronConfig = config_handle
                    .load()
                    .await
                    .expect("Loading config should not fail");
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
                assert_eq!(
                    config_handle
                        .load()
                        .await
                        .expect("Loading config should not fail"),
                    new_config
                );

                // Wait for the worker task to finish
                let _ = tokio::time::timeout(Duration::from_millis(100), worker_task).await;
            })
            .await;
    }

    /// Test that we can handle multiple nodes using the TestStorage backend
    #[test(tokio::test)]
    async fn test_two_configs_tst_sto() {
        static GLOBAL_LIST: StorageList<CriticalSectionRawMutex> = StorageList::new();
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
                let config_handle = match POSITRON_CONFIG1.attach(&GLOBAL_LIST).await {
                    Ok(ch) => ch,
                    Err(_) => panic!("Could not attach config 1 to list"),
                };

                // Obtain a handle for the second config. This should _not_ error!
                let Ok(_ch2) = POSITRON_CONFIG2.attach(&GLOBAL_LIST).await else {
                    panic!("Could not attach config 2 to list");
                };

                // Load data for the first handle
                let data: PositronConfig = config_handle
                    .load()
                    .await
                    .expect("Loading config should not fail");
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
                assert_eq!(
                    config_handle
                        .load()
                        .await
                        .expect("Loading config should not fail"),
                    new_config
                );

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
        static GLOBAL_LIST: StorageList<CriticalSectionRawMutex> = StorageList::new();
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
                    expecting_already_present
                        .load()
                        .await
                        .expect("Loading config should not fail"),
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
    #[should_panic]
    async fn test_duplicate_key() {
        static GLOBAL_LIST: StorageList<CriticalSectionRawMutex> = StorageList::new();
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
                // This must panic!
                let _expecting_panic = POSITRON_CONFIG2.attach(&GLOBAL_LIST).await;
            })
            .await;
    }
}
