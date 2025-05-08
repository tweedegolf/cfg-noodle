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

pub struct PinList<R: ScopedRawMutex> {
    list: BlockingMutex<R, List<PinListNodeInner<()>>>,
}

impl<R: ScopedRawMutex + ConstInit> PinList<R> {
    pub const fn new() -> Self {
        Self {
            list: BlockingMutex::new(List::new()),
        }
    }
}

impl<R: ScopedRawMutex> PinList<R> {
    pub fn process_reads(&'static self, in_flash: &HashMap<String, Vec<u8>>) {
        self.list.with_lock(|ls| {
            let iter = ls.iter_mut();
            for node in iter {
                let nodemut = unsafe { &mut *node.node.get() };
                let vtable = {
                    match nodemut.state {
                        State::Initial => Some((nodemut.vtable, nodemut.key)),
                        State::NonResident => None,
                        State::DefaultUnwritten => None,
                        State::ValidNoWriteNeeded => None,
                        State::NeedsWrite => None,
                        State::WriteStarted => None,
                    }
                };
                if let Some((vtable, key)) = vtable {
                    if let Some(val) = in_flash.get(key) {
                        let res = (vtable.deserialize)(
                            NonNull::from(nodemut),
                            val.as_slice(),
                        );
                        let nodemut = unsafe { &mut *node.node.get() };
                        if res.is_ok() {
                            nodemut.state = State::ValidNoWriteNeeded;
                            nodemut.state_change.as_ref().unwrap().wake();
                        } else {
                            // todo: is this right? if it's bad its bad
                            nodemut.state = State::NonResident;
                            nodemut.state_change.as_ref().unwrap().wake();
                        }
                    } else {
                        // Not in flash
                        nodemut.state = State::NonResident;
                        nodemut.state_change.as_ref().unwrap().wake();
                    }
                }
            }
        })
    }

