//! # Implementor's safety guide
//!
//! This is only relevant if you are working on (or curious about) the internals of
//! `cfg-noodle` - as a user, all safety invariants are maintained internally.
//!
//! ## The Safety Rules
//!
//! ### State Transitions
//!
//! ```text
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
//!
//! ### `StorageList` Rules
//!
//! 1. When adding (or removing) nodes, we MUST be holding the mutex.
//! 2. When changing the state of a node, we MUST be holding the mutex (for the full operation, e.g. also when writing to T in transition #2).
//! 3. The ONLY time the list may take a mut ref to the `t`, is while IN the Initial state, AND while holding the mutex
//!
//! ### `StorageListNode`/`StorageListNodeHandle` Rules
//!
//! 1. The Node header is ALWAYS shared, EXCEPT at the time of attach, at which point we will hold a mut ref to link to the list, this requires we hold the mutex.
//! 2. A Node must be attached to zero or one lists:
//!     * We must guarantee a node is prevented from being added to a second list, if it already exists in one.
//! 3. `attach()` MUST NOT complete/we MUST NOT create a StorageListNodeHdl UNTIL we are no longer in the Initial state.
//! 4. DURING `Node::attach()`, we may ONLY take a mut ref to `t` IF we are in the NonResident state, AND we hold the mutex.
//! 5. When calling `Hdl::write()`, we MUST hold the mutex.
//! 6. When calling `Hdl::load()`, we DO NOT need the mutex, because we've already guaranteed we are not in Initial/NonResident state, and the list will never attempt to mutate `t`, but may also have a shared ref to `t`.
