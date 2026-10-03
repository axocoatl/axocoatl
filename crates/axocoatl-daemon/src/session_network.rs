//! One network record writer per Session, and the read view behind
//! `GET /api/sessions/{id}/network`.
//!
//! Each open record is owned by a dedicated thread. Appends and reads are
//! messages to it, so the record has exactly one writer and the caller learns
//! the sequence number only after the line is in the file. The thread syncs
//! the file every 200 ms while it is dirty, and on close.

use std::collections::HashMap;
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axocoatl_session::execution_namespace::OwnedExecutionNamespace;
use axocoatl_session::network_record::{
    NetworkEvent, NetworkLine, NetworkRecord, NetworkRecordError, RecordLimits, RecordStats,
    ScreenshotRef, DEFAULT_MAX_BYTES, MAX_READ_LIMIT,
};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

/// How often a dirty record is synced to stable storage.
pub const RECORD_SYNC_INTERVAL: Duration = Duration::from_millis(200);
/// Default number of lines one read returns.
pub const DEFAULT_READ_LIMIT: usize = 200;

/// Where Session records come from. The daemon implementation asks the
/// Session dispatch registry, which owns each native Session's canonical
/// store.
pub(crate) trait RecordNamespaces: Send + Sync + 'static {
    /// The record's component namespace for its single writer, created on
    /// first use. Refused while the Session is closing.
    fn writer_namespace(&self, session_id: &str) -> Result<OwnedExecutionNamespace, String>;

    /// Read an existing record without opening a writer or creating one.
    /// `Ok(None)` when the Session has no record.
    fn read_existing(
        &self,
        session_id: &str,
        after: Option<u64>,
        limit: usize,
        limits: RecordLimits,
    ) -> Result<Option<(Vec<NetworkLine>, RecordStats)>, String>;

    /// Read a stored screenshot without opening a writer. `Ok(None)` when the
    /// Session has no record or no such screenshot.
    fn read_screenshot(
        &self,
        _session_id: &str,
        _sha256: &str,
    ) -> Result<Option<(String, Vec<u8>)>, String> {
        Ok(None)
    }
}

/// [`RecordNamespaces::writer_namespace`] for one canonical Session store.
pub(crate) fn writer_namespace(
    canonical: &axocoatl_session::execution_store::SessionExecutionStore,
) -> Result<OwnedExecutionNamespace, String> {
    canonical
        .component_namespace(
            axocoatl_session::execution_namespace::ExecutionComponent::NetworkRecord,
        )
        .map_err(|error| error.to_string())
}

/// [`RecordNamespaces::read_existing`] for one canonical Session store.
pub(crate) fn read_existing(
    canonical: &axocoatl_session::execution_store::SessionExecutionStore,
    after: Option<u64>,
    limit: usize,
    limits: RecordLimits,
) -> Result<Option<(Vec<NetworkLine>, RecordStats)>, String> {
    NetworkRecord::read_existing(canonical, after, limit, limits).map_err(|error| error.to_string())
}

/// [`RecordNamespaces::read_screenshot`] for one canonical Session store.
pub(crate) fn read_screenshot(
    canonical: &axocoatl_session::execution_store::SessionExecutionStore,
    sha256: &str,
) -> Result<Option<(String, Vec<u8>)>, String> {
    NetworkRecord::read_screenshot_existing(canonical, sha256).map_err(|error| error.to_string())
}

#[derive(Debug, thiserror::Error)]
pub enum RecordServiceError {
    #[error("network record for Session '{session}' is unavailable: {reason}")]
    Unavailable { session: String, reason: String },
    #[error(transparent)]
    Record(#[from] NetworkRecordError),
    #[error("network record writer stopped")]
    Stopped,
}

impl RecordServiceError {
    /// Whether the record refused because it reached its cap.
    pub fn is_full(&self) -> bool {
        matches!(self, Self::Record(NetworkRecordError::Full))
    }
}

/// One page of a Session's record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordPage {
    pub events: Vec<NetworkLine>,
    pub stats: RecordStats,
    /// Pass back as `after` to continue. Absent when nothing was read and no
    /// `after` was given.
    pub next_after: Option<u64>,
}

