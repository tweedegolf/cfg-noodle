//! # Intrusive Linked List for Configuration Storage
//! This implements the [StorageList] and its [StorageListNode]s

use crate::{
    error::{Error, LoadStoreError},
    logging::{debug, error, info},
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
    num::Wrapping,
    ptr::{self, NonNull},
};
use embedded_storage_async::nor_flash::NorFlash;
//use futures::TryFutureExt;
use maitake_sync::{Mutex, MutexGuard, WaitQueue};
use minicbor::{
    CborLen, Decode, Encode,
    encode::write::{Cursor, EndOfSlice},
    len_with,
};
use mutex::{ConstInit, ScopedRawMutex};
use sequential_storage::{cache::NoCache, queue};

pub type Counter = Wrapping<u8>;

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
    list: Mutex<List<NodeHeader>, R>,
    // TODO: do we need a `needs_read` WaitQueue?
    // Read is only needed after attaching, and attach() does not return before
    // the node is hydrated or initialized with a default value.
    // But what if a node is "late to the party"?
    needs_write: WaitQueue,
    reading_done: WaitQueue,
    writing_done: WaitQueue,
}

/// StorageListNode represents the storage of a single `T`, linkable to a `StorageList`
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
    key: [u8; KEY_LEN],
    /// The current state of the node. THIS IS SAFETY LOAD BEARING whether
    /// we can access the `T` in the `Node<T>`
    state: State,
    /// Counter that is increased on every write to flash.
    /// This helps determine which entry is the newest, should there ever be
    /// two entries for the same key.
    /// A `None` value indicates that the node has not been found in flash (yet)
    /// and the counter is not yet initialized.
    counter: Option<Counter>,
    /// This is the type-erased serialize/deserialize `VTable` that is
    /// unique to each `T`, and will be used by the storage worker
    /// to access the `t` indirectly for loading and storing.
    vtable: VTable,
}

/// The current state of the `Node<T>`.
///
/// This is used to determine how to unsafely interact with other pieces
/// of the `Node<T>`!
///
/// ## State transition diagram
/// TODO: Update this!
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
#[derive(Debug, Clone)]
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
    /// The node has been found but did not have the latest counter value
    /// assigned, so it is not from the latest write. The attach() function
    /// will set the config to the default value and return the old data, so
    /// the user can decide.
    ///
    /// After assigning the default value, the state must be set to
    /// `DefaultUnwritten`.
    OldData,
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
    /// The flush back to flash has started, but may not have completed yet
    ///
    /// TODO: this state needs some more design. Especially how we mark the state
    /// if the owner of the node writes ANOTHER new value while we are waiting
    /// for the flush to "stick".
    ///
    /// In this state, `t` IS valid, and may be read at any time (by the holder
    /// of the lock).
    WriteStarted,
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

#[derive(Clone, Copy)]
pub struct VTable {
    serialize: SerFn,
    deserialize: DeserFn,
}

/// Owns a flash and the range reserved for the `StorageList`
pub struct Flash<T: NorFlash> {
    flash: T,
    range: core::ops::Range<u32>,
}

impl<T: NorFlash> Flash<T> {
    pub fn new(flash: T, range: core::ops::Range<u32>) -> Self {
        Self { flash, range }
    }

