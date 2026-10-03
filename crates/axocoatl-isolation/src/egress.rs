//! The egress decision interface between Session sandboxes and the daemon.
//!
//! A sandbox that runs under `network: egress` holds an [`EgressAttachment`].
//! Its sidecar proxy reports every request over the control channel
//! ([`crate::egress_control`]); the [`EgressAuthority`] decides, records and
//! mints the credentials that processes present to the proxy.

use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use axocoatl_exec::egress::protocol::{CloseOutcome, RequestKind};

/// One request the sidecar is waiting on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenRequest {
    /// Sidecar generation; ids restart with each sidecar process.
    pub generation: u32,
    pub id: u64,
    pub kind: RequestKind,
    /// As the client wrote it: a name, an IPv4 literal or `[v6]`.
    pub host: String,
    pub port: u16,
    /// Hex SHA-256 of the presented credential.
    pub auth: Option<String>,
    pub method: Option<String>,
    pub path: Option<String>,
}

/// The authority's answer for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Connect to these addresses, in order.
    Allow { addrs: Vec<IpAddr> },
    /// Answer the client with this status, reason code and hint.
    Deny {
        status: u16,
        reason: String,
        hint: String,
    },
}

impl Decision {
    pub fn deny(status: u16, reason: &str, hint: impl Into<String>) -> Self {
        Self::Deny {
            status,
            reason: reason.to_string(),
            hint: hint.into(),
        }
    }
}

/// How an allowed connection ended, as the sidecar (or the control loop, on
/// a lost channel) reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseReport {
    pub generation: u32,
    pub id: u64,
    pub ip: Option<IpAddr>,
    pub up: u64,
    pub down: u64,
    pub ms: u64,
    pub outcome: CloseOutcome,
    pub error: Option<String>,
}

/// Sidecar lifecycle changes worth recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarEvent {
    Starting {
        generation: u32,
        container: Option<String>,
    },
    Ready {
        generation: u32,
        container: Option<String>,
    },
    ChannelLost {
        generation: u32,
        detail: String,
    },
    Restarting {
        generation: u32,
    },
    Failed {
        generation: u32,
        detail: String,
    },
    /// The restart budget is spent; the sidecar stays down.
    BudgetSpent {
        generation: u32,
        detail: String,
    },
    Stopped {
        generation: u32,
    },
}

/// What a credential is for. Agent, setup and terminal credentials use the
/// Session policy; provisioning uses the distribution presets; the browser
/// uses the browser policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GrantKind {
    Agent,
    Setup,
    Provisioning,
    Terminal,
    Browser,
}

/// Liveness check for a long-lived credential such as a terminal's. A
/// credential whose check fails is unbound at its next use.
pub type Liveness = Arc<dyn Fn() -> bool + Send + Sync>;

/// Who a credential is minted for. Fields that do not apply stay `None`.
#[derive(Clone)]
pub struct GrantSpec {
    pub kind: GrantKind,
    pub invocation_id: Option<String>,
    pub activation_id: Option<String>,
    pub node_id: Option<String>,
    pub agent: Option<String>,
    pub process: Option<String>,
    pub terminal_id: Option<String>,
    pub setup_index: Option<u32>,
    pub liveness: Option<Liveness>,
}

impl GrantSpec {
    pub fn new(kind: GrantKind) -> Self {
        Self {
            kind,
            invocation_id: None,
            activation_id: None,
            node_id: None,
            agent: None,
            process: None,
            terminal_id: None,
            setup_index: None,
            liveness: None,
        }
    }
}

impl fmt::Debug for GrantSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GrantSpec")
            .field("kind", &self.kind)
            .field("invocation_id", &self.invocation_id)
            .field("activation_id", &self.activation_id)
            .field("agent", &self.agent)
            .field("terminal_id", &self.terminal_id)
            .field("setup_index", &self.setup_index)
            .finish_non_exhaustive()
    }
}

/// A proxy URL that carries a credential. Never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct ProxySecret(String);

impl ProxySecret {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ProxySecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProxySecret(<redacted>)")
    }
}

/// A live credential. Dropping it unbinds the credential, closes the
/// connections opened with it and deletes its env file.
pub struct EgressGrant {
    /// 0600 env file for `podman exec --env-file`; `None` for the browser.
    pub env_file: Option<PathBuf>,
    /// First 16 hex of the credential's SHA-256.
    pub token_tag: String,
    /// The browser's proxy URL, passed only on the driver's stdin.
    pub proxy_url_for_stdin: Option<ProxySecret>,
    /// Unbinds the credential when dropped.
    #[allow(dead_code)]
    guard: Box<dyn Send + Sync>,
}

impl EgressGrant {
    pub fn new(
        env_file: Option<PathBuf>,
        token_tag: String,
        proxy_url_for_stdin: Option<ProxySecret>,
        guard: Box<dyn Send + Sync>,
    ) -> Self {
        Self {
            env_file,
            token_tag,
            proxy_url_for_stdin,
            guard,
        }
    }
}

impl fmt::Debug for EgressGrant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EgressGrant")
            .field("env_file", &self.env_file)
            .field("token_tag", &self.token_tag)
            .field("proxy_url_for_stdin", &self.proxy_url_for_stdin)
            .finish_non_exhaustive()
    }
}

/// Environment for one supervised process.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessEnv<'a> {
    pub env_file: Option<&'a Path>,
}

/// The daemon's policy decision point for one Session.
#[async_trait::async_trait]
pub trait EgressAuthority: Send + Sync + fmt::Debug {
    /// Mint a credential, record its binding and write its env file. The
    /// binding is recorded before the credential is returned.
    async fn grant(&self, spec: GrantSpec) -> Result<EgressGrant, String>;
    /// Decide one request. Must record the decision before returning.
    async fn decide(&self, open: OpenRequest) -> Decision;
    /// Record a finished connection.
    async fn closed(&self, report: CloseReport);
    /// Record a sidecar lifecycle change.
    async fn sidecar_event(&self, event: SidecarEvent);
    /// Use this control channel to revoke connections. Each sidecar
    /// generation attaches its own.
    fn attach_control(&self, _handle: crate::egress_control::ControlHandle) {}
    /// The generation a newly started sidecar takes. Connection ids restart
    /// with every sidecar process, so an authority whose record outlives one
    /// sidecar returns one more than the last generation it recorded, which
    /// keeps `(generation, id)` unique in that record.
    fn first_generation(&self) -> u32 {
        1
    }
    /// Never allow these addresses, whatever the policy lists: the gateways
    /// of the sidecar's network, which lead to the host running Podman.
    fn forbid_destinations(&self, _addrs: &[IpAddr]) {}
}

/// What a sandbox needs to run under `network: egress`.
#[derive(Clone, Debug)]
pub struct EgressAttachment {
    pub authority: Arc<dyn EgressAuthority>,
    /// Podman network for the sidecar; `None` is Podman's default network.
    pub sidecar_network: Option<String>,
    pub max_connections: u32,
    /// Extra `key=value` labels for the sidecar and its volumes (tests mark
    /// their objects with `io.axocoatl.test`).
    pub labels: Vec<String>,
}

impl EgressAttachment {
    pub fn new(authority: Arc<dyn EgressAuthority>) -> Self {
        Self {
            authority,
            sidecar_network: None,
            max_connections: axocoatl_exec::egress::protocol::DEFAULT_MAX_CONNECTIONS,
            labels: Vec::new(),
        }
    }
}
