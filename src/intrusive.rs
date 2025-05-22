//! This is a rough shape of an intrusive approach to storing configuration data.
//!
//! Some things are still "fake", for the sake of simplicity, but the goal is
//! to brain-dump the conceptual shape of the problem, for further evaluation
//! and development.
//!
//! It is (so far) poorly tested, and definitely could use tests, particularly
//! in concert with `miri`, to ensure that we are not doing any unsound things
//! with pointers.
//!
//! We will also need to make some modifications when we know "how" we want
//! to store data, and to make the current design no-std friendly.

use crate::error::Error;
use cordyceps::{
    Linked, List,
    list::{self, IterRaw},
};
use core::{
    cell::UnsafeCell,
    marker::PhantomPinned,
    mem::MaybeUninit,
    num::Wrapping,
    ptr::{self, NonNull},
};
use embedded_storage_async::nor_flash::MultiwriteNorFlash;
use log::{debug, error, info};
use maitake_sync::{Mutex, WaitQueue};
use minicbor::{
    CborLen, Decode, Encode,
    encode::write::{Cursor, EndOfSlice},
    len_with,
};
use mutex::{ConstInit, ScopedRawMutex};
use sequential_storage::{cache::NoCache, queue};
use std::fmt::Debug;

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
    /// This uses a `BlockingMutex`, a mutex that wraps access in a
    /// non-async closure. The mutex contains a `List`, which is a
    /// doubly-linked intrusive list. Unlike my other Pin-based research,
    /// in this impl we ONLY support `'static` nodes, which means we
    /// could MAYBE switch to an async mutex instead of a blocking mutex,
    /// because we don't need load-bearing-blocking-drop impls that unchain
    /// nodes from the list on drop (necessary when you are storing in Pin),
    /// which might make async interactions with the flash driver easier.
    ///
    /// This is a thing to verify for soundness before/after changing to
    /// an async mutex! Also: We almost CERTAINLY do NOT want to use
    /// embassy's async mutex - it only stores a single waker, which means
    /// if we have contention, we will have "waker churn" problems. You
    /// probably want to use `maitake-sync::Mutex`, which has the same
    /// generic lock interface, and uses a `WaitQueue`, which is an intrusive
    /// waker list, allowing for multiple wakers.
    ///
    /// See `impl Drop for StorageListNode` for more potential issues with an
    /// async mutex.
    ///
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
    // TODO: We probably want to put one or two `WaitQueue`s here, so
    // `StorageListNode`s can fire off an "I need to be hydrated" wake, and one
    // so `StorageListNode`s can fire off an "I have a pending write" wake.
    //
    // These would allow the storage worker task to be a bit more intelligent
    // or reactive to checking for "I have something to do", instead of having
    // to regularly poll.
    //
    // needs_read: WaitQueue,
    // needs_write: WaitQueue,
    reading_done: WaitQueue,
    writing_done: WaitQueue,
}

/// StorageListNode represents the storage of a single `T`, linkable to a `StorageList`
///
/// "end users" will [`attach`](Self::attach) it to the [`StorageList`] to make it part of
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
/// of `NodeHeader`, and we are using `repr(c)`` here to GUARANTEE that
/// a pointer to a `Node<T>` is the SAME VALUE as a pointer to that
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
    ///  and the counter is not yet initialized.
    counter: Option<Wrapping<u8>>,
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
///
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
    /// The value has been initialized using a default value (NOT from flash).
    ///
    /// TODO: Decide what our policy is for this: SHOULD we write default values
    /// back to flash, or keep them out of flash until there has been an explict
    /// change to the default value?
    /// If we don't flash it, we reduce wear on the flash and save some time.
    /// However, if the `default()` value changes (e.g., firmware update) this
    /// gives a different result.
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
pub struct Flash<T: MultiwriteNorFlash> {
    flash: T,
    range: core::ops::Range<u32>,
}

