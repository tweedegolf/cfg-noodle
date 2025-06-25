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
    marker::{PhantomData, PhantomPinned},
    mem::MaybeUninit,
    ptr::{self, NonNull, addr_of, addr_of_mut, null_mut},
    sync::atomic::{AtomicPtr, AtomicU8, Ordering},
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
    /// A field that stores the list we have been taken for. This is a null pointer
    /// on initial creation.
    ///
    /// The UPPERMOST bits (except the lowest) is used to store a pointer to the list
    /// that this node is attached to. The LOWERMOST bit is used to track whether
    /// a handle for this node is currently live.
    /// See [`attach_with_default`](Self::attach_with_default) for further explanation.
    taken_for_list: AtomicPtr<()>,
}

/// Handle for a `StorageListNode` that has been loaded and thus contains valid data
///
/// "end users" will interact with the `StorageListNodeHandle` to retrieve stored
/// configuration and store changes and store changes to the configuration they care about.
pub struct StorageListNodeHandle<T, R>
where
    T: 'static + MaybeDefmtFormat,
    R: ScopedRawMutex + 'static,
{
    /// We PRETEND we have a `&'static StorageList<R>`, it's actually
    /// stored in the inner->taken_for_list. It is important that we
    /// retain the `R` generic so that we can correctly interact with
    /// the pointer to the list, which has been erased in `taken_for_list`.
    list_ty: PhantomData<&'static StorageList<R>>,

    /// Store the StorageListNode for this handle
    inner: &'static StorageListNode<T>,
}

/// State of the [`StorageListNode<T>`]
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
///             ─ ─ ─ ─ (6)
///            │
///            ▼
///       ┌─────────┐   (1)    ┌─────────────┐
///       │ Initial │─────────▶│ NonResident │
///       └─────────┘          └─────────────┘
///            │(2)                   │(3)
///            ▼                      ▼
/// ┌────────────────────┐ (4) ┌─────────────┐
/// │ ValidNoWriteNeeded │◀────│ NeedsWrite  │
/// └────────────────────┘     └─────────────┘
///            │           (5)        ▲
///            └──────────────────────┘
/// ```
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum State {
    /// The node has been created, but never "hydrated" with data from the
    /// flash. In this state, we are waiting to be notified whether we can
    /// be filled with data from flash, or whether we will need to initialize
    /// using a default value or something.
    ///
    /// In this state, `t` is NOT valid, and is uninitialized. It MUST NOT be
    /// read.
    Initial,
    /// We attempted to load from flash, but as far as we know, the data does
    /// not exist in flash. The owner of the Node will need to initialize
    /// this data, usually in the wait loop of `attach`.
    ///
    /// In this state, `t` is NOT valid, and is uninitialized. It MUST NOT be
    /// read.
    NonResident,
    /// The value has been initialized using a value from flash. No writes are
    /// pending.
    ///
    /// In this state, `t` IS valid, and may be read at any time using the handle.
    ValidNoWriteNeeded,
    /// The value has been written, but these changes have NOT been flushed back
    /// to the flash. This includes the case where the node has "written back"
    /// a user-defined default value.
    ///
    /// In this state, `t` IS valid, and may be read at any time using the handle.
    NeedsWrite,
}

impl State {
    /// Convert State to u8.
    pub const fn into_u8(self) -> u8 {
        match self {
            State::Initial => 0,
            State::NonResident => 1,
            State::ValidNoWriteNeeded => 2,
            State::NeedsWrite => 3,
        }
    }
    /// Convert u8 to a [`State`].
    ///
    /// Panics if the u8 value does not have a matching state as
    /// returned by [`Self::into_u8`].
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0 => State::Initial,
            1 => State::NonResident,
            2 => State::ValidNoWriteNeeded,
            3 => State::NeedsWrite,
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
    t: MaybeUninit<T>,
}

