//! Some useful utilities.

mod const_fns;

#[cfg(feature = "std")]
mod types;

pub use const_fns::*;

#[cfg(feature = "std")]
pub use types::*;