    pub fn process_writes(&'static self, flash: &mut HashMap<String, Vec<u8>>) {
        self.list.with_lock(|ls| {
            let iter = ls.iter_mut();
            for node in iter {
                let nodemut = unsafe { &mut *node.node.get() };
                let vtable = {
                    match nodemut.state {
                        State::Initial => None,
                        State::NonResident => None,
                        State::DefaultUnwritten => None,
                        State::ValidNoWriteNeeded => None,
                        State::NeedsWrite => Some((nodemut.vtable, nodemut.key)),
                        State::WriteStarted => panic!("shouldn't be here"),
                    }
                };
                if let Some((vtable, key)) = vtable {
                    let mut buf = [0u8; 1024];
                    let res = (vtable.serialize)(
                        NonNull::from(nodemut),
                        buf.as_mut_slice(),
                    );

                    if let Ok(used) = res {
                        let res = flash.insert(key.to_string(), (buf[..used]).to_vec());
                        assert!(res.is_none());
                        let nodemut = unsafe { &mut *node.node.get() };
                        nodemut.state = State::ValidNoWriteNeeded;
                        nodemut.state_change.as_ref().unwrap().wake();
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
    inner: PinListNodeInner<T>,
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
            inner: PinListNodeInner {
                node: UnsafeCell::new(Node {
                    links: list::Links::new(),
                    key: path,
                    state: State::Initial,
                    state_change: None,
                    vtable: VTable::for_ty::<T>(),
                    t: MaybeUninit::uninit(),
                    _pin: PhantomPinned,
                }),
            },
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
            let noderef: &'static PinListNodeInner<T> = &self.inner;
            let noderef: NonNull<PinListNodeInner<T>> = NonNull::from(noderef);
            {
                // WITH the lock, put in our WaitCell
                let noderef: &PinListNodeInner<T> = unsafe { &*noderef.as_ptr() };
                let noderef: &mut Node<T> = unsafe { &mut *noderef.node.get() };
                noderef.state_change = Some(&self.state_change);
            }
            let noderef: NonNull<PinListNodeInner<()>> = noderef.cast();
            ls.push_front(noderef);
        });

        // now spin until we have a value, or we know it is non-resident
        self.state_change.wait_for(|| {
            // We need the lock to look at ourself
            list.list.with_lock(|_ls| {
                let noderef: &'static PinListNodeInner<T> = &self.inner;
                let noderef: &mut Node<T> = unsafe { &mut *noderef.node.get() };
                // Are we in a state with a valid T?
                match noderef.state {
                    State::Initial => false,
                    State::NonResident => {
                        // We are nonresident, we need to initialize
                        noderef.t = MaybeUninit::new(T::default());
                        noderef.state = State::DefaultUnwritten;
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
            let noderef: &'static PinListNodeInner<T> = &self.inner;
            let noderef: &mut Node<T> = unsafe { &mut *noderef.node.get() };
            // is T valid?
            match noderef.state {
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
            let noderef: &'static PinListNodeInner<T> = &self.inner;
            let noderef: &mut Node<T> = unsafe { &mut *noderef.node.get() };
            // determine state
            // TODO: allow writes from uninit state?
            match noderef.state {
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
            noderef.state = State::NeedsWrite;
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

// this is the equivalent of Node
#[pin_project]
#[repr(C)]
pub struct Node<T> {
    links: list::Links<PinListNodeInner<T>>,
    key: &'static str,
    state: State,
    state_change: Option<&'static WaitCell>,
    vtable: VTable,
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

// This is the equivalent of Waiter
pub struct PinListNodeInner<T> {
    node: UnsafeCell<Node<T>>,
}

unsafe impl<T> Linked<list::Links<PinListNodeInner<T>>> for PinListNodeInner<T> {
    type Handle = NonNull<PinListNodeInner<T>>;

    fn into_ptr(r: Self::Handle) -> NonNull<Self> {
        r
    }

    unsafe fn from_ptr(ptr: NonNull<Self>) -> Self::Handle {
        ptr
    }

    unsafe fn links(target: NonNull<Self>) -> NonNull<list::Links<PinListNodeInner<T>>> {
        // Safety: using `ptr::addr_of!` avoids creating a temporary
        // reference, which stacked borrows dislikes.
        let node: *const UnsafeCell<Node<T>> = unsafe { ptr::addr_of!((*target.as_ptr()).node) };

        // From WaitQueue, using its UnsafeCell
        // (*node).with_mut(|node| {
        //     let links = ptr::addr_of_mut!((*node).links);
        //     // Safety: since the `target` pointer is `NonNull`, we can assume
        //     // that pointers to its members are also not null, making this use
        //     // of `new_unchecked` fine.
        //     NonNull::new_unchecked(links)
        // })

        // AJM: reimpl
        let node: *mut Node<T> = unsafe { (*node).get() };
        let links = unsafe { ptr::addr_of_mut!((*node).links) };
        // Safety: since the `target` pointer is `NonNull`, we can assume
        // that pointers to its members are also not null, making this use
        // of `new_unchecked` fine.
        unsafe { NonNull::new_unchecked(links) }
    }
}

#[pinned_drop]
impl<T, R: ScopedRawMutex> PinnedDrop for PinListNode<T, R> {
    fn drop(mut self: Pin<&mut Self>) {
        let mut this = self.project();
        let ls = this.list.load(Ordering::Relaxed);
        if !ls.is_null() {
            let lsref: &PinList<R> = unsafe { &*ls };
            lsref.list.with_lock(|ls| {
                let ptr: NonNull<PinListNodeInner<T>> =
                    unsafe { NonNull::from(Pin::into_inner_unchecked(this.inner.as_mut())) };
                let ptr: NonNull<PinListNodeInner<()>> = ptr.cast();
                let _res: Option<NonNull<PinListNodeInner<()>>> = unsafe { ls.remove(ptr) };
                // TODO: do we need to cast back and drop `t` in place depending on state?
                todo!()
            })
        }
    }
}
