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

use cordyceps::{Linked, List, list};
use maitake_sync::WaitCell;
use minicbor::encode::write::{Cursor, EndOfSlice};
use minicbor::{Decode, Encode};
use mutex::{BlockingMutex, ConstInit, ScopedRawMutex};
use pin_project::{pin_project, pinned_drop};
use std::collections::HashMap;
use std::marker::PhantomPinned;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::{
    cell::UnsafeCell,
    pin::Pin,
    ptr::{self, NonNull},
};

/// "PinList" is the "global anchor" of all storage items.
///
/// It serves as the meeting point between two conceptual pieces:
///
/// 1. The `PinListNode`s, which each store one configuration item
/// 2. The worker task that owns the external flash, and serves the
///    role of loading data FROM flash (and putting it in the Nodes),
///    as well as the role of deciding when to write data TO flash
///    (retrieving it from each node).
///
/// It's called "PinList" because it comes from something else I was
/// researching: https://bsky.app/profile/jamesmunns.com/post/3lo6gkvyfmc2p,
/// where the storage could be stored in a pinned future, instead of a
/// static. It's not a very accurate name anymore, maybe something like
/// `StorageList` or `StorageListAnchor` is better now. I'll leave that
/// up to you to decide.
pub struct PinList<R: ScopedRawMutex> {
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
    /// The type parameter `R` allows us to be generic over kinds of mutex
    /// impls, allowing for use of a `std` mutex on `std`, and for things
    /// like `CriticalSectionRawMutex` or `ThreadModeRawMutex` on no-std
    /// targets.
    ///
    /// This mutex MUST be locked whenever you:
    ///
    /// 1. Want to append an item to the linked list, e.g. with `PinListNode`.
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
    list: BlockingMutex<R, List<NodeHeader>>,

    // TODO: We probably want to put one or two `WaitQueue`s here, one so
    // `PinListNode`s can fire off an "I need to be hydrated" wake, and one
    // so `PinListNode`s can fire off an "I have a pending write" wake.
    //
    // These would allow the storage worker task to be a bit more intelligent
    // or reactive to checking for "I have something to do", instead of having
    // to regularly poll.
    //
    // needs_read: WaitQueue,
    // neads_write: WaitQueue,
}

impl<R: ScopedRawMutex + ConstInit> PinList<R> {
    /// const constructor to make a new empty list. Intended to be used
    /// to create a static. See `main.rs` for example.
    pub const fn new() -> Self {
        Self {
            list: BlockingMutex::new(List::new()),
        }
    }
}

impl<R: ScopedRawMutex + ConstInit> Default for PinList<R> {
    /// this only exists to shut up the clippy lint about impl'ing default
    fn default() -> Self {
        Self::new()
    }
}

/// These are public methods for the `PinList`. They currently are intended to be
/// used by the "storage worker task", that decides when we actually want to
/// interact with the flash.
///
/// The current implementation is "fake", it's only meant for demos. Once we
/// figure out how we plan to store stuff, like `s-s::Queue`, or some new
/// theoretical `s-s::Latest` structure, these methods will need to be
/// re-worked.
///
/// For now they are oriented around using a HashMap.
impl<R: ScopedRawMutex> PinList<R> {
    /// This method:
    ///
    /// 1. Locks the mutex
    /// 2. Iterates through each node currently linked in the list
    /// 3. For Each node that says "I need to be hydrated", it sees if a matching
    ///    key record exists in the hashmap (our stand-in for the actual flash
    ///    contents). If it does, it will try to hand control over to the node
    ///    to extract data
    /// 4. On success, it will mark the node as now having valid data. On failure
    ///    it marks that (discussed more below, some of this might not be perfect
    ///    yet and requires more thought)
    pub fn process_reads(&'static self, in_flash: &HashMap<String, Vec<u8>>) {
        // Lock the list, remember, if we're touching nodes, we need to have the list
        // locked the entire time!
        self.list.with_lock(|ls| {
            // Traverse the linked list, hopping between intrusive nodes.
            for node in ls.iter_mut() {
                // SAFETY: We hold the lock, we are allowed to gain exclusive mut access
                // to the contents of the node.
                let vtable = {
                    match node.state {
                        State::Initial => Some((node.vtable, node.key)),
                        State::NonResident => None,
                        State::DefaultUnwritten => None,
                        State::ValidNoWriteNeeded => None,
                        State::NeedsWrite => None,
                        State::WriteStarted => None,
                    }
                };
                if let Some((vtable, key)) = vtable {
                    // Make a node pointer from a header pointer
                    let mut hdrptr: NonNull<NodeHeader> = NonNull::from(unsafe { Pin::into_inner_unchecked(node) });
                    let nodeptr: NonNull<Node<()>> = hdrptr.cast();

                    if let Some(val) = in_flash.get(key) {
                        let res = (vtable.deserialize)(
                            nodeptr,
                            val.as_slice(),
                        );
                        let hdrmut = unsafe { hdrptr.as_mut() };
                        if res.is_ok() {
                            hdrmut.state = State::ValidNoWriteNeeded;
                            hdrmut.state_change.as_ref().unwrap().wake();
                        } else {
                            // todo: is this right? if it's bad its bad
                            hdrmut.state = State::NonResident;
                            hdrmut.state_change.as_ref().unwrap().wake();
                        }
                    } else {
                        let hdrmut = unsafe { hdrptr.as_mut() };
                        // Not in flash
                        hdrmut.state = State::NonResident;
                        hdrmut.state_change.as_ref().unwrap().wake();
                    }
                }
            }
        })
    }

