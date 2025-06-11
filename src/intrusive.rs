//! # Intrusive Linked List for Configuration Storage
//! This implements the [`StorageList`] and its [`StorageListNode`]s

use crate::{
    error::{Error, LoadStoreError}, flash::{Elem, Flash, NdlDataStorage, NdlElemIter as _, NdlElemIterNode, SerData, StepResult}, logging::{debug, error, info}, skip_to_seq, step
};
use cordyceps::{
    Linked, List,
    list::{self, IterRaw},
};
use core::{
    cell::UnsafeCell,
    fmt::Debug,
    marker::PhantomPinned,
    mem::MaybeUninit,
    pin::Pin,
    ptr::{self, NonNull},
};
use maitake_sync::{Mutex, WaitQueue};
use minicbor::{
    CborLen, Decode, Encode,
    encode::write::{Cursor, EndOfSlice},
    len_with,
};
use mutex::{ConstInit, ScopedRawMutex};

/// This is fake until I pull in the CRC crate
struct FakeCrc32 {
    state: u32,
}

impl FakeCrc32 {
    fn new() -> Self {
        Self { state: 0 }
    }

    fn update(&mut self, data: &[u8]) {
        for b in data {
            self.state = self.state.wrapping_add((*b) as u32);
        }
    }

    fn finalize(self) -> u32 {
        self.state
    }
}

#[derive(Default)]
struct SeqState {
    next_seq: u32,
    has_hydrated: bool,
    last_three: [u32; 3],
}

impl SeqState {
    fn insert_good(&mut self, seq: u32) {
        assert_ne!(seq, 0);
        // todo: smarter than this
        let mut last_four = [
            self.last_three[0],
            self.last_three[1],
            self.last_three[2],
            seq,
        ];
        last_four.sort_unstable();
        self.last_three.copy_from_slice(&last_four[1..]);
        self.next_seq = self.last_three[2] + 1;
    }
}

struct StorageListInner {
    list: List<NodeHeader>,
    seq_state: SeqState,
}

impl StorageListInner {
    async fn get_or_populate_latest<S: NdlDataStorage>(
        &mut self,
        s: &mut S,
        buf: &mut [u8],
    ) -> Result<Option<u32>, LoadStoreError> {
        if self.seq_state.has_hydrated {
            let max = self.seq_state.last_three.iter().copied().max().unwrap_or(0);
            if max == 0 {
                return Ok(None);
            } else {
                return Ok(Some(max));
            }
        }

        // We have NOT hydrated, do so now
        let Ok(mut iter) = s.iter_elems().await else {
            todo!()
        };

        let mut current: Option<u32> = None;
        // todo: real crc
        let mut crc = FakeCrc32::new();

        'outer: loop {
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
                    Err(_e) => todo!(),
                };
            };

            match item.data() {
                Elem::Start { seq_no } => {
                    current = Some(seq_no);
                    // todo: real crc
                    // todo: We should make sure next_seq is ALWAYS larger than any seen start,
                    // to ensure we don't accidentally re-use a bad one.
                    crc = FakeCrc32::new();
                }
                Elem::Data { data } => {
                    if current.is_none() {
                        continue;
                    } else {
                        // todo: real crc
                        crc.update(data.key_val());
                    }
                }
                Elem::End { seq_no, calc_crc } => {
                    let check_crc = crc.finalize();
                    crc = FakeCrc32::new();
                    let Some(seq) = current.take() else {
                        continue;
                    };
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

        self.seq_state.has_hydrated = true;
        let max = self.seq_state.last_three.iter().copied().max().unwrap_or(0);
        if max == 0 {
            self.seq_state.next_seq = 1;
            Ok(None)
        } else {
            Ok(Some(max))
        }
    }

    async fn extract_all<S: NdlDataStorage>(
        &mut self,
        latest: u32,
        storage: &mut S,
        buf: &mut [u8],
    ) -> Result<(), LoadStoreError> {
        let Ok(mut queue_iter) = storage.iter_elems().await else {
            todo!()
        };

        let skipres: StepResult<()> = skip_to_seq!(queue_iter, buf, latest);
        match skipres {
            StepResult::Item(_) => {},
            StepResult::EndOfList => todo!(),
            StepResult::FlashError => todo!(),
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
                    Err(_e) => todo!(),
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
            let Some(node_header) = find_node(&mut self.list, kvpair.key) else {
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

                // Note: We MUST have destroyed `node` at this point, so we don't have aliasing
                // references of both the Node and the NodeHeader. Pointers are fine!
                //
                // Pass the `counter_is_some` as the `drop_old` flag because if the counter has been
                // populated, we must have read that node from flash once already and need to drop
                // the old value of `node.t`.
                //
                // TODO @James: Counter used here to indicate that the old `node.t` needs to be dropped.
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
        todo!()
    }

    fn mark_pending_nonpop(&mut self) {
        // Set nodes in initial states to non resident
        for hdrmut in self.list.iter_mut() {
            if matches!(hdrmut.state, State::Initial) {
                unsafe {
                    hdrmut.set_state(State::NonResident);
                }
            }
        }
    }

    fn needs_writing(&mut self) -> Result<bool, LoadStoreError> {
        // Set to true if any node in the list needs writing
        let mut needs_writing = false;

        // Iterate over all list nodes and
        // 1. Store the counter (if present).
        //    We technically only need the counter from one list item that has been
        //    read from flash and has counter != None.
        // 2. Check if any node needs writing or is in an invalid state.
        for hdrptr in self.list.iter_raw() {
            let header = unsafe { hdrptr.as_ref() };

            match header.state {
                // If no write is needed, we obviously won't write.
                State::ValidNoWriteNeeded => (),
                // If the node hasn't been written to flash yet and we initialized it
                // with a Default, we now write it to flash.
                State::DefaultUnwritten | State::NeedsWrite => needs_writing = true,
                // TODO: A node may be in `Initial` state, if it has been attached but
                // `process_reads` hasn't run, yet. Should we return early here and trigger
                // another process_reads?
                State::Initial => {
                    return Err(LoadStoreError::NeedsRead);
                }
                // This should never appear on a list that has been processed properly.
                // TODO @James: Not sure how we should recover from this. If it cannot happen
                // due to invalid input, we might just as well panic.
                State::NonResident => {
                    debug!("process_writes() on invalid state: {:?}", header.state);

                    return Err(LoadStoreError::AppError(Error::InvalidState(
                        header.key,
                        header.state,
                    )));
                }
            }
        }

        Ok(needs_writing)
    }
}

/// "Global anchor" of all storage items.
///
/// It serves as the meeting point between two conceptual pieces:
///
/// 1. The [`StorageListNode`]s, which each store one configuration item
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
    inner: Mutex<StorageListInner, R>,
    /// Notifies the worker task that nodes need to be read from flash.
    /// Woken when a new node is attached and requires data to be loaded.
    needs_read: WaitQueue,
    /// Notifies the worker task that nodes have pending writes to flash.
    /// Woken when a node's data is modified and needs to be persisted.
    needs_write: WaitQueue,
    /// Notifies waiting nodes that the read process has completed.
    /// Woken after `process_reads` finishes loading data from flash.
    reading_done: WaitQueue,
    /// Notifies waiting nodes that the write process has completed.
    /// Woken after `process_writes` finishes persisting data to flash.
    writing_done: WaitQueue,
}

impl<R: ScopedRawMutex> StorageList<R> {
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

/// Represents the storage of a single `T`, linkable to a `StorageList`
///
/// Users will [`attach`](Self::attach) it to the [`StorageList`] to make it part of
/// the "connected configuration system".
pub struct StorageListNode<T: 'static> {
    /// The `inner` data is "observable" in the linked list. We put it inside
    /// an [`UnsafeCell`], because it might be mutated "spookily" either through the
    /// `StorageListNode`, or via the linked list.
    inner: UnsafeCell<Node<T>>,
}

/// Handle for a `StorageListNode` that has been loaded and thus contains valid data
///
/// "end users" will interact with the `StorageListNodeHandle` to retrieve and store changes
/// to the configuration they care about.
pub struct StorageListNodeHandle<T, R>
where
    T: 'static,
    R: ScopedRawMutex + 'static,
{
    /// `StorageList` to which this node has been attached
    list: &'static StorageList<R>,

    /// Store the StorageListNode for this handle
    inner: &'static StorageListNode<T>,
}

/// Impl Debug to allow for using unwrap in tests.
impl<T, R> Debug for StorageListNodeHandle<T, R>
where
    T: 'static,
    R: ScopedRawMutex + 'static,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StorageListNodeHandle")
            .field("list", &"StorageList<R>")
            .field("inner", &"StorageListNode<T>")
            .finish()
    }
}

