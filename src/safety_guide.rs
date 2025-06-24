//! # Implementor's safety guide
//!
//! This is only relevant if you are working on (or curious about) the internals
//! of `cfg-noodle` - as a user, all safety invariants are maintained internally.
//!
//! `cfg-noodle` contains a lot of unsafe code, so to make review and
//! development easier, we maintain all of the "big picture" rules here so that
//! they can be referenced in various `// SAFETY` comments or where these
//! invariants are enforced.
//!
//! ## The Safety Rules
//!
//! ### Node States
//!
//! See the docs of [`State`](crate::storage_node::State) for the rules in each
//! state that a Node may occupy.
//!
//! ### State Transitions
//!
//! Nodes begin in the `Initial` state.
//!
//! ```text
//!             ─ ─ ─ ─ (6)
//!            │
//!            ▼
//!       ┌─────────┐   (1)    ┌─────────────┐
//!       │ Initial │─────────▶│ NonResident │
//!       └─────────┘          └─────────────┘
//!            │(2)                   │(3)
//!            ▼                      ▼
//! ┌────────────────────┐ (4) ┌─────────────┐
//! │ ValidNoWriteNeeded │◀────│ NeedsWrite  │
//! └────────────────────┘     └─────────────┘
//!            │           (5)        ▲
//!            └──────────────────────┘
//! ```
//!
//! ALL state transitions require holding the lock.
//!
//! 1. Initial -> NonResident
//!     * LIST ONLY TRANISTION
//!     * during `attach()`, on first `process_reads()`
//! 2. Initial -> ValidNoWriteNeeded
//!     * LIST ONLY TRANSITION
//!     * during `attach()`, on first `process_reads()`
//! 3. NonResident -> NeedsWrite
//!     * NODE ONLY TRANSITION
//!     * during `attach()`, after first `process_reads()`
//! 4. NeedsWrite -> ValidNoWriteNeeded
//!     * LIST ONLY TRANSITION
//!     * during `process_write()` call
//! 5. ValidNoWriteNeeded -> NeedsWrite
//!     * NODE ONLY TRANSITTION
//!     * during `write()` call
//! 6. (any state) -> Initial
//!     * NODE ONLY TRANSITION
//!     * during `detach()` call
//!
//! ### `StorageList` Rules
//!
//! 1. When attaching (or detaching) nodes, we MUST be holding the mutex.
//! 2. When changing the state of a node, we MUST be holding the mutex (for the
//!    full operation, e.g. also when writing to T in transition #2).
//! 3. The ONLY time the list may take a mut ref to the `t`, is while IN the
//!    Initial state, AND while holding the mutex
//!
//! ### `StorageListNode`/`StorageListNodeHandle` Rules
//!
//! 1. The Node header is ALWAYS shared, EXCEPT at the time of attach or detach,
//!    at which point we will hold a mut ref to (un)link to the list, this
//!    requires we hold the mutex.
//! 2. A Node must be attached to zero or one lists at one time:
//!     * We must guarantee a node is prevented from being added to a second
//!       list, if it already exists in one.
//!     * Users MUST detach the node from the list it is in before attaching it
//!       to a new list.
//! 3. A Node must have zero or one handles live at one time:
//!     * We must guarantee a handle is prevented from being created if the node
//!       already has a handle live
//!     * The previous handle MUST be dropped before we create a new handle
//! 4. `attach()` MUST NOT complete/we MUST NOT create a StorageListNodeHandle
//!    UNTIL we are no longer in the Initial state.
//! 5. DURING `StorageListNode::attach()`, we may ONLY take a mut ref to `t` IF
//!    we are in the NonResident state, AND we hold the mutex.
//! 6. When calling `StorageListNodeHandle::write()`, we MUST hold the mutex.
//! 7. When calling `StorageListNodeHandle::load()`, we DO NOT need the mutex,
//!    because we've already guaranteed we are not in Initial/NonResident state,
//!    and the list will never attempt to mutate `t`, but may also have a shared
//!    ref to `t`.