type Reply<T> = oneshot::Sender<Result<T, NetworkRecordError>>;

enum Command {
    Append {
        ts_ms: u64,
        event: Box<NetworkEvent>,
        control: bool,
        reply: Reply<u64>,
    },
    Read {
        after: Option<u64>,
        limit: usize,
        reply: Reply<RecordPage>,
    },
    StoreScreenshot {
        media_type: String,
        bytes: Vec<u8>,
        reply: Reply<ScreenshotRef>,
    },
    Close {
        reply: oneshot::Sender<()>,
    },
}

struct Writer {
    sender: std_mpsc::Sender<Command>,
}

/// Daemon-wide owner of every open Session network record.
pub struct SessionNetworkRecords {
    namespaces: Arc<dyn RecordNamespaces>,
    limits: RecordLimits,
    writers: tokio::sync::Mutex<HashMap<String, Writer>>,
}

impl std::fmt::Debug for SessionNetworkRecords {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionNetworkRecords")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn empty_stats(limits: RecordLimits) -> RecordStats {
    RecordStats {
        events: 0,
        bytes: 0,
        last_seq: 0,
        full: false,
        gaps: 0,
        max_events: limits.max_events,
    }
}

fn page(
    record: &NetworkRecord,
    after: Option<u64>,
    limit: usize,
) -> Result<RecordPage, NetworkRecordError> {
    let events = record.read_after(after, limit)?;
    let next_after = events.last().map(|line| line.seq).or(after);
    Ok(RecordPage {
        events,
        stats: record.stats(),
        next_after,
    })
}

fn run_writer(mut record: NetworkRecord, commands: std_mpsc::Receiver<Command>, session: String) {
    let mut last_sync = Instant::now();
    let mut failed: Option<String> = None;
    loop {
        let wait = if record.is_dirty() {
            RECORD_SYNC_INTERVAL.saturating_sub(last_sync.elapsed())
        } else {
            Duration::from_secs(3600)
        };
        match commands.recv_timeout(wait) {
            Ok(Command::Append {
                ts_ms,
                event,
                control,
                reply,
            }) => {
                let result = match &failed {
                    Some(_) => Err(NetworkRecordError::Poisoned),
                    None if control => record.append_control(ts_ms, *event),
                    None => record.append(ts_ms, *event),
                };
                let _ = reply.send(result);
            }
            Ok(Command::Read {
                after,
                limit,
                reply,
            }) => {
                let _ = reply.send(page(&record, after, limit));
            }
            Ok(Command::StoreScreenshot {
                media_type,
                bytes,
                reply,
            }) => {
                let result = match &failed {
                    Some(_) => Err(NetworkRecordError::Poisoned),
                    None => record.store_screenshot(&media_type, &bytes),
                };
                let _ = reply.send(result);
            }
            Ok(Command::Close { reply }) => {
                if let Err(error) = record.sync() {
                    tracing::error!(session = %session, %error, "network record final sync failed");
                }
                drop(record);
                let _ = reply.send(());
                return;
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                if let Err(error) = record.sync() {
                    tracing::error!(session = %session, %error, "network record final sync failed");
                }
                return;
            }
        }
        if record.is_dirty() && last_sync.elapsed() >= RECORD_SYNC_INTERVAL && failed.is_none() {
            if let Err(error) = record.sync() {
                // Later appends fail closed; the caller refuses the connection.
                tracing::error!(session = %session, %error, "network record sync failed");
                failed = Some(error.to_string());
            }
            last_sync = Instant::now();
        }
    }
}