    pub fn flash(&mut self) -> &mut T {
        &mut self.flash
    }
    pub fn range(&mut self) -> core::ops::Range<u32> {
        self.range.clone()
    }
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
            list: Mutex::new_with_raw_mutex(List::new(), R::INIT),
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
///
/// The current implementation is "fake", it's only meant for demos. Once we
/// figure out how we plan to store stuff, like `s-s::Queue`, or some new
/// theoretical `s-s::Latest` structure, these methods will need to be
/// re-worked.
///
/// For now they are oriented around using a HashMap.
impl<R: ScopedRawMutex> StorageList<R> {
    /// Process any nodes that are requesting flash data
    ///
    /// ## Params:
    /// - `flash`: a [`Flash`] object containing the necessary information to call [`queue::push`]
    /// - `buf`: a scratch-buffer that must be large enough to hold the largest serialized node
    ///   In practice, a buffer as large as the flash's page size is enough because sequential
    ///   storage can not store larger items.
    ///
    /// This method:
    ///
    /// 1. Locks the mutex
    ///
    /// // TODO: UPDATE THIS WITH NEW PROCEDURE
    /// 2. Iterates through each node currently linked in the list
    /// 3. For Each node that says "I need to be hydrated", it sees if a matching
    ///    key record exists in the hashmap (our stand-in for the actual flash
    ///    contents). If it does, it will try to hand control over to the node
    ///    to extract data
    /// 4. On success, it will mark the node as now having valid data. On failure
    ///    it marks that (discussed more below, some of this might not be perfect
    ///    yet and requires more thought)
    pub async fn process_reads(&'static self, flash: &mut Flash<impl NorFlash>, buf: &mut [u8]) {
        info!("Start process_reads, buffer has len {}", buf.len());
        // Lock the list, remember, if we're touching nodes, we need to have the list
        // locked the entire time!
        let mut ls = self.list.lock().await;
        debug!("process_reads locked list");

        // Store the counter of the last write_confirm block we found so that later on
        // we know if the latest counter we found has actually been confirmed
        let mut latest_write_confirm: Option<Counter> = None;
        let mut latest_counter: Option<Counter> = None;

        // Create a `QueueIterator` without caching
        let mut cache = NoCache::new();
        let mut queue_iter = queue::iter(&mut flash.flash, flash.range.clone(), &mut cache)
            .await
            .unwrap();
        while let Some(item) = queue_iter.next(buf).await.unwrap() {
            let key: &[u8; KEY_LEN] = extract_key(&item).expect("Invalid item: no key");
            latest_counter.replace(extract_counter(&item).expect("Invalid item: no counter"));
            let payload: &[u8] = extract_payload(&item).expect("Invalid item: no payload");

            debug!(
                "Extracted metadata: key {:?}, counter: {:?}, payload: {:?}",
                key, latest_counter, payload
            );
            // Check for the write_confirm key
            if is_write_confirm(&item) {
                info!("Found write_confirm for counter {:?}", latest_counter);
                latest_write_confirm.replace(latest_counter.unwrap()); // Safe to unwrap because we replace it before
            } else if let Some(node_header) = find_node(&mut ls, key) {
                // SAFETY: We hold the lock, we are allowed to gain exclusive mut access
                // to the contents of the node. We COPY OUT the vtable for later use,
                // which is important because we might throw away our `node` ptr shortly.
                let vtable = {
                    let node_header = unsafe { node_header.as_ref() };
                    match node_header.state {
                        State::Initial => Some(node_header.vtable),
                        State::NonResident => None,
                        State::DefaultUnwritten => None,
                        State::OldData => None,
                        // TODO: Handle the case where a key is found again (with a different counter value)
                        State::ValidNoWriteNeeded => None,
                        State::NeedsWrite => None,
                        State::WriteStarted => None,
                    }
                };
                if let Some(vtable) = vtable {
                    // Make a node pointer from a header pointer. This *consumes* the `Pin<&mut NodeHeader>`, meaning
                    // we are free to later re-invent other mutable ptrs/refs, AS LONG AS we still treat the data
                    // as pinned.
                    let mut hdrptr: NonNull<NodeHeader> = node_header;
                    let nodeptr: NonNull<Node<()>> = hdrptr.cast();

                    // Note: We MUST have destroyed `node` at this point, so we don't have aliasing
                    // references of both the Node and the NodeHeader. Pointers are fine!
                    //
                    // TODO: right now we only read if `t` is uninhabited, so we don't care about dropping
                    // or overwriting the value. IF WE EVER want the ability to "reload from flash", we
                    // might want to think about drop!
                    let res = (vtable.deserialize)(nodeptr, payload);

                    // SAFETY: We can re-magic a reference to the header, because the NonNull<Node<()>>
                    // does not have a live reference anymore
                    let hdrmut = unsafe { hdrptr.as_mut() };

                    if res.is_ok() {
                        // If it went okay, let the node know that it has been hydrated with data
                        // from the flash by setting its counter value
                        hdrmut.counter.replace(latest_counter.unwrap()); // Safe to unwrap because we replaced it before
                        self.reading_done.wake_all();
                    } else {
                        // If there WAS a key, but the deser failed, this means that either the data
                        // was corrupted, or there was a breaking schema change. Either way, we can't
                        // particularly recover from this. We might want to log this, but exposing
                        // the difference between this and "no data found" to the node probably won't
                        // actually be useful.
                        //
                        // todo: add logs? some kind of asserts?
                        hdrmut.state = State::NonResident;
                        self.reading_done.wake_all();
                    }
                    // The node has asked for data, and our flash has nothing for it. Let the node
                    // know that there's definitely nothing to wait for anymore, and it should
                    // figure out it's own data, probably from Default::default().
                }
            }
        }

        // Set nodes in initial states to non resident
        for node_header in ls.iter_raw() {
            // Make a node pointer from a header pointer. This *consumes* the `Pin<&mut NodeHeader>`, meaning
            // we are free to later re-invent other mutable ptrs/refs, AS LONG AS we still treat the data
            // as pinned.
            let mut hdrptr: NonNull<NodeHeader> = node_header;

            // SAFETY: We can re-magic a reference to the header, because the NonNull<Node<()>>
            // does not have a live reference anymore
            let hdrmut = unsafe { hdrptr.as_mut() };

            // After reading has finished, there are several possible situations:
            // - Node was found with the latest counter value
            //   hdrmut.counter == latest_counter -> `ValidNoWriteNeeded`
            // - Node was found, but with an older counter value
            //   hdrmut.counter != latest_counter
            //   - We have not found a write_confirm block (for the latest counter value)
            //     latest_counter != latest_write_confirm -> `OldData` and return an error in attach
            //   - We have found write_confirm block (for the latest counter value)
            //     latest_counter == latest_write_confirm -> `NonResident` and delete its payload (?)
            // - Node was not found - regardless of a write_confirm block -> `NonResident`
            //   hdrmut.counter == None
            //
            // If the hdrmut.state is not initial, the node has already been handled elsewhere
            if let State::Initial = hdrmut.state {
                debug!(
                    "post-processing node in initial state with counter {:?}, latest_counter {:?} and latest_write_confirm {:?}",
                    hdrmut.counter, latest_counter, latest_write_confirm
                );
                hdrmut.state = match (hdrmut.counter, latest_counter, latest_write_confirm) {
                    // Best case: node counter matches the latest counter value
                    (Some(counter), Some(latest), _) if counter == latest => {
                        State::ValidNoWriteNeeded
                    }
                    // Node contains an old counter and the latest counter has a write_confirm block
                    (Some(_counter), Some(latest), Some(write_confirm))
                        if latest == write_confirm =>
                    {
                        State::NonResident
                    }
                    // Node contains an old counter but the newest one does not have a write_confirm block
                    (Some(_counter), Some(_latest), _) => State::OldData,
                    // Node has not occured once (hence its counter is None)
                    (None, _, _) => State::NonResident,
                    // Invalid: if the state is initial and the node occured, it MUST have updated
                    // the latest_counter
                    (Some(_), None, _) => {
                        panic!("latest_counter has not been set. This can not happen.")
                    }
                };
            }
        }

        // TODO: Clarify if we should set the counter here and whether we should already
        // "bump" it at this point or only just before writing.
        // Set the counter to the latest one because know its value here.
        // Inside process_reads, it needs to be bumped, but there it is more difficult
        // to find out what the latest counter was due to wrapping.
        set_counter(&mut ls, latest_counter.unwrap_or_default());

        debug!("Reading done. Waking all.");
        self.reading_done.wake_all();
    }

