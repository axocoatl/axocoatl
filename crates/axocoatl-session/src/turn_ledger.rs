//! Durable, append-only session-turn records.
//!
//! The actor checkpoint remains an execution cache. This ledger is the
//! canonical user-visible record of what a session was asked to do, which
//! immutable context it used, and how the execution ended.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Cursor, Seek};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use axocoatl_core::SecureDir;
use serde::{Deserialize, Serialize};

const EVENT_SCHEMA_VERSION: u32 = 1;
const LEDGER_FILE_NAME: &str = "turns.v1.jsonl";

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Errors returned by the session turn ledger.
#[derive(Debug, thiserror::Error)]
pub enum SessionTurnError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("unsupported session-turn ledger schema version {0}")]
    UnsupportedVersion(u32),
    #[error("session turn not found: {0}")]
    NotFound(String),
    #[error("turn id {turn_id} already belongs to session {existing_session_id}")]
    TurnIdConflict {
        turn_id: String,
        existing_session_id: String,
    },
    #[error("idempotency key {key} already belongs to turn {existing_turn_id}")]
    IdempotencyConflict {
        key: String,
        existing_turn_id: String,
    },
    #[error("turn {turn_id} cannot transition from {from:?} to {to:?}")]
    InvalidTransition {
        turn_id: String,
        from: SessionTurnLifecycle,
        to: SessionTurnLifecycle,
    },
    #[error("turn {turn_id} belongs to session {actual_session_id}, not {expected_session_id}")]
    SessionMismatch {
        turn_id: String,
        expected_session_id: String,
        actual_session_id: String,
    },
    #[error("operation id {operation_id} was already used for a different event")]
    OperationConflict { operation_id: String },
    #[error("invalid terminal-turn import: {0}")]
    InvalidImport(String),
    #[error("corrupt session-turn ledger at line {line}: {message}")]
    Corrupt { line: usize, message: String },
}

/// Whether context is attached only to one turn or retained for the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnContextScope {
    ThisTurn,
    Session,
}

/// A structured reference to immutable context used by a turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionTurnContextReference {
    /// Stable content-addressed blob id or another immutable reference id.
    pub reference_id: String,
    /// Human-readable name captured at send time.
    pub display_name: String,
    /// Reference kind such as `upload`, `workspace_file`, `code_selection`, or
    /// `browser_selection`.
    pub kind: String,
    pub scope: TurnContextScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// Forward-compatible reference metadata. Values must not contain mutable
    /// content that would make a historical turn change after it is recorded.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

/// User and execution identity supplied when a turn begins.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BeginSessionTurn {
    /// Caller-generated durable id. Generate a `turn-<uuid>` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub session_id: String,
    pub user_input: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub context: Vec<SessionTurnContextReference>,
    /// Stable client request key used to make send/reconnect retries safe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// Extensible metadata for execution mode, originating route, or an
    /// associated attempt-set id.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

/// Durable lifecycle of a session turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionTurnLifecycle {
    Running,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl SessionTurnLifecycle {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }
}

/// Lifecycle update applied exactly once for an operation id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransitionSessionTurn {
    pub status: SessionTurnLifecycle,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

/// Extensible execution or attempt event attached to a turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordTurnExecution {
    /// Stable event kind such as `tool_started`, `attempt_started`, or
    /// `attempt_kept`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

/// One execution/attempt event in a materialized turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionTurnExecutionEvent {
    pub operation_id: String,
    pub recorded_at: u64,
    #[serde(flatten)]
    pub event: RecordTurnExecution,
}

/// One agent's durable output in a single- or multi-agent turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionTurnAgentOutput {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    pub recorded_at: u64,
}

/// Neutral transcript projection that server and daemon adapters can convert
/// to their protocol-specific message types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTranscriptMessage {
    pub turn_id: String,
    pub role: SessionTranscriptRole,
    pub content: String,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub incomplete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionTranscriptRole {
    User,
    Assistant,
}

/// Canonical materialized state of a session turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionTurn {
    pub id: String,
    pub session_id: String,
    pub user_input: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub context: Vec<SessionTurnContextReference>,
    pub status: SessionTurnLifecycle,
    #[serde(default)]
    pub partial_output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub metadata: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub execution_events: Vec<SessionTurnExecutionEvent>,
    #[serde(default)]
    pub agent_outputs: Vec<SessionTurnAgentOutput>,
    /// Rewind is logical and append-only. Superseded records remain auditable
    /// in the ledger but are omitted from normal list/search/transcript views.
    #[serde(default)]
    pub superseded: bool,
}

/// Fields that matched a text query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnSearchField {
    UserInput,
    Output,
    Error,
    Context,
}

