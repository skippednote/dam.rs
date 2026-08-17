//! Probe and derivative pipeline: image, video, PDF, office, audio.
//!
//! Ingest starts here: [`sniff`] decides what a file actually is, because the client's
//! declaration is evidence and not authority.

pub mod ingest;
pub mod sandbox;
pub mod sniff;

pub use sniff::{MediaClass, Sniffed};
