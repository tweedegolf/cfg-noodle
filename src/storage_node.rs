//! # Intrusive Linked List for Configuration Storage
//! This implements the [`StorageList`] and its [`StorageListNode`]s

use crate::{
    error::Error,
    logging::{debug, error},
    storage_list::StorageList,
};
use cordyceps::{Linked, list};
use core::{
    cell::UnsafeCell,
    fmt::Debug,
    marker::PhantomPinned,
    mem::MaybeUninit,
    pin::Pin,
    ptr::{self, NonNull},
};
use minicbor::{
    CborLen, Decode, Encode,
    encode::write::{Cursor, EndOfSlice},
    len_with,
};
use mutex::ScopedRawMutex;

/// Represents the storage of a single `T`, linkable to a `StorageList`
///
/// Users will [`attach`](Self::attach) it to the [`StorageList`] to make it part of
/// the "connected configuration system", and receive a [`StorageListNodeHandle`].
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

/// State of the [`StorageListNode<T>`].
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
///             └────────▶│ ValidNoWriteNeeded │────▶│ NeedsWrite │
///                       └────────────────────┘     └────────────┘
///                                  ▲                      │
///                                  └──────────────────────┘
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
    pub(crate) state: State,
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
    /// # Error
    /// This function will return an [`Error::DuplicateKey`] if a node
    /// with the same key already exists in the list.
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
            if inner.find_node(key).is_some() {
                error!("Key already in use: {:?}", key);
                return Err(Error::DuplicateKey);
            } else {
                inner.list.push_front(hdrnn);
            }
            // Let read task know we have work
            list.needs_read.wake();
            debug!("attach() release Lock on list");
        }

        // Wait until we have a value, or we know it is non-resident
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
            // This should never happen because it means we attached in
            // the middle of the read process, which implies that we AND
            // the read process have the list locked simultaneously.
            State::Initial => unreachable!("shouldn't observe this in attach"),
            // Not found in flash, init with default value.
            State::NonResident => {
                debug!(
                    "Node with key {:?} non resident. Init with default",
                    noderef.header.key
                );
                // We are nonresident, we need to initialize
                noderef.t = MaybeUninit::new(T::default());
                noderef.header.state = State::DefaultUnwritten;
            }
            // This state is set by this function if the node is non resident
            State::DefaultUnwritten => unreachable!("shouldn't observe this in attach"),
            // This is the usual case: key found in flash and node hydrated
            State::ValidNoWriteNeeded => (),
            // NeedsWrite indicates that this handle has done a write, which is not possible until after attach has completed
            State::NeedsWrite => unreachable!("shouldn't observe this in attach"),
        }

        Ok(StorageListNodeHandle { list, inner: self })
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

/// I think this is safe? If we can move data into the node, its
/// Sync-safety is guaranteed by the StorageList mutex.
unsafe impl<T> Sync for StorageListNode<T> where T: Send + 'static {}

// ---- impl StorageListNodeHandle ----

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

// ---- impl State ----
// ---- impl Node ----
// ---- impl NodeHeader ----

impl NodeHeader {
    pub(crate) unsafe fn set_state(self: Pin<&mut Self>, state: State) {
        unsafe {
            self.get_unchecked_mut().state = state;
        }
    }
}

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
///
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
    debug_assert!(matches!(noderef.header.state, State::Initial));
    noderef.t = MaybeUninit::new(t);

    Ok(len)
}
