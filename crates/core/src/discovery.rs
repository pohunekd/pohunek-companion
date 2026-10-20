//! Remote hosts learned from the local daemon's `host.discover`.

use std::net::IpAddr;

use pohunek_client::protocol::method::HostDiscover;
use pohunek_client::protocol::{HostClass, HostDiscoverParams, HostRecord};
use pohunek_client::{ExternalIdentity, LOCAL_HOST, remote_host_with_port};

use crate::badge::sanitize;
use crate::config::{DaemonConfig, HostConfig};
use crate::connect::{connect_discover, socket_for_host};
use crate::error::CoreError;

/// A reachable peer the companion connects to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredHost {
    /// `<overlay>:<identity selector>`: the session key's host. It never
    /// carries the port or an address.
    pub key: String,
    /// The route that reaches the peer: the key with the record's port.
    pub route: String,
    /// Sanitized display name: the record's name, else its fqdn, else the key.
    pub label: String,
}

/// A peer whose daemon speaks a protocol version this client cannot use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncompatibleHost {
    pub key: String,
    pub label: String,
    pub protocol_version: u32,
}

/// What one `host.discover` answer asks the companion to do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscoveryPlan {
    /// Peers to connect, in record order, without duplicates.
    pub connect: Vec<DiscoveredHost>,
    /// Peers shown as incompatible; never connected.
    pub incompatible: Vec<IncompatibleHost>,
}

/// Asks the local daemon for its peers. A failure (no overlay configured, no
/// local daemon) is the caller's to log; local and configured hosts keep
/// working.
pub async fn discover_records(config: &DaemonConfig) -> Result<Vec<HostRecord>, CoreError> {
    let socket = socket_for_host(LOCAL_HOST)?;
    let mut client = connect_discover(&socket, config).await?;
    Ok(client
        .call::<HostDiscover>(HostDiscoverParams { force: false })
        .await?)
}

/// Derives the peers to follow from `records`. Only `reachable_daemon` records
/// with an IP address and an identity become connect targets (the rule of the
/// Pohunek CLI's host fan-out); `version_mismatch` records are reported as
/// incompatible; `unreachable` and `candidate` records are ignored. A record
/// that is a configured `[[hosts]]` entry is left to that entry, which keeps
/// its configured key.
pub fn plan(records: &[HostRecord], configured: &[HostConfig]) -> DiscoveryPlan {
    let mut plan = DiscoveryPlan::default();
    for record in records {
        let Some(key) = host_key(record) else {
            continue;
        };
        if is_configured(record, &key, configured) {
            continue;
        }
        let label = label(record, &key);
        match &record.class {
            HostClass::ReachableDaemon { .. } => {
                if !has_ip_address(record) || plan.connect.iter().any(|h| h.key == key) {
                    continue;
                }
                let Ok(route) = remote_host_with_port(&key, record.port) else {
                    continue;
                };
                plan.connect.push(DiscoveredHost { key, route, label });
            }
            HostClass::VersionMismatch {
                daemon_protocol_version,
            } => {
                if plan.incompatible.iter().any(|h| h.key == key) {
                    continue;
                }
                plan.incompatible.push(IncompatibleHost {
                    key,
                    label,
                    protocol_version: *daemon_protocol_version,
                });
            }
            HostClass::Unreachable | HostClass::Candidate => {}
        }
    }
    plan
}

fn has_ip_address(record: &HostRecord) -> bool {
    record
        .address
        .as_deref()
        .is_some_and(|address| address.parse::<IpAddr>().is_ok())
}

