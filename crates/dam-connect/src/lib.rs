//! Connector registry, webhook outbox, oEmbed, asset browser API.
//!
//! Today: the webhook sender. `dam_db::webhooks` holds the outbox and decides what goes out in what order;
//! [`webhooks`] signs it, sends it, and classifies the answer.

pub mod webhooks;
