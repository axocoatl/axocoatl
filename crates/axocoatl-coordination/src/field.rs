//! Location-scoped pheromone field for repository work.
//!
//! `EventLattice` adds the same weight to every registered agent, so a threshold
//! crossing there says nothing about which agent should act. Stigmergy is local:
//! a deposit is left at a place and only agents near that place sense it. This
//! field applies that rule to a repository. Deposits are left on paths, each
//! sensor watches path patterns, and a sensor's intensity is the evaporated sum
//! of the deposits it can sense and has not already acted on.
//!
//! The field is pure data. Intensity is derived from durable wall-clock
//! timestamps, so the same record gives the same answer after a restart. The
//! caller supplies current source hashes: a source-bound deposit stops
//! attracting a sensor once a cited path that sensor watches changes, because
//! somebody acted there; owners of its other cited paths keep it. One source
//! counts once, however many deposits it leaves.
//! A crossing is a proposal for work, not authority to run it; admission,
//! grants and budgets stay with the caller.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Durable evaporation: the strength left after `elapsed_ms` with the given
/// half-life. It depends only on recorded wall-clock times, so a restarted
/// process derives the same intensity.
pub fn evaporate(strength: f64, elapsed_ms: u64, half_life_ms: Option<u64>) -> f64 {
    match half_life_ms {
        Some(half_life) if half_life > 0 => {
            strength * (-(elapsed_ms as f64) / half_life as f64).exp2()
        }
        _ => strength,
    }
}

const MAX_DEPOSITS: usize = 2048;
const MAX_DISPATCHES: usize = 1024;
const MAX_SENSORS: usize = 32;
const MAX_PATTERNS: usize = 32;
const MAX_PATHS: usize = 32;
const MAX_TEXT: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrailKind {
    /// A model-authored finding. A hypothesis, not a verdict.
    Finding,
    /// A model-authored pitfall or risk note.
    Pitfall,
    /// Source bytes changed after work completed.
    Change,
    /// A required check failed on a recorded tree.
    CheckFailure,
    /// A person flagged the location.
    Human,
}

impl TrailKind {
    /// Default deposit strength. Observed failures outweigh hypotheses, and a
    /// change alone is weaker than either so that it attracts review only from
    /// sensors configured to react to change.
    pub fn default_strength(self) -> f64 {
        match self {
            TrailKind::CheckFailure => 1.5,
            TrailKind::Finding | TrailKind::Pitfall | TrailKind::Human => 1.0,
            TrailKind::Change => 0.5,
        }
    }

    /// Whether the deposit describes the source bytes it observed. Such a
    /// deposit is superseded for a sensor once bytes it watches change.
    pub fn is_source_bound(self) -> bool {
        !matches!(self, TrailKind::Change)
    }
}