/// `<overlay>:<selector>` from the stable peer id, else the fqdn.
fn host_key(record: &HostRecord) -> Option<String> {
    let non_empty = |value: &Option<String>| {
        value
            .as_deref()
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    let identity = match (non_empty(&record.peer_id), non_empty(&record.fqdn)) {
        (Some(peer_id), _) => ExternalIdentity::peer_id(peer_id),
        (None, Some(fqdn)) => ExternalIdentity::fqdn(fqdn),
        (None, None) => return None,
    }
    .ok()?;
    if record.overlay.is_empty() {
        return None;
    }
    Some(format!("{}:{}", record.overlay, identity.selector()))
}

fn label(record: &HostRecord, key: &str) -> String {
    [record.name.as_deref(), record.fqdn.as_deref(), Some(key)]
        .into_iter()
        .flatten()
        .map(sanitize)
        .find(|label| !label.is_empty())
        .unwrap_or_else(|| sanitize(key))
}

/// Whether a configured entry names this record: its target, without an
/// explicit `@port`, equals the derived key or the record's peer id, fqdn or
/// name, bare or prefixed with the overlay.
fn is_configured(record: &HostRecord, key: &str, configured: &[HostConfig]) -> bool {
    let names: Vec<&str> = [
        record.peer_id.as_deref(),
        record.fqdn.as_deref(),
        record.name.as_deref(),
    ]
    .into_iter()
    .flatten()
    .filter(|name| !name.is_empty())
    .collect();
    configured.iter().any(|host| {
        let target = host
            .target
            .rsplit_once('@')
            .map_or(host.target.as_str(), |(selector, _)| selector);
        target == key
            || names.iter().any(|name| {
                target.eq_ignore_ascii_case(name)
                    || target
                        .strip_prefix(record.overlay.as_str())
                        .and_then(|rest| rest.strip_prefix(':'))
                        .is_some_and(|rest| rest.eq_ignore_ascii_case(name))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(name: &str, class: HostClass) -> HostRecord {
        HostRecord {
            name: Some(name.to_owned()),
            fqdn: Some(format!("{name}.example.net")),
            address: Some("100.92.30.40".to_owned()),
            port: 18722,
            overlay: "netbird".to_owned(),
            peer_id: Some(format!("key-{name}")),
            class,
        }
    }

    fn reachable() -> HostClass {
        HostClass::ReachableDaemon {
            daemon_version: "0.35.2".to_owned(),
        }
    }

    fn host(target: &str) -> HostConfig {
        HostConfig {
            target: target.to_owned(),
            label: None,
        }
    }

    #[test]
    fn reachable_peer_is_keyed_by_peer_id_and_routed_with_its_port() {
        let mut peer = record("air", reachable());
        peer.port = 28722;
        let plan = plan(&[peer], &[]);
        let identity = ExternalIdentity::peer_id("key-air").unwrap();
        assert_eq!(plan.connect.len(), 1);
        assert_eq!(
            plan.connect[0].key,
            format!("netbird:{}", identity.selector())
        );
        assert_eq!(
            plan.connect[0].route,
            format!("{}@28722", plan.connect[0].key)
        );
        assert_eq!(plan.connect[0].label, "air");
    }

    #[test]
    fn fqdn_is_the_identity_without_a_peer_id() {
        for peer_id in [None, Some(String::new())] {
            let mut peer = record("air", reachable());
            peer.peer_id = peer_id;
            let plan = plan(&[peer], &[]);
            let identity = ExternalIdentity::fqdn("air.example.net").unwrap();
            assert_eq!(
                plan.connect[0].key,
                format!("netbird:{}", identity.selector())
            );
        }
    }

    #[test]
    fn peer_without_identity_or_usable_address_is_not_connected() {
        let mut no_identity = record("a", reachable());
        no_identity.peer_id = None;
        no_identity.fqdn = None;
        let mut no_address = record("b", reachable());
        no_address.address = None;
        let mut bad_address = record("c", reachable());
        bad_address.address = Some("not-an-ip".to_owned());
        let mut zero_port = record("d", reachable());
        zero_port.port = 0;
        assert_eq!(
            plan(&[no_identity, no_address, bad_address, zero_port], &[]),
            DiscoveryPlan::default()
        );
    }

    #[test]
    fn only_version_mismatch_is_reported_and_unreachable_or_candidate_are_ignored() {
        let records = [
            record(
                "old",
                HostClass::VersionMismatch {
                    daemon_protocol_version: 2,
                },
            ),
            record("down", HostClass::Unreachable),
            record("none", HostClass::Candidate),
        ];
        let plan = plan(&records, &[]);
        assert!(plan.connect.is_empty());
        assert_eq!(plan.incompatible.len(), 1);
        assert_eq!(plan.incompatible[0].label, "old");
        assert_eq!(plan.incompatible[0].protocol_version, 2);
    }

    #[test]
    fn label_falls_back_to_fqdn_then_key_and_is_sanitized() {
        let mut peer = record("air", reachable());
        peer.name = Some("A\u{202e}ir\n".to_owned());
        assert_eq!(plan(&[peer.clone()], &[]).connect[0].label, "Air");
        peer.name = None;
        assert_eq!(
            plan(&[peer.clone()], &[]).connect[0].label,
            "air.example.net"
        );
        peer.fqdn = None;
        let plan = plan(&[peer], &[]);
        assert_eq!(plan.connect[0].label, plan.connect[0].key);
    }

    #[test]
    fn configured_entry_naming_the_same_host_keeps_its_key() {
        let peer = record("air", reachable());
        let key = host_key(&peer).unwrap();
        for target in [
            key.as_str(),
            "netbird:air",
            "netbird:air.example.net@28722",
            "netbird:key-air",
            "Air",
            "air.example.net",
        ] {
            assert!(
                plan(std::slice::from_ref(&peer), &[host(target)])
                    .connect
                    .is_empty(),
                "{target}"
            );
        }
        assert_eq!(plan(&[peer], &[host("netbird:other")]).connect.len(), 1);
    }

    #[test]
    fn duplicate_records_connect_once() {
        let peer = record("air", reachable());
        assert_eq!(plan(&[peer.clone(), peer], &[]).connect.len(), 1);
    }
}