impl SessionNetworkRecords {
    pub(crate) fn new(namespaces: Arc<dyn RecordNamespaces>, max_events: u64) -> Self {
        Self {
            namespaces,
            limits: RecordLimits {
                max_events: max_events.max(1),
                max_bytes: DEFAULT_MAX_BYTES,
            },
            writers: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn limits(&self) -> RecordLimits {
        self.limits
    }

    /// The writer for `session`, opening (and if needed creating) its record.
    async fn writer(&self, session: &str) -> Result<std_mpsc::Sender<Command>, RecordServiceError> {
        let mut writers = self.writers.lock().await;
        if let Some(writer) = writers.get(session) {
            return Ok(writer.sender.clone());
        }
        let unavailable = |reason: String| RecordServiceError::Unavailable {
            session: session.to_string(),
            reason,
        };
        let namespaces = self.namespaces.clone();
        let limits = self.limits;
        let owned_session = session.to_string();
        let record = tokio::task::spawn_blocking(move || {
            let namespace = namespaces.writer_namespace(&owned_session)?;
            NetworkRecord::open(namespace, limits).map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| unavailable(error.to_string()))?
        .map_err(unavailable)?;
        let (sender, receiver) = std_mpsc::channel();
        let thread_session = session.to_string();
        std::thread::Builder::new()
            .name("axocoatl-network-record".into())
            .spawn(move || run_writer(record, receiver, thread_session))
            .map_err(|error| unavailable(error.to_string()))?;
        writers.insert(
            session.to_string(),
            Writer {
                sender: sender.clone(),
            },
        );
        Ok(sender)
    }

    async fn send_append(
        &self,
        session: &str,
        event: NetworkEvent,
        control: bool,
    ) -> Result<u64, RecordServiceError> {
        let sender = self.writer(session).await?;
        let (reply, receive) = oneshot::channel();
        sender
            .send(Command::Append {
                ts_ms: now_ms(),
                event: Box::new(event),
                control,
                reply,
            })
            .map_err(|_| RecordServiceError::Stopped)?;
        Ok(receive.await.map_err(|_| RecordServiceError::Stopped)??)
    }

    /// Append an ordinary event and return its sequence number once the line
    /// is written. Creates the record on first use.
    pub async fn append(
        &self,
        session: &str,
        event: NetworkEvent,
    ) -> Result<u64, RecordServiceError> {
        self.send_append(session, event, false).await
    }

    /// Append a `policy`, `sidecar`, `unbind` or `limit` event, which may use
    /// the headroom reserved past the cap.
    pub async fn append_control(
        &self,
        session: &str,
        event: NetworkEvent,
    ) -> Result<u64, RecordServiceError> {
        self.send_append(session, event, true).await
    }

    /// Lines after `after`. Reads through the open writer, or else reads the
    /// stored record without opening one. A Session with no record reads as
    /// empty and is not given one.
    pub async fn read_after(
        &self,
        session: &str,
        after: Option<u64>,
        limit: usize,
    ) -> Result<RecordPage, RecordServiceError> {
        let limit = limit.clamp(1, MAX_READ_LIMIT);
        let open = self
            .writers
            .lock()
            .await
            .get(session)
            .map(|writer| writer.sender.clone());
        if let Some(sender) = open {
            let (reply, receive) = oneshot::channel();
            if sender
                .send(Command::Read {
                    after,
                    limit,
                    reply,
                })
                .is_ok()
            {
                if let Ok(page) = receive.await {
                    return Ok(page?);
                }
            }
        }
        let namespaces = self.namespaces.clone();
        let limits = self.limits;
        let owned_session = session.to_string();
        let stored = tokio::task::spawn_blocking(move || {
            namespaces.read_existing(&owned_session, after, limit, limits)
        })
        .await
        .map_err(|error| RecordServiceError::Unavailable {
            session: session.to_string(),
            reason: error.to_string(),
        })?
        .map_err(|reason| RecordServiceError::Unavailable {
            session: session.to_string(),
            reason,
        })?;
        Ok(match stored {
            Some((events, stats)) => {
                let next_after = events.last().map(|line| line.seq).or(after);
                RecordPage {
                    events,
                    stats,
                    next_after,
                }
            }
            None => RecordPage {
                events: Vec::new(),
                stats: empty_stats(self.limits),
                next_after: after,
            },
        })
    }

    /// Keep a screenshot beside the Session's record, through its single
    /// writer. Creates the record on first use.
    pub async fn store_screenshot(
        &self,
        session: &str,
        media_type: &str,
        bytes: Vec<u8>,
    ) -> Result<ScreenshotRef, RecordServiceError> {
        let sender = self.writer(session).await?;
        let (reply, receive) = oneshot::channel();
        sender
            .send(Command::StoreScreenshot {
                media_type: media_type.to_string(),
                bytes,
                reply,
            })
            .map_err(|_| RecordServiceError::Stopped)?;
        Ok(receive.await.map_err(|_| RecordServiceError::Stopped)??)
    }

    /// A stored screenshot's media type and bytes. Screenshots are written
    /// once and never change, so this reads them from disk without a writer.
    pub async fn read_screenshot(
        &self,
        session: &str,
        sha256: &str,
    ) -> Result<Option<(String, Vec<u8>)>, RecordServiceError> {
        let namespaces = self.namespaces.clone();
        let owned_session = session.to_string();
        let sha256 = sha256.to_string();
        tokio::task::spawn_blocking(move || namespaces.read_screenshot(&owned_session, &sha256))
            .await
            .map_err(|error| RecordServiceError::Unavailable {
                session: session.to_string(),
                reason: error.to_string(),
            })?
            .map_err(|reason| RecordServiceError::Unavailable {
                session: session.to_string(),
                reason,
            })
    }

    /// Current counts, without creating a record.
    pub async fn stats(&self, session: &str) -> Result<RecordStats, RecordServiceError> {
        Ok(self.read_after(session, Some(u64::MAX), 1).await?.stats)
    }

    /// Sync and close one Session's record, if open.
    pub async fn close(&self, session: &str) {
        let writer = self.writers.lock().await.remove(session);
        if let Some(writer) = writer {
            let (reply, receive) = oneshot::channel();
            if writer.sender.send(Command::Close { reply }).is_ok() {
                let _ = receive.await;
            }
        }
    }

    /// Sync and close every open record.
    pub async fn close_all(&self) {
        let sessions: Vec<String> = self.writers.lock().await.keys().cloned().collect();
        for session in sessions {
            self.close(&session).await;
        }
    }

    /// Whether a writer is open for `session`.
    pub async fn is_open(&self, session: &str) -> bool {
        self.writers.lock().await.contains_key(session)
    }
}

/// Egress sidecar state for the network view. Present once a sidecar runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarView {
    pub state: String,
    pub generation: u32,
    pub restarts: u32,
}

/// One compiled policy rule for the network view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRuleView {
    pub id: String,
    pub text: String,
    pub source: String,
}

/// One scope's compiled policy for the network view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyView {
    pub scope: String,
    pub revision: u64,
    pub digest: String,
    pub rules: Vec<PolicyRuleView>,
}