impl<T: MultiwriteNorFlash> Flash<T> {
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
    pub async fn process_reads(
        &'static self,
        flash: &mut Flash<impl MultiwriteNorFlash>,
        buf: &mut [u8],
    ) {
        info!("Start process_reads, buffer has len {}", buf.len());
        // Lock the list, remember, if we're touching nodes, we need to have the list
        // locked the entire time!
        let mut ls = self.list.lock().await;
        debug!("process_reads locked list");

        // Create a `QueueIterator` without caching
        let mut cache = NoCache::new();
        let mut queue_iter = queue::iter(&mut flash.flash, flash.range.clone(), &mut cache)
            .await
            .unwrap();
        while let Some(item) = queue_iter.next(buf).await.unwrap() {
            let key: &[u8; KEY_LEN] = extract_key(&item).expect("Invalid item: no key");
            let _counter: u8 = extract_counter(&item).expect("Invalid item: no counter"); // TODO handle counter
            let payload: &[u8] = extract_payload(&item).expect("Invalid item: no payload");

            debug!(
                "Extracted metadata: key {:?}, counter: {:?}, payload: {:?}",
                key, _counter, payload
            );
            // Check for the write_confirm key
            if is_write_confirm(&item) {
                info!("Found write_confirm");
                // write_confirm_found = true;
                // TODO: what next? And what if we never run into this?
            } else if let Some(node_header) = find_node(&mut ls, key) {
                // SAFETY: We hold the lock, we are allowed to gain exclusive mut access
                // to the contents of the node. We COPY OUT the vtable and the &'static str
                // key, for later use, which is important because we might throw away our
                // `node` ptr shortly.
                let vtable = {
                    let node_header = unsafe { node_header.as_ref() };
                    match node_header.state {
                        State::Initial => Some(node_header.vtable),
                        State::NonResident => None,
                        State::DefaultUnwritten => None,
                        State::ValidNoWriteNeeded => None, // TODO: Handle the case where this comes up again with a different counter value
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

                    // TODO: Can this happen before the `if let Some(val)` so we don't repeat it in the `else`?
                    // SAFETY: We can re-magic a reference to the header, because the NonNull<Node<()>>
                    // does not have a live reference anymore
                    let hdrmut = unsafe { hdrptr.as_mut() };

                    if res.is_ok() {
                        // If it went okay, let the node know that it has been hydrated with data
                        // from the flash
                        hdrmut.state = State::ValidNoWriteNeeded;
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
                    //
                    // TODO: if we read from the external flash in "chunks", e.g. one page at a time,
                    // we MIGHT not want to mark this fully "NonResident" yet, and wait until we have
                    // processed ALL chunks, and THEN mark any `Initial` nodes as `NonResident`.
                    // let hdrmut = unsafe { hdrptr.as_mut() };
                    // hdrmut.state = State::NonResident;
                    // self.reading_done.wake_all();
                }
            }
        }
        // TODO: handle write_confirm_found == false

        // Set nodes in initial states to non resident
        for node_header in ls.iter_raw() {
            // Make a node pointer from a header pointer. This *consumes* the `Pin<&mut NodeHeader>`, meaning
            // we are free to later re-invent other mutable ptrs/refs, AS LONG AS we still treat the data
            // as pinned.
            let mut hdrptr: NonNull<NodeHeader> = node_header;

            // SAFETY: We can re-magic a reference to the header, because the NonNull<Node<()>>
            // does not have a live reference anymore
            let hdrmut = unsafe { hdrptr.as_mut() };

            if let State::Initial = hdrmut.state {
                hdrmut.state = State::NonResident
            }
        }
        debug!("Reading done. Waking all.");
        self.reading_done.wake_all();
    }

    /// Process any nodes that are requesting to WRITE to flash data
    ///
    /// This method:
    ///
    /// 1. Locks the mutex
    /// 2. Iterates through each node currently linked in the list
    /// 3. For Each node that says "I have made changes", it will try to hand
    ///    control over to the node to serialize to a scratch buffer
    /// 4. On success, it will mark the node as no longer having changes
    ///
    /// This function might change significantly depending on how our actual flash
    /// writes work, where we might have a temporary "we've serialized the data already,
    /// but we don't know if the flash write succeeded yet".
    pub async fn process_writes(
        &'static self,
        flash: &mut Flash<impl MultiwriteNorFlash>,
    ) -> Result<(), Error> {
        debug!("Start process_writes");

        // Lock the list, remember, if we're touching nodes, we need to have the list
        // locked the entire time!
        let mut ls = self.list.lock().await;
        debug!("process_writes locked list");

        // Check if any node in the list needs writing
        let mut needs_writing = false;
        // TODO: Should we go over all nodes or just start writing once we hit the first node that needs writing?
        for node in ls.iter_raw() {
            let node = unsafe { node.as_ref() };
            match node.state {
                // If no write is needed, we obviously won't write.
                State::ValidNoWriteNeeded => (),
                // If the node hasn't been written to flash yet and we initialized it
                // with a Default, we now write it to flash.
                State::DefaultUnwritten | State::NeedsWrite => needs_writing = true,
                // TODO: Handle this differently?
                // Neither of these cases should appear on a list that has been processed properly
                State::Initial | State::NonResident | State::WriteStarted => {
                    return Err(Error::InvalidState(node.key));
                }
            }
        }

        // If the list is unchanged, there is no need to write it to flash!
        if !needs_writing {
            info!("List does not need writing. Exiting.");
            return Ok(());
        }

        let mut iter = StaticRawIter {
            iter: ls.iter_raw(),
        };

        while let Some(hdrptr) = iter.next() {
            // Todo: use a provided scratch buffer
            let mut buf = [0u8; 1024];
            // Attempt to serialize
            let res = serialize_node(hdrptr.ptr, &mut buf);

            if let Ok(used) = res {
                // Write to flash
                // TODO: THIS DOES NOT WORK! hdrptr is !Send and crossing the await is not allowed.
                queue::push(
                    &mut flash.flash,
                    flash.range.clone(),
                    &mut NoCache::new(),
                    &buf[..used],
                    false,
                )
                .await
                .unwrap();
            } else {
                // I'm honestly not sure what we should do if serialization failed.
                // If it was a simple "out of space" because we have a batch of writes
                // already, we might want to try again later. If it failed for some
                // other reason, or it fails even with an empty page (e.g. serializes
                // to more than 1k or 4k or something), there's really not much to be
                // done, other than log.
                panic!("why did ser fail");
            }
        }

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
    T: Debug, // TODO: Remove all those Debug trait bounds? Just added for debuggin
              //R: ScopedRawMutex + 'static,
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
    // If the value is not found in flash, a default value is used
    pub async fn attach<R>(
        &'static self,
        list: &'static StorageList<R>,
    ) -> Result<StorageListNodeHandle<T, R>, Error>
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
            // TODO: Check safety of this!
            let key = unsafe { &nodenn.as_ref().header.key };
            if find_node(&mut ls, key).is_some() {
                error!("Key already in use: {:?}", key);
                return Err(Error::DuplicateKey);
            } else {
                ls.push_front(hdrnn);
            }
            debug!("attach() release Lock on list")
        }

        // now spin until we have a value, or we know it is non-resident
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
                State::Initial => continue,
                State::NonResident => {
                    // We are nonresident, we need to initialize
                    noderef.t = MaybeUninit::new(T::default());
                    noderef.header.state = State::DefaultUnwritten;
                    break;
                }
                State::DefaultUnwritten => todo!("shouldn't observe this in attach"),
                State::ValidNoWriteNeeded => break,
                State::NeedsWrite => todo!("shouldn't observe this in attach"),
                State::WriteStarted => todo!("shouldn't observe this in attach"),
            }
        }

