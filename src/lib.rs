#![no_std]
pub mod error;
pub mod intrusive;

#[allow(unused)]
pub(crate) mod logging {
    #[cfg(feature = "std")]
    pub use log::*;

    #[cfg(feature = "defmt")]
    pub use defmt::*;

    // No-op macros when no logging feature is enabled
    #[cfg(not(any(feature = "std", feature = "defmt")))]
    macro_rules! trace {
        ($($arg:tt)*) => {};
    }

    #[cfg(not(any(feature = "std", feature = "defmt")))]
    macro_rules! debug {
        ($($arg:tt)*) => {};
    }

    #[cfg(not(any(feature = "std", feature = "defmt")))]
    macro_rules! info {
        ($($arg:tt)*) => {};
    }

    #[cfg(not(any(feature = "std", feature = "defmt")))]
    macro_rules! log_warn {
        ($($arg:tt)*) => {};
    }

    #[cfg(not(any(feature = "std", feature = "defmt")))]
    macro_rules! error {
        ($($arg:tt)*) => {};
    }
    #[cfg(not(any(feature = "std", feature = "defmt")))]
    pub(crate) use {debug, error, info, trace, log_warn as warn};
}