    pub fn process_writes(&'static self, flash: &mut HashMap<String, Vec<u8>>) {
        self.list.with_lock(|ls| {
            let iter = ls.iter_mut();
            for node in iter {
                let vtable = {
                    match node.state {
                        State::Initial => None,
                        State::NonResident => None,
                        State::DefaultUnwritten => None,
                        State::ValidNoWriteNeeded => None,
                        State::NeedsWrite => Some((node.vtable, node.key)),
                        State::WriteStarted => panic!("shouldn't be here"),
                    }
                };
                if let Some((vtable, key)) = vtable {
                    let mut hdrptr: NonNull<NodeHeader> = NonNull::from(unsafe { Pin::into_inner_unchecked(node) });
                    let nodeptr: NonNull<Node<()>> = hdrptr.cast();

                    let mut buf = [0u8; 1024];
                    let res = (vtable.serialize)(
                        nodeptr,
                        buf.as_mut_slice(),
                    );

                    if let Ok(used) = res {
                        let hdrmut = unsafe { hdrptr.as_mut() };
                        let res = flash.insert(key.to_string(), (buf[..used]).to_vec());
                        assert!(res.is_none());
                        hdrmut.state = State::ValidNoWriteNeeded;
                        hdrmut.state_change.as_ref().unwrap().wake();
                    } else {
                        panic!("why did ser fail");
                    }
                }
            }
        })
    }
}

unsafe impl<T, R> Sync for PinListNode<T, R>
where
    T: Send + 'static,
    R: ScopedRawMutex + 'static,
{}

// This is the equivalent of Wait
#[pin_project(PinnedDrop)]
pub struct PinListNode<T, R>
where
    T: 'static,
    R: ScopedRawMutex + 'static,
{
    // morally: Option<&'static PinList<R>>
    list: AtomicPtr<PinList<R>>,
    state_change: WaitCell,

    #[pin]
    inner: UnsafeCell<Node<T>>,
}

impl<T, R> PinListNode<T, R>
where
    T: 'static,
    T: Encode<()>,
    for<'a> T: Decode<'a, ()>,
    T: Default + Clone,
    R: ScopedRawMutex + 'static,
{
    pub const fn new(path: &'static str) -> Self {
        Self {
            list: AtomicPtr::new(ptr::null_mut()),
            state_change: WaitCell::new(),
            inner: UnsafeCell::new(Node {
                header: NodeHeader {
                    links: list::Links::new(),
                    key: path,
                    state: State::Initial,
                    state_change: None,
                    vtable: VTable::for_ty::<T>(),
                },
                t: MaybeUninit::uninit(),
                _pin: PhantomPinned,
            }),
        }
    }

    // Attaches, waits for hydration. If value not in flash, a default value is used
    pub async fn attach(&'static self, list: &'static PinList<R>) -> Result<(), ()> {
        // TODO: We need some kind of "taken" flag to prevent multiple
        // calls to attach, maybe return some kind of handle. Could maybe just
        // compare and swap with this?
        if !self.list.load(Ordering::Relaxed).is_null() {
            return Err(());
        }

        // todo: assert not already attached
        // todo: assert key uniqueness
        list.list.with_lock(|ls| {
            let nodeptr: *mut Node<T> = self.inner.get();
            {
                // WITH the lock, put in our WaitCell
                let noderef = unsafe { &mut *nodeptr };
                noderef.header.state_change = Some(&self.state_change);
            }
            let nodenn: NonNull<Node<T>> = unsafe { NonNull::new_unchecked(nodeptr) };
            // NOTE: We EXPLICITLY case the outer Node<T> ptr, instead of using the header
            // pointer, so when we cast back later, the pointer has the correct provenance!
            let hdrnn: NonNull<NodeHeader> = nodenn.cast();
            ls.push_front(hdrnn);
        });

        // now spin until we have a value, or we know it is non-resident
        self.state_change.wait_for(|| {
            // We need the lock to look at ourself
            list.list.with_lock(|_ls| {
                let nodeptr: *mut Node<T> = self.inner.get();
                let noderef = unsafe { &mut *nodeptr };
                // Are we in a state with a valid T?
                match noderef.header.state {
                    State::Initial => false,
                    State::NonResident => {
                        // We are nonresident, we need to initialize
                        noderef.t = MaybeUninit::new(T::default());
                        noderef.header.state = State::DefaultUnwritten;
                        true
                    },
                    State::DefaultUnwritten => todo!("shouldn't observe this in attach"),
                    State::ValidNoWriteNeeded => {
                        true
                    },
                    State::NeedsWrite => todo!("shouldn't observe this in attach"),
                    State::WriteStarted => todo!("shouldn't observe this in attach"),
                }
            })
        }).await.expect("waitcell should never close");

        let ls: *const PinList<R> = list;
        let ls: *mut PinList<R> = ls.cast_mut();
        self.list.store(ls, Ordering::Relaxed);

        Ok(())
    }

    pub fn load(&'static self) -> T {
        let list = self.list.load(Ordering::Relaxed);
        let list = unsafe { list.as_ref().unwrap() };
        list.list.with_lock(|_ls| {
            let nodeptr: *mut Node<T> = self.inner.get();
            let noderef = unsafe { &mut *nodeptr };
            // is T valid?
            match noderef.header.state {
                State::Initial => todo!(),
                State::NonResident => todo!(),
                State::DefaultUnwritten => {},
                State::ValidNoWriteNeeded => {},
                State::NeedsWrite => {},
                State::WriteStarted => {},
            }
            // yes!
            unsafe {
                noderef.t.assume_init_ref().clone()
            }
        })
    }

    pub fn write(&'static self, t: &T) {
        let list = self.list.load(Ordering::Relaxed);
        let list = unsafe { list.as_ref().unwrap() };
        list.list.with_lock(|_ls| {
            let nodeptr: *mut Node<T> = self.inner.get();
            let noderef = unsafe { &mut *nodeptr };
            // determine state
            // TODO: allow writes from uninit state?
            match noderef.header.state {
                State::Initial => todo!("shouldn't observe"),
                State::NonResident => todo!("shouldn't observe"),
                State::DefaultUnwritten => {},
                State::ValidNoWriteNeeded => {},
                State::NeedsWrite => {},
                State::WriteStarted => todo!("how to handle?"),
            }
            // yes!
            let mut t: T = t.clone();
            unsafe {
                let mutref: &mut T = noderef.t.assume_init_mut();
                core::mem::swap(&mut t, mutref);
            }
            noderef.header.state = State::NeedsWrite;
            // old T is dropped
        })
    }
}

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
#[repr(u32)]
pub enum State {
    Initial = 0,
    NonResident = 1,
    DefaultUnwritten = 2,
    ValidNoWriteNeeded = 3,
    NeedsWrite = 4,
    WriteStarted = 5,
}