        Ok(StorageListNodeHandle { list, inner: self })
    }
}

/// Key length in bytes
pub const KEY_LEN: usize = 4;

/// Find a `NodeHeader` with the given `key`.
///
/// If `key` is not in the `List`, this function returns `None`.
fn find_node(list: &mut List<NodeHeader>, key: &[u8; KEY_LEN]) -> Option<NonNull<NodeHeader>> {
    // TODO: Check if the safety requirements are upheld!
    // It seems like we should not use `iter` for the same reason we use `raw_iter` instead in read/write
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
fn extract_counter(item: &[u8]) -> Result<u8, Error> {
    item.get(KEY_LEN).ok_or(Error::Deserialization).copied()
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

/// Serialize a list node into `buf`.
///
/// Returns the number of bytes written to `buf` or an error.
pub fn serialize_node(headerptr: NonNull<NodeHeader>, buf: &mut [u8]) -> Result<usize, ()> {
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
    (vtable.serialize)(nodeptr, &mut buf[KEY_LEN + 1..]).map(|len| len + KEY_LEN + 1)
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
    /// TODO: we probably want a nicer handler that errors out, or awaits
    /// if the data isn't loaded yet.
    ///
    /// Or we could have `attach` return some kind of `StorageListNodeHandle` that
    /// denotes "yes the StorageList has been loaded at least once", and guarantees
    /// that this load won't fail. This might also help with the "only attach once"
    /// guarantee! Oh also it might solve the `AtomicPtr` thing, because only the
    /// `StorageListNodeHandle` would need to hold the `&'static StorageList`, NOT the
    /// `StorageListNode`. I think we should definitely do this!
    ///
    /// Note that we *copy out*, instead of returning a ref, because we MUST hold
    /// the guard as long as &T is live. For the blocking mutex, that's all kinds
    /// of problematic, but even if we switch to an async mutex, holding a `MutexGuard<T>`
    /// or similiar **will inhibit all other flash operations**, which is bad!
    ///
    /// So just hold the lock and copy out.
    pub async fn load(&self) -> T {
        // Lock the list if we look at ourself
        let _lock = self.list.list.lock().await;

        let nodeptr: *mut Node<T> = self.inner.inner.get();
        let noderef = unsafe { &mut *nodeptr };
        // is T valid?
        match noderef.header.state {
            State::Initial => todo!(),
            State::NonResident => todo!(),
            State::DefaultUnwritten => {}
            State::ValidNoWriteNeeded => {}
            State::NeedsWrite => {}
            State::WriteStarted => {}
        }
        // yes!
        unsafe { noderef.t.assume_init_ref().clone() }
    }

    /// Write data to the buffer, and mark the buffer as "needs to be flushed".
    pub async fn write(&self, t: &T) {
        let _lock = self.list.list.lock().await;

        let nodeptr: *mut Node<T> = self.inner.inner.get();
        let noderef = unsafe { &mut *nodeptr };
        // determine state
        // TODO: allow writes from uninit state?
        match noderef.header.state {
            State::Initial => todo!("shouldn't observe"),
            State::NonResident => todo!("shouldn't observe"),
            State::DefaultUnwritten => {}
            State::ValidNoWriteNeeded => {}
            State::NeedsWrite => {}
            State::WriteStarted => todo!("how to handle?"),
        }
        // We do a swap instead of a write here, to ensure that we
        // call `drop` on the "old" contents of the buffer.
        let mut t: T = t.clone();
        unsafe {
            let mutref: &mut T = noderef.t.assume_init_mut();
            core::mem::swap(&mut t, mutref);
        }
        noderef.header.state = State::NeedsWrite;
        // old T is dropped
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

    info!(
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
        Err(_) => return Err(()),
    };

    let len = len_with(&t, &mut ());
    println!("Finished deserializing, len_with(): {}", len);

    noderef.t = MaybeUninit::new(t);

    Ok(len)
}

struct StaticRawIter<'a> {
    iter: IterRaw<'a, NodeHeader>,
}
unsafe impl<'a> Send for StaticRawIter<'a> {}

struct SendPtr {
    ptr: NonNull<NodeHeader>,
}
unsafe impl Send for SendPtr {}

impl<'a> StaticRawIter<'a> {
    fn next(&mut self) -> Option<SendPtr> {
        self.iter.next().map(|ptr| SendPtr { ptr })
    }
}

#[cfg(test)]
mod test {
    use std::time::Duration;

    use super::*;
    use log::warn;
    use minicbor::CborLen;
    use test_log::test;
    // use mock_flash::MockFlashBase;
    use mutex::raw_impls::cs::CriticalSectionRawMutex;
    use sequential_storage::mock_flash::WriteCountCheck;
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
            flash: sequential_storage::mock_flash::MockFlashBase::<10, 16, 256>::new(
                WriteCountCheck::OnceOnly,
                None,
                true,
            ),
            range: 0x0000..0x1000,
        };
        let range = flash.range();
        sequential_storage::erase_all(&mut flash.flash(), range)
            .await
            .unwrap();

        info!("Spawn worker_task");
        let worker_task = tokio::task::spawn(async move {
            let mut buf = vec![0u8; 4096];

            for _ in 0..10 {
                GLOBAL_LIST.process_reads(&mut flash, &mut buf).await;
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                if let Err(e) = GLOBAL_LIST.process_writes(&mut flash).await {
                    error!("Error in process_writes: {}", e);
                }

                info!("NEW WRITES: {:?}", flash.flash().print_items().await);
            }
        });

        // Obtain a handle for the first config
        let config_handle = POSITRON_CONFIG1
            .attach(&GLOBAL_LIST)
            .await
            .expect("This should not error");

        // Obtain another handle for the same config.
        // This should error because we try adding the same key.
        let expecting_error = POSITRON_CONFIG1.attach(&GLOBAL_LIST).await;
        assert!(
            expecting_error.is_err(),
            "Second call to attach should fail"
        );

        // Obtain a handle for the second config. This should _not_ error!
        let _expecting_no_error = POSITRON_CONFIG2
            .attach(&GLOBAL_LIST)
            .await
            .expect("This should not error!");

        // Load data for thje first handle
        let data: PositronConfig = config_handle.load().await;
        info!("T3 Got {data:?}");

        // Write a new config to first handle
        let new_config = PositronConfig {
            up: 15,
            down: 25,
            strange: 108,
        };
        config_handle.write(&new_config).await;

        // Give the worker_task some time to process the write
        sleep(Duration::from_millis(100)).await;

        // Assert that the loaded value equals the written value
        assert_eq!(config_handle.load().await, new_config);

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
        }
        let nodeptr: *mut Node<PositronConfig> = POSITRON_CONFIG.inner.get();
        let nodenn: NonNull<Node<PositronConfig>> = unsafe { NonNull::new_unchecked(nodeptr) };
        let hdrnn: NonNull<NodeHeader> = nodenn.cast();

        let mut serialization_buf = [0u8; 4096];
        let used = serialize_node(hdrnn, &mut serialization_buf)
            .expect("Serializing node should not fail");

        // Now serialize again by calling encode() directly and verify both match
        let mut serialize_control = vec![];
        let len = len_with(&custom_config, &mut ());
        minicbor::encode(&custom_config, &mut serialize_control).unwrap();
        // The serialized node will contain some metadata, but the last `len` bytes must match
        assert_eq!(
            serialize_control[..len],
            serialization_buf[used - len..used]
        );

        let worker_task = tokio::task::spawn(async move {
            let mut flash = Flash {
                flash: sequential_storage::mock_flash::MockFlashBase::<10, 16, 256>::new(
                    WriteCountCheck::OnceOnly,
                    None,
                    true,
                ),
                range: 0x0000..0x1000,
            };

            info!("Pushing to flash: {:?}", &serialization_buf[..used]);
            let range = flash.range(); // get cloned range so we can borrow flash mutably in push()
            queue::push(
                &mut flash.flash(),
                range,
                &mut NoCache::new(),
                &mut serialization_buf[..used],
                false,
            )
            .await
            .expect("pushing to flash should not fail here");

            warn!("Flash content: {}", flash.flash.print_items().await);

            for _ in 0..2 {
                // Reuse the serialization buf, no need to create a new one
                GLOBAL_LIST
                    .process_reads(&mut flash, &mut serialization_buf)
                    .await;
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        });

        // Obtain a handle for the config. It should match the custom_config.
        // This should _not_ error!
        let expecting_already_present = POSITRON_CONFIG
            .attach(&GLOBAL_LIST)
            .await
            .expect("This should not error!");

        assert_eq!(
            custom_config,
            expecting_already_present.load().await,
            "Key should already be present"
        );

        worker_task.await.unwrap();
    }
}