/// This is the actual "linked list node" that is chained to the list
///
/// There is trickery afoot! The linked list is actually a linked list
/// of `NodeHeader`, and we are using `repr(c)` here to GUARANTEE that
/// a pointer to a ``Node<T>`` is the SAME VALUE as a pointer to that
/// node's `NodeHeader`. We will use this for type punning!
#[repr(C)]
pub struct Node<T> {
    // LOAD BEARING: MUST BE THE FIRST ELEMENT IN A REPR-C STRUCT
    header: NodeHeader,
    // This type is !Unpin due to the heuristic from:
    // <https://github.com/rust-lang/rust/pull/82834>
    // It might not be absolutely necessary anymore (?) to have this, but
    // it is still used in the `pin` examples, so we keep it:
    // https://doc.rust-lang.org/std/pin/index.html#a-self-referential-struct
    _pin: PhantomPinned,

    // We generally want this to be in the tail position, because the size of T varies
    // and the pointer to the `NodeHeader` must not change as described above.
    //
    // TODO @James: This is very difficult to access from tests but maybe we would
    // want to have an (test-)interface that lets us create a node with a specific payload
    // such that we can directly de-/serialize nodes and compare the results in a
    // unit test. Do you think that makes sense? Or is there a better way to
    // have unit tests for this?
    t: MaybeUninit<T>,
}

/// The non-typed parts of a [`Node<T>`].
///
/// The `NodeHeader` serves as the actual type that is linked together in
/// the linked list, and is the primary interface the storage worker will
/// use. It MUST be the first field of a `Node<T>`, so it is safe to
/// type-pun a `NodeHeader` ptr into a `Node<T>` ptr and back.
pub struct NodeHeader {
    /// The doubly linked list pointers
    links: list::Links<NodeHeader>,
    /// The "key" of our "key:value" store. Must be unique across the list.
    key: &'static str,
    /// The current state of the node. THIS IS SAFETY LOAD BEARING whether
    /// we can access the `T` in the `Node<T>`
    state: State,
    /// This is the type-erased serialize/deserialize `VTable` that is
    /// unique to each `T`, and will be used by the storage worker
    /// to access the `t` indirectly for loading and storing.
    vtable: VTable,
}

impl NodeHeader {
    unsafe fn set_state(self: Pin<&mut Self>, state: State) {
        unsafe {
            self.get_unchecked_mut().state = state;
        }
    }
}

/// State of the [`Node<T>`].
///
/// This is used to determine how to unsafely interact with other pieces
/// of the `Node<T>`!
///
/// ## State transition diagram
/// TODO: Update this when the statemachine is finalized
/// ```text
/// ┌─────────┐    ┌─────────────┐   ┌────────────────────┐
/// │ Initial │─┬─▶│ NonResident │──▶│  DefaultUnwritten  │─┐
/// └─────────┘ │  └─────────────┘   └────────────────────┘ │
///             │                                           ▼
///             │         ┌────────────────────┐     ┌────────────┐
///             └────────▶│ ValidNoWriteNeeded │────▶│ NeedsWrite │◀─┐
///                       └────────────────────┘     └────────────┘  │
///                                  ▲                      │        │
///                                  │                      ▼        │
///                                  │              ┌──────────────┐ │
///                                  └──────────────│ WriteStarted │─┘
///                                                 └──────────────┘
/// ```
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "arbitrary", derive(arbitrary::Arbitrary))]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum State {
    /// The node has been created, but never "hydrated" with data from the
    /// flash. In this state, we are waiting to be notified whether we can
    /// be filled with data from flash, or whether we will need to initialize
    /// using a default value or something.
    ///
    /// In this state, `t` is NOT valid, and is uninitialized. it MUST NOT be
    /// read.
    Initial,
    /// We attempted to load from flash, but as far as we know, the data does
    /// not exist in flash. The owner of the Node will need to initialize
    /// this data, usually in the wait loop of `attach`.
    ///
    /// In this state, `t` is NOT valid, and is uninitialized. it MUST NOT be
    /// read.
    NonResident,
    /// The value has been initialized using a default value (NOT from flash)
    /// and needs to be written to flash.
    ///
    /// In this state, `t` IS valid, and may be read at any time (by the holder
    /// of the lock).
    DefaultUnwritten,
    /// The value has been initialized using a value from flash. No writes are
    /// pending.
    ///
    /// In this state, `t` IS valid, and may be read at any time (by the holder
    /// of the lock).
    ValidNoWriteNeeded,
    /// The value has been written, but these changes have NOT been flushed back
    /// to the flash.
    ///
    /// In this state, `t` IS valid, and may be read at any time (by the holder
    /// of the lock).
    NeedsWrite,
}

/// A function where a type-erased (`void*`) node pointer goes in, and the
/// proper `T` is serialized to the buffer.
///
/// If successful, returns the number of byte written to the buffer.
type SerFn = fn(NonNull<Node<()>>, &mut [u8]) -> Result<usize, ()>;

/// A function where the type-erased node pointer goes in, and we attempt
/// to deserialize a T from the buffer.
///
/// If successful, returns the number of bytes read from the buffer.
type DeserFn = fn(NonNull<Node<()>>, &[u8]) -> Result<usize, ()>;