pub struct NodeHeader {
    links: list::Links<NodeHeader>,
    key: &'static str,
    state: State,
    state_change: Option<&'static WaitCell>,
    vtable: VTable,
}

// this is the equivalent of Node
#[pin_project]
#[repr(C)]
pub struct Node<T> {
    // LOAD BEARING: MUST BE THE FIRST ELEMENT IN A REPR-C STRUCT
    header: NodeHeader,
    // This type is !Unpin due to the heuristic from:
    // <https://github.com/rust-lang/rust/pull/82834>
    _pin: PhantomPinned,

    // MUST be in tail position!
    #[pin]
    t: MaybeUninit<T>,
}

type SerFn = fn(NonNull<Node<()>>, &mut [u8]) -> Result<usize, ()>;
// TODO: How to get "bytes consumed"? Do we need a custom decoder?
type DeserFn = fn(NonNull<Node<()>>, &[u8]) -> Result<(), ()>;

#[derive(Clone, Copy)]
pub struct VTable {
    serialize: SerFn,
    deserialize: DeserFn,
}

impl VTable {
    const fn for_ty<T>() -> Self
    where
        T: 'static,
        T: Encode<()>,
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

// node.t MUST be in a valid state
fn serialize<T>(node: NonNull<Node<()>>, buf: &mut [u8]) -> Result<usize, ()>
where
    T: 'static,
    T: Encode<()>,
{
    let node: NonNull<Node<T>> = node.cast();
    let noderef: &Node<T> = unsafe { node.as_ref() };
    let tref: &MaybeUninit<T> = &noderef.t;
    let tref: &T = unsafe { tref.assume_init_ref() };

    let mut cursor = Cursor::new(buf);
    let res: Result<(), minicbor::encode::Error<EndOfSlice>> = minicbor::encode(tref, &mut cursor);
    match res {
        Ok(()) => Ok(cursor.position()),
        Err(_e) => {
            // We know this was an "end of slice" error
            Err(())
        }
    }
}

fn deserialize<T>(node: NonNull<Node<()>>, buf: &[u8]) -> Result<(), ()>
where
    T: 'static,
    for<'a> T: Decode<'a, ()>,
{
    let mut node: NonNull<Node<T>> = node.cast();
    let noderef: &mut Node<T> = unsafe { node.as_mut() };

    let res = minicbor::decode::<T>(buf);

    let t = match res {
        Ok(t) => t,
        Err(_) => return Err(()),
    };

    noderef.t = MaybeUninit::new(t);

    Ok(())
}

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

#[pinned_drop]
impl<T, R: ScopedRawMutex> PinnedDrop for PinListNode<T, R> {
    fn drop(mut self: Pin<&mut Self>) {
        todo!("We probably don't actually need drop?")
    }
}
