//! Remote hosts learned from the local daemon's `host.discover`.

use std::net::IpAddr;

use pohunek_client::protocol::method::HostDiscover;
use pohunek_client::protocol::{HostClass, HostDiscoverParams, HostRecord};
use pohunek_client::{ExternalIdentity, LOCAL_HOST, remote_host_with_port};
use serde::{Deserialize, Serialize};

use crate::badge::sanitize;
use crate::config::{DaemonConfig, HostConfig};
use crate::connect::{connect_discover, host_naming_this_machine, socket_for_host};
use crate::error::CoreError;
use crate::routes::set_route;
use tracing::{debug, warn};

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
    /// `(configured target, route)` for a configured entry without an
    /// explicit port that names a reachable peer: the route carries the
    /// record's port.
    pub configured_routes: Vec<(String, String)>,
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
        if let Some(host) = configured_entry(record, &key, configured) {
            if matches!(record.class, HostClass::ReachableDaemon { .. })
                && !host.target.contains('@')
                && let Ok(route) = remote_host_with_port(&host.target, record.port)
            {
                plan.configured_routes.push((host.target.clone(), route));
            }
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

/// A remote host the New session dialog lists. The sidebar hands its list to
/// the dialog on the command line (see [`encode_dialog_hosts`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DialogHost {
    /// The session key's host: a configured target or a discovered key.
    pub target: String,
    /// The label the sidebar shows for the host.
    pub label: String,
    /// Why the host cannot be picked; `None` when it can.
    pub unavailable: Option<String>,
    /// The connection route registered for `target`; `None` when the target
    /// itself is the route.
    pub route: Option<String>,
}

