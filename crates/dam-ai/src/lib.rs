//! Local inference (ONNX), hosted-model clients, enrichment DAG.
//!
//! The hosted half is M5: [`model`] is the seam, [`anthropic`] and [`openai_compatible`] are the two clients
//! that cover the field, and [`testing`] is the recorded transport their suites drive. Local inference (M4) is
//! the other half and is deliberately later — see TASKS.md on why hosted models came first.

pub mod anthropic;
pub mod credential;
pub mod http;
pub mod model;
pub mod openai_compatible;
pub mod pricing;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
