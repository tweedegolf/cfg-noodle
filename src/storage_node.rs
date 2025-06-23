//! # Intrusive Linked List for Configuration Storage
//! This implements the [`StorageList`] and its [`StorageListNode`]s

use crate::{
    error::Error,
    logging::{MaybeDefmtFormat, debug, error},
    storage_list::StorageList,
};
use cordyceps::{Linked, list};
use core::{
    cell::UnsafeCell,
    fmt::Debug,
    marker::PhantomPinned,
    mem::MaybeUninit,
    ptr::{self, NonNull, addr_of, addr_of_mut},
    sync::atomic::{AtomicU8, Ordering},
};
use minicbor::{
    CborLen, Decode, Encode,
    encode::write::{Cursor, EndOfSlice},
    len_with,
};
use mutex_traits::ScopedRawMutex;

/// Represents the storage of a single `T`, linkable to a `StorageList`
///
/// Users will [`attach`](Self::attach) it to the [`StorageList`] to make it part of
/// the "connected configuration system", and receive a [`StorageListNodeHandle`].
pub struct StorageListNode<T: 'static + MaybeDefmtFormat> {
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
    T: 'static + MaybeDefmtFormat,
    R: ScopedRawMutex + 'static,
{
    /// `StorageList` to which this node has been attached
    list: &'static StorageList<R>,

    /// Store the StorageListNode for this handle
    inner: &'static StorageListNode<T>,
}

/// State of the [`StorageListNode<T>`].
///
/// This is used to determine how to unsafely interact with other pieces
/// of the `Node<T>`!
///
/// ## Safety
/// The State is stored in the nodes as an AtomicU8 so that after initial
/// hydration the configuration data stored in the node can be loaded without
/// locking the list mutex (i.e., without delay).
///
/// This is sound because atomics can be shared and the "valid" (i.e. loadable)
/// states are "latching" (the I/O worker NEVER modifies the content of node.t).
/// See <https://github.com/tweedegolf/cfg-noodle/issues/34>
///
/// ## State transition diagram
///
/// ```text
/// ┌─────────┐    ┌─────────────┐   ┌────────────────────┐
/// │ Initial │─┬─▶│ NonResident │──▶│  DefaultUnwritten  │─┐
/// └─────────┘ │  └─────────────┘   └────────────────────┘ │
///             │                                           ▼
///             │         ┌────────────────────┐     ┌────────────┐
///             └────────▶│ ValidNoWriteNeeded │────▶│ NeedsWrite │
///                       └────────────────────┘     └────────────┘
///                                  ▲                      │
///                                  └──────────────────────┘
/// ```
#[derive(Clone, Debug, PartialEq)]
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

impl State {
    /// Convert State to u8.
    pub const fn into_u8(self) -> u8 {
        match self {
            State::Initial => 0,
            State::NonResident => 1,
            State::DefaultUnwritten => 2,
            State::ValidNoWriteNeeded => 3,
            State::NeedsWrite => 4,
        }
    }
    /// Convert u8 to state.
    ///
    /// Panics if the u8 value does not have a matchin state as
    /// returned by [`Self::into_u8`].
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0 => State::Initial,
            1 => State::NonResident,
            2 => State::DefaultUnwritten,
            3 => State::ValidNoWriteNeeded,
            4 => State::NeedsWrite,
            _ => panic!("Invalid state value"),
        }
    }
}

