//! Request-bus helpers (SQLite `requests` table of the SaveLens store).
//!
//! Requests are consumed selectively with [`Store::take_requests_for`]: the
//! overlay only takes `toggle-overlay` requests and the daemon only takes its
//! own actions, so neither process can swallow the other's requests.

use savelens_cas::{CasResult, Store};

/// Action name of "show / hide the overlay" requests.
pub const TOGGLE_OVERLAY: &str = "toggle-overlay";

/// Takes (and marks handled) the pending requests whose action is `action`,
/// oldest first, as `(id, payload)`; every other request stays pending.
pub fn take_requests_matching(store: &Store, action: &str) -> CasResult<Vec<(i64, serde_json::Value)>> {
    Ok(store.take_requests_for(&[action])?.into_iter().map(|(id, _, payload)| (id, payload)).collect())
}

/// Queues a `toggle-overlay` request (used by other front-ends / tests).
pub fn request_toggle_overlay(store: &Store) -> CasResult<i64> {
    store.push_request(TOGGLE_OVERLAY, &serde_json::json!({}))
}
