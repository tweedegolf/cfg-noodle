#[cfg(loom)]
use loom::sync;

#[cfg(all(not(loom), not(feature = "std")))]
use core::sync;

#[cfg(all(not(loom), feature = "std"))]
use std::sync;

pub mod atomic {
    pub use super::sync::atomic::{AtomicPtr, AtomicU8, Ordering};
}

pub use sync::Arc;