/// The non-typed parts of a [`Node<T>`]
///
/// The `NodeHeader` serves as the actual type that is linked together in
/// the linked list, and is the primary interface the worker task will
/// use. It MUST be the first field of a `Node<T>`, so it is safe to
/// type-pun a `NodeHeader` ptr into a `Node<T>` ptr and back.
pub(crate) struct NodeHeader {
    /// The doubly linked list pointers
    links: list::Links<NodeHeader>,
    /// The "key" of our "key:value" store. Must be unique across the list.
    pub(crate) key: &'static str,
    /// The current state of the node. THIS IS SAFETY LOAD BEARING whether
    /// we can access the `T` in the `Node<T>`.
    pub(crate) state: AtomicU8,
    /// This is the type-erased serialize/deserialize `VTable` that is
    /// unique to each `T`, and will be used by the worker task
    /// to access the `t` indirectly for loading and storing.
    pub(crate) vtable: VTable,
}

/// Function table for type-erased serialization and deserialization operations
///
/// The `VTable` contains function pointers that allow the storage system to serialize
/// and deserialize nodes without knowing their concrete types at compile time. Each
/// node type `T` gets its own `VTable` instance created via [`VTable::for_ty`], which
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
type SerFn = unsafe fn(NonNull<Node<()>>, &mut [u8]) -> Result<usize, ()>;

