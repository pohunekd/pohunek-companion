//! Keeps a [`SessionStore`] current against the local daemon: subscribe,
//! seed, stream events, and reconnect with exponential backoff.

use std::time::{Duration, Instant};

use pohunek_client::Subscription;
use pohunek_client::protocol::Event;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout_at;
use tracing::{info, warn};

use crate::config::{DaemonConfig, HostConfig};
use crate::connect::{fetch_seed, socket_for_host, subscribe};
use crate::discovery::DiscoveredHost;
use crate::error::CoreError;
use crate::model::{Applied, SessionStore};
use crate::routes::set_route;
use crate::session::SessionRow;
use pohunek_client::LOCAL_HOST;

/// What the sidebar should show next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveUpdate {
    /// The store is seeded and the event stream is attached.
    Connected,
    /// The full current session list, ordered by session id; sent only when it
    /// differs from the previous one.
    Sessions(Vec<SessionRow>),
    /// The daemon connection is down; `attempt` counts consecutive failures.
    /// `code` is the daemon's protocol error code when it refused the
    /// connection with one.
    Reconnecting {
        attempt: u32,
        reason: String,
        code: Option<String>,
    },
    /// The host is no longer followed because discovery stopped listing it;
    /// `reason` is shown as its status. Never sent by [`run_live`].
    Lost { reason: String },
}

type EventMessage = Result<Event, CoreError>;

/// Aborts the event reader when a connection cycle ends.
struct ReaderGuard(JoinHandle<()>);

impl Drop for ReaderGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The daemon one [`run_live`] loop follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveHost {
    /// `local`, or the configured target verbatim: the session keys' host and
    /// the route handed to the client.
    pub target: String,
    /// Sanitized name shown on the rows of a remote host; `None` for local.
    pub label: Option<String>,
}

impl LiveHost {
    pub fn local() -> Self {
        Self {
            target: LOCAL_HOST.to_owned(),
            label: None,
        }
    }

    /// A discovered peer: followed under its port-free key, dialled through
    /// the registered route.
    pub fn discovered(host: &DiscoveredHost) -> Self {
        set_route(&host.key, &host.route);
        Self {
            target: host.key.clone(),
            label: Some(host.label.clone()),
        }
    }

    pub fn remote(host: &HostConfig) -> Self {
        Self {
            target: host.target.clone(),
            label: Some(host.display_label()),
        }
    }
}

/// Runs for the life of the process: connects, keeps the list current through
/// `publish`, and reconnects after every failure or closed stream. Hosts are
/// independent: each runs its own loop with its own seed, stream, re-seed
/// timer and backoff.
pub async fn run_live(
    host: &LiveHost,
    config: &DaemonConfig,
    idle_threshold: Duration,
    mut publish: impl FnMut(LiveUpdate),
) {
    let mut cycle: u64 = 0;
    let mut failures: u32 = 0;
    loop {
        cycle += 1;
        let attempt = failures + 1;
        info!(cycle, attempt, host = %host.target, "connecting to the daemon");
        let mut store = SessionStore::new(host.target.clone());
        let mut established_at = None;
        let error = run_cycle(
            host,
            config,
            idle_threshold,
            &mut store,
            &mut established_at,
            &mut publish,
        )
        .await;
        let report = store.take_report();
        if let Some(since) = established_at {
            // A connection that dropped soon after it was established keeps
            // the backoff growing, so a flapping daemon is not hit at the
            // initial interval forever.
            if since.elapsed() >= config.reconnect_max() {
                failures = 0;
            }
            warn!(cycle, attempt, host = %host.target, outcome = "disconnected", error = %error, report = ?report,
                "daemon connection lost");
        } else {
            warn!(cycle, attempt, host = %host.target, outcome = "failed", error = %error, "daemon connection failed");
        }
        failures += 1;
        publish(LiveUpdate::Reconnecting {
            attempt: failures,
            reason: error.to_string(),
            code: error.protocol_code().map(str::to_owned),
        });
        let delay = backoff(config, failures);
        info!(
            cycle,
            next_attempt = failures,
            delay_ms = delay.as_millis() as u64,
            "waiting before reconnecting"
        );
        tokio::time::sleep(delay).await;
    }
}

/// Wait before retry number `failures` (1-based): the initial wait doubled per
/// earlier failure, capped at the configured maximum.
fn backoff(config: &DaemonConfig, failures: u32) -> Duration {
    let shift = failures.saturating_sub(1).min(32);
    let millis = config
        .reconnect_initial_ms
        .saturating_mul(1u64 << shift)
        .min(config.reconnect_max_ms);
    Duration::from_millis(millis)
}

/// One connection: subscribe first so no event is lost, seed, apply what was
/// buffered meanwhile, then stream. Only returns with the reason it ended.
async fn run_cycle(
    host: &LiveHost,
    config: &DaemonConfig,
    idle_threshold: Duration,
    store: &mut SessionStore,
    established_at: &mut Option<Instant>,
    publish: &mut impl FnMut(LiveUpdate),
) -> CoreError {
    match cycle_inner(host, config, idle_threshold, store, established_at, publish).await {
        Ok(never) => match never {},
        Err(err) => err,
    }
}