/// Function table for type-erased serialization and deserialization operations.
///
/// The `VTable` contains function pointers that allow the storage system to serialize
/// and deserialize nodes without knowing their concrete types at compile time. Each
/// node type `T` gets its own vtable instance created via [`VTable::for_ty`], which
/// stores monomorphized function pointers for that specific type.
///
/// This enables the storage worker to operate on a heterogeneous linked list of
/// nodes with different payload types (`Node<T1>`, `Node<T2>`, etc.) through a
/// uniform interface using type erasure.
///
/// # Fields
/// * `serialize` - Function pointer to serialize a `T` value to a byte buffer
/// * `deserialize` - Function pointer to deserialize a `T` value from a byte buffer
#[derive(Clone, Copy)]
struct VTable {
    serialize: SerFn,
    deserialize: DeserFn,
}

// --------------------------------------------------------------------------
// impl StorageList
// --------------------------------------------------------------------------

impl<R: ScopedRawMutex + ConstInit> StorageList<R> {
    /// const constructor to make a new empty list. Intended to be used
    /// to create a static.
    ///
    /// ```
    /// # use cfg_noodle::intrusive::StorageList;
    /// # use mutex::raw_impls::cs::CriticalSectionRawMutex;
    /// static GLOBAL_LIST: StorageList<CriticalSectionRawMutex> = StorageList::new();
    /// ```
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new_with_raw_mutex(
                StorageListInner {
                    list: List::new(),
                    seq_state: SeqState {
                        next_seq: 0,
                        has_hydrated: false,
                        last_three: [0, 0, 0],
                    },
                },
                R::INIT,
            ),
            needs_read: WaitQueue::new(),
            needs_write: WaitQueue::new(),
            reading_done: WaitQueue::new(),
            writing_done: WaitQueue::new(),
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
    /// ## Params:
    /// - `flash`: a [`Flash`] object containing the necessary information to call [`queue::push`]
    /// - `buf`: a scratch-buffer that must be large enough to hold the largest serialized node
    ///   In practice, a buffer as large as the flash's page size is enough because sequential
    ///   storage can not store larger items.
    ///
    /// TODO: Document what this function does and when it must be called
    pub async fn process_reads<S: NdlDataStorage>(
        &'static self,
        storage: &mut S,
        buf: &mut [u8],
    ) -> Result<(), LoadStoreError> {
        info!("Start process_reads");
        // Lock the list, remember, if we're touching nodes, we need to have the list
        // locked the entire time!
        let mut inner = self.inner.lock().await;
        debug!("process_reads locked list");

        // // Store the counter of the last write_confirm block we found so that later on
        // // we know if the latest counter we found has actually been confirmed
        // let mut latest_write_confirm: Option<Counter> = None;
        // let mut latest_counter: Option<Counter> = None;

        // Have we already determined the latest valid write record?
        let res = inner.get_or_populate_latest(storage, buf).await?;

        // If we have something: populate all present items
        if let Some(latest) = res {
            inner.extract_all(latest, storage, buf).await?;
        }

        // Now, we either have NOTHING, or we have populated all items that DO exist.
        inner.mark_pending_nonpop();

        // // TODO: Clarify if we should set the counter here and whether we should already
        // // "bump" it at this point or only just before writing.
        // // @James, same story. There must be a better solution for handling the counter than this.
        // //
        // // Set the counter to the latest one because know its value here.
        // // Inside process_reads, it needs to be bumped, but there it is more difficult
        // // to find out what the latest counter was due to wrapping.
        // set_counter(&mut ls, latest_counter.unwrap_or_default());

        debug!("Reading done. Waking all.");
        self.reading_done.wake_all();
        Ok(())
    }

    /// Process writes to flash.
    ///
    /// If any of the nodes has pending writes, this function writes the
    /// entire list to flash.
    ///
    /// ## Arguments:
    /// * `flash`: a [`Flash`] object the list should be written to
    /// * `read_buf`: a scratch buffer used to store data read from flash during verification.
    ///   Must be able to hold any serialized node.
    /// * `serde_buf`: a scratch buffer used to serialize nodes for writing and verification.
    ///   Must be able to hold any serialized node.
    ///
    /// Both buffers have the same minimum size requirement: they must be large enough to hold
    /// the largest serialized node. In practice, a buffer as large as the flash's page size is
    /// sufficient because sequential storage cannot store larger items.
    ///
    /// This method:
    /// 1. Locks the mutex
    /// 2. Iterates through each node currently linked in the list
    /// 3. Serializes each node and writes it to flash
    /// 4. Verifies the written data matches what was intended to be written
    /// 5. On success, marks all nodes as no longer having pending changes
    pub async fn process_writes<S: NdlDataStorage>(
        &'static self,
        storage: &mut S,
        serde_buf: &mut [u8],
    ) -> Result<(), LoadStoreError> {
        debug!("Start process_writes");

        // Lock the list, remember, if we're touching nodes, we need to have the list
        // locked the entire time!
        let mut inner = self.inner.lock().await;
        debug!("process_writes locked list");

        let needs_writing = inner.needs_writing()?;

        // If the list is unchanged, there is no need to write it to flash!
        if !needs_writing {
            debug!("List does not need writing. Returning.");
            return Ok(());
        }

        debug!("Attempt write_to_flash");

        // TODO: Retries have to be handled in the OUTER context, so we can
        // start over from the beginning.
        let next_seq = inner.seq_state.next_seq;
        let rpt = write_to_flash(&mut inner.list, serde_buf, storage, next_seq).await?;

        // Read back all items in the list any verify the data on the flash
        // actually is what we wanted to write.
        verify_list_in_flash(storage, rpt, serde_buf).await?;
        inner.seq_state.insert_good(next_seq);
        inner.seq_state.next_seq += 1;

        // // TODO(AJM): Do we want to defer pops to later?
        // //
        // // TODO @James: Even though there is no harm in passing the `serde_buf`
        // // (we have it either way), it is unnecessary. This function will only call
        // // seq-stor::queue::pop(), which needs a buffer to write the flash data to.
        // // But we are not interested in this data and only throw it away. Afaik,
        // // seq-stor currently does not allow to just "delete" old data and always
        // // requires `pop`. Should we attempt to optimize it or just leave it as is?
        // delete_old_items(flash, max_counter, serde_buf).await?;

        // The write must have succeeded. So mark all nodes accordingly.
        for mut hdrptr in inner.list.iter_raw() {
            // Mark the store as complete, wake the node if it cares about state
            // changes.
            //
            // SAFETY: We hold the lock to the list, so no other thread can modify
            // the node while we are modifying it.
            //
            // TODO @James: I think this is fine because we hold the lock to the list.
            // But still, please double-check that this is really okay.
            let hdrmut = unsafe { hdrptr.as_mut() };
            hdrmut.state = State::ValidNoWriteNeeded;
        }
        self.writing_done.wake_all();

        Ok(())
    }
}