/// A function where the type-erased node pointer goes in, and a `T` is
/// deserialized from the buffer.
///
/// If successful, returns the number of bytes read from the buffer.
type DeserFn = unsafe fn(NonNull<Node<()>>, &[u8]) -> Result<usize, ()>;

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
    /// Attaches the `StorageListNode` to a `StorageList` and waits for hydration.
    ///
    /// This function is a wrapper around [`attach_with_default`](Self::attach_with_default) using
    /// `T`'s `Default::default()` value. See [`attach_with_default`](Self::attach_with_default)
    /// for details and possbile errors.
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
    /// Make a new [`StorageListNode`], initially empty and unattached
    pub const fn new(path: &'static str) -> Self {
        Self {
            inner: UnsafeCell::new(Node {
                header: NodeHeader {
                    links: list::Links::new(),
                    key: path,
                    state: AtomicU8::new(State::Initial.into_u8()),
                    vtable: VTable::for_ty::<T>(),
                },
                t: MaybeUninit::uninit(),
                _pin: PhantomPinned,
            }),
            taken_for_list: AtomicPtr::new(null_mut()),
        }
    }

    /// Attaches the [`StorageListNode`] to a [`StorageList`] and waits for hydration.
    /// If the value is not found in flash, use the default value provided by the closure `f`.
    ///
    /// After some safety checks, this function will notify the worker tasks that it has added
    /// a node to the list and needs hydration from flash.
    /// When the worker task is done and no data for this node's key has been found in flash,
    /// it will initialize with the provided default.
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
        let already_attached;

        // Add a scope so that the list mutex is unlocked afterwards
        {
            let mut inner = list.inner.lock().await;
            debug!("attach() got Lock on list");

            // We're about to do bit-hacks. Use the least significant bit of the
            // pointer to store whether a handle exists or not. This requires that
            // the alignment of the pointer is > 1, so we can ensure that the lowest
            // bit should always be 0.
            // Also, byte_add requires that the computed offset stays in bounds of the
            // allocated object, so that must be at least 2 bytes.
            let _: () = const {
                let align = align_of::<StorageList<R>>();
                assert!(align > 1, "bithacking requires alignment greater than 1");

                let size_bytes = size_of::<StorageList<R>>();
                assert!(size_bytes >= 2, "bithacking requires size bytes >=2");
            };

            // SAFETY: StorageListNode Rule 2: Node must be attached to zero or one
            // lists at a time.
            //
            // Check: Is this node eligible to take? A node is eligible to take
            // if EITHER it is linked to the current list already, but no handle
            // exists, OR if the node is not attached to any list.
            let list_ptr: *const StorageList<R> = list;
            let list_ptr: *mut StorageList<R> = list_ptr.cast_mut();
            let list_ptr: *mut () = list_ptr.cast();
            // SAFETY: the const asserts above ensure that this is safe to do.
            let list_ptr_taken: *mut () = unsafe { list_ptr.byte_add(1) };

            // Attempt to swap the contents with a pointer to the current list
            let old =
                self.taken_for_list
                    .fetch_update(Ordering::Release, Ordering::Acquire, |old| {
                        // If the old value was null (no list, not attached), OR
                        // if the old value was for THIS list (not attached), then
                        // swap for THIS list + attached
                        if old.is_null() || old == list_ptr {
                            Some(list_ptr_taken)
                        } else {
                            None
                        }
                    });

            match old {
                Ok(ptr) if ptr.is_null() => {
                    already_attached = false;
                    debug!("Taking node for first time");
                }
                Ok(_ptr) => {
                    already_attached = true;
                    debug!("Retaking node");
                }
                Err(ptr) if ptr == list_ptr_taken => {
                    error!("Node already taken");
                    return Err(Error::AlreadyTaken);
                }
                Err(_ptr) => {
                    error!("Node already exists in other list");
                    return Err(Error::NodeInOtherList);
                }
            }

            // Check: passed! If we are already attached to this list, we do not need
            // (or want!) to re-run the attach logic.
            if !already_attached {
                let nodeptr: *mut Node<T> = self.inner.get();
                // SAFETY: nodeptr is not null because `self` is still valid.
                let nodenn: NonNull<Node<T>> = unsafe { NonNull::new_unchecked(nodeptr) };
                // NOTE: We EXPLICITLY cast the outer Node<T> ptr, instead of using the header
                // pointer, so when we cast back later, the pointer has the correct provenance!
                let hdrnn: NonNull<NodeHeader> = nodenn.cast();

                // Check if the key already exists in the list.
                // This also prevents attaching to the list more than once.
                // SAFETY: StorageListNode Rule 1: NodeHeader access is ALWAYS shared except at
                // the time of attach or detach. This is where we are, so we are allowed to hold
                // a mut ref to link to the list. We must also lock the mutex, which we did above.
                let key = unsafe { &nodenn.as_ref().header.key };
                if inner.find_node(key).is_some() {
                    error!("Key already in use: {:?}", key);
                    self.taken_for_list.store(null_mut(), Ordering::Release);
                    return Err(Error::DuplicateKey);
                } else {
                    inner.list.push_front(hdrnn);
                }
            }

            // Let read task know we MAY have work. This may not be true if we
            // are re-attaching, but `process_reads` is smart enough to skip
            // a read if no nodes actually are in a state where they need data.
            list.needs_read.wake();

            debug!("attach() release Lock on list");
        }

        debug!("Waiting for reading_done");

        // We know our node is now on the list. Create a ref to the header which
        // we can use to observe state changes.
        let nodeptr: *mut Node<T> = self.inner.get();
        // ?SAFE?: StorageListNode Rule 1: NodeHeader access is ALWAYS shared,
        // StorageListInner is only usable with the mutex locked, preventing attach/detach
        // of new nodes.
        let hdrref: &NodeHeader = unsafe { &*addr_of!((*nodeptr).header) };

        // Wait for the state to reach any non-Initial value. This handles races
        // where the I/O worker ALREADY loaded us between releasing the lock and
        // now (would require interrupts/threads/multicore), and handles cases
        // where we get a spurious wake BEFORE we've actually been loaded.
        // SAFETY: StorageListNode Rule 4: we must only return a handle if we are
        // no longer in the Initial state.
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
                let _inner = list.inner.lock().await;
                // SAFETY: StorageListNode Rule 5:
                // We must only take a mut ref to `t` IF we are in the `NonResident` state
                // and we locked the mutex, which we just did.
                let body: &mut MaybeUninit<T> = unsafe { &mut *addr_of_mut!((*nodeptr).t) };
                *body = MaybeUninit::new(f());
                // We do hold the lock, use Relaxed ordering.
                hdrref
                    .state
                    .store(State::NeedsWrite.into_u8(), Ordering::Relaxed);
            }
            // This state is set by this function if the node is non resident
            State::NeedsWrite if !already_attached => {
                unreachable!("shouldn't observe this in first attach")
            }
            // If this isn't our first attach, we might see already pending data
            State::NeedsWrite => (),
            // This is the usual case: key found in flash and node hydrated
            State::ValidNoWriteNeeded => (),
        }

        Ok(StorageListNodeHandle {
            list_ty: PhantomData,
            inner: self,
        })
    }

    /// Detach a node from a given list
    ///
    /// This is generally never needed: in most cases nodes live on the same StorageList
    /// for their entire lifetime. This only exists to answer the theoretical "well technically"
    /// state where you could want to attach a node to a different list. Usage is strongly
    /// discouraged.
    ///
    /// This function returns an error if:
    ///
    /// * a [`StorageListNodeHandle`] exists for this node
    /// * the `list` does not match the [`StorageList`] used for the last attach
    ///
    /// This function succeeds if:
    ///
    /// * This node is not currently (or was never) attached to a list
    /// * No [`StorageListNodeHandle`] is live AND the `list` matches the currently attached list
    ///
    /// On success, this node is reset to the Initial state, and if the node contains any data,
    /// it is dropped.
    ///
    /// This node will no longer participate in any future reads/writes/garbage collection
    /// of the list it is detached from.
    pub async fn detach<R>(&'static self, list: &'static StorageList<R>) -> Result<(), Error>
    where
        R: ScopedRawMutex + 'static,
    {
        debug!("Detaching node");

        // Again: we are doing bit-hacks. See `attach_with_default` for an explanation.
        let list_ptr: *const StorageList<R> = list;
        let list_ptr: *mut StorageList<R> = list_ptr.cast_mut();
        let list_ptr: *mut () = list_ptr.cast();
        // SAFETY: Byte offset fits into an `isize` and list_ptr is derived from an
        // allocated object. Also, the resulting pointer is in bounds of the allocated object
        // because StorageList is at least 2 bytes. This is ensured with const asserts in
        // attach().
        let list_ptr_taken: *mut () = unsafe { list_ptr.byte_add(1) };

        // First, take the mutex for this list.
        let mut inner = list.inner.lock().await;

        // Attempt to move from ATTACHED but NOT TAKEN for this list to
        // Not Attached and Not Taken
        let old = self.taken_for_list.compare_exchange(
            list_ptr,
            null_mut(),
            Ordering::AcqRel,
            Ordering::Acquire,
        );

        match old {
            Ok(_ptr) => {
                debug!("Attached and not taken")
            }
            Err(ptr) if ptr == list_ptr_taken => {
                error!("Node still taken, can't release");
                return Err(Error::AlreadyTaken);
            }
            Err(ptr) if ptr.is_null() => {
                error!("Node not attached to anything, already detached");
                return Ok(());
            }
            Err(_ptr) => {
                error!("Node exists in some other list: can't detach");
                return Err(Error::NodeInOtherList);
            }
        }

        // While still holding the lock, detach this node from this list.
        // On detach, we also reset the node to it's initial state.
        //
        // SAFETY:
        // StorageList Rule 1: When detaching, we must be holding the mutex.
        // StorageList Rule 2: When changing the state of a Node, we must be
        //  holding the mutex.
        // StorageListNode Rule 1: When detaching, we must hold the mutex so we
        //  are allowed to unlink from the list.
        unsafe {
            let hdr: NonNull<NodeHeader> =
                NonNull::new_unchecked(addr_of_mut!((*self.inner.get()).header));
            let res = inner.list.remove(hdr);
            debug_assert!(res.is_some());

            // We know we have exclusive access to the node.
            let mut_ref: &mut Node<T> = &mut *self.inner.get();
            match State::from_u8(mut_ref.header.state.load(Ordering::Acquire)) {
                // In these states, there is no data to drop
                State::Initial | State::NonResident => {}
                // In these states, there IS data to drop
                State::ValidNoWriteNeeded | State::NeedsWrite => {
                    mut_ref.t.assume_init_drop();
                }
            }
            mut_ref
                .header
                .state
                .store(State::Initial.into_u8(), Ordering::Release);
        }

        Ok(())
    }
}