async fn cycle_inner(
    host: &LiveHost,
    config: &DaemonConfig,
    idle_threshold: Duration,
    store: &mut SessionStore,
    established_at: &mut Option<Instant>,
    publish: &mut impl FnMut(LiveUpdate),
) -> Result<std::convert::Infallible, CoreError> {
    let socket = socket_for_host(&host.target)?;
    let subscription = subscribe(&host.target, &socket, config).await?;
    let (sender, mut events) = mpsc::unbounded_channel();
    let _reader = ReaderGuard(tokio::spawn(read_events(subscription, sender)));

    let seed = fetch_seed(&host.target, &socket, config).await?;
    info!(
        host = %host.target,
        sessions = seed.sessions.len(),
        notifications = seed.notifications.len(),
        "seeded from the daemon"
    );
    store.apply_seed(seed.sessions, seed.notifications);
    while let Ok(message) = events.try_recv() {
        store.apply_event(&message?);
    }
    *established_at = Some(Instant::now());
    info!(host = %host.target, "daemon connection established");
    let mut shown = rows(host, store, idle_threshold);
    publish(LiveUpdate::Connected);
    publish(LiveUpdate::Sessions(shown.clone()));

    let interval = config.reseed_interval();
    let mut next_reseed = tokio::time::Instant::now() + interval;
    loop {
        // The idle timer exists only while some session waits on the idle
        // threshold; otherwise the loop wakes for the next event or re-seed.
        let idle_deadline = store.next_idle_deadline(Instant::now(), idle_threshold);
        let applied = tokio::select! {
            received = timeout_at(next_reseed, events.recv()) => match received {
                Ok(message) => {
                    let event = message.ok_or(CoreError::StreamClosed)??;
                    store.apply_event(&event) == Applied::Applied
                }
                Err(_elapsed) => {
                    // Events arriving during the fetch wait in the channel and
                    // are applied after the seed, as on connect.
                    let reseeded = reseed(host, config, &socket, store, &mut events).await?;
                    next_reseed = tokio::time::Instant::now() + interval;
                    reseeded
                }
            },
            () = idle_deadline_reached(idle_deadline) => true,
        };
        if applied {
            let current = rows(host, store, idle_threshold);
            if current != shown {
                shown = current;
                publish(LiveUpdate::Sessions(shown.clone()));
            }
        }
    }
}

/// Re-reads the seed and reconciles the store with it, then applies the events
/// buffered meanwhile. A failed fetch is logged and leaves the store and the
/// subscription untouched. Returns whether the store may have changed; an
/// error means the event stream failed.
async fn reseed(
    host: &LiveHost,
    config: &DaemonConfig,
    socket: &std::path::Path,
    store: &mut SessionStore,
    events: &mut mpsc::UnboundedReceiver<EventMessage>,
) -> Result<bool, CoreError> {
    let seed = match fetch_seed(&host.target, socket, config).await {
        Ok(seed) => seed,
        Err(err) => {
            warn!(error = %err, "periodic re-seed failed, retrying at the next interval");
            return Ok(false);
        }
    };
    let sessions = seed.sessions.len();
    let change = store.apply_seed(seed.sessions, seed.notifications);
    while let Ok(message) = events.try_recv() {
        store.apply_event(&message?);
    }
    if change.is_empty() {
        info!(sessions, "periodic re-seed found no difference");
    } else {
        warn!(
            sessions,
            added = change.added,
            removed = change.removed,
            changed = change.changed,
            attention = change.attention,
            "periodic re-seed corrected the session list"
        );
    }
    Ok(true)
}

/// Forwards the subscription's events; the final message is the reason the
/// stream ended.
async fn read_events(mut subscription: Subscription, sender: mpsc::UnboundedSender<EventMessage>) {
    loop {
        let message = match subscription.next_event().await {
            Ok(Some(event)) => Ok(event),
            Ok(None) => Err(CoreError::StreamClosed),
            Err(err) => Err(err.into()),
        };
        let last = message.is_err();
        if sender.send(message).is_err() || last {
            return;
        }
    }
}

/// Completes at `deadline`; never completes without one.
async fn idle_deadline_reached(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}

fn rows(host: &LiveHost, store: &SessionStore, idle_threshold: Duration) -> Vec<SessionRow> {
    let now = Instant::now();
    let mut rows: Vec<SessionRow> = store
        .sessions_with_state(now, idle_threshold)
        .map(|(_, info, owner_state)| {
            let waiting_since = store.waiting_since(&info.id);
            let known_label = info
                .project_id
                .as_deref()
                .and_then(|id| store.project_label(id));
            SessionRow::from_host(
                &host.target,
                host.label.as_deref(),
                info.clone(),
                owner_state,
                waiting_since,
                known_label,
            )
        })
        .collect();
    rows.sort_by(|a, b| a.key.session_id.cmp(&b.key.session_id));
    rows
}
