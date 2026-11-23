//! Android activity integration.

#[cfg(target_os = "android")]
mod platform;

#[cfg(any(target_os = "android", test))]
pub(crate) mod text;

#[cfg(target_os = "android")]
pub use platform::{Event, dispatch, run};

#[cfg(target_os = "android")]
pub(crate) use platform::Activity;