/// This is the actual "linked list node" that is chained to the list
///
/// There is trickery afoot! The linked list is actually a linked list
/// of `NodeHeader`, and we are using `repr(c)` here to GUARANTEE that
/// a pointer to a ``Node<T>`` is the SAME VALUE as a pointer to that
/// node's `NodeHeader`. We will use this for type punning!
#[repr(C)]
pub(crate) struct Node<T> {
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
pub(crate) struct NodeHeader {
    /// The doubly linked list pointers
    links: list::Links<NodeHeader>,
    /// The "key" of our "key:value" store. Must be unique across the list.
    pub(crate) key: &'static str,
    /// The current state of the node. THIS IS SAFETY LOAD BEARING whether
    /// we can access the `T` in the `Node<T>`
    pub(crate) state: AtomicU8,
    /// Mark that this node has a handle attached to it. So if this is true, a new call
    /// to `attach` will fail. If this is false, a new handle can be obtained with attach.
    pub(crate) handle_attached: bool,
    /// This is the type-erased serialize/deserialize `VTable` that is
    /// unique to each `T`, and will be used by the storage worker
    /// to access the `t` indirectly for loading and storing.
    pub(crate) vtable: VTable,
}

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
pub(crate) struct VTable {
    pub(crate) serialize: SerFn,
    pub(crate) deserialize: DeserFn,
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

// ---- impl StorageListNode ----
impl<T> StorageListNode<T>
where
    T: 'static,
    T: Encode<()>,
    T: CborLen<()>,
    for<'a> T: Decode<'a, ()>,
    T: Clone,
    T: Default,
    T: MaybeDefmtFormat,
{
    /// Attaches node to a list and waits for hydration.
    /// If the value is not found in flash, a default value is used.
    ///
    /// # Error
    /// This function will return an [`Error::DuplicateKey`] if a node
    /// with the same key already exists in the list.
    ///
    /// # Example
    /// ```
    /// # async {
    ///   use cfg_noodle::{StorageList, StorageListNode};
    ///   use minicbor::*;
    ///   use mutex::raw_impls::cs::CriticalSectionRawMutex;
    ///
    ///   #[derive(Default, Debug, Encode, Decode, Clone, PartialEq, CborLen)]
    ///   struct MyStoredVar(#[n(0)] u8);
    ///
    ///   static MY_CONFIG: StorageListNode<MyStoredVar> = StorageListNode::new("config/myconfig");
    ///   static MY_LIST: StorageList<CriticalSectionRawMutex> = StorageList::new();
    ///
    ///   MY_CONFIG.attach(&MY_LIST).await;
    /// # };
    pub async fn attach<R>(
        &'static self,
        list: &'static StorageList<R>,
    ) -> Result<StorageListNodeHandle<T, R>, Error>
    where
        R: ScopedRawMutex + 'static,
    {
        self.attach_with_default(list, Default::default).await
    }
}

impl<T> StorageListNode<T>
where
    T: 'static,
    T: Encode<()>,
    T: CborLen<()>,
    for<'a> T: Decode<'a, ()>,
    T: Clone,
    T: MaybeDefmtFormat,
{
    /// Make a new StorageListNode, initially empty and unattached
    pub const fn new(path: &'static str) -> Self {
        Self {
            inner: UnsafeCell::new(Node {
                header: NodeHeader {
                    links: list::Links::new(),
                    key: path,
                    state: AtomicU8::new(State::Initial.into_u8()),
                    handle_attached: false,
                    vtable: VTable::for_ty::<T>(),
                },
                t: MaybeUninit::uninit(),
                _pin: PhantomPinned,
            }),
        }
    }

    /// Attaches node to a list and waits for hydration.
    /// If the value is not found in flash, use the default value provided by the closure `f`
    ///
    /// # Error
    /// This function will return an [`Error::DuplicateKey`] if a node
    /// with the same key already exists in the list.
    ///
    /// # Example
    /// ```
    /// # async {
    ///   use cfg_noodle::{StorageList, StorageListNode};
    ///   use minicbor::*;
    ///   use mutex::raw_impls::cs::CriticalSectionRawMutex;
    ///
    ///   #[derive(Debug, Encode, Decode, Clone, PartialEq, CborLen)]
    ///   struct MyStoredVar(#[n(0)] u8);
    ///
    ///   static MY_CONFIG: StorageListNode<MyStoredVar> = StorageListNode::new("config/myconfig");
    ///   static MY_LIST: StorageList<CriticalSectionRawMutex> = StorageList::new();
    ///
    ///   MY_CONFIG.attach_with_default(&MY_LIST, || MyStoredVar(123)).await;
    /// # };
    /// ```
    pub async fn attach_with_default<R, F: FnOnce() -> T>(
        &'static self,
        list: &'static StorageList<R>,
        f: F,
    ) -> Result<StorageListNodeHandle<T, R>, Error>
    where
        R: ScopedRawMutex + 'static,
    {
        debug!("Attaching new node");

        // Add a scope so that the Lock on the List is dropped

        {
            let mut list_inner = list.inner.lock().await;
            debug!("attach() got Lock on list");

            let nodeptr: *mut Node<T> = self.inner.get();
            let nodenn: NonNull<Node<T>> = unsafe { NonNull::new_unchecked(nodeptr) };
            // NOTE: We EXPLICITLY cast the outer Node<T> ptr, instead of using the header
            // pointer, so when we cast back later, the pointer has the correct provenance!
            // 
            // TODO SAFETY: making this `mut` while we only take `&self` instead of `&mut self`
            // is probably unsound. So I think we should be taking an `&mut self`. But passing static
            // as mut doesn't work for the caller. How do we solve this?
            let mut hdrnn: NonNull<NodeHeader> = nodenn.cast();

            // Check if the key already exists in the list.
            // This also prevents attaching to the list more than once.
            // SAFETY: We hold the lock, we are allowed to gain exclusive mut access
            // to the contents of the node.
            let key = unsafe { &nodenn.as_ref().header.key };
            match list_inner.find_node(key) {
                Some(node) if unsafe { node.as_ref().handle_attached } => {
                    error!("Key already in use and attached to handle: {:?}", key);
                    return Err(Error::DuplicateKey);
                }
                _ => {
                    // Mark node as handle attached and push it to the list
                    unsafe {
                        hdrnn.as_mut().handle_attached = true;
                    }
                    list_inner.list.push_front(hdrnn);
                }
            }
            // Let read task know we have work
            list.needs_read.wake();
            debug!("attach() release Lock on list");
        }

        debug!("Waiting for reading_done");

        // We know our node is now on the list. Create a ref to the header which
        // we can use to observe state changes.
        //
        // SAFETY: we can always create a shared ref to the header
        let nodeptr: *mut Node<T> = self.inner.get();
        let hdrref: &NodeHeader = unsafe { &*addr_of!((*nodeptr).header) };

        // Wait for the state to reach any non-Initial value. This handles races
        // where the I/O worker ALREADY loaded us between releasing the lock and
        // now (would require interrupts/threads/multicore), and handles cases
        // where we get a spurious wake BEFORE we've actually been loaded.
        let state = list
            .reading_done
            .wait_for_value(|| {
                // We do NOT hold the lock, use Acquire ordering.
                let state = State::from_u8(hdrref.state.load(Ordering::Acquire));
                if state != State::Initial {
                    Some(state)
                } else {
                    None
                }
            })
            .await
            .expect("waitqueue never closes");

        // Are we in a state with a valid T?
        match state {
            // This should never happen because `wait_for_value` explicitly does not yield
            // until we've reached the Non-Initial state.
            State::Initial => unreachable!("shouldn't observe this in attach"),
            // Not found in flash, init with default value.
            State::NonResident => {
                debug!(
                    "Node with key {:?} non resident. Init with default",
                    hdrref.key
                );
                // We are nonresident, we need to initialize
                //
                // SAFETY: We only create a mutref of body when we are in a state where we have
                // exclusive access, the queue will NEVER attempt to perceive the body when
                // we are in the NonResident state. Although we COULD safely just write ourselves
                // now, we still take the lock so that we do not "magically" become DefaultUnwritten
                // while the i/o worker is in the process of writing, which could cause us to be
                // marked as written even though have not been, e.g. we showed up as "not eligible"
                // when first checked in `write_to_flash`, but later in `process_writes` we WOULD
                // have been, and marked as a completed write.
                let _inner = list.inner.lock().await;
                let body: &mut MaybeUninit<T> = unsafe { &mut *addr_of_mut!((*nodeptr).t) };
                *body = MaybeUninit::new(f());
                // We do NOT hold the lock, use Release ordering.
                hdrref
                    .state
                    .store(State::DefaultUnwritten.into_u8(), Ordering::Release);
            }
            // This state is set by this function if the node is non resident
            State::DefaultUnwritten => unreachable!("shouldn't observe this in attach"),
            // This is the usual case: key found in flash and node hydrated
            State::ValidNoWriteNeeded => (),
            // NeedsWrite indicates that this handle has done a write, which is not possible until after attach has completed
            State::NeedsWrite => unreachable!("shouldn't observe this in attach"),
        }

        let _lock = list.inner.lock().await;
        debug!("attach() got Lock on list");

        let nodeptr: *mut Node<T> = self.inner.get();
        let mut nodenn: NonNull<Node<T>> = unsafe { NonNull::new_unchecked(nodeptr) };

        // SAFETY: We hold the lock, we are allowed to gain exclusive mut access
        // to the contents of the node.
        unsafe { nodenn.as_mut().header.handle_attached = true };

        Ok(StorageListNodeHandle { list, inner: self })
    }
}

impl<T: MaybeDefmtFormat, R: ScopedRawMutex> Drop for StorageListNodeHandle<T, R> {
    fn drop(&mut self) {
        // We require that `StorageListNode` is in a static so it should never actually be
        // possible to drop a StorageListNode.
        //
        // However, we want to allow the handle to be dropped and mark the node accordingly
        // so it can be attached later on.
        let nodeptr: *mut Node<T> = self.inner.inner.get();
        let mut nodenn: NonNull<Node<T>> = unsafe { NonNull::new_unchecked(nodeptr) };

        unsafe {
            nodenn.as_mut().header.handle_attached = false;
        }
    }
}

/// I think this is safe? If we can move data into the node, its
/// Sync-safety is guaranteed by the StorageList mutex.
unsafe impl<T> Sync for StorageListNode<T> where T: Send + 'static + MaybeDefmtFormat {}

// ---- impl StorageListNodeHandle ----

impl<T, R> StorageListNodeHandle<T, R>
where
    T: Clone + Send + 'static + MaybeDefmtFormat,
    R: ScopedRawMutex + 'static,
{
    /// Obtain the key of a node
    pub fn key(&self) -> &str {
        // The node holds an `&'static str`, so we can access it without locking the list
        let nodeptr: *mut Node<T> = self.inner.inner.get();
        let noderef = unsafe { &*nodeptr };
        noderef.header.key
    }

    /// This is a `load` function that copies out the data
    pub fn load(&self) -> T {
        // No need to lock the list because if the state is proper, we can always
        // read node.t. The I/O worker will never change node.t except at initial
        // hydration. Later, only write() will change node.t, but it requires &self.

        let nodeptr: *mut Node<T> = self.inner.inner.get();
        let noderef = unsafe { &mut *nodeptr };

        // We DON'T hold a lock to the list, so use Acquire for load
        let state = State::from_u8(noderef.header.state.load(Ordering::Acquire));

        // is T valid?
        match state {
            // All these states implicate that process_reads/_writes has not finished
            // but they hold a lock on the list, so we should never reach this piece
            // of code while holding a lock ourselves
            //
            // TODO @James: Again, if we are in an invalid state, should we rather panic?
            State::Initial | State::NonResident => {
                unreachable!("This state should not be observed")
            }
            // Handle all states here explicitly to avoid bugs with a catch-all
            State::DefaultUnwritten | State::ValidNoWriteNeeded | State::NeedsWrite => {}
        }
        // yes!
        unsafe { noderef.t.assume_init_ref().clone() }
    }

    /// Write data to the buffer, and mark the buffer as "needs to be flushed".
    pub async fn write(&self, t: &T) -> Result<(), Error> {
        let _inner = self.list.inner.lock().await;

        let nodeptr: *mut Node<T> = self.inner.inner.get();
        let noderef = unsafe { &mut *nodeptr };

        let state = State::from_u8(noderef.header.state.load(Ordering::Relaxed));

        match state {
            // `Initial` and `NonResident` can not occur for the same reason as
            // outlined in load(). OldData should have been replaced with a Default value.
            State::Initial | State::NonResident => {
                debug!("write() on invalid state: {:?}", state);
                return Err(Error::InvalidState(noderef.header.key, state));
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

        // We hold a lock to the list, so use Relaxed
        noderef
            .header
            .state
            .store(State::NeedsWrite.into_u8(), Ordering::Relaxed);

        // Let the write task know we have work
        self.list.needs_write.wake();

        // old T is dropped
        Ok(())
    }
}

/// Impl Debug to allow for using unwrap in tests.
impl<T, R> Debug for StorageListNodeHandle<T, R>
where
    T: 'static + MaybeDefmtFormat,
    R: ScopedRawMutex + 'static,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StorageListNodeHandle")
            .field("list", &"StorageList<R>")
            .field("inner", &"StorageListNode<T>")
            .finish()
    }
}

// ---- impl State ----
// ---- impl Node ----
// ---- impl NodeHeader ----

/// This is cordyceps' intrusive linked list trait. It's mostly how you
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

// ---- impl VTable ----

impl VTable {
    /// This is a tricky helper method that automatically builds a vtable by
    /// monomorphizing generic functions into non-generic function pointers.
    const fn for_ty<T>() -> Self
    where
        T: 'static,
        T: Encode<()>,
        T: CborLen<()>,
        T: MaybeDefmtFormat,
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
///
/// `node.t` MUST be in a valid state before calling this function, AND
/// the mutex must be held the whole time we are here, because we are reading
/// from `t`!
fn serialize<T>(node: NonNull<Node<()>>, buf: &mut [u8]) -> Result<usize, ()>
where
    T: 'static,
    T: Encode<()>,
    T: minicbor::CborLen<()>,
    T: MaybeDefmtFormat,
{
    let node: NonNull<Node<T>> = node.cast();
    let noderef: &Node<T> = unsafe { node.as_ref() };
    let tref: &MaybeUninit<T> = &noderef.t;
    let tref: &T = unsafe { tref.assume_init_ref() };

    let mut cursor = Cursor::new(buf);
    let res: Result<(), minicbor::encode::Error<EndOfSlice>> = minicbor::encode(tref, &mut cursor);

    debug!(
        "Finished serializing: {} bytes written, len_with(): {}, content: {:?}",
        cursor.position(),
        len_with(tref, &mut ()),
        &cursor.get_ref()[..cursor.position()],
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
/// type. Deserialization only ever occurs once
///
/// # Arguments
///
/// * `node` - A type-erased pointer to a `Node<()>` that will be cast to `Node<T>`
/// * `buf` - A byte slice containing CBOR-encoded data to deserialize into type `T`
///
/// # Returns
/// * `Ok(usize)` - The number of bytes consumed from the buffer during deserialization
/// * `Err(())` - If CBOR deserialization fails
///
/// # Safety
/// The caller must ensure that:
/// - The storage list mutex is held during the entire operation to prevent concurrent access
/// - The `node` pointer is valid and points to a properly initialized `Node<T>` in the
///   `State::Initial` state
fn deserialize<T>(node: NonNull<Node<()>>, buf: &[u8]) -> Result<usize, ()>
where
    T: 'static,
    T: CborLen<()>,
    for<'a> T: Decode<'a, ()>,
{
    let node: NonNull<Node<T>> = node.cast();

    // SAFETY: We can always make a shared pointer to the header
    let hdrref: &NodeHeader = unsafe { &*addr_of!((*node.as_ptr()).header) };

    let res = minicbor::decode::<T>(buf);

    let t = match res {
        Ok(t) => t,
        // TODO: Should we return the actual error here or not?
        Err(_) => return Err(()),
    };

    let len = len_with(&t, &mut ());

    // We require the caller to hold a lock to the list, so use Relaxed
    debug_assert!(matches!(
        State::from_u8(hdrref.state.load(Ordering::Relaxed)),
        State::Initial
    ));
    // SAFETY: We only call deser with the lock held, and in the Initial state
    let body: &mut MaybeUninit<T> = unsafe { &mut *addr_of_mut!((*node.as_ptr()).t) };
    *body = MaybeUninit::new(t);

    Ok(len)
}