/// Record counts for the network view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordSummary {
    pub events: u64,
    pub bytes: u64,
    pub max_events: u64,
    pub full: bool,
    pub gaps: u64,
}

impl From<RecordStats> for RecordSummary {
    fn from(stats: RecordStats) -> Self {
        Self {
            events: stats.events,
            bytes: stats.bytes,
            max_events: stats.max_events,
            full: stats.full,
            gaps: stats.gaps,
        }
    }
}

/// Body of `GET /api/sessions/{id}/network`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionNetworkView {
    pub session_id: String,
    /// `sandbox.network`: `bridge`, `none` or `egress`.
    pub mode: String,
    pub sidecar: Option<SidecarView>,
    pub policies: Vec<PolicyView>,
    pub private_destinations: Vec<String>,
    pub record: RecordSummary,
    pub events: Vec<NetworkLine>,
    pub next_after: Option<u64>,
    /// Things the person should know about this Session's network, such as
    /// a configuration file inside its Workspace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// Agents' requests for hosts (`request_network_access`), pending ones
    /// first. Only a person approves or rejects them.
    #[serde(default)]
    pub proposals: Vec<crate::session_network_proposals::ProposalView>,
}

/// Body of `POST /api/sessions/{id}/network/allow`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkAllowRequest {
    /// The person's id for this change; a resend with the same id is refused.
    pub command_id: String,
    /// `session` or `browser`.
    pub scope: String,
    /// One exact host name: no wildcard, IP address or private range.
    pub host: String,
    /// Defaults to `[443]`.
    #[serde(default)]
    pub ports: Option<Vec<u16>>,
}