/// Failure to read the dialog hosts from the command line.
#[derive(Debug, thiserror::Error)]
#[error("invalid dialog hosts: {0}")]
pub struct DialogHostsError(#[from] serde_json::Error);

/// The hosts as one command-line word.
pub fn encode_dialog_hosts(hosts: &[DialogHost]) -> Result<String, DialogHostsError> {
    Ok(serde_json::to_string(hosts)?)
}

/// Reads the word [`encode_dialog_hosts`] produced.
pub fn decode_dialog_hosts(arg: &str) -> Result<Vec<DialogHost>, DialogHostsError> {
    Ok(serde_json::from_str(arg)?)
}

/// Registers the routes of `hosts` so a connect to their targets dials the
/// right port.
pub fn register_dialog_routes(hosts: &[DialogHost]) {
    for host in hosts {
        if let Some(route) = &host.route {
            set_route(&host.target, route);
        }
    }
}

/// The hosts after Local in the dialog: configured entries, then the
/// discovered reachable peers, then the incompatible ones (listed, not
/// pickable).
pub fn dialog_hosts(configured: &[HostConfig], plan: &DiscoveryPlan) -> Vec<DialogHost> {
    let host = |target: &str, label: &str, unavailable: Option<String>, route: Option<String>| {
        DialogHost {
            target: target.to_owned(),
            label: label.to_owned(),
            unavailable,
            route,
        }
    };
    configured
        .iter()
        .map(|entry| {
            let route = plan
                .configured_routes
                .iter()
                .find(|(target, _)| *target == entry.target)
                .map(|(_, route)| route.clone());
            host(&entry.target, &entry.display_label(), None, route)
        })
        .chain(
            plan.connect
                .iter()
                .map(|found| host(&found.key, &found.label, None, Some(found.route.clone()))),
        )
        .chain(plan.incompatible.iter().map(|peer| {
            host(
                &peer.key,
                &peer.label,
                Some(format!(
                    "{}: incompatible daemon (protocol {})",
                    peer.label, peer.protocol_version
                )),
                None,
            )
        }))
        .collect()
}

/// The dialog's remote hosts: configured entries plus what the local daemon
/// discovers, with the connection routes of the discovered ones registered.
/// A peer that is this machine is left out, as in the sidebar. The second
/// value is why discovery failed, if it did; configured hosts stay listed.
pub async fn discover_dialog_hosts(
    config: &DaemonConfig,
    configured: &[HostConfig],
) -> (Vec<DialogHost>, Option<String>) {
    let (mut plan, failure) = match discover_records(config).await {
        Ok(records) => (plan(&records, configured), None),
        Err(err) => (DiscoveryPlan::default(), Some(err.to_string())),
    };
    for (target, route) in &plan.configured_routes {
        set_route(target, route);
    }
    for found in &plan.connect {
        set_route(&found.key, &found.route);
    }
    // The checks run concurrently so slow peers do not add up before the
    // dialog appears.
    let mut checks = tokio::task::JoinSet::new();
    for (index, found) in std::mem::take(&mut plan.connect).into_iter().enumerate() {
        let config = config.clone();
        checks.spawn(async move {
            let named = host_naming_this_machine(std::slice::from_ref(&found.key), &config).await;
            (index, found, named)
        });
    }
    let mut checked = Vec::new();
    while let Some(joined) = checks.join_next().await {
        match joined {
            Ok(done) => checked.push(done),
            Err(err) => warn!(error = %err, "check of a discovered host did not finish"),
        }
    }
    checked.sort_by_key(|(index, ..)| *index);
    let mut remote = Vec::new();
    for (_, found, named) in checked {
        match named {
            Ok(None) => remote.push(found),
            Ok(Some(_)) => debug!(host = %found.key, "discovered peer is this machine"),
            // An unverifiable peer is not offered: a create on this machine
            // under a remote key would be placed under the wrong host.
            Err(err) => warn!(host = %found.key, error = %err, "cannot check the discovered host"),
        }
    }
    plan.connect = remote;
    (dialog_hosts(configured, &plan), failure)
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

/// The configured entry that names this record: its target, without an
/// explicit `@port`, equals the derived key or the record's peer id, fqdn or
/// name, bare or prefixed with the overlay.
fn configured_entry<'a>(
    record: &HostRecord,
    key: &str,
    configured: &'a [HostConfig],
) -> Option<&'a HostConfig> {
    let names: Vec<&str> = [
        record.peer_id.as_deref(),
        record.fqdn.as_deref(),
        record.name.as_deref(),
    ]
    .into_iter()
    .flatten()
    .filter(|name| !name.is_empty())
    .collect();
    configured.iter().find(|host| {
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
    fn configured_entry_without_a_port_dials_the_records_port() {
        let mut peer = record("air", reachable());
        peer.port = 28722;
        let routes = plan(&[peer.clone()], &[host("Air"), host("netbird:air@9")]).configured_routes;
        assert_eq!(routes, [("Air".to_owned(), "Air@28722".to_owned())]);
    }

    #[test]
    fn dialog_lists_configured_then_discovered_then_incompatible() {
        let records = [
            record("air", reachable()),
            record(
                "old",
                HostClass::VersionMismatch {
                    daemon_protocol_version: 2,
                },
            ),
        ];
        let configured = [HostConfig {
            target: "box".to_owned(),
            label: Some("Box".to_owned()),
        }];
        let hosts = dialog_hosts(&configured, &plan(&records, &configured));
        let summary: Vec<_> = hosts
            .iter()
            .map(|h| (h.label.as_str(), h.unavailable.is_some()))
            .collect();
        assert_eq!(summary, [("Box", false), ("air", false), ("old", true)]);
        assert_eq!(hosts[0].target, "box");
        assert!(
            hosts[2]
                .unavailable
                .as_deref()
                .unwrap()
                .contains("protocol 2")
        );
    }

    #[test]
    fn dialog_hosts_keep_their_routes_across_the_command_line() {
        let mut peer = record("air", reachable());
        peer.port = 28722;
        let configured = [host("box")];
        let hosts = dialog_hosts(&configured, &plan(&[peer], &configured));
        assert!(hosts[0].route.is_none());
        assert!(hosts[1].route.as_deref().unwrap().ends_with("@28722"));
        let decoded = decode_dialog_hosts(&encode_dialog_hosts(&hosts).unwrap()).unwrap();
        assert_eq!(decoded, hosts);
    }

    #[test]
    fn duplicate_records_connect_once() {
        let peer = record("air", reachable());
        assert_eq!(plan(&[peer.clone(), peer], &[]).connect.len(), 1);
    }
}
