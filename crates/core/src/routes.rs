//! Connection routes of discovered hosts.
//!
//! A discovered host's session key carries the port-free overlay selector, so
//! layout positions survive a port change; the route that reaches it, with its
//! port, is registered here and applied wherever a connection or an attach
//! command is built from a key.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

static ROUTES: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

/// Registers `route` as the connection target of the host `key`.
pub fn set_route(key: &str, route: &str) {
    ROUTES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_or_insert_with(HashMap::new)
        .insert(key.to_owned(), route.to_owned());
}

/// The route to dial for the host `key`: the registered one, else `key`.
pub fn route_for(key: &str) -> String {
    ROUTES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .and_then(|routes| routes.get(key).cloned())
        .unwrap_or_else(|| key.to_owned())
}