// /// Removes old flash entries until reaching the newly written list with the specified counter.
// ///
// /// This function iterates through flash storage, removing outdated entries by popping them
// /// from the queue until it encounters an entry with the target counter value, which indicates
// /// the beginning of the newly written list.
// ///
// /// # Arguments
// /// * `flash` - The flash storage device and address range to clean up
// /// * `counter` - The counter value that marks the start of the newly written list
// /// * `serde_buf` - Buffer used for flash operations (must be large enough for any flash item)
// ///
// /// # Returns
// /// * `Ok(())` - If cleanup completed successfully
// /// * `Err(LoadStoreError::FlashRead)` - If reading from flash fails
// /// * `Err(LoadStoreError::FlashWrite)` - If removing old items fails
// async fn delete_old_items<F: MultiwriteNorFlash>(
//     flash: &mut Flash<F, impl CacheImpl>,
//     counter: Counter,
//     serde_buf: &mut [u8],
// ) -> Result<(), LoadStoreError> {
//     // Iterate over the items in the flash.
//     // First only peek() and check if it is the desired counter.
//     // If so, return – we have reached the newly written list.
//     // If not, pop that item from the list.
//     loop {
//         if let Some(item) = flash
//             .peek(serde_buf)
//             .await
//             // .map_err(LoadStoreError::FlashRead)?
//             .map_err(|_| LoadStoreError::FlashRead)?
//         {
//             if extract_counter(item).is_ok_and(|c| c == counter) {
//                 return Ok(());
//             } else {
//                 flash
//                     .pop(serde_buf)
//                     .await
//                     // .map_err(LoadStoreError::FlashWrite)?;
//                     .map_err(|_| LoadStoreError::FlashWrite)?;
//             }
//         }
//     }
// }

/// Verifies that the storage list was correctly written to flash memory.
///
/// This function compares each node in the storage list against the corresponding
/// item stored in flash to ensure data integrity after a write operation. It iterates
/// through flash items until it finds the beginning of the newly-written list (identified
/// by matching counter values), then serializes each list node and compares it with
/// the flash data.
///
/// # Arguments
/// * `ls` - A mutable guard to the locked storage list
/// * `read_buf` - Buffer used for reading items from flash
/// * `serde_buf` - Buffer used for serializing nodes for comparison
/// * `flash` - The flash storage device and address range to read from
///
/// # Returns
/// * `Ok(())` - If all nodes match their corresponding flash entries
/// * `Err(LoadError::WriteVerificationFailed)` - If any node doesn't match its flash entry
/// * `Err(LoadError::FlashRead)` - If reading from flash fails
///
/// # Safety
/// The caller must hold the storage list mutex (enforced by the `MutexGuard` parameter)
/// to ensure exclusive access during verification.
async fn verify_list_in_flash<S: NdlDataStorage>(
    storage: &mut S,
    rpt: WriteReport,
    buf: &mut [u8],
) -> Result<(), LoadStoreError> {
    // Create a `QueueIterator`
    let mut queue_iter = storage
        .iter_elems()
        .await
        .map_err(|_| LoadStoreError::FlashRead)?;

    let seq = rpt.seq;
    let skipres: StepResult<()> = skip_to_seq!(queue_iter, buf, seq);
    match skipres {
        StepResult::Item(_) => {},
        StepResult::EndOfList => todo!(),
        StepResult::FlashError => todo!(),
    };

    let mut crc = FakeCrc32::new();
    let mut ctr = 0;
    loop {
        let res: StepResult<_> = step!(queue_iter, buf);
        let item = match res {
            StepResult::Item(i) => i,
            StepResult::EndOfList => {
                debug!("Surprising end of list");
                return Err(LoadStoreError::WriteVerificationFailed)
            },
            StepResult::FlashError => return Err(LoadStoreError::FlashRead),
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
                debug!("check: {}, {}, {}, {} => {}", seq_chk, crc_chk1, crc_chk2, ctr_chk, good);
                if good {
                    return Ok(());
                } else {
                    debug!("Consistency check failed");
                    return Err(LoadStoreError::WriteVerificationFailed);
                }
            }
        }
    }
}

struct WriteReport {
    seq: u32,
    crc: u32,
    ct: usize,
}

/// Writes all nodes in the storage list to flash memory.
///
/// This function iterates through all nodes in the storage list, serializes each node's
/// data into the provided buffer, and writes it to flash.
/// Each node is written as a separate flash item containing its key, counter,
/// and serialized payload.
///
/// # Arguments
/// * `ls` - A mutable guard to the locked storage list containing nodes to write
/// * `buf` - A scratch buffer used for serialization (must be large enough for the largest node)
/// * `flash` - The flash storage device and address range to write to
///
/// # Returns
/// * `Ok(())` - If all nodes were successfully serialized and written to flash
/// * `Err(StoreError::FlashWrite)` - If a flash write operation failed
/// * `Err(StoreError::AppError(Error::Serialization))` - If node serialization failed
///
/// # Safety
/// - The caller must hold the storage list mutex (enforced by the `MutexGuard` parameter)
///   to ensure exclusive access during the write operation.
/// - The list nodes must be in a valid state for writing (e.g., counter must be set)
async fn write_to_flash<S: NdlDataStorage>(
    ls: &mut List<NodeHeader>,
    buf: &mut [u8],
    storage: &mut S,
    seq_no: u32,
) -> Result<WriteReport, LoadStoreError> {
    let iter = StaticRawIter {
        iter: ls.iter_raw(),
    };
    let mut check_crc = FakeCrc32::new();
    storage
        .push(&Elem::Start { seq_no })
        .await
        .map_err(|_| LoadStoreError::FlashWrite)?;
    let mut ctr = 0;

    for hdrptr in iter {
        // Attempt to serialize: split off first item to reserve space for element
        // discriminant
        let (_first, rest) = buf.split_first_mut().ok_or(LoadStoreError::FlashWrite)?;

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
        let serdat = SerData::new(used);
        check_crc.update(serdat.key_val());
        // Try writing to flash
        storage
            .push(&Elem::Data { data: serdat })
            .await
            .map_err(|_| LoadStoreError::FlashWrite)?;
        ctr += 1;
    }
    let crc = check_crc.finalize();

    storage
        .push(&Elem::End {
            seq_no,
            calc_crc: crc,
        })
        .await
        .map_err(|_| LoadStoreError::FlashWrite)?;

    Ok(WriteReport {
        seq: seq_no,
        crc,
        ct: ctr,
    })
}

// --------------------------------------------------------------------------
// impl StorageListNode
// --------------------------------------------------------------------------

/// I think this is safe? If we can move data into the node, its
/// Sync-safety is guaranteed by the StorageList mutex.
unsafe impl<T> Sync for StorageListNode<T> where T: Send + 'static {}

