//! #229: this node's place in the node exchange, read live from its settings.
//!
//! - `node_name` (`snv`, `pp`): empty = the exchange is off (no serving, no asking).
//! - `peer_api_key`: the key this node's peer API accepts (`X-SP-Peer-Key`);
//!   empty = the peer API answers 404.
//! - `peers`: a JSON list of [`PeerConfig`] — the nodes asked first.
//!
//! The secrets (keys, a Cloudflare Access client secret) never leave the node
//! in clear: `GET /api/v1/settings` (`api::settings`) shows each peer's secrets
//! as [`MASK`] ([`shown_peers`]); a PATCH that sends a masked peer back keeps
//! the stored secret ([`unmask_peers`]); [`checked`] refuses an exchange
//! setting that does not hold; `Debug` prints the mask; no error text echoes
//! a secret or the `peers` text.

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
