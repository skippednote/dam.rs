//! Connector registry, webhook outbox, oEmbed, asset browser API.
//!
//! Today: the webhook sender and the browse token. `dam_db::webhooks` holds the outbox and decides what goes
//! out in what order; [`webhooks`] signs it, sends it, and classifies the answer. [`browse_token`] is the
//! short-lived credential a connected site mints for its own browser, signed with the same secret it signs
//! render URLs with — so a picker needs no long-lived key and no round trip.

pub mod browse_token;
pub mod webhooks;