/// Where a deposit came from. References are opaque to the field; the host
/// resolves them against its own durable records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TrailCause {
    KnowledgeProposal {
        proposal_id: String,
        note_id: String,
        journal_id: String,
        /// The turn whose accepted work proposed it, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
    KnowledgeNote {
        note_id: String,
        revision: u64,
    },
    Turn {
        session_id: String,
        turn_id: String,
    },
    CheckRun {
        turn_id: String,
        run_id: String,
    },
    Human {
        #[serde(default)]
        author: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrailDeposit {
    /// Stable identity. Redelivery of the same cause yields the same id.
    pub id: String,
    pub kind: TrailKind,
    pub paths: Vec<String>,
    /// Content hashes observed at deposit time, keyed by path.
    #[serde(default)]
    pub observed: BTreeMap<String, String>,
    pub strength: f64,
    pub deposited_at_ms: u64,
    /// The sensor that produced this deposit. A sensor never senses its own.
    #[serde(default)]
    pub producer: Option<String>,
    pub summary: String,
    pub cause: TrailCause,
    /// Supporting files and the hashes observed for them. They never route
    /// the deposit or retire it; a change is shown to whoever acts on it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub evidence: BTreeMap<String, String>,
}

/// Who an observed source change is attributed to.
#[derive(Debug, Clone, PartialEq)]
pub struct ChangeAttribution {
    /// The sensor that made it; a sensor never senses its own change.
    pub producer: Option<String>,
    pub cause: TrailCause,
    /// How the deposit names its author, for example an Agent label.
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrailSensor {
    /// Team slot identity.
    pub id: String,
    pub label: String,
    /// Repository path patterns this sensor is responsible for.
    pub watches: Vec<String>,
    pub threshold: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldPolicy {
    /// Evaporation half-life. `None` keeps deposits at full strength.
    #[serde(default)]
    pub half_life_ms: Option<u64>,
    pub sensors: Vec<TrailSensor>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrailContribution {
    pub deposit: String,
    pub weight: f64,
    /// Another contribution from the same source that carried their one vote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counted_with: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrailDispatch {
    pub id: String,
    pub sensor: String,
    pub at_ms: u64,
    pub intensity: f64,
    pub threshold: f64,
    pub contributions: Vec<TrailContribution>,
    /// A person dispatched it, below its threshold or to release a hold.
    #[serde(default)]
    pub manual: bool,
    /// The threshold was crossed while this sensor already had work pending;
    /// the crossing was held and dispatched once that work cleared.
    #[serde(default)]
    pub latched: bool,
}

/// Why a deposit does not currently contribute to a sensor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrailExclusion {
    OutOfScope,
    OwnDeposit,
    Consumed,
    SourceChanged,
    SourceMissing,
    Withdrawn,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SensedDeposit {
    pub deposit: String,
    pub weight: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excluded: Option<TrailExclusion>,
    /// Cited or evidence paths this sensor does not watch whose bytes changed
    /// since the deposit. It still applies here; the change is for review.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_changed: Vec<String>,
    /// Another deposit from the same source that carries their shared vote.
    /// One source counts once, however many deposits it left.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counted_with: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sensing {
    pub sensor: String,
    pub intensity: f64,
    pub threshold: f64,
    pub crossed: bool,
    pub deposits: Vec<SensedDeposit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepositOutcome {
    Added,
    Duplicate,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FieldError {
    #[error("invalid field input: {0}")]
    Invalid(String),
    #[error("deposit {0} already exists with different content")]
    Conflict(String),
    #[error("field capacity exceeded")]
    Capacity,
    #[error("unknown sensor: {0}")]
    UnknownSensor(String),
    #[error("dispatch {0} already recorded")]
    DuplicateDispatch(String),
}

/// Durable field state: every deposit, every dispatch and what each sensor has
/// already consumed. Nothing is evicted; retention is the caller's decision.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignalField {
    #[serde(default)]
    deposits: Vec<TrailDeposit>,
    #[serde(default)]
    consumed: BTreeMap<String, BTreeSet<String>>,
    #[serde(default)]
    dispatches: Vec<TrailDispatch>,
    /// Deposits a person rejected or dismissed, with the reason.
    #[serde(default)]
    withdrawn: BTreeMap<String, String>,
    /// Last observed content of watched sources, for change detection.
    #[serde(default)]
    baseline: BTreeMap<String, String>,
    #[serde(default)]
    baseline_recorded: bool,
}

impl FieldPolicy {
    pub fn validate(&self) -> Result<(), FieldError> {
        if self.sensors.is_empty() || self.sensors.len() > MAX_SENSORS {
            return Err(FieldError::Invalid(format!(
                "a field needs between 1 and {MAX_SENSORS} sensors"
            )));
        }
        if self.half_life_ms == Some(0) {
            return Err(FieldError::Invalid(
                "half-life must be positive; omit it to disable evaporation".into(),
            ));
        }
        let mut ids = BTreeSet::new();
        for sensor in &self.sensors {
            bounded("sensor id", &sensor.id)?;
            bounded("sensor label", &sensor.label)?;
            if !ids.insert(sensor.id.as_str()) {
                return Err(FieldError::Invalid(format!(
                    "sensor {} appears twice",
                    sensor.id
                )));
            }
            if sensor.watches.is_empty() || sensor.watches.len() > MAX_PATTERNS {
                return Err(FieldError::Invalid(format!(
                    "{} needs between 1 and {MAX_PATTERNS} watched patterns",
                    sensor.label
                )));
            }
            for pattern in &sensor.watches {
                validate_pattern(pattern)?;
            }
            if !sensor.threshold.is_finite() || sensor.threshold <= 0.0 {
                return Err(FieldError::Invalid(format!(
                    "{} needs a positive finite threshold",
                    sensor.label
                )));
            }
        }
        Ok(())
    }

    pub fn sensor(&self, id: &str) -> Option<&TrailSensor> {
        self.sensors.iter().find(|sensor| sensor.id == id)
    }
}

impl TrailSensor {
    pub fn watches_path(&self, path: &str) -> bool {
        self.watches
            .iter()
            .any(|pattern| pattern_matches(pattern, path))
    }
}

impl SignalField {
    pub fn deposits(&self) -> &[TrailDeposit] {
        &self.deposits
    }

    pub fn dispatches(&self) -> &[TrailDispatch] {
        &self.dispatches
    }

    pub fn deposit_by_id(&self, id: &str) -> Option<&TrailDeposit> {
        self.deposits.iter().find(|deposit| deposit.id == id)
    }

    pub fn is_consumed(&self, sensor: &str, deposit: &str) -> bool {
        self.consumed
            .get(sensor)
            .is_some_and(|set| set.contains(deposit))
    }

    /// Record a deposit. Redelivering the same deposit is a no-op; the same id
    /// with different content is refused rather than overwritten, so repeated
    /// delivery never counts as independent corroboration.
    pub fn deposit(&mut self, deposit: TrailDeposit) -> Result<DepositOutcome, FieldError> {
        validate_deposit(&deposit)?;
        if let Some(existing) = self.deposit_by_id(&deposit.id) {
            return if existing == &deposit {
                Ok(DepositOutcome::Duplicate)
            } else {
                Err(FieldError::Conflict(deposit.id))
            };
        }
        if self.deposits.len() >= MAX_DEPOSITS {
            return Err(FieldError::Capacity);
        }
        self.deposits.push(deposit);
        Ok(DepositOutcome::Added)
    }

    /// What one sensor feels now. `current` returns the present content hash of
    /// a path, or `None` when the path is unavailable.
    ///
    /// A deposit is retired for this sensor only when a path it cites and this
    /// sensor watches changed; other owners keep it. Deposits from one source
    /// (one producing turn, one check run, one person's note) share one vote:
    /// only the strongest counts, so volume is not corroboration.
    pub fn sense(
        &self,
        policy: &FieldPolicy,
        sensor: &TrailSensor,
        now_ms: u64,
        current: &dyn Fn(&str) -> Option<String>,
    ) -> Sensing {
        let mut deposits = Vec::new();
        let mut sources = Vec::new();
        for deposit in &self.deposits {
            if !deposit.paths.iter().any(|path| sensor.watches_path(path)) {
                continue;
            }
            let weight = evaporate(
                deposit.strength,
                now_ms.saturating_sub(deposit.deposited_at_ms),
                policy.half_life_ms,
            );
            let (applicable, evidence_changed) = applicability(deposit, sensor, current);
            let excluded = if self.withdrawn.contains_key(&deposit.id) {
                Some(TrailExclusion::Withdrawn)
            } else if deposit.producer.as_deref() == Some(sensor.id.as_str()) {
                Some(TrailExclusion::OwnDeposit)
            } else if self.is_consumed(&sensor.id, &deposit.id) {
                Some(TrailExclusion::Consumed)
            } else {
                applicable
            };
            deposits.push(SensedDeposit {
                deposit: deposit.id.clone(),
                weight,
                excluded,
                evidence_changed,
                counted_with: None,
            });
            sources.push(source_key(deposit));
        }
        // The strongest applicable deposit of each source carries its vote.
        let mut votes: BTreeMap<&str, usize> = BTreeMap::new();
        for (index, sensed) in deposits.iter().enumerate() {
            if sensed.excluded.is_some() {
                continue;
            }
            let holder = votes.entry(sources[index].as_str()).or_insert(index);
            if sensed.weight > deposits[*holder].weight {
                *holder = index;
            }
        }
        for index in 0..deposits.len() {
            if deposits[index].excluded.is_some() {
                continue;
            }
            let holder = votes[sources[index].as_str()];
            if holder != index {
                deposits[index].counted_with = Some(deposits[holder].deposit.clone());
            }
        }
        let intensity: f64 = votes.values().map(|index| deposits[*index].weight).sum();
        Sensing {
            sensor: sensor.id.clone(),
            intensity,
            threshold: sensor.threshold,
            crossed: intensity > 0.0 && intensity >= sensor.threshold,
            deposits,
        }
    }

    pub fn sense_all(
        &self,
        policy: &FieldPolicy,
        now_ms: u64,
        current: &dyn Fn(&str) -> Option<String>,
    ) -> Vec<Sensing> {
        policy
            .sensors
            .iter()
            .map(|sensor| self.sense(policy, sensor, now_ms, current))
            .collect()
    }

    pub fn withdrawal(&self, deposit: &str) -> Option<&str> {
        self.withdrawn.get(deposit).map(String::as_str)
    }

    /// Stop a deposit from attracting work. The record stays for audit.
    pub fn withdraw(&mut self, deposit: &str, reason: String) -> Result<bool, FieldError> {
        bounded("withdrawal reason", &reason)?;
        if self.deposit_by_id(deposit).is_none() {
            return Err(FieldError::Invalid(format!("unknown deposit {deposit}")));
        }
        if self.withdrawn.contains_key(deposit) {
            return Ok(false);
        }
        self.withdrawn.insert(deposit.to_owned(), reason);
        Ok(true)
    }

    pub fn baseline(&self) -> Option<&BTreeMap<String, String>> {
        self.baseline_recorded.then_some(&self.baseline)
    }

    /// Compare watched sources with the last observation and deposit one
    /// `Change` per added, modified or removed path. The first observation only
    /// records the baseline: existing code is not news. Returns the new ids.
    pub fn observe_sources(
        &mut self,
        sources: BTreeMap<String, String>,
        at_ms: u64,
        producer: Option<String>,
        cause: TrailCause,
        attribution: &str,
    ) -> Result<Vec<String>, FieldError> {
        self.observe_sources_with(sources, at_ms, |_| ChangeAttribution {
            producer: producer.clone(),
            cause: cause.clone(),
            label: attribution.to_owned(),
        })
    }

    /// As [`observe_sources`](Self::observe_sources), attributing each changed
    /// path separately, for example to the turn whose own captures show it.
    pub fn observe_sources_with(
        &mut self,
        sources: BTreeMap<String, String>,
        at_ms: u64,
        attribute: impl Fn(&str) -> ChangeAttribution,
    ) -> Result<Vec<String>, FieldError> {
        for path in sources.keys() {
            validate_path(path)?;
        }
        if !self.baseline_recorded {
            self.baseline = sources;
            self.baseline_recorded = true;
            return Ok(Vec::new());
        }
        let changed: Vec<(String, Option<String>)> = sources
            .iter()
            .filter(|(path, hash)| self.baseline.get(*path) != Some(*hash))
            .map(|(path, hash)| (path.clone(), Some(hash.clone())))
            .chain(
                self.baseline
                    .keys()
                    .filter(|path| !sources.contains_key(*path))
                    .map(|path| (path.clone(), None)),
            )
            .collect();
        let mut added = Vec::new();
        let mut next = self.clone();
        for (path, hash) in changed {
            let verb = match (&hash, self.baseline.contains_key(&path)) {
                (None, _) => "removed",
                (Some(_), false) => "added",
                (Some(_), true) => "changed",
            };
            let ChangeAttribution {
                producer,
                cause,
                label: attribution,
            } = attribute(&path);
            let cause_key = match &cause {
                TrailCause::Turn { turn_id, .. } | TrailCause::CheckRun { turn_id, .. } => {
                    turn_id.clone()
                }
                TrailCause::KnowledgeProposal { proposal_id, .. } => proposal_id.clone(),
                TrailCause::KnowledgeNote { note_id, revision } => format!("{note_id}@{revision}"),
                TrailCause::Human { .. } => format!("observed@{at_ms}"),
            };
            let deposit = TrailDeposit {
                id: format!(
                    "change:{:016x}",
                    stable_digest(&[&cause_key, &path, hash.as_deref().unwrap_or("removed")])
                ),
                kind: TrailKind::Change,
                paths: vec![path.clone()],
                observed: BTreeMap::new(),
                strength: TrailKind::Change.default_strength(),
                deposited_at_ms: at_ms,
                producer,
                summary: format!("{attribution} {verb} {path}"),
                cause,
                evidence: BTreeMap::new(),
            };
            if next.deposit(deposit.clone())? == DepositOutcome::Added {
                added.push(deposit.id);
            }
        }
        next.baseline = sources;
        *self = next;
        Ok(added)
    }

    /// Deposits that no sensor is responsible for. They stay visible instead of
    /// being assigned to an arbitrary agent.
    pub fn unrouted(&self, policy: &FieldPolicy) -> Vec<&TrailDeposit> {
        self.deposits
            .iter()
            .filter(|deposit| {
                !policy.sensors.iter().any(|sensor| {
                    deposit.paths.iter().any(|path| sensor.watches_path(path))
                        && deposit.producer.as_deref() != Some(sensor.id.as_str())
                })
            })
            .collect()
    }

    /// Record that a sensor acted. Its contributing deposits are consumed for
    /// that sensor only: the classic reset-on-activation, made durable and
    /// attributable. `manual` records a person dispatching (below threshold or
    /// past a hold); `latched` records a crossing held while earlier work was
    /// pending.
    pub fn record_dispatch(
        &mut self,
        id: String,
        sensing: &Sensing,
        now_ms: u64,
        manual: bool,
        latched: bool,
    ) -> Result<TrailDispatch, FieldError> {
        bounded("dispatch id", &id)?;
        if self.dispatches.iter().any(|dispatch| dispatch.id == id) {
            return Err(FieldError::DuplicateDispatch(id));
        }
        if !manual && !latched && !sensing.crossed {
            return Err(FieldError::Invalid(
                "only a crossed threshold dispatches automatically".into(),
            ));
        }
        let contributions: Vec<TrailContribution> = sensing
            .deposits
            .iter()
            .filter(|sensed| sensed.excluded.is_none())
            .map(|sensed| TrailContribution {
                deposit: sensed.deposit.clone(),
                weight: sensed.weight,
                counted_with: sensed.counted_with.clone(),
            })
            .collect();
        if contributions.is_empty() {
            return Err(FieldError::Invalid(
                "a dispatch needs at least one contributing deposit".into(),
            ));
        }
        if self.dispatches.len() >= MAX_DISPATCHES {
            return Err(FieldError::Capacity);
        }
        let consumed = self.consumed.entry(sensing.sensor.clone()).or_default();
        for contribution in &contributions {
            consumed.insert(contribution.deposit.clone());
        }
        let dispatch = TrailDispatch {
            id,
            sensor: sensing.sensor.clone(),
            at_ms: now_ms,
            intensity: sensing.intensity,
            threshold: sensing.threshold,
            contributions,
            manual,
            latched,
        };
        self.dispatches.push(dispatch.clone());
        Ok(dispatch)
    }
}

/// FNV-1a over length-prefixed parts: a compact, stable identity for derived
/// deposits. It is an idempotency key, not a security boundary.
pub fn stable_digest(parts: &[&str]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for byte in (part.len() as u64)
            .to_le_bytes()
            .iter()
            .chain(part.as_bytes())
        {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

/// Whether a source-bound deposit still applies to one sensor. Only cited
/// paths the sensor watches can retire it there; other cited paths and
/// evidence whose bytes changed are returned for review instead.
fn applicability(
    deposit: &TrailDeposit,
    sensor: &TrailSensor,
    current: &dyn Fn(&str) -> Option<String>,
) -> (Option<TrailExclusion>, Vec<String>) {
    if !deposit.kind.is_source_bound() {
        return (None, Vec::new());
    }
    let mut changed_elsewhere = Vec::new();
    for (path, observed) in &deposit.observed {
        let watched = sensor.watches_path(path);
        match current(path) {
            Some(hash) if &hash == observed => {}
            Some(_) if watched => return (Some(TrailExclusion::SourceChanged), Vec::new()),
            None if watched => return (Some(TrailExclusion::SourceMissing), Vec::new()),
            _ => changed_elsewhere.push(path.clone()),
        }
    }
    for (path, observed) in &deposit.evidence {
        if current(path).as_ref() != Some(observed) {
            changed_elsewhere.push(path.clone());
        }
    }
    (None, changed_elsewhere)
}

/// The independent source a deposit speaks for: one producing turn, one check
/// run, one note across its revisions, one person's flag, one unattributed
/// observation of edits. Redelivery already shares an id.
fn source_key(deposit: &TrailDeposit) -> String {
    match &deposit.cause {
        TrailCause::KnowledgeProposal {
            turn_id: Some(turn),
            ..
        } => format!("turn:{turn}:{}", deposit.producer.as_deref().unwrap_or("")),
        TrailCause::Turn { turn_id, .. } => format!(
            "turn:{turn_id}:{}",
            deposit.producer.as_deref().unwrap_or("")
        ),
        // Every revision of one note is the same person's one claim.
        TrailCause::KnowledgeNote { note_id, .. } => format!("note:{note_id}"),
        // One unattributed edit observed at once is one source, however many
        // files it touched, as a turn's edit is.
        TrailCause::Human { author } if deposit.kind == TrailKind::Change => format!(
            "observed:{}:{}",
            author.as_deref().unwrap_or(""),
            deposit.deposited_at_ms
        ),
        _ => format!("deposit:{}", deposit.id),
    }
}

fn validate_deposit(deposit: &TrailDeposit) -> Result<(), FieldError> {
    bounded("deposit id", &deposit.id)?;
    bounded("deposit summary", &deposit.summary)?;
    if deposit.paths.is_empty() || deposit.paths.len() > MAX_PATHS {
        return Err(FieldError::Invalid(format!(
            "a deposit needs between 1 and {MAX_PATHS} paths"
        )));
    }
    if deposit.evidence.len() > MAX_PATHS {
        return Err(FieldError::Invalid(format!(
            "a deposit cites at most {MAX_PATHS} evidence paths"
        )));
    }
    for path in deposit
        .paths
        .iter()
        .chain(deposit.observed.keys())
        .chain(deposit.evidence.keys())
    {
        validate_path(path)?;
    }
    if deposit
        .observed
        .keys()
        .any(|path| !deposit.paths.contains(path))
    {
        return Err(FieldError::Invalid(
            "observed hashes must describe deposited paths".into(),
        ));
    }
    if !deposit.strength.is_finite() || deposit.strength <= 0.0 || deposit.strength > 100.0 {
        return Err(FieldError::Invalid(
            "deposit strength must be positive and bounded".into(),
        ));
    }
    Ok(())
}

fn bounded(label: &str, value: &str) -> Result<(), FieldError> {
    if value.trim().is_empty() || value.len() > MAX_TEXT || value.contains('\0') {
        return Err(FieldError::Invalid(format!(
            "{label} must be non-empty and bounded"
        )));
    }
    Ok(())
}

/// Repository-relative, normalized, no traversal.
pub fn validate_path(path: &str) -> Result<(), FieldError> {
    if path.is_empty()
        || path.len() > 1024
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || path
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(FieldError::Invalid(format!(
            "{path:?} is not a normalized repository path"
        )));
    }
    Ok(())
}

fn validate_pattern(pattern: &str) -> Result<(), FieldError> {
    let trimmed = pattern.strip_suffix('/').unwrap_or(pattern);
    if trimmed.is_empty()
        || pattern.len() > 512
        || pattern.starts_with('/')
        || pattern.contains('\\')
        || pattern.contains('\0')
        || trimmed
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(FieldError::Invalid(format!(
            "{pattern:?} is not a repository path pattern"
        )));
    }
    Ok(())
}

/// Gitignore-flavoured matching. A pattern without `/` matches a file name at
/// any depth; a pattern with `/` is anchored at the repository root. A trailing
/// `/` names a directory and everything under it. `**` spans segments, `*` and
/// `?` stay within one segment.
pub fn pattern_matches(pattern: &str, path: &str) -> bool {
    if let Some(directory) = pattern.strip_suffix('/') {
        return pattern_matches(&format!("{directory}/**"), path);
    }
    let path: Vec<&str> = path.split('/').collect();
    if !pattern.contains('/') {
        return path
            .last()
            .is_some_and(|name| segment_matches(pattern.as_bytes(), name.as_bytes()));
    }
    let pattern: Vec<&str> = pattern.split('/').collect();
    segments_match(&pattern, &path)
}

fn segments_match(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|skip| segments_match(rest, &path[skip..])),
        Some((head, rest)) => path.split_first().is_some_and(|(segment, tail)| {
            segment_matches(head.as_bytes(), segment.as_bytes()) && segments_match(rest, tail)
        }),
    }
}

fn segment_matches(pattern: &[u8], text: &[u8]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some((b'*', rest)) => (0..=text.len()).any(|skip| segment_matches(rest, &text[skip..])),
        Some((b'?', rest)) => !text.is_empty() && segment_matches(rest, &text[1..]),
        Some((byte, rest)) => text.first() == Some(byte) && segment_matches(rest, &text[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: u64 = 60_000;

    fn sensor(id: &str, watches: &[&str], threshold: f64) -> TrailSensor {
        TrailSensor {
            id: id.into(),
            label: id.into(),
            watches: watches.iter().map(|w| (*w).into()).collect(),
            threshold,
        }
    }

    fn policy(half_life_ms: Option<u64>) -> FieldPolicy {
        FieldPolicy {
            half_life_ms,
            sensors: vec![
                sensor("orders", &["lib/orders.js"], 1.0),
                sensor("catalog", &["lib/catalog.js"], 1.0),
                sensor("reviewer", &["lib/"], 0.5),
            ],
        }
    }

    fn finding(id: &str, path: &str, hash: &str, at: u64, producer: Option<&str>) -> TrailDeposit {
        TrailDeposit {
            id: id.into(),
            kind: TrailKind::Finding,
            paths: vec![path.into()],
            observed: BTreeMap::from([(path.into(), hash.into())]),
            strength: TrailKind::Finding.default_strength(),
            deposited_at_ms: at,
            producer: producer.map(Into::into),
            summary: format!("finding on {path}"),
            cause: TrailCause::KnowledgeProposal {
                proposal_id: id.into(),
                note_id: id.into(),
                journal_id: "journal".into(),
                turn_id: None,
            },
            evidence: BTreeMap::new(),
        }
    }

    fn change(id: &str, path: &str, at: u64, producer: &str) -> TrailDeposit {
        TrailDeposit {
            id: id.into(),
            kind: TrailKind::Change,
            paths: vec![path.into()],
            observed: BTreeMap::new(),
            strength: TrailKind::Change.default_strength(),
            deposited_at_ms: at,
            producer: Some(producer.into()),
            summary: format!("{producer} changed {path}"),
            cause: TrailCause::Turn {
                session_id: "s".into(),
                turn_id: id.into(),
            },
            evidence: BTreeMap::new(),
        }
    }

    fn unchanged(path: &str) -> Option<String> {
        Some(
            match path {
                "lib/orders.js" => "orders-v1",
                "lib/catalog.js" => "catalog-v1",
                _ => "other",
            }
            .into(),
        )
    }

    fn sensing<'a>(all: &'a [Sensing], id: &str) -> &'a Sensing {
        all.iter().find(|sensing| sensing.sensor == id).unwrap()
    }

    #[test]
    fn a_deposit_is_sensed_only_where_it_was_left() {
        let policy = policy(None);
        let mut field = SignalField::default();
        field
            .deposit(finding(
                "f1",
                "lib/orders.js",
                "orders-v1",
                0,
                Some("reviewer"),
            ))
            .unwrap();
        let all = field.sense_all(&policy, MINUTE, &unchanged);
        assert!(sensing(&all, "orders").crossed);
        assert_eq!(sensing(&all, "orders").intensity, 1.0);
        // The catalog owner is not woken by a finding about orders.
        assert!(sensing(&all, "catalog").deposits.is_empty());
        assert_eq!(sensing(&all, "catalog").intensity, 0.0);
        // The producer does not excite itself.
        assert_eq!(
            sensing(&all, "reviewer").deposits[0].excluded,
            Some(TrailExclusion::OwnDeposit)
        );
    }

    #[test]
    fn redelivery_does_not_reinforce_but_independent_evidence_does() {
        let policy = policy(None);
        let mut field = SignalField::default();
        let first = change("c1", "lib/orders.js", 0, "catalog");
        assert_eq!(field.deposit(first.clone()).unwrap(), DepositOutcome::Added);
        assert_eq!(field.deposit(first).unwrap(), DepositOutcome::Duplicate);
        let orders = &field.sense_all(&policy, 0, &unchanged)[0];
        assert_eq!(orders.intensity, 0.5);
        assert!(!orders.crossed);
        field
            .deposit(change("c2", "lib/orders.js", 0, "catalog"))
            .unwrap();
        let orders = &field.sense_all(&policy, 0, &unchanged)[0];
        assert_eq!(orders.intensity, 1.0);
        assert!(orders.crossed);
        let mut altered = change("c2", "lib/orders.js", 0, "catalog");
        altered.summary = "different".into();
        assert_eq!(
            field.deposit(altered),
            Err(FieldError::Conflict("c2".into()))
        );
    }

    #[test]
    fn evaporation_uses_durable_wall_clock_time() {
        let policy = policy(Some(30 * MINUTE));
        let mut field = SignalField::default();
        field
            .deposit(finding("f1", "lib/orders.js", "orders-v1", 0, None))
            .unwrap();
        let orders = &field.sense_all(&policy, 30 * MINUTE, &unchanged)[0];
        assert!((orders.intensity - 0.5).abs() < 1e-9);
        assert!(!orders.crossed);
        // A second independent finding a half-life later reinforces the trail.
        field
            .deposit(finding(
                "f2",
                "lib/orders.js",
                "orders-v1",
                30 * MINUTE,
                None,
            ))
            .unwrap();
        let orders = &field.sense_all(&policy, 30 * MINUTE, &unchanged)[0];
        assert!((orders.intensity - 1.5).abs() < 1e-9);
        assert!(orders.crossed);
        // Serialization round-trip gives the same answer: nothing is process-local.
        let restored: SignalField =
            serde_json::from_str(&serde_json::to_string(&field).unwrap()).unwrap();
        assert_eq!(
            restored.sense_all(&policy, 30 * MINUTE, &unchanged),
            field.sense_all(&policy, 30 * MINUTE, &unchanged)
        );
    }

    #[test]
    fn a_finding_stops_attracting_once_its_source_changes() {
        let policy = policy(None);
        let mut field = SignalField::default();
        field
            .deposit(finding(
                "f1",
                "lib/orders.js",
                "orders-v1",
                0,
                Some("reviewer"),
            ))
            .unwrap();
        let fixed = |path: &str| {
            if path == "lib/orders.js" {
                Some("orders-v2".to_string())
            } else {
                unchanged(path)
            }
        };
        let orders = &field.sense_all(&policy, 0, &fixed)[0];
        assert_eq!(orders.intensity, 0.0);
        assert_eq!(
            orders.deposits[0].excluded,
            Some(TrailExclusion::SourceChanged)
        );
        let removed = |_: &str| None;
        assert_eq!(
            field.sense_all(&policy, 0, &removed)[0].deposits[0].excluded,
            Some(TrailExclusion::SourceMissing)
        );
    }

    #[test]
    fn dispatch_consumes_only_for_the_sensor_that_acted() {
        let policy = policy(None);
        let mut field = SignalField::default();
        field
            .deposit(finding("f1", "lib/orders.js", "orders-v1", 0, None))
            .unwrap();
        let all = field.sense_all(&policy, 0, &unchanged);
        let dispatch = field
            .record_dispatch("d1".into(), sensing(&all, "orders"), 0, false, false)
            .unwrap();
        assert_eq!(dispatch.contributions.len(), 1);
        let all = field.sense_all(&policy, 0, &unchanged);
        assert!(!sensing(&all, "orders").crossed);
        assert_eq!(
            sensing(&all, "orders").deposits[0].excluded,
            Some(TrailExclusion::Consumed)
        );
        // The reviewer watching all of lib/ still senses the same deposit.
        assert!(sensing(&all, "reviewer").crossed);
        assert_eq!(
            field.record_dispatch("d1".into(), sensing(&all, "reviewer"), 0, false, false),
            Err(FieldError::DuplicateDispatch("d1".into()))
        );
        // Nothing left to act on: an automatic dispatch is refused.
        assert!(field
            .record_dispatch("d2".into(), sensing(&all, "orders"), 0, false, false)
            .is_err());
    }

    #[test]
    fn a_person_can_dispatch_below_threshold_with_attribution() {
        let policy = policy(None);
        let mut field = SignalField::default();
        field
            .deposit(change("c1", "lib/orders.js", 0, "catalog"))
            .unwrap();
        let all = field.sense_all(&policy, 0, &unchanged);
        assert!(!sensing(&all, "orders").crossed);
        let dispatch = field
            .record_dispatch("d1".into(), sensing(&all, "orders"), 0, true, false)
            .unwrap();
        assert!(dispatch.manual);
        assert_eq!(dispatch.intensity, 0.5);
    }

    #[test]
    fn a_held_crossing_dispatches_after_evaporation_without_a_new_crossing() {
        let policy = policy(Some(30 * MINUTE));
        let mut field = SignalField::default();
        field
            .deposit(change("c1", "lib/orders.js", 0, "orders"))
            .unwrap();
        // Exactly at threshold when deposited, below it a moment later.
        assert!(field.sense_all(&policy, 0, &unchanged)[2].crossed);
        let later = field.sense_all(&policy, MINUTE, &unchanged);
        assert!(!sensing(&later, "reviewer").crossed);
        assert!(field
            .record_dispatch(
                "d1".into(),
                sensing(&later, "reviewer"),
                MINUTE,
                false,
                false
            )
            .is_err());
        let dispatch = field
            .record_dispatch(
                "d1".into(),
                sensing(&later, "reviewer"),
                MINUTE,
                false,
                true,
            )
            .unwrap();
        assert!(dispatch.latched && !dispatch.manual);
        assert!(dispatch.intensity < dispatch.threshold);
    }

    #[test]
    fn unowned_locations_remain_visible() {
        let policy = policy(None);
        let mut field = SignalField::default();
        field
            .deposit(finding("f1", "docs/README.md", "d", 0, None))
            .unwrap();
        field
            .deposit(finding("f2", "lib/orders.js", "orders-v1", 0, None))
            .unwrap();
        let unrouted: Vec<_> = field
            .unrouted(&policy)
            .iter()
            .map(|d| d.id.clone())
            .collect();
        assert_eq!(unrouted, vec!["f1".to_string()]);
    }

    #[test]
    fn source_changes_become_attributed_deposits_after_the_baseline() {
        let policy = policy(None);
        let mut field = SignalField::default();
        let turn = |id: &str| TrailCause::Turn {
            session_id: "s".into(),
            turn_id: id.into(),
        };
        let initial = BTreeMap::from([
            ("lib/orders.js".to_string(), "orders-v1".to_string()),
            ("lib/catalog.js".to_string(), "catalog-v1".to_string()),
        ]);
        // Existing code is the baseline, not a change.
        assert!(field
            .observe_sources(initial.clone(), 0, None, turn("t0"), "setup")
            .unwrap()
            .is_empty());
        assert_eq!(field.baseline(), Some(&initial));
        let mut edited = initial.clone();
        edited.insert("lib/catalog.js".into(), "catalog-v2".into());
        edited.remove("lib/orders.js");
        edited.insert("lib/new.js".into(), "new-v1".into());
        let added = field
            .observe_sources(
                edited.clone(),
                10,
                Some("catalog".into()),
                turn("t1"),
                "catalog",
            )
            .unwrap();
        assert_eq!(added.len(), 3);
        let summaries: BTreeSet<_> = field.deposits().iter().map(|d| d.summary.clone()).collect();
        assert!(summaries.contains("catalog changed lib/catalog.js"));
        assert!(summaries.contains("catalog removed lib/orders.js"));
        assert!(summaries.contains("catalog added lib/new.js"));
        // The same observation again adds nothing.
        assert!(field
            .observe_sources(edited, 20, Some("catalog".into()), turn("t2"), "catalog")
            .unwrap()
            .is_empty());
        // The catalog owner does not review its own change; the reviewer does.
        let current = |path: &str| unchanged(path);
        let all = field.sense_all(&policy, 20, &current);
        assert_eq!(sensing(&all, "catalog").intensity, 0.0);
        assert!(sensing(&all, "reviewer").crossed);
        assert_eq!(sensing(&all, "orders").intensity, 0.5);
    }

    #[test]
    fn a_withdrawn_deposit_stays_visible_but_stops_attracting() {
        let policy = policy(None);
        let mut field = SignalField::default();
        field
            .deposit(finding("f1", "lib/orders.js", "orders-v1", 0, None))
            .unwrap();
        assert!(field.withdraw("f1", "rejected by a person".into()).unwrap());
        assert!(!field.withdraw("f1", "again".into()).unwrap());
        assert!(field.withdraw("missing", "x".into()).is_err());
        let orders = &field.sense_all(&policy, 0, &unchanged)[0];
        assert!(!orders.crossed);
        assert_eq!(orders.deposits[0].excluded, Some(TrailExclusion::Withdrawn));
        assert_eq!(field.withdrawal("f1"), Some("rejected by a person"));
    }

    #[test]
    fn patterns_follow_repository_conventions() {
        assert!(pattern_matches("lib/orders.js", "lib/orders.js"));
        assert!(!pattern_matches("lib/orders.js", "lib/orders.jsx"));
        assert!(pattern_matches("lib/", "lib/a/b.js"));
        assert!(!pattern_matches("lib/", "library/a.js"));
        assert!(pattern_matches("*.test.js", "lib/deep/x.test.js"));
        assert!(pattern_matches("lib/*.js", "lib/x.js"));
        assert!(!pattern_matches("lib/*.js", "lib/a/x.js"));
        assert!(pattern_matches("lib/**/*.js", "lib/x.js"));
        assert!(pattern_matches("lib/**/*.js", "lib/a/b/x.js"));
        assert!(pattern_matches("**", "anything/at/all"));
        assert!(pattern_matches("lib/?.js", "lib/a.js"));
        assert!(!pattern_matches("lib/?.js", "lib/ab.js"));
    }

    #[test]
    fn policies_and_deposits_are_validated() {
        let mut bad = policy(None);
        bad.sensors[0].threshold = 0.0;
        assert!(bad.validate().is_err());
        let mut bad = policy(None);
        bad.sensors[0].watches = vec!["../outside".into()];
        assert!(bad.validate().is_err());
        let mut bad = policy(None);
        bad.sensors.push(bad.sensors[0].clone());
        assert!(bad.validate().is_err());
        assert!(policy(Some(0)).validate().is_err());
        assert!(policy(Some(MINUTE)).validate().is_ok());

        let mut field = SignalField::default();
        let mut deposit = finding("f1", "lib/orders.js", "h", 0, None);
        deposit.paths = vec!["/etc/passwd".into()];
        deposit.observed.clear();
        assert!(field.deposit(deposit).is_err());
        let mut deposit = finding("f1", "lib/orders.js", "h", 0, None);
        deposit.observed.insert("lib/other.js".into(), "h".into());
        assert!(field.deposit(deposit).is_err());
        let mut deposit = finding("f1", "lib/orders.js", "h", 0, None);
        deposit.strength = f64::NAN;
        assert!(field.deposit(deposit).is_err());
    }

    #[test]
    fn each_changed_path_is_attributed_to_the_turn_that_made_it() {
        let policy = policy(None);
        let mut field = SignalField::default();
        let initial = BTreeMap::from([
            ("lib/orders.js".to_string(), "orders-v1".to_string()),
            ("lib/catalog.js".to_string(), "catalog-v1".to_string()),
        ]);
        field
            .observe_sources(
                initial,
                0,
                None,
                TrailCause::Human { author: None },
                "setup",
            )
            .unwrap();
        // Two turns settled between observations; each changed its own file.
        let edited = BTreeMap::from([
            ("lib/orders.js".to_string(), "orders-v2".to_string()),
            ("lib/catalog.js".to_string(), "catalog-v2".to_string()),
        ]);
        field
            .observe_sources_with(edited, 10, |path| {
                let owner = if path == "lib/orders.js" {
                    "orders"
                } else {
                    "catalog"
                };
                ChangeAttribution {
                    producer: Some(owner.into()),
                    cause: TrailCause::Turn {
                        session_id: "s".into(),
                        turn_id: format!("turn-{owner}"),
                    },
                    label: owner.into(),
                }
            })
            .unwrap();
        let summaries: BTreeSet<_> = field.deposits().iter().map(|d| d.summary.clone()).collect();
        assert!(summaries.contains("orders changed lib/orders.js"));
        assert!(summaries.contains("catalog changed lib/catalog.js"));
        // Nobody senses their own change; the reviewer senses both.
        let current = |path: &str| {
            Some(
                if path == "lib/orders.js" {
                    "orders-v2"
                } else {
                    "catalog-v2"
                }
                .to_string(),
            )
        };
        let all = field.sense_all(&policy, 10, &current);
        assert_eq!(sensing(&all, "orders").intensity, 0.0);
        assert_eq!(sensing(&all, "catalog").intensity, 0.0);
        assert_eq!(sensing(&all, "reviewer").intensity, 1.0);
    }

    fn proposal(id: &str, paths: &[(&str, &str)], turn: &str, producer: &str) -> TrailDeposit {
        TrailDeposit {
            id: id.into(),
            kind: TrailKind::Finding,
            paths: paths.iter().map(|(path, _)| (*path).into()).collect(),
            observed: paths
                .iter()
                .map(|(path, hash)| ((*path).into(), (*hash).into()))
                .collect(),
            strength: TrailKind::Finding.default_strength(),
            deposited_at_ms: 0,
            producer: Some(producer.into()),
            summary: format!("finding {id}"),
            cause: TrailCause::KnowledgeProposal {
                proposal_id: id.into(),
                note_id: id.into(),
                journal_id: "journal".into(),
                turn_id: Some(turn.into()),
            },
            evidence: BTreeMap::new(),
        }
    }

    #[test]
    fn a_multi_file_finding_is_retired_only_for_owners_of_the_changed_file() {
        let policy = policy(None);
        let mut field = SignalField::default();
        field
            .deposit(proposal(
                "both",
                &[
                    ("lib/orders.js", "orders-v1"),
                    ("lib/catalog.js", "catalog-v1"),
                ],
                "t1",
                "reviewer",
            ))
            .unwrap();
        // The orders owner fixed its half; the catalog half is still owed.
        let current = |path: &str| {
            Some(
                if path == "lib/orders.js" {
                    "orders-v2"
                } else {
                    "catalog-v1"
                }
                .to_string(),
            )
        };
        let all = field.sense_all(&policy, 0, &current);
        let orders = sensing(&all, "orders");
        assert_eq!(
            orders.deposits[0].excluded,
            Some(TrailExclusion::SourceChanged)
        );
        let catalog = sensing(&all, "catalog");
        assert!(catalog.crossed);
        assert_eq!(
            catalog.deposits[0].evidence_changed,
            vec!["lib/orders.js".to_string()]
        );
    }

    #[test]
    fn evidence_changes_are_annotated_but_never_retire_or_route() {
        let policy = policy(None);
        let mut field = SignalField::default();
        let mut deposit = proposal("f", &[("lib/catalog.js", "catalog-v1")], "t1", "reviewer");
        deposit.evidence = BTreeMap::from([("lib/orders.js".to_string(), "orders-v1".to_string())]);
        field.deposit(deposit).unwrap();
        let current = |path: &str| {
            Some(
                if path == "lib/orders.js" {
                    "orders-v2"
                } else {
                    "catalog-v1"
                }
                .to_string(),
            )
        };
        let all = field.sense_all(&policy, 0, &current);
        assert_eq!(sensing(&all, "orders").deposits.len(), 0);
        let catalog = sensing(&all, "catalog");
        assert!(catalog.crossed);
        assert_eq!(
            catalog.deposits[0].evidence_changed,
            vec!["lib/orders.js".to_string()]
        );
    }

    #[test]
    fn one_source_counts_once_however_many_deposits_it_leaves() {
        let policy = policy(None);
        let mut field = SignalField::default();
        for id in ["a", "b", "c"] {
            field
                .deposit(proposal(
                    id,
                    &[("lib/catalog.js", "catalog-v1")],
                    "t1",
                    "reviewer",
                ))
                .unwrap();
        }
        let current = |path: &str| unchanged(path);
        let all = field.sense_all(&policy, 0, &current);
        let catalog = sensing(&all, "catalog");
        assert_eq!(catalog.intensity, 1.0);
        assert_eq!(
            catalog
                .deposits
                .iter()
                .filter(|sensed| sensed.counted_with.as_deref() == Some("a"))
                .count(),
            2
        );
        // An independent turn is a second source and adds its own vote.
        field
            .deposit(proposal(
                "d",
                &[("lib/catalog.js", "catalog-v1")],
                "t2",
                "orders",
            ))
            .unwrap();
        let all = field.sense_all(&policy, 0, &current);
        assert_eq!(sensing(&all, "catalog").intensity, 2.0);
        // Dispatch consumes every deposit of the source it acted on.
        let catalog = sensing(&all, "catalog").clone();
        let dispatch = field
            .record_dispatch("dispatch".into(), &catalog, 0, false, false)
            .unwrap();
        assert_eq!(dispatch.contributions.len(), 4);
        assert_eq!(
            sensing(&field.sense_all(&policy, 0, &current), "catalog").intensity,
            0.0
        );
    }

    #[test]
    fn note_revisions_and_one_unattributed_observation_each_count_once() {
        let policy = policy(None);
        let mut field = SignalField::default();
        for revision in [1, 2] {
            field
                .deposit(TrailDeposit {
                    id: format!("note:n@{revision}"),
                    kind: TrailKind::Human,
                    paths: vec!["lib/catalog.js".into()],
                    observed: BTreeMap::from([("lib/catalog.js".into(), "catalog-v1".into())]),
                    strength: 1.0,
                    deposited_at_ms: revision,
                    producer: None,
                    summary: "person's note".into(),
                    cause: TrailCause::KnowledgeNote {
                        note_id: "n".into(),
                        revision,
                    },
                    evidence: BTreeMap::new(),
                })
                .unwrap();
        }
        let current = |path: &str| unchanged(path);
        assert_eq!(
            sensing(&field.sense_all(&policy, 2, &current), "catalog").intensity,
            1.0
        );

        let mut field = SignalField::default();
        field
            .observe_sources(
                BTreeMap::from([
                    ("lib/orders.js".to_string(), "orders-v1".to_string()),
                    ("lib/catalog.js".to_string(), "catalog-v1".to_string()),
                ]),
                0,
                None,
                TrailCause::Human { author: None },
                "setup",
            )
            .unwrap();
        field
            .observe_sources(
                BTreeMap::from([
                    ("lib/orders.js".to_string(), "orders-v2".to_string()),
                    ("lib/catalog.js".to_string(), "catalog-v2".to_string()),
                ]),
                5,
                None,
                TrailCause::Human { author: None },
                "A workspace edit",
            )
            .unwrap();
        let current = |path: &str| Some(format!("{}-v2", &path[4..path.len() - 3]));
        assert_eq!(
            sensing(&field.sense_all(&policy, 5, &current), "reviewer").intensity,
            0.5
        );
    }
}