/// Body of `POST /api/sessions/{id}/network/revoke`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkRevokeRequest {
    pub command_id: String,
    pub scope: String,
    pub host: String,
}

/// Answer to an allow or revoke: the scope's new policy revision and digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkPolicyChanged {
    pub revision: u64,
    pub digest: String,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use axocoatl_session::execution_ownership::{LegacyFormatOwnership, UpgradedFormatOwnership};
    use axocoatl_session::execution_store::{ExecutionStoreOwner, SessionExecutionStore};
    use axocoatl_session::network_record::{LimitKind, SidecarState};
    use axocoatl_session::turn_contract::SessionId;
    use std::sync::Mutex;

    /// Real Session stores for record tests, by Session id.
    pub(crate) struct Stores {
        _root: tempfile::TempDir,
        _ownership: Arc<UpgradedFormatOwnership>,
        stores: Mutex<HashMap<String, SessionExecutionStore>>,
    }

    impl Stores {
        pub(crate) fn new(sessions: &[&str]) -> Arc<Self> {
            let root = tempfile::tempdir().unwrap();
            let ownership = Arc::new(
                LegacyFormatOwnership::acquire(root.path())
                    .unwrap()
                    .upgrade()
                    .unwrap(),
            );
            let stores = sessions
                .iter()
                .map(|session| {
                    (
                        (*session).to_string(),
                        SessionExecutionStore::open(
                            ownership.clone(),
                            ExecutionStoreOwner {
                                workspace_id: "workspace".into(),
                                session_id: SessionId::new(*session).unwrap(),
                            },
                        )
                        .unwrap(),
                    )
                })
                .collect();
            Arc::new(Self {
                _root: root,
                _ownership: ownership,
                stores: Mutex::new(stores),
            })
        }
    }

    impl RecordNamespaces for Stores {
        fn writer_namespace(&self, session_id: &str) -> Result<OwnedExecutionNamespace, String> {
            let stores = self.stores.lock().unwrap();
            let store = stores.get(session_id).ok_or("not a native Session")?;
            writer_namespace(store)
        }

        fn read_existing(
            &self,
            session_id: &str,
            after: Option<u64>,
            limit: usize,
            limits: RecordLimits,
        ) -> Result<Option<(Vec<NetworkLine>, RecordStats)>, String> {
            let stores = self.stores.lock().unwrap();
            let store = stores.get(session_id).ok_or("not a native Session")?;
            read_existing(store, after, limit, limits)
        }

        fn read_screenshot(
            &self,
            session_id: &str,
            sha256: &str,
        ) -> Result<Option<(String, Vec<u8>)>, String> {
            let stores = self.stores.lock().unwrap();
            let store = stores.get(session_id).ok_or("not a native Session")?;
            read_screenshot(store, sha256)
        }
    }

    #[tokio::test]
    async fn screenshots_are_stored_through_the_writer_and_read_without_one() {
        let stores = Stores::new(&["s"]);
        let records = SessionNetworkRecords::new(stores, 50_000);
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend([1u8; 64]);
        let stored = records
            .store_screenshot("s", "image/png", png.clone())
            .await
            .unwrap();
        assert_eq!(stored.bytes, 72);
        records.close("s").await;
        let (media, bytes) = records
            .read_screenshot("s", &stored.sha256)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((media.as_str(), bytes), ("image/png", png));
        assert!(records
            .read_screenshot("s", &"f".repeat(64))
            .await
            .unwrap()
            .is_none());
        assert!(records
            .store_screenshot("s", "image/png", b"GIF89a".to_vec())
            .await
            .is_err());
    }

    fn sidecar(state: SidecarState) -> NetworkEvent {
        NetworkEvent::Sidecar {
            state,
            generation: 1,
            container: None,
            detail: None,
        }
    }

    #[tokio::test]
    async fn appends_are_ordered_per_session_and_readable() {
        let stores = Stores::new(&["a", "b"]);
        let records = SessionNetworkRecords::new(stores, 50_000);
        assert_eq!(
            records
                .append("a", sidecar(SidecarState::Starting))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            records
                .append("a", sidecar(SidecarState::Ready))
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            records
                .append("b", sidecar(SidecarState::Ready))
                .await
                .unwrap(),
            1
        );
        let page = records.read_after("a", None, 200).await.unwrap();
        assert_eq!(page.events.len(), 2);
        assert_eq!(page.next_after, Some(2));
        assert_eq!(page.stats.events, 2);
        let rest = records.read_after("a", Some(2), 200).await.unwrap();
        assert!(rest.events.is_empty());
        assert_eq!(rest.next_after, Some(2));
        assert_eq!(records.stats("b").await.unwrap().events, 1);
    }

    #[tokio::test]
    async fn reading_a_session_without_a_record_creates_nothing() {
        let stores = Stores::new(&["quiet"]);
        let records = SessionNetworkRecords::new(stores, 50_000);
        let page = records.read_after("quiet", None, 200).await.unwrap();
        assert!(page.events.is_empty());
        assert_eq!(page.next_after, None);
        assert_eq!(page.stats.max_events, 50_000);
        assert!(!records.is_open("quiet").await);
        assert!(records.read_after("unknown", None, 10).await.is_err());
        assert!(records
            .append("unknown", sidecar(SidecarState::Ready))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn close_syncs_and_a_reopen_continues_the_sequence() {
        let stores = Stores::new(&["s"]);
        let records = SessionNetworkRecords::new(stores.clone(), 50_000);
        for _ in 0..3 {
            records
                .append("s", sidecar(SidecarState::Ready))
                .await
                .unwrap();
        }
        records.close("s").await;
        assert!(!records.is_open("s").await);
        // A new owner reads the existing record without being asked to create.
        let page = records.read_after("s", None, 10).await.unwrap();
        assert_eq!(page.stats.last_seq, 3);
        assert_eq!(
            records
                .append("s", sidecar(SidecarState::Stopped))
                .await
                .unwrap(),
            4
        );
        records.close_all().await;
        assert!(!records.is_open("s").await);
    }

    #[tokio::test]
    async fn a_full_record_refuses_ordinary_events_but_records_why() {
        let stores = Stores::new(&["s"]);
        let records = SessionNetworkRecords::new(stores, 2);
        records
            .append("s", sidecar(SidecarState::Ready))
            .await
            .unwrap();
        records
            .append("s", sidecar(SidecarState::Ready))
            .await
            .unwrap();
        let error = records
            .append("s", sidecar(SidecarState::Ready))
            .await
            .unwrap_err();
        assert!(error.is_full(), "{error}");
        let seq = records
            .append_control(
                "s",
                NetworkEvent::Limit {
                    what: LimitKind::RecordFull,
                    detail: "2 events".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(seq, 3);
        assert!(records.stats("s").await.unwrap().full);
    }

    #[tokio::test]
    async fn concurrent_appends_get_unique_contiguous_sequences() {
        let stores = Stores::new(&["s"]);
        let records = Arc::new(SessionNetworkRecords::new(stores, 50_000));
        let mut tasks = Vec::new();
        for _ in 0..64 {
            let records = records.clone();
            tasks.push(tokio::spawn(async move {
                records
                    .append("s", sidecar(SidecarState::Ready))
                    .await
                    .unwrap()
            }));
        }
        let mut seqs = Vec::new();
        for task in tasks {
            seqs.push(task.await.unwrap());
        }
        seqs.sort_unstable();
        assert_eq!(seqs, (1..=64).collect::<Vec<_>>());
        let page = records.read_after("s", None, 1000).await.unwrap();
        assert_eq!(page.stats.gaps, 0);
        assert_eq!(page.events.len(), 64);
    }

    #[tokio::test]
    async fn the_writer_syncs_dirty_records_on_its_own() {
        let stores = Stores::new(&["s"]);
        let records = SessionNetworkRecords::new(stores.clone(), 50_000);
        records
            .append("s", sidecar(SidecarState::Ready))
            .await
            .unwrap();
        tokio::time::sleep(RECORD_SYNC_INTERVAL * 3).await;
        // The record stays readable and open after a timed sync.
        assert_eq!(records.stats("s").await.unwrap().events, 1);
        assert!(records.is_open("s").await);
    }
}