/// A session-turn search result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionTurnSearchHit {
    pub turn: SessionTurn,
    pub matched_fields: Vec<TurnSearchField>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum LedgerPayload {
    Begin {
        turn: Box<SessionTurn>,
    },
    AppendOutput {
        turn_id: String,
        chunk: String,
    },
    Transition {
        turn_id: String,
        transition: TransitionSessionTurn,
    },
    Execution {
        turn_id: String,
        execution: RecordTurnExecution,
    },
    AgentOutput {
        turn_id: String,
        agent_id: String,
        model: Option<String>,
        output: String,
        attempt_id: Option<String>,
    },
    /// One atomic import of a legacy transcript. Keeping every already-terminal
    /// turn in one newline-delimited event makes crash recovery all-or-nothing:
    /// replay sees the whole batch or ignores a trailing partial line.
    ImportTerminalTurns {
        session_id: String,
        turns: Vec<SessionTurn>,
    },
    RewindSession {
        session_id: String,
        keep_through_turn_id: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct LedgerEvent {
    schema_version: u32,
    operation_id: String,
    recorded_at: u64,
    #[serde(flatten)]
    payload: LedgerPayload,
}

/// Result of appending output to a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendTurnOutput {
    pub turn_id: String,
    pub partial_output_len: usize,
}

/// Append-only JSONL store for canonical session turns.
///
/// Each operation is appended, flushed, and `sync_data`'d before in-memory
/// state is changed. A trailing partial line caused by a crash is ignored on
/// reload; malformed complete records remain an error rather than silently
/// rewriting history.
pub struct SessionTurnStore {
    path: PathBuf,
    secure_dir: SecureDir,
    file_name: OsString,
    turns: HashMap<String, SessionTurn>,
    order: Vec<String>,
    begin_keys: HashMap<String, String>,
    operations: HashMap<String, LedgerPayload>,
    events: Vec<LedgerEvent>,
    valid_len: u64,
}

impl SessionTurnStore {
    /// Open `{dir}/turns.v1.jsonl`, creating the directory if needed, and
    /// replay every durable record.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, SessionTurnError> {
        let dir = dir.into();
        let secure_dir = SecureDir::open_or_create_all(&dir)?;
        Self::open_at(secure_dir, OsString::from(LEDGER_FILE_NAME))
    }

    /// Open the ledger directory relative to the control-plane data root.
    pub fn open_in(
        data_root: impl AsRef<Path>,
        relative: impl AsRef<Path>,
    ) -> Result<Self, SessionTurnError> {
        let data_root = SecureDir::open(data_root)?;
        Self::open_in_secure(&data_root, relative)
    }

    pub fn open_in_secure(
        data_root: &SecureDir,
        relative: impl AsRef<Path>,
    ) -> Result<Self, SessionTurnError> {
        let secure_dir = data_root.child(relative)?;
        Self::open_at(secure_dir, OsString::from(LEDGER_FILE_NAME))
    }

    /// Open an explicitly named ledger file. Primarily useful to embedders and
    /// migration tooling.
    pub fn open_file(path: impl Into<PathBuf>) -> Result<Self, SessionTurnError> {
        let path = path.into();
        let parent = path.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("turn-ledger path has no parent: {}", path.display()),
            )
        })?;
        let file_name = path
            .file_name()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("turn-ledger path has no filename: {}", path.display()),
                )
            })?
            .to_os_string();
        let secure_dir = SecureDir::open_or_create_all(parent)?;
        Self::open_at(secure_dir, file_name)
    }

    fn open_at(secure_dir: SecureDir, file_name: OsString) -> Result<Self, SessionTurnError> {
        if !secure_dir.is_file(&file_name)? {
            secure_dir.append(&file_name, b"", true)?;
        }
        let path = secure_dir.path().join(&file_name);
        let mut store = Self {
            path,
            secure_dir,
            file_name,
            turns: HashMap::new(),
            order: Vec::new(),
            begin_keys: HashMap::new(),
            operations: HashMap::new(),
            events: Vec::new(),
            valid_len: 0,
        };
        store.replay()?;
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Begin a turn. Repeating the same idempotency key or turn id with the
    /// same content returns the existing record without appending another
    /// event. A conflicting retry is rejected.
    pub fn begin(&mut self, begin: BeginSessionTurn) -> Result<SessionTurn, SessionTurnError> {
        let id = begin
            .turn_id
            .clone()
            .unwrap_or_else(|| format!("turn-{}", uuid::Uuid::new_v4()));
        let now = now_millis();
        let turn = SessionTurn {
            id: id.clone(),
            session_id: begin.session_id,
            user_input: begin.user_input,
            agent_id: begin.agent_id,
            model: begin.model,
            context: begin.context,
            status: SessionTurnLifecycle::Running,
            partial_output: String::new(),
            final_output: None,
            error: None,
            created_at: now,
            updated_at: now,
            completed_at: None,
            idempotency_key: begin.idempotency_key.clone(),
            metadata: begin.metadata,
            execution_events: Vec::new(),
            agent_outputs: Vec::new(),
            superseded: false,
        };

        if let Some(key) = begin.idempotency_key.as_deref() {
            if let Some(existing_id) = self.begin_keys.get(key) {
                let existing = self.turns.get(existing_id).expect("indexed turn exists");
                if equivalent_begin(existing, &turn) {
                    return Ok(existing.clone());
                }
                return Err(SessionTurnError::IdempotencyConflict {
                    key: key.to_string(),
                    existing_turn_id: existing_id.clone(),
                });
            }
        }

        if let Some(existing) = self.turns.get(&id) {
            if equivalent_begin(existing, &turn) {
                return Ok(existing.clone());
            }
            return Err(SessionTurnError::TurnIdConflict {
                turn_id: id,
                existing_session_id: existing.session_id.clone(),
            });
        }

        let event = LedgerEvent {
            schema_version: EVENT_SCHEMA_VERSION,
            operation_id: begin
                .idempotency_key
                .map(|key| format!("begin:{key}"))
                .unwrap_or_else(|| format!("begin:{id}")),
            recorded_at: now,
            payload: LedgerPayload::Begin {
                turn: Box::new(turn),
            },
        };
        self.append_and_apply(event)?;
        Ok(self.turns.get(&id).expect("just inserted").clone())
    }

    /// Append one output chunk exactly once for `operation_id`.
    pub fn append_output(
        &mut self,
        turn_id: &str,
        operation_id: impl Into<String>,
        chunk: impl Into<String>,
    ) -> Result<AppendTurnOutput, SessionTurnError> {
        let chunk = chunk.into();
        let payload = LedgerPayload::AppendOutput {
            turn_id: turn_id.to_string(),
            chunk,
        };
        self.append_operation(operation_id.into(), payload)?;
        let turn = self
            .turns
            .get(turn_id)
            .ok_or_else(|| SessionTurnError::NotFound(turn_id.to_string()))?;
        Ok(AppendTurnOutput {
            turn_id: turn_id.to_string(),
            partial_output_len: turn.partial_output.len(),
        })
    }

    /// Transition a running turn to a terminal state exactly once.
    pub fn transition(
        &mut self,
        turn_id: &str,
        operation_id: impl Into<String>,
        transition: TransitionSessionTurn,
    ) -> Result<SessionTurn, SessionTurnError> {
        let payload = LedgerPayload::Transition {
            turn_id: turn_id.to_string(),
            transition,
        };
        self.append_operation(operation_id.into(), payload)?;
        self.get(turn_id)
            .ok_or_else(|| SessionTurnError::NotFound(turn_id.to_string()))
    }

    /// Record execution or attempt metadata exactly once.
    pub fn record_execution(
        &mut self,
        turn_id: &str,
        operation_id: impl Into<String>,
        execution: RecordTurnExecution,
    ) -> Result<SessionTurn, SessionTurnError> {
        let payload = LedgerPayload::Execution {
            turn_id: turn_id.to_string(),
            execution,
        };
        self.append_operation(operation_id.into(), payload)?;
        self.get(turn_id)
            .ok_or_else(|| SessionTurnError::NotFound(turn_id.to_string()))
    }

    /// Record one agent's final output, preserving all participants in a
    /// multi-agent turn instead of collapsing them into one string.
    pub fn record_agent_output(
        &mut self,
        turn_id: &str,
        operation_id: impl Into<String>,
        agent_id: impl Into<String>,
        model: Option<String>,
        output: impl Into<String>,
        attempt_id: Option<String>,
    ) -> Result<SessionTurn, SessionTurnError> {
        let payload = LedgerPayload::AgentOutput {
            turn_id: turn_id.to_string(),
            agent_id: agent_id.into(),
            model,
            output: output.into(),
            attempt_id,
        };
        self.append_operation(operation_id.into(), payload)?;
        self.get(turn_id)
            .ok_or_else(|| SessionTurnError::NotFound(turn_id.to_string()))
    }

    /// Atomically import a complete legacy transcript as terminal turns.
    ///
    /// The entire batch is one fsynced ledger event. Repeating `operation_id`
    /// with the same batch is idempotent, including after a process restart.
    pub fn import_terminal_turns(
        &mut self,
        session_id: impl Into<String>,
        operation_id: impl Into<String>,
        turns: Vec<SessionTurn>,
    ) -> Result<Vec<SessionTurn>, SessionTurnError> {
        let session_id = session_id.into();
        let ids: Vec<String> = turns.iter().map(|turn| turn.id.clone()).collect();
        self.append_operation(
            operation_id.into(),
            LedgerPayload::ImportTerminalTurns { session_id, turns },
        )?;
        ids.into_iter()
            .map(|id| {
                self.get(&id)
                    .ok_or_else(|| SessionTurnError::NotFound(id.clone()))
            })
            .collect()
    }

    /// Mark every turn after `keep_through_turn_id` superseded. Passing `None`
    /// rewinds the whole session. The boundary must belong to this session.
    pub fn rewind(
        &mut self,
        session_id: &str,
        keep_through_turn_id: Option<&str>,
        operation_id: impl Into<String>,
    ) -> Result<Vec<SessionTurn>, SessionTurnError> {
        if let Some(turn_id) = keep_through_turn_id {
            let turn = self
                .turns
                .get(turn_id)
                .ok_or_else(|| SessionTurnError::NotFound(turn_id.to_string()))?;
            if turn.session_id != session_id {
                return Err(SessionTurnError::SessionMismatch {
                    turn_id: turn_id.to_string(),
                    expected_session_id: session_id.to_string(),
                    actual_session_id: turn.session_id.clone(),
                });
            }
        }
        self.append_operation(
            operation_id.into(),
            LedgerPayload::RewindSession {
                session_id: session_id.to_string(),
                keep_through_turn_id: keep_through_turn_id.map(str::to_string),
            },
        )?;
        Ok(self.list(session_id))
    }

    /// On daemon startup, terminalize turns whose executor did not survive the
    /// process. One durable transition is emitted per turn.
    pub fn reconcile_orphaned_running(
        &mut self,
        reason: &str,
    ) -> Result<Vec<SessionTurn>, SessionTurnError> {
        let ids: Vec<String> = self
            .order
            .iter()
            .filter_map(|id| self.turns.get(id))
            .filter(|turn| turn.status == SessionTurnLifecycle::Running)
            .map(|turn| turn.id.clone())
            .collect();
        let mut reconciled = Vec::with_capacity(ids.len());
        for id in ids {
            reconciled.push(self.transition(
                &id,
                format!("startup-interrupt:{id}"),
                TransitionSessionTurn {
                    status: SessionTurnLifecycle::Interrupted,
                    final_output: None,
                    error: Some(reason.to_string()),
                    metadata: serde_json::Map::new(),
                },
            )?);
        }
        Ok(reconciled)
    }

    pub fn get(&self, turn_id: &str) -> Option<SessionTurn> {
        self.turns.get(turn_id).cloned()
    }

    /// List a session's turns in creation order.
    pub fn list(&self, session_id: &str) -> Vec<SessionTurn> {
        self.order
            .iter()
            .filter_map(|id| self.turns.get(id))
            .filter(|turn| turn.session_id == session_id && !turn.superseded)
            .cloned()
            .collect()
    }

    /// List every canonical turn for a session, including turns hidden by a
    /// later rewind. This is for integrity repair and audit/pinning, not normal
    /// transcript presentation.
    pub fn list_including_superseded(&self, session_id: &str) -> Vec<SessionTurn> {
        self.order
            .iter()
            .filter_map(|id| self.turns.get(id))
            .filter(|turn| turn.session_id == session_id)
            .cloned()
            .collect()
    }

    /// Preview visible turns through an inclusive rewind boundary without
    /// mutating the append-only ledger. `None` projects an empty conversation.
    pub fn turns_through(
        &self,
        session_id: &str,
        keep_through_turn_id: Option<&str>,
    ) -> Result<Vec<SessionTurn>, SessionTurnError> {
        let Some(boundary) = keep_through_turn_id else {
            return Ok(Vec::new());
        };
        let turns = self.list(session_id);
        let Some(index) = turns.iter().position(|turn| turn.id == boundary) else {
            let Some(existing) = self.turns.get(boundary) else {
                return Err(SessionTurnError::NotFound(boundary.to_string()));
            };
            return Err(SessionTurnError::SessionMismatch {
                turn_id: boundary.to_string(),
                expected_session_id: session_id.to_string(),
                actual_session_id: existing.session_id.clone(),
            });
        };
        Ok(turns.into_iter().take(index + 1).collect())
    }

    /// Case-insensitive literal search over visible transcript and context
    /// metadata, scoped to one session when `session_id` is provided.
    pub fn search(&self, session_id: Option<&str>, query: &str) -> Vec<SessionTurnSearchHit> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return Vec::new();
        }
        self.order
            .iter()
            .filter_map(|id| self.turns.get(id))
            .filter(|turn| !turn.superseded)
            .filter(|turn| session_id.is_none_or(|session_id| turn.session_id == session_id))
            .filter_map(|turn| {
                let mut matched_fields = Vec::new();
                if contains_case_insensitive(&turn.user_input, &query) {
                    matched_fields.push(TurnSearchField::UserInput);
                }
                if turn
                    .final_output
                    .as_deref()
                    .is_some_and(|output| contains_case_insensitive(output, &query))
                    || contains_case_insensitive(&turn.partial_output, &query)
                    || turn
                        .agent_outputs
                        .iter()
                        .any(|agent| contains_case_insensitive(&agent.output, &query))
                {
                    matched_fields.push(TurnSearchField::Output);
                }
                if turn
                    .error
                    .as_deref()
                    .is_some_and(|error| contains_case_insensitive(error, &query))
                {
                    matched_fields.push(TurnSearchField::Error);
                }
                if turn.context.iter().any(|context| {
                    contains_case_insensitive(&context.display_name, &query)
                        || contains_case_insensitive(&context.kind, &query)
                        || context
                            .origin
                            .as_deref()
                            .is_some_and(|origin| contains_case_insensitive(origin, &query))
                }) {
                    matched_fields.push(TurnSearchField::Context);
                }
                (!matched_fields.is_empty()).then(|| SessionTurnSearchHit {
                    turn: turn.clone(),
                    matched_fields,
                })
            })
            .collect()
    }

    /// Project visible turns to protocol-neutral user/assistant messages.
    pub fn transcript(&self, session_id: &str) -> Vec<SessionTranscriptMessage> {
        let mut messages = Vec::new();
        for turn in self.list(session_id) {
            messages.extend(transcript_messages_for_turn(&turn));
        }
        messages
    }

    /// Preview the visible transcript after a rewind boundary without writing
    /// an event. The daemon uses this to prepare its checkpoint projection
    /// before committing the canonical rewind.
    pub fn transcript_through(
        &self,
        session_id: &str,
        keep_through_turn_id: Option<&str>,
    ) -> Result<Vec<SessionTranscriptMessage>, SessionTurnError> {
        let mut messages = Vec::new();
        for turn in self.turns_through(session_id, keep_through_turn_id)? {
            messages.extend(transcript_messages_for_turn(&turn));
        }
        Ok(messages)
    }

    /// Physically remove one session's records by atomically rewriting the
    /// ledger. This is intentionally separate from rewind and is the deletion
    /// boundary the daemon should pair with session removal.
    pub fn delete_session(&mut self, session_id: &str) -> Result<usize, SessionTurnError> {
        let removed = self
            .turns
            .values()
            .filter(|turn| turn.session_id == session_id)
            .count();
        if removed == 0 {
            return Ok(0);
        }
        let retained_events = self.retained_events_excluding_session(session_id);
        let bytes = serialize_events(&retained_events)?;
        self.secure_dir.atomic_write(&self.file_name, &bytes)?;
        *self = Self::open_at(self.secure_dir.clone(), self.file_name.clone())?;
        Ok(removed)
    }

    pub fn len(&self) -> usize {
        self.turns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }

    fn append_operation(
        &mut self,
        operation_id: String,
        payload: LedgerPayload,
    ) -> Result<(), SessionTurnError> {
        if let Some(existing) = self.operations.get(&operation_id) {
            return if existing == &payload {
                Ok(())
            } else {
                Err(SessionTurnError::OperationConflict { operation_id })
            };
        }
        self.validate_payload(&payload)?;
        self.append_and_apply(LedgerEvent {
            schema_version: EVENT_SCHEMA_VERSION,
            operation_id,
            recorded_at: now_millis(),
            payload,
        })
    }

    fn validate_payload(&self, payload: &LedgerPayload) -> Result<(), SessionTurnError> {
        match payload {
            LedgerPayload::Begin { turn } => {
                if let Some(existing) = self.turns.get(&turn.id) {
                    return Err(SessionTurnError::TurnIdConflict {
                        turn_id: turn.id.clone(),
                        existing_session_id: existing.session_id.clone(),
                    });
                }
                if let Some(key) = &turn.idempotency_key {
                    if let Some(existing_turn_id) = self.begin_keys.get(key) {
                        return Err(SessionTurnError::IdempotencyConflict {
                            key: key.clone(),
                            existing_turn_id: existing_turn_id.clone(),
                        });
                    }
                }
                Ok(())
            }
            LedgerPayload::AppendOutput { turn_id, .. }
            | LedgerPayload::AgentOutput { turn_id, .. } => {
                let turn = self
                    .turns
                    .get(turn_id)
                    .ok_or_else(|| SessionTurnError::NotFound(turn_id.clone()))?;
                if turn.status.is_terminal() {
                    return Err(SessionTurnError::InvalidTransition {
                        turn_id: turn_id.clone(),
                        from: turn.status,
                        to: turn.status,
                    });
                }
                Ok(())
            }
            LedgerPayload::Execution { turn_id, .. } => self
                .turns
                .contains_key(turn_id)
                .then_some(())
                .ok_or_else(|| SessionTurnError::NotFound(turn_id.clone())),
            LedgerPayload::Transition {
                turn_id,
                transition,
            } => {
                let turn = self
                    .turns
                    .get(turn_id)
                    .ok_or_else(|| SessionTurnError::NotFound(turn_id.clone()))?;
                if turn.status != SessionTurnLifecycle::Running || !transition.status.is_terminal()
                {
                    return Err(SessionTurnError::InvalidTransition {
                        turn_id: turn_id.clone(),
                        from: turn.status,
                        to: transition.status,
                    });
                }
                Ok(())
            }
            LedgerPayload::ImportTerminalTurns { session_id, turns } => {
                if self
                    .turns
                    .values()
                    .any(|turn| turn.session_id == *session_id)
                {
                    return Err(SessionTurnError::InvalidImport(format!(
                        "session {session_id} already has canonical turns"
                    )));
                }
                let mut ids = HashSet::with_capacity(turns.len());
                let mut keys = HashSet::with_capacity(turns.len());
                for turn in turns {
                    if turn.session_id != *session_id {
                        return Err(SessionTurnError::InvalidImport(format!(
                            "turn {} belongs to session {}, not {session_id}",
                            turn.id, turn.session_id
                        )));
                    }
                    if !turn.status.is_terminal() {
                        return Err(SessionTurnError::InvalidImport(format!(
                            "turn {} is not terminal",
                            turn.id
                        )));
                    }
                    if turn.completed_at.is_none() || turn.updated_at < turn.created_at {
                        return Err(SessionTurnError::InvalidImport(format!(
                            "turn {} has invalid terminal timestamps",
                            turn.id
                        )));
                    }
                    if !ids.insert(turn.id.clone()) {
                        return Err(SessionTurnError::TurnIdConflict {
                            turn_id: turn.id.clone(),
                            existing_session_id: session_id.clone(),
                        });
                    }
                    if let Some(existing) = self.turns.get(&turn.id) {
                        return Err(SessionTurnError::TurnIdConflict {
                            turn_id: turn.id.clone(),
                            existing_session_id: existing.session_id.clone(),
                        });
                    }
                    if let Some(key) = &turn.idempotency_key {
                        if !keys.insert(key.clone()) {
                            return Err(SessionTurnError::IdempotencyConflict {
                                key: key.clone(),
                                existing_turn_id: turn.id.clone(),
                            });
                        }
                        if let Some(existing_turn_id) = self.begin_keys.get(key) {
                            return Err(SessionTurnError::IdempotencyConflict {
                                key: key.clone(),
                                existing_turn_id: existing_turn_id.clone(),
                            });
                        }
                    }
                }
                Ok(())
            }
            LedgerPayload::RewindSession {
                session_id,
                keep_through_turn_id,
            } => {
                if let Some(turn_id) = keep_through_turn_id {
                    let turn = self
                        .turns
                        .get(turn_id)
                        .ok_or_else(|| SessionTurnError::NotFound(turn_id.clone()))?;
                    if turn.session_id != *session_id {
                        return Err(SessionTurnError::SessionMismatch {
                            turn_id: turn_id.clone(),
                            expected_session_id: session_id.clone(),
                            actual_session_id: turn.session_id.clone(),
                        });
                    }
                }
                Ok(())
            }
        }
    }

    fn append_and_apply(&mut self, event: LedgerEvent) -> Result<(), SessionTurnError> {
        self.repair_partial_tail()?;
        let mut bytes = serde_json::to_vec(&event)?;
        bytes.push(b'\n');
        self.secure_dir.append(&self.file_name, &bytes, true)?;
        self.valid_len = self.valid_len.saturating_add(bytes.len() as u64);
        self.events.push(event.clone());
        self.apply(event);
        Ok(())
    }

    fn replay(&mut self) -> Result<(), SessionTurnError> {
        let bytes = self.secure_dir.read(&self.file_name)?;
        let file_len = bytes.len() as u64;
        let mut reader = BufReader::new(Cursor::new(bytes));
        let mut line = Vec::new();
        let mut line_number = 0;
        loop {
            line.clear();
            let read = reader.read_until(b'\n', &mut line)?;
            if read == 0 {
                break;
            }
            line_number += 1;
            let complete = line.ends_with(b"\n");
            if !complete && reader.stream_position()? == file_len {
                // An interrupted append cannot be trusted; the next successful
                // writer repairs it before appending. Check completeness while
                // the line is still bytes: a crash can stop inside a multibyte
                // UTF-8 code point, which must not make an uncommitted tail
                // brick ledger startup.
                break;
            }
            self.valid_len = reader.stream_position()?;
            let mut json = line.strip_suffix(b"\n").unwrap_or(&line);
            if let Some(without_cr) = json.strip_suffix(b"\r") {
                json = without_cr;
            }
            let event: LedgerEvent =
                serde_json::from_slice(json).map_err(|error| SessionTurnError::Corrupt {
                    line: line_number,
                    message: error.to_string(),
                })?;
            if event.schema_version > EVENT_SCHEMA_VERSION {
                return Err(SessionTurnError::UnsupportedVersion(event.schema_version));
            }
            if let Some(existing) = self.operations.get(&event.operation_id) {
                if existing != &event.payload {
                    return Err(SessionTurnError::Corrupt {
                        line: line_number,
                        message: format!(
                            "operation id {} has conflicting payloads",
                            event.operation_id
                        ),
                    });
                }
                continue;
            }
            self.validate_replayed_payload(&event.payload, line_number)?;
            self.events.push(event.clone());
            self.apply(event);
        }
        Ok(())
    }

    fn validate_replayed_payload(
        &self,
        payload: &LedgerPayload,
        line: usize,
    ) -> Result<(), SessionTurnError> {
        self.validate_payload(payload)
            .map_err(|error| SessionTurnError::Corrupt {
                line,
                message: error.to_string(),
            })
    }

    fn apply(&mut self, event: LedgerEvent) {
        let recorded_at = event.recorded_at;
        let operation_id = event.operation_id.clone();
        match &event.payload {
            LedgerPayload::Begin { turn } => {
                if let Some(key) = &turn.idempotency_key {
                    self.begin_keys.insert(key.clone(), turn.id.clone());
                }
                self.order.push(turn.id.clone());
                self.turns.insert(turn.id.clone(), turn.as_ref().clone());
            }
            LedgerPayload::AppendOutput { turn_id, chunk } => {
                let turn = self.turns.get_mut(turn_id).expect("validated turn exists");
                turn.partial_output.push_str(chunk);
                turn.updated_at = turn.updated_at.max(recorded_at);
            }
            LedgerPayload::Transition {
                turn_id,
                transition,
            } => {
                let turn = self.turns.get_mut(turn_id).expect("validated turn exists");
                turn.status = transition.status;
                turn.final_output = transition.final_output.clone().or_else(|| {
                    (!turn.partial_output.is_empty()).then(|| turn.partial_output.clone())
                });
                turn.error = transition.error.clone();
                turn.metadata.extend(transition.metadata.clone());
                turn.updated_at = turn.updated_at.max(recorded_at);
                turn.completed_at = Some(recorded_at);
            }
            LedgerPayload::Execution { turn_id, execution } => {
                let turn = self.turns.get_mut(turn_id).expect("validated turn exists");
                turn.execution_events.push(SessionTurnExecutionEvent {
                    operation_id: operation_id.clone(),
                    recorded_at,
                    event: execution.clone(),
                });
                turn.updated_at = turn.updated_at.max(recorded_at);
            }
            LedgerPayload::AgentOutput {
                turn_id,
                agent_id,
                model,
                output,
                attempt_id,
            } => {
                let turn = self.turns.get_mut(turn_id).expect("validated turn exists");
                turn.agent_outputs.push(SessionTurnAgentOutput {
                    agent_id: agent_id.clone(),
                    model: model.clone(),
                    output: output.clone(),
                    attempt_id: attempt_id.clone(),
                    recorded_at,
                });
                turn.updated_at = turn.updated_at.max(recorded_at);
            }
            LedgerPayload::ImportTerminalTurns { turns, .. } => {
                for turn in turns {
                    if let Some(key) = &turn.idempotency_key {
                        self.begin_keys.insert(key.clone(), turn.id.clone());
                    }
                    self.order.push(turn.id.clone());
                    self.turns.insert(turn.id.clone(), turn.clone());
                }
            }
            LedgerPayload::RewindSession {
                session_id,
                keep_through_turn_id,
            } => {
                let mut after_boundary = keep_through_turn_id.is_none();
                for id in &self.order {
                    let turn = self.turns.get_mut(id).expect("ordered turn exists");
                    if turn.session_id != *session_id {
                        continue;
                    }
                    if after_boundary {
                        turn.superseded = true;
                    }
                    if keep_through_turn_id.as_deref() == Some(turn.id.as_str()) {
                        after_boundary = true;
                    }
                }
            }
        }
        self.operations.insert(operation_id, event.payload);
    }

    fn repair_partial_tail(&mut self) -> Result<(), SessionTurnError> {
        let actual_len = self.secure_dir.file_len(&self.file_name)?;
        if actual_len > self.valid_len {
            let mut bytes = self.secure_dir.read(&self.file_name)?;
            bytes.truncate(self.valid_len as usize);
            self.secure_dir.atomic_write(&self.file_name, &bytes)?;
        }
        Ok(())
    }

    fn retained_events_excluding_session(&self, session_id: &str) -> Vec<LedgerEvent> {
        self.events
            .iter()
            .filter_map(|event| {
                let payload = &event.payload;
                let belongs = match payload {
                    LedgerPayload::Begin { turn } => turn.session_id == session_id,
                    LedgerPayload::AppendOutput { turn_id, .. }
                    | LedgerPayload::Transition { turn_id, .. }
                    | LedgerPayload::Execution { turn_id, .. }
                    | LedgerPayload::AgentOutput { turn_id, .. } => self
                        .turns
                        .get(turn_id)
                        .is_some_and(|turn| turn.session_id == session_id),
                    LedgerPayload::ImportTerminalTurns {
                        session_id: event_session_id,
                        ..
                    } => event_session_id == session_id,
                    LedgerPayload::RewindSession {
                        session_id: event_session_id,
                        ..
                    } => event_session_id == session_id,
                };
                (!belongs).then(|| event.clone())
            })
            .collect()
    }
}