impl<T> StorageListNode<T>
where
    T: 'static,
    T: Encode<()>,
    T: CborLen<()>,
    for<'a> T: Decode<'a, ()>,
    T: Default + Clone,
    // TODO @James: Should we remove all those Debug trait bounds? I added them to make debugging easier
    T: Debug,
{
    /// Make a new StorageListNode, initially empty and unattached
    pub const fn new(path: &'static str) -> Self {
        Self {
            inner: UnsafeCell::new(Node {
                header: NodeHeader {
                    links: list::Links::new(),
                    key: path,
                    state: State::Initial,
                    vtable: VTable::for_ty::<T>(),
                },
                t: MaybeUninit::uninit(),
                _pin: PhantomPinned,
            }),
        }
    }

    /// Attaches node to a list and waits for hydration.
    /// If the value is not found in flash, a default value is used.
    ///
    /// If the value is found in flash but only from an older write (e.g.
    /// because the latest write operation was interrupted), the node
    /// is initialized with the default, and the value from flash is
    /// returned in the second element of the Result::Error tuple.
    ///
    /// # Panics
    /// This function will panic if a node with the same key(-hash) already
    /// exists in the list.
    ///
    /// TODO: Re-evaluate the panicking behavior and whether or not to return
    /// and "old data" in the Error case.
    pub async fn attach<R>(
        &'static self,
        list: &'static StorageList<R>,
    ) -> Result<StorageListNodeHandle<T, R>, (StorageListNodeHandle<T, R>, T)>
    where
        R: ScopedRawMutex + 'static,
    {
        debug!("Attaching new node");

        // Add a scope so that the Lock on the List is dropped
        {
            let mut inner = list.inner.lock().await;
            debug!("attach() got Lock on list");

            let nodeptr: *mut Node<T> = self.inner.get();
            let nodenn: NonNull<Node<T>> = unsafe { NonNull::new_unchecked(nodeptr) };
            // NOTE: We EXPLICITLY cast the outer Node<T> ptr, instead of using the header
            // pointer, so when we cast back later, the pointer has the correct provenance!
            let hdrnn: NonNull<NodeHeader> = nodenn.cast();

            // Check if the key already exists in the list.
            // This also prevents attaching to the list more than once.
            // SAFETY: We hold the lock, we are allowed to gain exclusive mut access
            // to the contents of the node.
            let key = unsafe { &nodenn.as_ref().header.key };
            if find_node(&mut inner.list, key).is_some() {
                error!("Key already in use: {:?}", key);
                panic!("Node with the same key-hash already in the list");
            } else {
                inner.list.push_front(hdrnn);
            }
            // Let read task know we have work
            list.needs_read.wake();
            debug!("attach() release Lock on list");
        }

        // Wait until we have a value, or we know it is non-resident
        loop {
            debug!("Waiting for reading_done");
            list.reading_done
                .wait()
                .await
                .expect("waitcell should never close");

            // We need the lock to look at ourself!
            let _inner = list.inner.lock().await;

            // We have the lock, we can gaze into the UnsafeCell
            let nodeptr: *mut Node<T> = self.inner.get();
            let noderef: &mut Node<T> = unsafe { &mut *nodeptr };
            // Are we in a state with a valid T?
            match noderef.header.state {
                // TODO @James: This should not really happen because it means we attached in
                // the middle of the read process, which implies that we AND the read process
                // have the list locked simultaneously.
                // Is "handling" it with a "continue" reasonable or should we panic because it
                // technically is an invalid state at this point.
                State::Initial => continue,
                State::NonResident => {
                    debug!(
                        "Node with key {:?} non resident. Init with default",
                        noderef.header.key
                    );
                    // We are nonresident, we need to initialize
                    noderef.t = MaybeUninit::new(T::default());
                    noderef.header.state = State::DefaultUnwritten;
                    break;
                }
                State::DefaultUnwritten => todo!("shouldn't observe this in attach"),
                State::ValidNoWriteNeeded => break,
                State::NeedsWrite => todo!("shouldn't observe this in attach"),
            }
        }

        Ok(StorageListNodeHandle { list, inner: self })
    }
}

// --------------------------------------------------------------------------
// impl StorageListNodeHandle
// --------------------------------------------------------------------------

impl<T, R> StorageListNodeHandle<T, R>
where
    T: Clone + Send + 'static,
    R: ScopedRawMutex + 'static,
{
    /// This is a `load` function that copies out the data
    ///
    /// Note that we *copy out*, instead of returning a ref, because we MUST hold
    /// the guard as long as &T is live. For a blocking mutex, that's all kinds
    /// of problematic, but even with the async mutex, holding a `MutexGuard<T>`
    /// or similiar **will inhibit all other flash operations**, which is bad!
    ///
    /// So just hold the lock and copy out.
    pub async fn load(&self) -> Result<T, Error> {
        // Lock the list if we look at ourself
        let _inner = self.list.inner.lock().await;

        let nodeptr: *mut Node<T> = self.inner.inner.get();
        let noderef = unsafe { &mut *nodeptr };
        // is T valid?
        match noderef.header.state {
            // All these states implicate that process_reads/_writes has not finished
            // but they hold a lock on the list, so we should never reach this piece
            // of code while holding a lock ourselves
            //
            // TODO @James: Again, if we are in an invalid state, should we rather panic?
            State::Initial | State::NonResident => {
                return Err(Error::InvalidState(
                    noderef.header.key,
                    noderef.header.state,
                ));
            }
            // Handle all states here explicitly to avoid bugs with a catch-all
            State::DefaultUnwritten | State::ValidNoWriteNeeded | State::NeedsWrite => {}
        }
        // yes!
        Ok(unsafe { noderef.t.assume_init_ref().clone() })
    }

    /// Write data to the buffer, and mark the buffer as "needs to be flushed".
    pub async fn write(&self, t: &T) -> Result<(), Error> {
        let _inner = self.list.inner.lock().await;

        let nodeptr: *mut Node<T> = self.inner.inner.get();
        let noderef = unsafe { &mut *nodeptr };
        match noderef.header.state {
            // `Initial` and `NonResident` can not occur for the same reason as
            // outlined in load(). OldData should have been replaced with a Default value.
            State::Initial | State::NonResident => {
                debug!("write() on invalid state: {:?}", noderef.header.state);
                return Err(Error::InvalidState(
                    noderef.header.key,
                    noderef.header.state,
                ));
            }
            State::DefaultUnwritten | State::ValidNoWriteNeeded | State::NeedsWrite => {}
        }
        // We do a swap instead of a write here, to ensure that we
        // call `drop` on the "old" contents of the buffer.
        let mut t: T = t.clone();
        unsafe {
            let mutref: &mut T = noderef.t.assume_init_mut();
            core::mem::swap(&mut t, mutref);
        }
        noderef.header.state = State::NeedsWrite;

        // Let the write task know we have work
        self.list.needs_write.wake();

        // old T is dropped
        Ok(())
    }
}

