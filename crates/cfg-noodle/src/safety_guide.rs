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
//! ### Rules List
//!
//! 1. `StorageListNode`, `Node<T>`, and `NodeHeader`s (collectively, "the Node") are ALWAYS
//!    shared. Nodes have four fields that utilize inner-mutability with certain rules:
//!     1. The "List Pointer": `StorageListNode->taken_for_list`
//!         * The "List Pointer" is used to track which list (if any) the Node is attached to
//!         * The "List Pointer" is used to track whether a `StorageListNodeHandle` is live or not
//!     2. The "Data Item": `Node->t`
//!     3. The "State": `NodeHeader->state`
//!     4. The "Linked List": `NodeHeader->links`
//! 2. The Node "List Pointer" may be modified IFF:
//!     1. When Attaching a Node to a List, the list's mutex is locked AND:
//!         1. The "List Pointer" is currently null, OR
//!         2. The "List Pointer" points to the list we are attaching to, and we are re-attaching
//!     2. When dropping a `StorageListNodeHandle`:
//!         1. The lowest bit of the "List Pointer" is changed from `0b1` to `0b0`
//!     3. When detaching a Node from a List:
//!         1. No `StorageListNodeHandle` is live for this Node, AND
//!         2. The mutex for the List that the List Pointer points to is locked,
//!         3. The "List Pointer" is set to null when the detach is complete.
//! 3. The Node "Data Item" may be modified IFF:
//!     1. The Node is attached to a List, AND:
//!         1. The mutex for the List is locked
//!         2. . Any of the following are true:
//!             1. If updated by `process_reads()`: The Node is in the "Initial" state, OR
//!             2. If updated by `attach()`: The Node is in the "NonResident" state, OR
//!             3. If updated by `Handle::write()` (no additional requirements).
//!     2. The Node is NOT attached to a List, and the Data Item is being reset during `detach()`.
//! 4. The Node "State" may be be modified IFF:
//!     1. If updated by the List, the List's mutex is locked, AND:
//!         1. The List is moving the state from Initial to NonResident, OR
//!         2. The List is moving the state from Initial to ValidNoWriteNeeded, OR
//!         3. The List is moving the state from NeedsWrite to ValidNoWriteNeeded
//!     2. If updated by the Node:
//!         1. The list's mutex is locked AND the Node is moving the state from NonResident -> NeedsWrite
//!         2. The list's mutex is locked AND the Node is moving the state from ValidNoWriteNeeded -> NeedsWrite
//!         3. The node is detached from any lists AND the Node is moving from any state to Initial.
//! 5. The Node "Linked List" may be modified IFF:
//!     1. When attaching to a list:
//!         1. The Node must not already be attached to a List, AND
//!         2. The mutex for the list the node is being attached to is locked
//!     2. When detaching from a list:
//!         1. No `StorageListNodeHandle` is live for this Node, AND
//!         2. The mutex for the list the node is being detached from is locked
//! 6. A `StorageListNodeHandle` may NOT be created unless:
//!     1. The Node is attached to a list
//!     2. No other `StorageListNodeHandle` is live for this Node
//!     3. The Node's "List Pointer"'s lowest bit is set to `0b1` to denote a handle is live
//!     4. The Node is NOT in the Initial or NonResident states.
//! 7. Shared access to the "Data Item" is allowed IFF:
//!     1. If accessed via the StorageListNodeHandle
//!     2. If accessed via the List:
//!         1. The Node is NOT in the Initial or NonResident state
//!         2. The mutex for the List is held
//! 8. The List may traverse the linked list of nodes IFF the mutex for the list is held
//!
