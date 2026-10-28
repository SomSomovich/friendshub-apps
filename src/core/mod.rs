//! Native integration with `friendshub-core`.
//!
//! The core owns networking, account authentication, SQLCipher-backed
//! persistence, and cryptographic operations. This module provides a safe
//! application-facing handle and runs the core's synchronous calls away from
//! the Slint event loop.

mod client;
mod events;

#[path = "../platform/storage.rs"]
pub mod storage;

pub use client::{CoreCallFailure, CoreClient};
pub use events::CoreEventHandler;