impl<T> Drop for StorageListNode<T> {
    fn drop(&mut self) {
        // If we DO want to be able to drop, we probably want to unlink from the list.
        //
        // However, we more or less require that `StorageListNode` is in a static
        // (or some kind of linked pointer), so it should never actually be possible
        // to drop a StorageListNode.
        //
        // This could be problematic since we use an async mutex!
        // TODO @James: You left a note here about this being a problem with the async mutex.
        // Is there anything we could/should do here since we switched to an async mutex?
        todo!("We probably don't actually need drop?")
    }
}

// --------------------------------------------------------------------------
// impl NodeHeader
// --------------------------------------------------------------------------

/// This is cordycep's intrusive linked list trait. It's mostly how you
/// shift around pointers semantically.
unsafe impl Linked<list::Links<NodeHeader>> for NodeHeader {
    type Handle = NonNull<NodeHeader>;

    fn into_ptr(r: Self::Handle) -> NonNull<Self> {
        r
    }

    unsafe fn from_ptr(ptr: NonNull<Self>) -> Self::Handle {
        ptr
    }

    unsafe fn links(target: NonNull<Self>) -> NonNull<list::Links<NodeHeader>> {
        // Safety: using `ptr::addr_of!` avoids creating a temporary
        // reference, which stacked borrows dislikes.
        let node = unsafe { ptr::addr_of_mut!((*target.as_ptr()).links) };
        unsafe { NonNull::new_unchecked(node) }
    }
}

// --------------------------------------------------------------------------
// impl VTable
// --------------------------------------------------------------------------

impl VTable {
    /// This is a tricky helper method that automatically builds a vtable by
    /// monomorphizing generic functions into non-generic function pointers.
    const fn for_ty<T>() -> Self
    where
        T: 'static,
        T: Encode<()>,
        T: CborLen<()>,
        T: Debug,
        for<'a> T: Decode<'a, ()>,
    {
        let ser = serialize::<T>;
        let deser = deserialize::<T>;
        Self {
            serialize: ser,
            deserialize: deser,
        }
    }
}

/// Tricky monomorphizing serialization function.
///
/// Casts a type-erased node pointer into the "right" type, so that we can do
/// serialization with it.
///
/// # Safety
/// `node.t` MUST be in a valid state before calling this function, AND
/// the mutex must be held the whole time we are here, because we are reading
/// from `t`!
fn serialize<T>(node: NonNull<Node<()>>, buf: &mut [u8]) -> Result<usize, ()>
where
    T: 'static,
    T: Encode<()>,
    T: Debug,
    T: minicbor::CborLen<()>,
{
    let node: NonNull<Node<T>> = node.cast();
    let noderef: &Node<T> = unsafe { node.as_ref() };
    let tref: &MaybeUninit<T> = &noderef.t;
    let tref: &T = unsafe { tref.assume_init_ref() };

    let mut cursor = Cursor::new(buf);
    let res: Result<(), minicbor::encode::Error<EndOfSlice>> = minicbor::encode(tref, &mut cursor);

    debug!(
        "Finished serializing: {} bytes written, len_with(): {}, content: {:?}, type: {:#?}",
        cursor.position(),
        len_with(tref, &mut ()),
        &cursor.get_ref()[..cursor.position()],
        tref
    );
    // Make sure len_with returns the correct number of bytes
    // Important because we depend on it in deserialize()
    debug_assert_eq!(cursor.position(), len_with(tref, &mut ()));

    match res {
        Ok(()) => Ok(cursor.position()),
        Err(_e) => {
            // We know this was an "end of slice" error
            Err(())
        }
    }
}

/// Tricky monomorphizing deserialization function.
///
/// This function performs type-erased deserialization by casting a generic node pointer
/// to the correct concrete type, then deserializing CBOR data from a buffer into that
/// type. It handles both initialization of uninitialized data and replacement of existing
/// data with proper drop semantics.
///
/// # Arguments
/// * `node` - A type-erased pointer to a `Node<()>` that will be cast to `Node<T>`
/// * `buf` - A byte slice containing CBOR-encoded data to deserialize into type `T`
/// * `drop_old`
///     - If `true`, properly drops the existing value before replacing it.
///     - If `false`, assumes the target is uninitialized and directly writes to it.
///
/// # Returns
/// * `Ok(usize)` - The number of bytes consumed from the buffer during deserialization
/// * `Err(())` - If CBOR deserialization fails
///
/// # Safety
/// The caller must ensure that:
/// - The storage list mutex is held during the entire operation to prevent concurrent access
/// - The `node` pointer is valid and points to a properly initialized `Node<T>`
/// - If `drop_old` is `true`, the target `MaybeUninit<T>` contains a valid, initialized `T`
/// - If `drop_old` is `false`, the target `MaybeUninit<T>` is uninitialized
/// - The buffer contains valid CBOR data that can be decoded into type `T`
///
/// TODO @James: Please review these safety requirements. Are they correct and complete?
fn deserialize<T>(node: NonNull<Node<()>>, buf: &[u8]) -> Result<usize, ()>
where
    T: 'static,
    T: CborLen<()>,
    for<'a> T: Decode<'a, ()>,
{
    let mut node: NonNull<Node<T>> = node.cast();
    let noderef: &mut Node<T> = unsafe { node.as_mut() };

    let res = minicbor::decode::<T>(buf);

    let t = match res {
        Ok(t) => t,
        // TODO: Should we return the actual error here or not?
        Err(_) => return Err(()),
    };

    let len = len_with(&t, &mut ());

    // TODO(AJM): anything to do here?
    assert!(matches!(noderef.header.state, State::Initial));
    noderef.t = MaybeUninit::new(t);

    Ok(len)
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

pub struct KvPair<'a> {
    key: &'a str,
    body: &'a [u8],
}

