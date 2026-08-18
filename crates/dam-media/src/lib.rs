//! Probe and derivative pipeline: image, video, PDF, office, audio.
//!
//! Ingest starts here: [`sniff`] decides what a file actually is, because the client's
//! declaration is evidence and not authority.

// Unit tests assert on values that are known-good by construction, so the panic lints that keep
// production code honest only add noise there. Matches dam-store.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]

pub mod derive;
pub mod ingest;
pub mod probe;
pub mod proxy;
pub mod sandbox;
pub mod sniff;
pub mod vips;

pub use sniff::{MediaClass, Sniffed};