fn serialize_events(events: &[LedgerEvent]) -> Result<Vec<u8>, SessionTurnError> {
    let mut bytes = Vec::new();
    for event in events {
        serde_json::to_writer(&mut bytes, event)?;
        bytes.push(b'\n');
    }
    Ok(bytes)
}

fn equivalent_begin(existing: &SessionTurn, candidate: &SessionTurn) -> bool {
    existing.id == candidate.id
        && existing.session_id == candidate.session_id
        && existing.user_input == candidate.user_input
        && existing.agent_id == candidate.agent_id
        && existing.model == candidate.model
        && existing.context == candidate.context
        && existing.idempotency_key == candidate.idempotency_key
        && existing.metadata == candidate.metadata
}

fn contains_case_insensitive(value: &str, lower_query: &str) -> bool {
    value.to_lowercase().contains(lower_query)
}

fn transcript_messages_for_turn(turn: &SessionTurn) -> Vec<SessionTranscriptMessage> {
    let mut messages = vec![SessionTranscriptMessage {
        turn_id: turn.id.clone(),
        role: SessionTranscriptRole::User,
        content: turn.user_input.clone(),
        created_at: turn.created_at,
        agent_id: None,
        incomplete: false,
    }];
    if turn.agent_outputs.is_empty() {
        if let Some(content) = turn
            .final_output
            .clone()
            .or_else(|| (!turn.partial_output.is_empty()).then(|| turn.partial_output.clone()))
        {
            messages.push(SessionTranscriptMessage {
                turn_id: turn.id.clone(),
                role: SessionTranscriptRole::Assistant,
                content,
                created_at: turn.updated_at,
                agent_id: turn.agent_id.clone(),
                incomplete: turn.status != SessionTurnLifecycle::Completed,
            });
        }
    } else {
        messages.extend(
            turn.agent_outputs
                .iter()
                .map(|agent| SessionTranscriptMessage {
                    turn_id: turn.id.clone(),
                    role: SessionTranscriptRole::Assistant,
                    content: agent.output.clone(),
                    created_at: agent.recorded_at,
                    agent_id: Some(agent.agent_id.clone()),
                    incomplete: turn.status != SessionTurnLifecycle::Completed,
                }),
        );
        let completed_len: usize = turn
            .agent_outputs
            .iter()
            .map(|agent| agent.output.len())
            .sum();
        if let Some(tail) = turn
            .partial_output
            .get(completed_len..)
            .filter(|tail| !tail.is_empty())
        {
            messages.push(SessionTranscriptMessage {
                turn_id: turn.id.clone(),
                role: SessionTranscriptRole::Assistant,
                content: tail.to_string(),
                created_at: turn.updated_at,
                agent_id: None,
                incomplete: true,
            });
        }
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::{Seek, Write};
    use tempfile::tempdir;

    fn begin(session_id: &str, turn_id: &str, key: &str) -> BeginSessionTurn {
        BeginSessionTurn {
            turn_id: Some(turn_id.to_string()),
            session_id: session_id.to_string(),
            user_input: "Explain the quarterly report".to_string(),
            agent_id: Some("coder".to_string()),
            model: Some("test-model".to_string()),
            context: vec![SessionTurnContextReference {
                reference_id: "sha256:abc".to_string(),
                display_name: "Q2-report.pdf".to_string(),
                kind: "upload".to_string(),
                scope: TurnContextScope::ThisTurn,
                media_type: Some("application/pdf".to_string()),
                content_sha256: Some("abc".to_string()),
                origin: Some("local_upload".to_string()),
                metadata: serde_json::Map::new(),
            }],
            idempotency_key: Some(key.to_string()),
            metadata: serde_json::Map::new(),
        }
    }

    fn complete() -> TransitionSessionTurn {
        TransitionSessionTurn {
            status: SessionTurnLifecycle::Completed,
            final_output: Some("Revenue increased.".to_string()),
            error: None,
            metadata: serde_json::Map::new(),
        }
    }

    fn imported_turn(id: &str, input: &str, output: Option<&str>) -> SessionTurn {
        let status = if output.is_some() {
            SessionTurnLifecycle::Completed
        } else {
            SessionTurnLifecycle::Interrupted
        };
        SessionTurn {
            id: id.to_string(),
            session_id: "ses-legacy".to_string(),
            user_input: input.to_string(),
            agent_id: Some("coder".to_string()),
            model: None,
            context: Vec::new(),
            status,
            partial_output: String::new(),
            final_output: output.map(str::to_string),
            error: (output.is_none()).then(|| "legacy turn was interrupted".to_string()),
            created_at: 1_700_000_000_000,
            updated_at: 1_700_000_001_000,
            completed_at: Some(1_700_000_001_000),
            idempotency_key: Some(format!("legacy-{id}")),
            metadata: serde_json::Map::new(),
            execution_events: Vec::new(),
            agent_outputs: Vec::new(),
            superseded: false,
        }
    }

    #[test]
    fn durable_turn_roundtrips_after_restart() {
        let dir = tempdir().unwrap();
        {
            let mut store = SessionTurnStore::open(dir.path()).unwrap();
            store.begin(begin("ses-a", "turn-a", "request-a")).unwrap();
            store
                .append_output("turn-a", "stream-1", "Revenue ")
                .unwrap();
            store
                .record_execution(
                    "turn-a",
                    "attempt-1",
                    RecordTurnExecution {
                        kind: "attempt_started".to_string(),
                        execution_id: Some("run-a".to_string()),
                        attempt_id: Some("way-1".to_string()),
                        metadata: serde_json::Map::new(),
                    },
                )
                .unwrap();
            store.transition("turn-a", "finish-a", complete()).unwrap();
        }

        let store = SessionTurnStore::open(dir.path()).unwrap();
        let turn = store.get("turn-a").unwrap();
        assert_eq!(turn.status, SessionTurnLifecycle::Completed);
        assert_eq!(turn.partial_output, "Revenue ");
        assert_eq!(turn.final_output.as_deref(), Some("Revenue increased."));
        assert_eq!(turn.context[0].display_name, "Q2-report.pdf");
        assert_eq!(turn.execution_events.len(), 1);
        assert_eq!(store.list("ses-a"), vec![turn]);
    }

    #[test]
    fn terminal_import_is_one_event_and_retry_is_exactly_once_after_restart() {
        let dir = tempdir().unwrap();
        let turns = vec![
            imported_turn("turn-legacy-0", "First request", Some("First answer")),
            imported_turn("turn-legacy-1", "Second 🦎 request", None),
        ];
        let path;
        {
            let mut store = SessionTurnStore::open(dir.path()).unwrap();
            path = store.path().to_path_buf();
            let imported = store
                .import_terminal_turns("ses-legacy", "import:ses-legacy:7", turns.clone())
                .unwrap();
            assert_eq!(imported, turns);
        }

        let bytes_before_retry = std::fs::read(&path).unwrap();
        assert_eq!(
            bytes_before_retry
                .iter()
                .filter(|byte| **byte == b'\n')
                .count(),
            1
        );
        let mut reopened = SessionTurnStore::open_file(&path).unwrap();
        let retried = reopened
            .import_terminal_turns("ses-legacy", "import:ses-legacy:7", turns.clone())
            .unwrap();
        assert_eq!(retried, turns);
        assert_eq!(reopened.list("ses-legacy"), turns);
        assert_eq!(std::fs::read(&path).unwrap(), bytes_before_retry);
    }

    #[test]
    fn partial_terminal_import_event_is_ignored_and_retry_repairs_it() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(LEDGER_FILE_NAME);
        let turns = vec![
            imported_turn("turn-legacy-0", "First request", Some("First answer")),
            imported_turn("turn-legacy-1", "Second 🦎 request", None),
        ];
        let event = LedgerEvent {
            schema_version: EVENT_SCHEMA_VERSION,
            operation_id: "import:ses-legacy:7".to_string(),
            recorded_at: 1_700_000_002_000,
            payload: LedgerPayload::ImportTerminalTurns {
                session_id: "ses-legacy".to_string(),
                turns: turns.clone(),
            },
        };
        let bytes = serde_json::to_vec(&event).unwrap();
        let unicode = "🦎".as_bytes();
        let unicode_start = bytes
            .windows(unicode.len())
            .position(|window| window == unicode)
            .expect("fixture includes a multibyte code point");
        std::fs::write(&path, &bytes[..unicode_start + 2]).unwrap();

        let mut reopened = SessionTurnStore::open_file(&path).unwrap();
        assert!(reopened.list_including_superseded("ses-legacy").is_empty());
        reopened
            .import_terminal_turns("ses-legacy", "import:ses-legacy:7", turns.clone())
            .unwrap();
        drop(reopened);

        let recovered = SessionTurnStore::open_file(&path).unwrap();
        assert_eq!(recovered.list("ses-legacy"), turns);
        assert_eq!(
            std::fs::read(&path)
                .unwrap()
                .iter()
                .filter(|byte| **byte == b'\n')
                .count(),
            1
        );
    }

    #[test]
    fn terminal_import_rejects_a_fully_rewound_existing_session() {
        let dir = tempdir().unwrap();
        let mut store = SessionTurnStore::open(dir.path()).unwrap();
        store
            .begin(begin("ses-legacy", "turn-existing", "existing"))
            .unwrap();
        store
            .transition("turn-existing", "finish-existing", complete())
            .unwrap();
        store.rewind("ses-legacy", None, "rewind-existing").unwrap();
        assert!(store.list("ses-legacy").is_empty());
        assert_eq!(store.list_including_superseded("ses-legacy").len(), 1);

        assert!(matches!(
            store.import_terminal_turns(
                "ses-legacy",
                "import:ses-legacy:7",
                vec![imported_turn(
                    "turn-legacy-0",
                    "Old request",
                    Some("Old answer")
                )],
            ),
            Err(SessionTurnError::InvalidImport(_))
        ));
        assert_eq!(store.list_including_superseded("ses-legacy").len(), 1);
    }

    #[test]
    fn operations_are_idempotent_but_conflicts_are_rejected() {
        let dir = tempdir().unwrap();
        let mut store = SessionTurnStore::open(dir.path()).unwrap();
        let first = store.begin(begin("ses-a", "turn-a", "request-a")).unwrap();
        let retried = store.begin(begin("ses-a", "turn-a", "request-a")).unwrap();
        assert_eq!(first, retried);

        store.append_output("turn-a", "stream-1", "once").unwrap();
        store.append_output("turn-a", "stream-1", "once").unwrap();
        assert_eq!(store.get("turn-a").unwrap().partial_output, "once");
        assert!(matches!(
            store.append_output("turn-a", "stream-1", "different"),
            Err(SessionTurnError::OperationConflict { .. })
        ));

        let mut conflicting = begin("ses-a", "turn-b", "request-a");
        conflicting.user_input = "Different".to_string();
        assert!(matches!(
            store.begin(conflicting),
            Err(SessionTurnError::IdempotencyConflict { .. })
        ));
    }

    #[test]
    fn terminal_state_is_one_way_and_transition_retry_is_safe() {
        let dir = tempdir().unwrap();
        let mut store = SessionTurnStore::open(dir.path()).unwrap();
        store.begin(begin("ses-a", "turn-a", "request-a")).unwrap();
        store.transition("turn-a", "finish-a", complete()).unwrap();
        store.transition("turn-a", "finish-a", complete()).unwrap();
        assert!(matches!(
            store.append_output("turn-a", "late", "too late"),
            Err(SessionTurnError::InvalidTransition { .. })
        ));
        assert!(matches!(
            store.transition(
                "turn-a",
                "cancel-a",
                TransitionSessionTurn {
                    status: SessionTurnLifecycle::Cancelled,
                    final_output: None,
                    error: None,
                    metadata: serde_json::Map::new(),
                },
            ),
            Err(SessionTurnError::InvalidTransition { .. })
        ));
    }

    #[test]
    fn search_is_scoped_and_reports_matching_fields() {
        let dir = tempdir().unwrap();
        let mut store = SessionTurnStore::open(dir.path()).unwrap();
        store.begin(begin("ses-a", "turn-a", "request-a")).unwrap();
        let mut other = begin("ses-b", "turn-b", "request-b");
        other.user_input = "No report here".to_string();
        other.context.clear();
        store.begin(other).unwrap();
        store.transition("turn-a", "finish-a", complete()).unwrap();

        let hits = store.search(Some("ses-a"), "q2-report");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].matched_fields, vec![TurnSearchField::Context]);
        let hits = store.search(None, "revenue increased");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].matched_fields, vec![TurnSearchField::Output]);
    }

    #[test]
    fn replay_ignores_only_a_trailing_partial_event() {
        let dir = tempdir().unwrap();
        let path;
        {
            let mut store = SessionTurnStore::open(dir.path()).unwrap();
            path = store.path().to_path_buf();
            store.begin(begin("ses-a", "turn-a", "request-a")).unwrap();
        }
        let valid_len = std::fs::metadata(&path).unwrap().len();
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"schema_version\":1").unwrap();
        file.sync_data().unwrap();

        let store = SessionTurnStore::open_file(&path).unwrap();
        assert!(store.get("turn-a").is_some());

        // Repair is deliberately explicit: truncate the uncommitted tail,
        // just as an integration recovery step would before accepting writes.
        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(valid_len).unwrap();
        file.seek(std::io::SeekFrom::End(0)).unwrap();
        let mut store = SessionTurnStore::open_file(&path).unwrap();
        store
            .append_output("turn-a", "stream-1", "survives")
            .unwrap();
        assert_eq!(store.get("turn-a").unwrap().partial_output, "survives");
    }

    #[test]
    fn a_complete_malformed_event_is_not_silently_skipped() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(LEDGER_FILE_NAME);
        std::fs::write(&path, b"not-json\n").unwrap();
        assert!(matches!(
            SessionTurnStore::open_file(path),
            Err(SessionTurnError::Corrupt { line: 1, .. })
        ));
    }

    #[test]
    fn future_schema_versions_are_rejected_safely() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(LEDGER_FILE_NAME);
        let event = serde_json::json!({
            "schema_version": 2,
            "operation_id": "future",
            "recorded_at": 1,
            "kind": "begin",
            "turn": {
                "id": "turn-a",
                "session_id": "ses-a",
                "user_input": "hello",
                "status": "running",
                "partial_output": "",
                "created_at": 1,
                "updated_at": 1
            }
        });
        std::fs::write(&path, format!("{event}\n")).unwrap();
        assert!(matches!(
            SessionTurnStore::open_file(path),
            Err(SessionTurnError::UnsupportedVersion(2))
        ));
    }

    #[test]
    fn partial_tail_is_repaired_before_the_next_append() {
        let dir = tempdir().unwrap();
        let path;
        {
            let mut store = SessionTurnStore::open(dir.path()).unwrap();
            path = store.path().to_path_buf();
            store.begin(begin("ses-a", "turn-a", "request-a")).unwrap();
        }
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"interrupted\":")
            .unwrap();
        let mut reopened = SessionTurnStore::open_file(&path).unwrap();
        reopened
            .append_output("turn-a", "stream-1", "valid")
            .unwrap();
        drop(reopened);
        assert_eq!(
            SessionTurnStore::open_file(path)
                .unwrap()
                .get("turn-a")
                .unwrap()
                .partial_output,
            "valid"
        );
    }

    #[test]
    fn restart_reconciliation_marks_running_turns_interrupted() {
        let dir = tempdir().unwrap();
        {
            let mut store = SessionTurnStore::open(dir.path()).unwrap();
            store.begin(begin("ses-a", "turn-a", "request-a")).unwrap();
        }
        let mut store = SessionTurnStore::open(dir.path()).unwrap();
        let turns = store
            .reconcile_orphaned_running("daemon restarted before execution completed")
            .unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].status, SessionTurnLifecycle::Interrupted);
        drop(store);
        assert_eq!(
            SessionTurnStore::open(dir.path())
                .unwrap()
                .get("turn-a")
                .unwrap()
                .status,
            SessionTurnLifecycle::Interrupted
        );
    }

    #[test]
    fn rewind_is_append_only_and_delete_session_physically_removes_history() {
        let dir = tempdir().unwrap();
        let path;
        {
            let mut store = SessionTurnStore::open(dir.path()).unwrap();
            path = store.path().to_path_buf();
            store.begin(begin("ses-a", "turn-a", "request-a")).unwrap();
            store.begin(begin("ses-a", "turn-b", "request-b")).unwrap();
            store.begin(begin("ses-b", "turn-c", "request-c")).unwrap();
            store.rewind("ses-a", Some("turn-a"), "rewind-a").unwrap();
            assert_eq!(store.list("ses-a").len(), 1);
            assert!(store.get("turn-b").unwrap().superseded);
        }
        let mut store = SessionTurnStore::open_file(&path).unwrap();
        assert_eq!(store.list("ses-a").len(), 1);
        assert_eq!(store.delete_session("ses-a").unwrap(), 2);
        assert!(store.get("turn-a").is_none());
        assert!(store.get("turn-b").is_none());
        assert!(store.get("turn-c").is_some());
        drop(store);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("turn-a"));
        assert!(!raw.contains("turn-b"));
        assert!(raw.contains("turn-c"));
    }

    #[test]
    fn transcript_preview_does_not_mutate_the_rewind_boundary() {
        let dir = tempdir().unwrap();
        let mut store = SessionTurnStore::open(dir.path()).unwrap();
        for (turn_id, key) in [("turn-a", "key-a"), ("turn-b", "key-b")] {
            store.begin(begin("ses-a", turn_id, key)).unwrap();
            store
                .transition(
                    turn_id,
                    format!("done:{turn_id}"),
                    TransitionSessionTurn {
                        status: SessionTurnLifecycle::Completed,
                        final_output: Some(format!("answer {turn_id}")),
                        error: None,
                        metadata: serde_json::Map::new(),
                    },
                )
                .unwrap();
        }
        let preview = store.transcript_through("ses-a", Some("turn-a")).unwrap();
        assert_eq!(preview.len(), 2);
        assert_eq!(store.list("ses-a").len(), 2, "preview is read-only");
        assert!(store.transcript_through("ses-a", Some("missing")).is_err());
    }

    #[test]
    fn transcript_preserves_each_agent_output() {
        let dir = tempdir().unwrap();
        let mut store = SessionTurnStore::open(dir.path()).unwrap();
        store.begin(begin("ses-a", "turn-a", "request-a")).unwrap();
        store
            .record_agent_output(
                "turn-a",
                "agent-a-output",
                "reviewer",
                Some("model-a".to_string()),
                "Review result",
                None,
            )
            .unwrap();
        store
            .record_agent_output(
                "turn-a",
                "agent-b-output",
                "tester",
                Some("model-b".to_string()),
                "Test result",
                None,
            )
            .unwrap();
        store.transition("turn-a", "finish-a", complete()).unwrap();
        let transcript = store.transcript("ses-a");
        assert_eq!(transcript.len(), 3);
        assert_eq!(transcript[1].agent_id.as_deref(), Some("reviewer"));
        assert_eq!(transcript[2].agent_id.as_deref(), Some("tester"));
    }

    #[test]
    fn transcript_tolerates_stream_and_agent_output_byte_mismatch() {
        let dir = tempdir().unwrap();
        let mut store = SessionTurnStore::open(dir.path()).unwrap();
        store.begin(begin("ses-a", "turn-a", "request-a")).unwrap();
        store.append_output("turn-a", "stream-a", "🦎").unwrap();
        store
            .record_agent_output(
                "turn-a",
                "agent-a-output",
                "reviewer",
                Some("model-a".to_string()),
                "x",
                None,
            )
            .unwrap();

        let transcript = store.transcript("ses-a");
        assert_eq!(transcript.len(), 2);
        assert_eq!(transcript[1].content, "x");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_ancestor_and_predictable_temp_cannot_escape_ledger() {
        use std::os::unix::fs::symlink;

        let parent = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let sentinel = outside.path().join("sentinel");
        std::fs::write(&sentinel, b"safe").unwrap();
        symlink(outside.path(), parent.path().join("linked")).unwrap();
        assert!(SessionTurnStore::open(parent.path().join("linked")).is_err());

        symlink(&sentinel, parent.path().join("turns.v1.jsonl.tmp")).unwrap();
        let mut store = SessionTurnStore::open(parent.path()).unwrap();
        store.begin(begin("ses-a", "turn-a", "request-a")).unwrap();
        assert_eq!(std::fs::read(sentinel).unwrap(), b"safe");
    }
}