/// Find a `NodeHeader` with the given `key` in the storage list.
///
/// This function searches through the intrusive linked list of storage nodes to find
/// a node with a matching key hash. So this _can_ return false positives, i.e., two
/// distinct keys with the same hash value.
///
/// # Arguments
/// * `list` - A mutable guard to the locked storage list
/// * `key` - The key hash to search for
///
/// # Returns
/// * `Some(NonNull<NodeHeader>)` - A non-null pointer to the matching node header
/// * `None` - If no node with the given key exists in the list
///
/// # Safety
/// This function is safe to call as long as the caller holds the storage list mutex
/// (which is enforced by taking a `MutexGuard` parameter).
fn find_node(list: &mut List<NodeHeader>, key: &str) -> Option<NonNull<NodeHeader>> {
    // TODO @James: Same as before, please check if the safety requirements in the docstring
    // are enough (together with explicity requiring a MutexGuard)
    // Also I think we should use `raw_iter` instead of `iter` for the same reason as we do everywhere else.
    // But is that true? Or could we use `iter` if we don't do any pointer casting.
    //
    // SAFETY: Since we hold a lock on the List (taking MutexGuard as a parameter)
    // we have exclusive access to the contents of the list.
    list.iter_raw()
        .find(|item| unsafe { item.as_ref() }.key == key)
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

// /// Get the `counter` byte from a list item in the flash.
// ///
// /// Used to extract the counter from a `QueueIteratorItem`.
// fn extract_counter(item: &[u8]) -> Result<Counter, Error> {
//     item.get(KEY_LEN)
//         .ok_or(Error::Deserialization)
//         .map(|c| Wrapping(*c))
// }

/// Serialize a storage node by writing its key, counter, and payload data
/// into the provided buffer.
///
/// # Format
/// - `0..KEY_LEN`: The node's key
/// - `KEY_LEN`: The counter value
/// - `KEY_LEN+1..`: The serialized payload data (using the node's vtable)
///
/// # Arguments
/// * `headerptr` - A non-null pointer to the NodeHeader to serialize
/// * `serde_buf` - The buffer to write the serialized data into
///
/// # Returns
/// * `Ok(usize)` - The total number of bytes written to the buffer
/// * `Err(Error::Serialization)` - If serialization fails (e.g., buffer too small, serialization error)
///
/// # Safety
/// The caller must ensure that:
/// - `headerptr` points to a valid NodeHeader
/// - The storage list mutex is held during the entire operation
/// - The buffer is large enough to hold the serialized data
pub fn serialize_node(
    headerptr: NonNull<NodeHeader>,
    serde_buf: &mut [u8],
) -> Result<usize, Error> {
    let (vtable, key) = {
        let node = unsafe { headerptr.as_ref() };
        (node.vtable, node.key)
    };

    debug!("serializing node with key <{:?}>", key,);

    let mut cursor = Cursor::new(&mut *serde_buf);
    let res: Result<(), minicbor::encode::Error<EndOfSlice>> = minicbor::encode(key, &mut cursor);
    let Ok(()) = res else {
        return Err(Error::Serialization);
    };
    let used = cursor.position();
    let Some(remain) = serde_buf.get_mut(used..) else {
        return Err(Error::Serialization);
    };

    let nodeptr: NonNull<Node<()>> = headerptr.cast();

    // Serialize payload into buffer
    (vtable.serialize)(nodeptr, remain)
        .map(|len| len + used)
        .map_err(|_| Error::Serialization)
}

#[cfg(test)]
mod test {
    use super::SeqState;

    #[test]
    fn seq_sort() {
        let mut s = SeqState::default();
        assert_eq!(s.last_three, [0, 0, 0]);
        s.insert_good(1);
        assert_eq!(s.last_three, [0, 0, 1]);
        s.insert_good(2);
        assert_eq!(s.last_three, [0, 1, 2]);
        s.insert_good(3);
        assert_eq!(s.last_three, [1, 2, 3]);
        s.insert_good(4);
        assert_eq!(s.last_three, [2, 3, 4]);
        s.insert_good(1);
        assert_eq!(s.last_three, [2, 3, 4]);
    }
}

// // TODO @James: I've already created some of the test cases from
// // https://github.com/tweedegolf/cfg-noodle/issues/16 in tests/integration_tests.rs
// // and the tests in this module are mostly obsolete. What is still missing, though, are
// // unit tests that could be (re)used for fuzzing.
// // I think we should first decide on entire counter handling before writing these since
// // the function signatures and output might change. But once that is done, we can gather
// // a list of tests that should be implemented and we both write some of them.
// // And we might want to keep in mind that the fuzzer probably wants to reuse some of that code.
// // In general, we probably must decide on what the interface to the outside looks, first.
// #[cfg(test)]
// mod test {
//     extern crate std;
//     use super::*;

//     use core::time::Duration;
//     use minicbor::CborLen;
//     use mutex::raw_impls::cs::CriticalSectionRawMutex;
//     use sequential_storage::cache::NoCache;
//     use sequential_storage::mock_flash::MockFlashBase;
//     use sequential_storage::mock_flash::WriteCountCheck;
//     use test_log::test;
//     use tokio::task::JoinHandle;
//     use tokio::time::sleep;

//     #[derive(Debug, Encode, Decode, Clone, PartialEq, CborLen)]
//     struct PositronConfig {
//         #[n(0)]
//         up: u8,
//         #[n(1)]
//         down: u16,
//         #[n(2)]
//         strange: u32,
//     }

//     impl Default for PositronConfig {
//         fn default() -> Self {
//             Self {
//                 up: 10,
//                 down: 20,
//                 strange: 103,
//             }
//         }
//     }

//     // TODO: This type, the get_mock_flash() and worker_task() are not only specific to sequential storage's
//     // mock flash, but also copy&pasted in the integration tests. If we go with the flash-trait approach,
//     // these could be generic.
//     type MockFlash = Flash<MockFlashBase<10, 16, 256>, NoCache>;

//     fn get_mock_flash() -> MockFlash {
//         let mut flash = MockFlashBase::<10, 16, 256>::new(WriteCountCheck::OnceOnly, None, true);
//         // TODO: Figure out why miri tests with unaligned buffers and whether
//         // this needs any fixing. For now just disable the alignment check in MockFlash
//         flash.alignment_check = false;
//         Flash::new(flash, 0x0000..0x1000, NoCache::new())
//     }

//     fn worker_task<R: ScopedRawMutex + Sync>(
//         list: &'static StorageList<R>,
//         mut flash: MockFlash,
//         mut read_buf: [u8; 4096],
//         mut serde_buf: [u8; 4096],
//     ) -> JoinHandle<()> {
//         tokio::task::spawn(async move {
//             loop {
//                 info!("worker_task waiting for needs_* signal");
//                 match embassy_futures::select::select(
//                     list.needs_read.wait(),
//                     list.needs_write.wait(),
//                 )
//                 .await
//                 {
//                     embassy_futures::select::Either::First(_) => {
//                         info!("worker task got needs_read signal");
//                         if let Err(e) = list.process_reads(&mut flash, &mut read_buf).await {
//                             error!("Error in process_reads: {:?}", e);
//                         }
//                     }
//                     embassy_futures::select::Either::Second(_) => {
//                         info!("worker task got needs_write signal");
//                         if let Err(e) = list
//                             .process_writes(&mut flash, &mut read_buf, &mut serde_buf)
//                             .await
//                         {
//                             error!("Error in process_writes: {:?}", e);
//                         }

//                         info!("Wrote to flash: {}", flash.flash().print_items().await);
//                     }
//                 }
//             }
//         })
//     }

//     #[test(tokio::test)]
//     async fn test_two_configs() {
//         static GLOBAL_LIST: StorageList<CriticalSectionRawMutex> = StorageList::new();
//         static POSITRON_CONFIG1: StorageListNode<PositronConfig> =
//             StorageListNode::new("positron/config1");
//         static POSITRON_CONFIG2: StorageListNode<PositronConfig> =
//             StorageListNode::new("positron/config2");

//         let flash = get_mock_flash();

//         let read_buf = [0u8; 4096];
//         let serde_buf = [0u8; 4096];

//         info!("Spawn worker_task");
//         let worker_task = worker_task(&GLOBAL_LIST, flash, read_buf, serde_buf);

//         // Obtain a handle for the first config
//         let config_handle = match POSITRON_CONFIG1.attach(&GLOBAL_LIST).await {
//             Ok(ch) => ch,
//             Err(_) => panic!("Could not attach config 1 to list"),
//         };

//         // Obtain a handle for the second config. This should _not_ error!
//         if POSITRON_CONFIG2.attach(&GLOBAL_LIST).await.is_err() {
//             panic!("Could not attach config 2 to list");
//         }

//         // Load data for the first handle
//         let data: PositronConfig = config_handle
//             .load()
//             .await
//             .expect("Loading config should not fail");
//         info!("T3 Got {data:?}");

//         // Assert that the counter is at default value because we
//         // haven't read this from flash
//         assert_eq!(
//             unsafe { config_handle.inner.inner.get().as_ref() }
//                 .expect("Getting inner at this point should not fail")
//                 .header
//                 .counter,
//             Some(Counter::default())
//         );

//         // Write a new config to first handle
//         let new_config = PositronConfig {
//             up: 15,
//             down: 25,
//             strange: 108,
//         };
//         config_handle
//             .write(&new_config)
//             .await
//             .expect("Writing config to node should not fail");

//         // Give the worker_task some time to process the write
//         sleep(Duration::from_millis(100)).await;

//         // Assert that the loaded value equals the written value
//         assert_eq!(
//             config_handle
//                 .load()
//                 .await
//                 .expect("Loading config should not fail"),
//             new_config
//         );

//         // Assert that the counter is now 1
//         assert_eq!(
//             unsafe { config_handle.inner.inner.get().as_ref() }
//                 .expect("Getting inner at this point should not fail")
//                 .header
//                 .counter
//                 .expect("Counter should have a value at this point"),
//             Wrapping(1)
//         );

//         // Wait for the worker task to finish
//         let _ = tokio::time::timeout(Duration::from_secs(2), worker_task).await;
//     }

//     #[test(tokio::test)]
//     async fn test_load_existing() {
//         // This test will write a config to the flash first and then read it back to check
//         // whether reading an item from flash works.

//         static GLOBAL_LIST: StorageList<CriticalSectionRawMutex> = StorageList::new();
//         static POSITRON_CONFIG: StorageListNode<PositronConfig> =
//             StorageListNode::new("positron/config");

//         let mut serde_buf = [0u8; 4096];
//         let read_buf = [0u8; 4096];

//         // Serialize the custom_config config so we can write it to our flash
//         let custom_config = PositronConfig {
//             up: 1,
//             down: 22,
//             strange: 333,
//         };

//         // TODO: The following pointer operations are a very cumbersome way
//         // of serializing the node just so we can push it to the flash and read back...
//         unsafe {
//             POSITRON_CONFIG
//                 .inner
//                 .get()
//                 .as_mut()
//                 .expect("Pointer should not be null")
//                 .t = MaybeUninit::new(custom_config.clone());
//             POSITRON_CONFIG
//                 .inner
//                 .get()
//                 .as_mut()
//                 .expect("Pointer should not be null")
//                 .header
//                 .counter = Some(Wrapping(0));
//         }
//         let nodeptr: *mut Node<PositronConfig> = POSITRON_CONFIG.inner.get();
//         let nodenn: NonNull<Node<PositronConfig>> = unsafe { NonNull::new_unchecked(nodeptr) };
//         let hdrnn: NonNull<NodeHeader> = nodenn.cast();

//         let used = serialize_node(hdrnn, &mut serde_buf).expect("Serializing node should not fail");

//         // Now serialize again by calling encode() directly and verify both match
//         let mut serialize_control = std::vec![];
//         let len = len_with(&custom_config, &mut ());
//         minicbor::encode(&custom_config, &mut serialize_control).expect("Encoding should not fail");
//         // The serialized node will contain some metadata, but the last `len` bytes must match
//         assert_eq!(serialize_control[..len], serde_buf[used - len..used]);

//         let mut flash = get_mock_flash();
//         info!("Pushing to flash: {:?}", &serde_buf[..used]);
//         flash
//             .push(&serde_buf[..used])
//             .await
//             .expect("pushing to flash should not fail here");

//         let worker_task = worker_task(&GLOBAL_LIST, flash, read_buf, serde_buf);

//         // Obtain a handle for the config. It should match the custom_config.
//         // This should _not_ error!
//         let expecting_already_present = match POSITRON_CONFIG.attach(&GLOBAL_LIST).await {
//             Ok(ch) => ch,
//             Err(_) => panic!("Could not attach config to list"),
//         };

//         assert_eq!(
//             custom_config,
//             expecting_already_present
//                 .load()
//                 .await
//                 .expect("Loading config should not fail"),
//             "Key should already be present"
//         );

//         expecting_already_present
//             .write(&custom_config)
//             .await
//             .expect("Writing config to node should not fail");

//         sleep(Duration::from_millis(50)).await;

//         // Assert that the counter is now 1
//         assert_eq!(
//             unsafe { expecting_already_present.inner.inner.get().as_ref() }
//                 .expect("Node should exist")
//                 .header
//                 .counter
//                 .expect("Counter should have a value"),
//             Wrapping(1)
//         );

//         // Wait for the worker task to finish
//         let _ = tokio::time::timeout(Duration::from_secs(2), worker_task).await;
//     }

//     #[test(tokio::test)]
//     #[should_panic]
//     async fn test_duplicate_key() {
//         static GLOBAL_LIST: StorageList<CriticalSectionRawMutex> = StorageList::new();
//         static POSITRON_CONFIG1: StorageListNode<PositronConfig> =
//             StorageListNode::new("positron/config");
//         static POSITRON_CONFIG2: StorageListNode<PositronConfig> =
//             StorageListNode::new("positron/config");

//         let flash = get_mock_flash();

//         info!("Spawn worker_task");
//         let serde_buf = [0u8; 4096];
//         let read_buf = [0u8; 4096];
//         let worker_task = worker_task(&GLOBAL_LIST, flash, read_buf, serde_buf);

//         // Obtain a handle for the first config
//         let _config_handle = match POSITRON_CONFIG1.attach(&GLOBAL_LIST).await {
//             Ok(ch) => ch,
//             Err(_) => panic!("Could not attach config 1 to list"),
//         };

//         // Obtain a handle for the second config. It has the same key as the first.
//         // This must panic!
//         let _expecting_panic = POSITRON_CONFIG2.attach(&GLOBAL_LIST).await;

//         // Wait for the worker task to finish
//         let _ = tokio::time::timeout(Duration::from_secs(2), worker_task).await;
//     }
// }