    /// Process writes to flash.
    ///
    /// If any of the nodes has pending writes, this function writes the
    /// entire list to flash.
    ///
    /// ## Arguments:
    /// * `flash`: a [`Flash`] object containing the necessary information to call [`queue::push`]
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
    /// 3. For each node that has pending changes, serializes the node data
    /// 4. Writes all changed nodes to flash using sequential storage
    /// 5. Verifies the written data matches what was intended to be written
    /// 6. On success, marks all nodes as no longer having pending changes
    pub async fn process_writes<F: NorFlash>(
        &'static self,
        flash: &mut Flash<F>,
        read_buf: &mut [u8],
        serde_buf: &mut [u8],
    ) -> Result<(), LoadStoreError<F::Error>> {
        debug!("Start process_writes");

        // Lock the list, remember, if we're touching nodes, we need to have the list
        // locked the entire time!
        let mut ls = self.list.lock().await;
        debug!("process_writes locked list");

        // Set to true if any node in the list needs writing
        let mut needs_writing = false;

        // Track the newest counter in the list. This will be the new counter value
        // when the list is written.
        // If no counter is present yet, continue with the default value.
        let mut max_counter: Counter = Default::default();

        // Iterate over all list nodes and
        // 1. Store the counter (if present).
        //    We technically only need the counter from one list item that has been
        //    read from flash and has counter != None.
        // 2. Check if any node needs writing or is in an invalid state.
        for hdrptr in ls.iter_raw() {
            let header = unsafe { hdrptr.as_ref() };

            // If the node has a counter, it must have been read from flash.
            // If the counter is `None`, it can be ignored.
            if let Some(counter) = header.counter {
                max_counter = counter;
            }

            match header.state {
                // If no write is needed, we obviously won't write.
                State::ValidNoWriteNeeded => (),
                // If the node hasn't been written to flash yet and we initialized it
                // with a Default, we now write it to flash.
                State::DefaultUnwritten | State::NeedsWrite | State::OldData => {
                    needs_writing = true
                }
                // TODO: A node may be in `Initial` state, if it has been attached but
                // `process_reads` hasn't run, yet. Should we return early here and trigger
                // another process_reads?
                State::Initial => {
                    return Err(LoadStoreError::NeedsRead);
                }
                // Neither of these cases can appear on a list that has been processed properly.
                // Not sure how we should recover from this...
                State::NonResident | State::WriteStarted => {
                    debug!("process_writes() on invalid state: {:?}", header.state);

                    return Err(LoadStoreError::AppError(Error::InvalidState(
                        header.key,
                        header.state.clone(),
                    )));
                }
            }
        }

        // If the list is unchanged, there is no need to write it to flash!
        if !needs_writing {
            debug!("List does not need writing. Returning.");
            return Ok(());
        }

        // Increase the counter for this list by one...
        set_counter(&mut ls, max_counter + Wrapping(1));

        debug!("Attempt write_to_flash");
        // ... and try writing the list to flash one time.
        // If this fails, try again. If it fails again, return the error.
        if let Err(e) = write_to_flash(&mut ls, serde_buf, flash).await {
            error!(
                "First writing attempt failed! Error: {}. Trying again...",
                e
            );

            // Increase the counter for this list by two
            set_counter(&mut ls, max_counter + Wrapping(2));
            // Try one more time, but return any error
            write_to_flash(&mut ls, serde_buf, flash).await?;
        }

        verify_list_in_flash(&mut ls, read_buf, serde_buf, flash).await?;

        // The write must have succeeded. So mark all nodes accordingly.
        for mut hdrptr in ls.iter_raw() {
            // Mark the store as complete, wake the node if it cares about state
            // changes.
            // TODO: will nodes ever care about state changes other than
            // the initial hydration?
            let hdrmut = unsafe { hdrptr.as_mut() };
            hdrmut.state = State::ValidNoWriteNeeded;
            self.writing_done.wake_all();
        }

        Ok(())
    }
}

/// Sets the counter value for all nodes in the storage list.
///
/// This function iterates through all nodes in the linked list and updates their
/// counter field to the specified value. The counter is used to track write operations
/// and determine which entries are newest when multiple entries exist for the same key.
///
/// # Arguments
/// * `ls` - A mutable guard to the locked storage list
/// * `new_counter` - The counter value to assign to all nodes
///
/// # Safety
/// This function uses unsafe code to mutate node headers through raw pointers.
/// It is safe because the caller must hold the storage list mutex (enforced by
/// the `MutexGuard` parameter), ensuring exclusive access to the list contents.
fn set_counter(
    ls: &mut MutexGuard<'_, List<NodeHeader>, impl ScopedRawMutex>,
    new_counter: Counter,
) {
    debug!("Setting counter for list to: {}", new_counter);
    // Update counters before writing the list to flash
    // We do this before serialization because that may fail and
    // we want to avoid diverging counter values...
    for mut hdrptr in ls.iter_raw() {
        let header = unsafe { hdrptr.as_mut() };
        header.counter.replace(new_counter);
    }
}

/// Verifies that the storage list was correctly written to flash memory.
///
/// This function compares each node in the storage list against the corresponding
/// items stored in flash to ensure data integrity after a write operation. It iterates
/// through flash items until it finds the beginning of the newly-written list (identified
/// by matching counter values), then serializes each list node and compares it with
/// the flash data.
///
/// # Arguments
/// * `ls` - A mutable guard to the locked storage list containing nodes to verify
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
async fn verify_list_in_flash<F: NorFlash>(
    ls: &mut MutexGuard<'_, List<NodeHeader>, impl ScopedRawMutex>,
    read_buf: &mut [u8],
    serde_buf: &mut [u8],
    flash: &mut Flash<F>,
) -> Result<(), LoadStoreError<F::Error>> {
    // Create a `QueueIterator` without caching
    let mut cache = NoCache::new();
    let mut queue_iter = queue::iter(&mut flash.flash, flash.range.clone(), &mut cache)
        .await
        .unwrap();

    let mut iter = StaticRawIter {
        iter: ls.iter_raw(),
    };

    // Set true on the first occurence of the correct counter.
    // That means, we have hit the first element the newly-written list.
    let mut counter_found = false;

    // Iterate over the nodes in the list and...
    while let Some(hdrptr) = iter.next() {
        let header = unsafe { hdrptr.ptr.as_ref() };

        // Get the next item from the queue.
        // This loop will continue until we hit the first correct counter
        // in the flash. That will be the beginning of the newly-written list.
        // Then serialize the node and compare to the item in flash.
        while let Some(item) = queue_iter
            .next(read_buf)
            .await
            .map_err(LoadStoreError::FlashRead)?
        {
            // If the counter was found already, skip the check. If the counter is wrong,
            // comparing the serialized values will fail anyway.
            // Once we reach the correct counter, we are at the first element written and
            // can start processing. If the counter is wrong (or not present), continue:
            if !counter_found
                && !extract_counter(&item).is_ok_and(|counter| counter == header.counter.unwrap())
            {
                continue;
            }
            counter_found = true;

            // Serialize into the second buffer so we know what it *should* look like.
            let res = serialize_node(hdrptr.ptr, serde_buf);

            // Check if value in flash matches the serialized node
            if !res.is_ok_and(|len| item.into_buf() == &serde_buf[..len]) {
                return Err(LoadStoreError::WriteVerificationFailed);
            }
            // Value matched, so we can continue with the next list node
            break;
        }
    }
    Ok(())
}
/// Writes all nodes in the storage list to flash memory.
///
/// This function iterates through all nodes in the storage list, serializes each node's
/// data into the provided buffer, and writes it to flash using the sequential storage
/// queue. Each node is written as a separate flash item containing its key, counter,
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
/// The caller must hold the storage list mutex (enforced by the `MutexGuard` parameter)
/// to ensure exclusive access during the write operation.
async fn write_to_flash<F: NorFlash>(
    ls: &mut MutexGuard<'_, List<NodeHeader>, impl ScopedRawMutex>,
    buf: &mut [u8],
    flash: &mut Flash<F>,
) -> Result<(), LoadStoreError<F::Error>> {
    let mut iter = StaticRawIter {
        iter: ls.iter_raw(),
    };

    while let Some(hdrptr) = iter.next() {
        // Attempt to serialize
        let res = serialize_node(hdrptr.ptr, buf);

        if let Ok(used) = res {
            debug!(
                "Pushing to flash: {:?}, addr {:x}",
                &buf[..used],
                buf.as_ptr() as usize
            );
            // Try writing to flash
            queue::push(
                &mut flash.flash,
                flash.range.clone(),
                &mut NoCache::new(),
                &buf[..used],
                false,
            )
            .await
            .map_err(LoadStoreError::FlashWrite)?;
        } else {
            // I'm honestly not sure what we should do if serialization failed.
            // If it was a simple "out of space" because we have a batch of writes
            // already, we might want to try again later. If it failed for some
            // other reason, or it fails even with an empty page (e.g. serializes
            // to more than 1k or 4k or something), there's really not much to be
            // done, other than log.
            // For now, we just return an error and let the caller decide what to do.
            return Err(LoadStoreError::AppError(Error::Serialization));
        }
    }
    Ok(())
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
    T: Debug, // TODO: Remove all those Debug trait bounds? Just added for debugging
{
    /// Make a new StorageListNode, initially empty and unattached
    pub const fn new(path: &'static str) -> Self {
        Self {
            inner: UnsafeCell::new(Node {
                header: NodeHeader {
                    links: list::Links::new(),
                    key: const_fnv1a_hash::fnv1a_hash_str_32(path).to_le_bytes(),
                    state: State::Initial,
                    vtable: VTable::for_ty::<T>(),
                    counter: None,
                },
                t: MaybeUninit::uninit(),
                _pin: PhantomPinned,
            }),
        }
    }

    // Attaches node to a list and waits for hydration.
    // If the value is not found in flash, a default value is used.
    //
    // If the value is found in flash but only from an older write (e.g.
    // because the latest write operation was interrupted), the node
    // is initialized with the default, and the value from flash is
    // returned in the second element of the Result::Error tuple.
    //
    // # Panics
    // This function will panic if a node with the same key(-hash) already
    // exists in the list.
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
            let mut ls = list.list.lock().await;
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
            if find_node(&mut ls, key).is_some() {
                error!("Key already in use: {:?}", key);
                panic!("Node with the same key-hash already in the list");
            } else {
                ls.push_front(hdrnn);
            }
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
            let _lock = list.list.lock().await;

            // We have the lock, we can gaze into the UnsafeCell
            let nodeptr: *mut Node<T> = self.inner.get();
            let noderef: &mut Node<T> = unsafe { &mut *nodeptr };
            // Are we in a state with a valid T?
            match noderef.header.state {
                // TODO: This should not really happen because it means we attached in
                // the middle of the read process. Which would us and the read process
                // have the list locked simultaneously.
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
                State::OldData => {
                    // TODO: Check safety requirements.
                    // This is taken from write()
                    //
                    // SAFETY: we may access `noderef.t` because we hold the lock.
                    // We do a swap instead of a write here, to ensure that we
                    // call `drop` on the "old" contents of the buffer.
                    let mut t = T::default();
                    unsafe {
                        let mutref: &mut T = noderef.t.assume_init_mut();
                        core::mem::swap(&mut t, mutref);
                    }

                    // Since we set the default value, it is effectively this state
                    noderef.header.state = State::DefaultUnwritten;

                    return Err((StorageListNodeHandle { list, inner: self }, t));
                }
                State::ValidNoWriteNeeded => break,
                State::NeedsWrite => todo!("shouldn't observe this in attach"),
                State::WriteStarted => todo!("shouldn't observe this in attach"),
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
        let _lock = self.list.list.lock().await;

        let nodeptr: *mut Node<T> = self.inner.inner.get();
        let noderef = unsafe { &mut *nodeptr };
        // is T valid?
        match noderef.header.state {
            // All these states implicate that process_reads/_writes has not finished
            // but they hold a lock on the list, so we should never reach this piece
            // of code while holding a lock ourselves
            State::Initial | State::NonResident | State::WriteStarted | State::OldData => {
                return Err(Error::InvalidState(
                    noderef.header.key,
                    noderef.header.state.clone(),
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
        let _lock = self.list.list.lock().await;

        let nodeptr: *mut Node<T> = self.inner.inner.get();
        let noderef = unsafe { &mut *nodeptr };
        // determine state
        // TODO: allow writes from uninit (`Initial`) state?
        match noderef.header.state {
            // `Initial` and `NonResident` can not occur for the same reason as
            // outlined in load().
            // `WriteStarted` can not happen either: process_writes locks the list and
            // therefore we cannot reach this code while process_writes is running and
            // it will not finish (and release the lock) until it finished writing.
            State::WriteStarted | State::Initial | State::NonResident | State::OldData => {
                debug!("write() on invalid state: {:?}", noderef.header.state);
                return Err(Error::InvalidState(
                    noderef.header.key,
                    noderef.header.state.clone(),
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
        // This is problematic since we use an async mutex!
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
/// Casts a type-erased node pointer into the "right" type, so that we can do
/// deserialization with it.
///
/// # Safety
/// The mutex must be held the whole time we are here, because we are
/// writing to `t`!
///
/// TODO: this ASSUMES that we will only ever call `deserialize` once, on initial
/// hydration, so we don't need to worry about dropping the previous contents of `t`,
/// because there shouldn't be any. If we want to offer a "reload from flash" feature,
/// we may need to add a `needs drop` flag here to swap out the data.
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

    noderef.t = MaybeUninit::new(t);

    Ok(len)
}

// --------------------------------------------------------------------------
// Helper structs and functions
// --------------------------------------------------------------------------

/// Wrapper for [`IterRaw`] that implements `Send`, until this issue is resolved:
/// https://github.com/hawkw/mycelium/issues/535
struct StaticRawIter<'a> {
    iter: IterRaw<'a, NodeHeader>,
}
unsafe impl Send for StaticRawIter<'_> {}

impl StaticRawIter<'_> {
    fn next(&mut self) -> Option<SendPtr> {
        self.iter.next().map(|ptr| SendPtr { ptr })
    }
}

/// Wrapper for [`NonNull<NodeHeader>`] that implements `Send`.
///
/// ## Safety
/// This must only be used when the List mutex is locked and Node and Anchor
/// live &'static.
/// TODO: Review and expand on the exact requirements for using this.
struct SendPtr {
    ptr: NonNull<NodeHeader>,
}
unsafe impl Send for SendPtr {}

/// Node Key length in bytes
pub const KEY_LEN: usize = 4;

/// Find a `NodeHeader` with the given `key` in the storage list.
///
/// This function searches through the intrusive linked list of storage nodes to find
/// a node with a matching key.
///
/// # Arguments
/// * `list` - A mutable guard to the locked storage list
/// * `key` - The key to search for (fixed-size byte array)
///
/// # Returns
/// * `Some(NonNull<NodeHeader>)` - A non-null pointer to the matching node header
/// * `None` - If no node with the given key exists in the list
///
/// # Safety
/// This function is safe to call as long as the caller holds the storage list mutex
/// (which is enforced by taking a `MutexGuard` parameter). The raw iteration and
/// unsafe pointer dereferencing are sound because:
/// - The mutex guard ensures exclusive access to the list contents
/// - All nodes in the list are guaranteed to be valid and properly initialized
///   (payload is not read)
/// - The `NonNull` pointers returned by `iter_raw()` are guaranteed to be valid
fn find_node<R: ScopedRawMutex>(
    list: &mut MutexGuard<List<NodeHeader>, R>,
    key: &[u8; KEY_LEN],
) -> Option<NonNull<NodeHeader>> {
    // TODO: Check if the safety requirements are upheld!
    // It seems like we should not use `iter` for the same reason we use `raw_iter` instead in read/write
    // SAFETY: Since we hold a lock on the List (taking MutexGuard as a parameter)
    // we have exclusive access to the contents of the list.
    list.iter_raw()
        .find(|item| unsafe { item.as_ref() }.key == *key)
}

/// Get the `key` bytes from a list item in the flash.
/// Used to extract the key from a `QueueIteratorItem`.
fn extract_key(item: &[u8]) -> Result<&[u8; KEY_LEN], Error> {
    item[..KEY_LEN]
        .try_into()
        .map_err(|_| Error::Deserialization)
}

/// Get the `counter` byte from a list item in the flash.
/// Used to extract the counter from a `QueueIteratorItem`.
fn extract_counter(item: &[u8]) -> Result<Counter, Error> {
    item.get(KEY_LEN)
        .ok_or(Error::Deserialization)
        .map(|c| Wrapping(*c))
}

/// Get the `payload` bytes from a list item in the flash.
/// Used to extract the payload from a `QueueIteratorItem`.
fn extract_payload(item: &[u8]) -> Result<&[u8], Error> {
    item.get(KEY_LEN + 1..).ok_or(Error::Deserialization)
}

/// Check if the list item in the flash is a `write_confirm` block.
fn is_write_confirm(item: &[u8]) -> bool {
    item[..KEY_LEN].iter().all(|&elem| elem == 0)
}

/// This function serializes a storage node by writing its key, counter, and payload data
/// into the provided buffer in a specific format:
/// - Bytes 0..KEY_LEN: The node's key
/// - Byte KEY_LEN: The counter value (or 0 if no counter is set)
/// - Bytes KEY_LEN+1..: The serialized payload data (using the node's vtable)
///
/// # Arguments
/// * `headerptr` - A non-null pointer to the NodeHeader to serialize
/// * `buf` - The buffer to write the serialized data into
///
/// # Returns
/// * `Ok(usize)` - The total number of bytes written to the buffer
/// * `Err(())` - If serialization fails (e.g., buffer too small, serialization error)
///
/// # Safety
/// The caller must ensure that:
/// - `headerptr` points to a valid NodeHeader
/// - The storage list mutex is held during the entire operation
/// - The buffer is large enough to hold the serialized data
pub fn serialize_node(headerptr: NonNull<NodeHeader>, buf: &mut [u8]) -> Result<usize, Error> {
    let (vtable, key, counter) = {
        let node = unsafe { headerptr.as_ref() };
        (node.vtable, node.key, node.counter)
    };

    debug!(
        "serializing node with key <{:?}>, counter <{:?}>",
        key, counter
    );

    let nodeptr: NonNull<Node<()>> = headerptr.cast();

    // Attempt to serialize
    buf[0..KEY_LEN].copy_from_slice(&key);
    buf[KEY_LEN] = counter.map(|c| c.0).unwrap_or(0);
    (vtable.serialize)(nodeptr, &mut buf[KEY_LEN + 1..])
        .map(|len| len + KEY_LEN + 1)
        .map_err(|_| Error::Serialization)
}

#[cfg(all(test, feature = "_test"))]
mod test {
    extern crate std;
    use super::*;

    use crate::logging::warn;
    use core::time::Duration;
    use minicbor::CborLen;
    use mutex::raw_impls::cs::CriticalSectionRawMutex;
    use sequential_storage::mock_flash::MockFlashBase;
    use sequential_storage::mock_flash::WriteCountCheck;
    use test_log::test;
    use tokio::time::sleep;

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

    #[test(tokio::test)]
    async fn test_two_configs() {
        static GLOBAL_LIST: StorageList<CriticalSectionRawMutex> = StorageList::new();
        static POSITRON_CONFIG1: StorageListNode<PositronConfig> =
            StorageListNode::new("positron/config1");
        static POSITRON_CONFIG2: StorageListNode<PositronConfig> =
            StorageListNode::new("positron/config2");

        let mut flash = Flash {
            flash: MockFlashBase::<10, 16, 256>::new(WriteCountCheck::OnceOnly, None, true),
            range: 0x0000..0x1000,
        };
        // TODO: Figure out why miri tests with unaligned buffers and whether
        // this needs any fixing. For now just disable the alignment check in MockFlash
        flash.flash().alignment_check = false;

        let range = flash.range();
        sequential_storage::erase_all(&mut flash.flash(), range)
            .await
            .unwrap();

        info!("Spawn worker_task");
        let worker_task = tokio::task::spawn(async move {
            let read_buf = &mut [0u8; 4096];
            let serde_buf = &mut [0u8; 4096];

            for _ in 0..10 {
                GLOBAL_LIST.process_reads(&mut flash, read_buf).await;
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                match GLOBAL_LIST
                    .process_writes(&mut flash, read_buf, serde_buf)
                    .await
                {
                    Ok(_) | Err(LoadStoreError::NeedsRead) => continue,
                    Err(e) => error!("Error in process_writes: {}", e),
                }

                info!("NEW WRITES: {}", flash.flash().print_items().await);
            }
        });

        // Obtain a handle for the first config
        let config_handle = match POSITRON_CONFIG1.attach(&GLOBAL_LIST).await {
            Ok(ch) => ch,
            Err(_) => panic!("Could not attach config 1 to list"),
        };

        // Obtain a handle for the second config. This should _not_ error!
        let expecting_no_error = POSITRON_CONFIG2.attach(&GLOBAL_LIST).await;
        if expecting_no_error.is_err() {
            panic!("Could not attach config 2 to list");
        }
       
       
        // Load data for the first handle
        let data: PositronConfig = config_handle.load().await.unwrap();
        info!("T3 Got {data:?}");

        // Assert that the counter is at default value because we
        // haven't read this from flash
        assert_eq!(
            unsafe { config_handle.inner.inner.get().as_ref() }
                .unwrap()
                .header
                .counter,
            Some(Counter::default())
        );

        // Write a new config to first handle
        let new_config = PositronConfig {
            up: 15,
            down: 25,
            strange: 108,
        };
        config_handle.write(&new_config).await.unwrap();

        // Give the worker_task some time to process the write
        sleep(Duration::from_millis(100)).await;

        // Assert that the loaded value equals the written value
        assert_eq!(config_handle.load().await.unwrap(), new_config);

        // Assert that the counter is now 1
        assert_eq!(
            unsafe { config_handle.inner.inner.get().as_ref() }
                .unwrap()
                .header
                .counter
                .unwrap(),
            Wrapping(1)
        );

        // Wait for the worker task to finish
        worker_task.await.unwrap();
    }

    #[test(tokio::test)]
    async fn test_load_existing() {
        // This test will write a config to the flash first and then read it back to check
        // whether reading an item from flash works.

        static GLOBAL_LIST: StorageList<CriticalSectionRawMutex> = StorageList::new();
        static POSITRON_CONFIG: StorageListNode<PositronConfig> =
            StorageListNode::new("positron/config");

        // Serialize the custom_config config so we can write it to our flash
        let custom_config = PositronConfig {
            up: 1,
            down: 22,
            strange: 333,
        };

        // TODO: The following pointer operations are a very cumbersome way
        // of serializing the node just so we can push it to the flash and read back...
        unsafe {
            POSITRON_CONFIG.inner.get().as_mut().unwrap().t =
                MaybeUninit::new(custom_config.clone());
            POSITRON_CONFIG.inner.get().as_mut().unwrap().header.counter = Some(Wrapping(0));
        }
        let nodeptr: *mut Node<PositronConfig> = POSITRON_CONFIG.inner.get();
        let nodenn: NonNull<Node<PositronConfig>> = unsafe { NonNull::new_unchecked(nodeptr) };
        let hdrnn: NonNull<NodeHeader> = nodenn.cast();

        let mut serde_buf = [0u8; 4096];
        let mut read_buf = [0u8; 4096];
        let used = serialize_node(hdrnn, &mut serde_buf).expect("Serializing node should not fail");

        // Now serialize again by calling encode() directly and verify both match
        let mut serialize_control = std::vec![];
        let len = len_with(&custom_config, &mut ());
        minicbor::encode(&custom_config, &mut serialize_control).unwrap();
        // The serialized node will contain some metadata, but the last `len` bytes must match
        assert_eq!(serialize_control[..len], serde_buf[used - len..used]);

        let worker_task = tokio::task::spawn(async move {
            let mut flash = Flash {
                flash: MockFlashBase::<10, 16, 256>::new(WriteCountCheck::OnceOnly, None, true),
                range: 0x0000..0x1000,
            };

            info!("Pushing to flash: {:?}", &serde_buf[..used]);
            let range = flash.range(); // get cloned range so we can borrow flash mutably in push()
            queue::push(
                &mut flash.flash(),
                range,
                &mut NoCache::new(),
                &serde_buf[..used],
                false,
            )
            .await
            .expect("pushing to flash should not fail here");

            for _ in 0..10 {
                warn!("Flash content: {}", flash.flash.print_items().await);
                // Reuse the serialization buf, no need to create a new one
                GLOBAL_LIST.process_reads(&mut flash, &mut read_buf).await;
                if let Err(e) = GLOBAL_LIST
                    .process_writes(&mut flash, &mut read_buf, &mut serde_buf)
                    .await
                {
                    error!("Error in process_writes: {}", e);
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        });

        // Obtain a handle for the config. It should match the custom_config.
        // This should _not_ error!
        let expecting_already_present = match POSITRON_CONFIG.attach(&GLOBAL_LIST).await {
            Ok(ch) => ch,
            Err(_) => panic!("Could not attach config to list"),
        };

        assert_eq!(
            custom_config,
            expecting_already_present.load().await.unwrap(),
            "Key should already be present"
        );

        expecting_already_present
            .write(&custom_config)
            .await
            .unwrap();

        sleep(Duration::from_millis(50)).await;

        // Assert that the counter is now 1
        assert_eq!(
            unsafe { expecting_already_present.inner.inner.get().as_ref() }
                .unwrap()
                .header
                .counter
                .unwrap(),
            Wrapping(1)
        );

        worker_task.await.unwrap();
    }
    #[test(tokio::test)]
    #[should_panic]
    async fn test_duplicate_key() {
        static GLOBAL_LIST: StorageList<CriticalSectionRawMutex> = StorageList::new();
        static POSITRON_CONFIG1: StorageListNode<PositronConfig> =
            StorageListNode::new("positron/config");
        static POSITRON_CONFIG2: StorageListNode<PositronConfig> =
            StorageListNode::new("positron/config");

        let mut flash = Flash {
            flash: MockFlashBase::<10, 16, 256>::new(WriteCountCheck::OnceOnly, None, true),
            range: 0x0000..0x1000,
        };
        // TODO: Figure out why miri tests with unaligned buffers and whether
        // this needs any fixing. For now just disable the alignment check in MockFlash
        flash.flash().alignment_check = false;

        let range = flash.range();
        sequential_storage::erase_all(&mut flash.flash(), range)
            .await
            .unwrap();

        info!("Spawn worker_task");
        let worker_task = tokio::task::spawn(async move {
            let mut serde_buf = std::vec![0u8; 4096];
            let mut read_buf = std::vec![0u8; 4096];

            for _ in 0..10 {
                GLOBAL_LIST.process_reads(&mut flash, &mut read_buf).await;
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                if let Err(e) = GLOBAL_LIST
                    .process_writes(&mut flash, &mut read_buf, &mut serde_buf)
                    .await
                {
                    error!("Error in process_writes: {}", e);
                }

                info!("NEW WRITES: {}", flash.flash().print_items().await);
            }
        });

        // Obtain a handle for the first config
        let _config_handle = match POSITRON_CONFIG1.attach(&GLOBAL_LIST).await {
            Ok(ch) => ch,
            Err(_) => panic!("Could not attach config 1 to list"),
        };

        // Obtain a handle for the second config. It has the same key as the first.
        // This must panic!
        let _expecting_panic = POSITRON_CONFIG2.attach(&GLOBAL_LIST).await;

        // Wait for the worker task to finish
        worker_task.await.unwrap();
    }
}