/// SAFETY: Sync-safety is guaranteed by adhering to the rules in the [`crate::safety_guide`]
/// and the StorageList mutex.
unsafe impl<T> Sync for StorageListNode<T> where T: Send + 'static + MaybeDefmtFormat {}

// ---- impl StorageListNodeHandle ----

impl<T, R> StorageListNodeHandle<T, R>
where
    T: Clone + Send + 'static + MaybeDefmtFormat,
    R: ScopedRawMutex + 'static,
{
    /// Obtain the key of a node
    pub fn key(&self) -> &str {
        let nodeptr: *mut Node<T> = self.inner.inner.get();
        // SAFETY: `self` is still valid so we can create a pointer
        let nodenn: NonNull<Node<T>> = unsafe { NonNull::new_unchecked(nodeptr) };
        let hdrnn: NonNull<NodeHeader> = nodenn.cast();
        // SAFETY: StorageListNode Rule 1: the Node header is always shared
        unsafe { hdrnn.as_ref().key }
    }

    fn list(&self) -> &'static StorageList<R> {
        let ptr: *mut () = self.inner.taken_for_list.load(Ordering::Acquire);
        debug_assert!(!ptr.is_null(), "handle should not be null");
        debug_assert_eq!((ptr as usize) & 1, 1, "'is taken' bit should be set");

        // SAFETY: the existence of a StorageListNodeHandle ensures that the taken_for_list
        // field is set one byte past the start of a StorageList. We can safely decrement
        // this and use it as a static reference.
        unsafe {
            let ptr: *mut () = ptr.byte_sub(1);
            let ptr: *const () = ptr.cast_const();
            let ptr: *const StorageList<R> = ptr.cast();
            &*ptr
        }
    }

    /// This is a `load` function that copies out the data
    pub fn load(&self) -> T {
        // No need to lock the list because if the state is proper, we can always
        // read node.t. The I/O worker will never change node.t except at initial
        // hydration. Later, only write() will change node.t, but it requires &mut self.
        let nodeptr: *mut Node<T> = self.inner.inner.get();

        // SAFETY: StorageListNodeHandle Rule 1: Header is always shared
        let state = unsafe {
            let hdrptr: *const NodeHeader = addr_of!((*nodeptr).header);
            let hdr_ref: &NodeHeader = &*hdrptr;
            // We DON'T hold a lock to the list, so use Acquire for load
            State::from_u8(hdr_ref.state.load(Ordering::Acquire))
        };

        // is T valid?
        match state {
            // All these states implicate that process_reads/_writes has not finished
            // but they hold a lock on the list, so we should never reach this piece
            // of code while holding a lock ourselves.
            State::Initial | State::NonResident => {
                unreachable!("This state should not be observed")
            }
            // Handle all states here explicitly to avoid bugs with a catch-all
            State::ValidNoWriteNeeded | State::NeedsWrite => {}
        }
        // yes!
        //
        // SAFETY: StorageListNodeHandle Rule 7 - Shared access to T is allowed without
        // locking the mutex
        unsafe {
            let tptr: *const MaybeUninit<T> = addr_of!((*nodeptr).t);
            (*tptr).assume_init_ref().clone()
        }
    }

    /// Write data to the buffer, and mark the buffer as "needs to be flushed".
    ///
    /// It will await a lock on the [StorageList] because it modifies the contents of the
    /// list node. This may add some delay if the worker task currently holds
    /// a lock on the list for reading from or writing to flash.
    pub async fn write(&mut self, t: &T) -> Result<(), Error> {
        // Lock the list to get exclusive access to its contents and be allowed
        // to modify it.
        let _inner = self.list().inner.lock().await;

        let nodeptr: *mut Node<T> = self.inner.inner.get();

        // SAFETY: StorageListNodeHandle Rule 1: Header is always shared
        let (key, state) = unsafe {
            let hdrptr: *const NodeHeader = addr_of!((*nodeptr).header);
            let hdr_ref: &NodeHeader = &*hdrptr;
            // We DON'T hold a lock to the list, so use Acquire for load
            (
                hdr_ref.key,
                State::from_u8(hdr_ref.state.load(Ordering::Acquire)),
            )
        };

        match state {
            // `Initial` and `NonResident` can not occur for the same reason as
            // outlined in load(). OldData should have been replaced with a Default value.
            State::Initial | State::NonResident => {
                debug!("write() on invalid state: {:?}", state);
                return Err(Error::InvalidState(key, state));
            }
            State::ValidNoWriteNeeded | State::NeedsWrite => {}
        }
        // We do a swap instead of a write here, to ensure that we
        // call `drop` on the "old" contents of the buffer.
        let mut t: T = t.clone();
        // SAFETY:
        // StorageListNode Rule 6: we must hold the mutex for the entire
        //  write operation.
        // StorageList Rule 2: When changing the state of a node, we must
        //  lock the mutex.
        unsafe {
            let noderef = &mut *nodeptr;

            let mutref: &mut T = noderef.t.assume_init_mut();
            core::mem::swap(&mut t, mutref);

            // We hold a lock to the list, so use Relaxed
            noderef
                .header
                .state
                .store(State::NeedsWrite.into_u8(), Ordering::Relaxed);
        }

        // Let the write task know we have work
        self.list().needs_write.wake();

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

impl<T, R> Drop for StorageListNodeHandle<T, R>
where
    T: 'static + MaybeDefmtFormat,
    R: ScopedRawMutex + 'static,
{
    fn drop(&mut self) {
        let res =
            self.inner
                .taken_for_list
                .fetch_update(Ordering::Release, Ordering::Acquire, |old| {
                    debug_assert!(!old.is_null());
                    debug_assert_eq!((old as usize) & 1, 1, "'is taken' bit should be set");
                    
                    // SAFETY: the existence of a StorageListNodeHandle ensures that the taken_for_list
                    // field is set one byte past the start of a StorageList. We can safely decrement
                    // this and use it as a static reference.
                    Some(unsafe { old.byte_sub(1) })
                });
        debug_assert!(res.is_ok());
    }
}

// ---- impl State ----
// ---- impl Node ----
// ---- impl NodeHeader ----

/// This is cordyceps' intrusive linked list trait. It's mostly how you
/// shift around pointers semantically.
/// 
/// SAFETY: Requirements from the trait definition are followed:
///
/// - Implementation ensures that implementors are pinned in memory while they are
///   in the intrusive collection. They nodes are required to be static, so they
///   are never dropped or moved in memory.
/// - The type implementing this trait must not implement [Unpin].
///   -> Ensured by PhantomPinned in Node
unsafe impl Linked<list::Links<NodeHeader>> for NodeHeader {
    type Handle = NonNull<NodeHeader>;

    fn into_ptr(r: Self::Handle) -> NonNull<Self> {
        r
    }

    unsafe fn from_ptr(ptr: NonNull<Self>) -> Self::Handle {
        ptr
    }

    unsafe fn links(target: NonNull<Self>) -> NonNull<list::Links<NodeHeader>> {
        // SAFETY: using `ptr::addr_of!` avoids creating a temporary
        // reference, which stacked borrows dislikes.
        let node = unsafe { ptr::addr_of_mut!((*target.as_ptr()).links) };
        // SAFETY: caller has to ensure that it the pointer points to a
        // valid instance of Self
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

/// Tricky monomorphizing serialization function
///
/// Casts a type-erased node pointer into the "right" type, so that we can do
/// serialization with it.
///
/// # Safety
///
/// `node.t` MUST be in a valid state before calling this function, AND
/// the mutex must be held the whole time we are here, because we are reading
/// from `t`!
unsafe fn serialize<T>(node: NonNull<Node<()>>, buf: &mut [u8]) -> Result<usize, ()>
where
    T: 'static,
    T: Encode<()>,
    T: minicbor::CborLen<()>,
    T: MaybeDefmtFormat,
{
    let node: NonNull<Node<T>> = node.cast();
    // SAFETY: Caller ensures that node.t is in a valid state
    let noderef: &Node<T> = unsafe { node.as_ref() };
    let tref: &MaybeUninit<T> = &noderef.t;
    // SAFETY: Caller ensures that node.t is in a valid state
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

/// Tricky monomorphizing deserialization function
///
/// This function performs type-erased deserialization by casting a generic node pointer
/// to the correct concrete type, then deserializing CBOR data from a buffer into that
/// type. Deserialization only ever occurs once.
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
unsafe fn deserialize<T>(node: NonNull<Node<()>>, buf: &[u8]) -> Result<usize, ()>
where
    T: 'static,
    T: CborLen<()>,
    for<'a> T: Decode<'a, ()>,
{
    let node: NonNull<Node<T>> = node.cast();

    // SAFETY: StorageListNode Rule 1: access to node header is always shared
    // except for when the mutex is locked during attach. But the caller is
    // required to hold the mutex.
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
