use super::*;
use axocoatl_core::ReasoningEffort;

/// Profiles written by 1.2.0: one non-reasoning text/tool endpoint priced by
/// prompt and completion alone. Still accepted, and still checked against the
/// live catalog, so a Session that retained one keeps working.
const LEGACY_SCHEMA: u32 = 1;
const LEGACY_EVIDENCE: &str = "openrouter-model-catalog:no-reasoning-contract-v1";
/// Profiles that may carry a reasoning contract and whose two price ceilings
/// are the highest input and output rates of every priced component and tier.
const SCHEMA: u32 = 2;

/// Provider chat templates and tool preambles add tokens that are not in the
/// request bytes (Anthropic's tool-use system prompt alone is several hundred
/// tokens). Every prompt bound includes this many tokens for them.
pub const PROMPT_TEMPLATE_ALLOWANCE: usize = 4096;
/// OpenRouter documents a 1,024-token minimum reasoning budget, and the
/// request's `max_tokens` must stay above it.
pub const MINIMUM_REASONING_ALLOWANCE: usize = 1024;
/// Bytes a Session keeps per response token while a call streams. A token of
/// text is about four bytes; reasoning text is kept twice (streamed as it
/// arrives, and in the reasoning blocks sent back with tool results), each
/// JSON-escaped and framed. A response allowance whose tokens at this rate
/// pass a call's response byte bound is refused before inference.
pub const STREAMED_BYTES_PER_RESPONSE_TOKEN: usize = 10;

/// OpenRouter's service tiers other than the standard one. A tier endpoint
/// (`openai/flex`, `openai/fast`, `google-vertex/flex`) is never matched by
/// its base slug and serves a request only when the request opts into it;
/// it trades latency, availability or price. Native requests use the
/// standard tier only, so such endpoints are not selected.
const SERVICE_TIERS: [&str; 4] = ["flex", "fast", "priority", "ultrafast"];
/// Model id suffixes that opt into a service tier.
const TIER_VARIANTS: [&str; 2] = [":nitro", ":floor"];

/// Whether an endpoint tag names a service-tier endpoint.
pub(super) fn service_tier_endpoint(tag: &str) -> bool {
    tag.rsplit_once('/')
        .is_some_and(|(_, suffix)| SERVICE_TIERS.contains(&suffix))
}

fn tier_variant(model: &str) -> bool {
    TIER_VARIANTS.iter().any(|suffix| model.ends_with(suffix))
}

/// Priced features a request can only start by asking for them. Axocoatl's
/// native request never does: it sends text, function tools only, no
/// `modalities`, no `web_search_options`, an exact model id that is not an
/// `:online` variant, and an explicit `{"id":"web","enabled":false}` plugin.
const UNREQUESTED_FEATURES: [&str; 6] = [
    "web_search",
    "image",
    "audio",
    "input_audio_cache",
    "image_output",
    "audio_output",
];

/// Which `reasoning.effort` values the catalog says a model accepts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeOpenRouterEfforts {
    /// `supported_efforts` is absent: the model exposes no effort selection.
    Unavailable,
    /// `supported_efforts` is null: every gateway effort is accepted.
    Any,
    /// Exactly these, as listed.
    Listed(Vec<String>),
}

/// The reasoning object of a model in OpenRouter's catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeOpenRouterReasoning {
    /// The model rejects requests that turn reasoning off.
    pub mandatory: bool,
    /// The model reasons when a request says nothing about reasoning.
    pub enabled_by_default: bool,
    pub efforts: NativeOpenRouterEfforts,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
}

/// The `reasoning` object Axocoatl sends with every call of one Agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeOpenRouterReasoningRequest {
    /// `{"effort": ...}`.
    Effort(ReasoningEffort),
    /// `{"enabled": true}`: reasoning at OpenRouter's default (medium) level,
    /// for a model that exposes no effort selection.
    Enabled,
    /// `{"enabled": false}`, for a model that allows reasoning off but has no
    /// `none` effort.
    Disabled,
}

impl NativeOpenRouterReasoningRequest {
    pub(super) fn wire(self) -> Value {
        match self {
            Self::Effort(effort) => json!({"effort": effort.as_str()}),
            Self::Enabled => json!({"enabled": true}),
            Self::Disabled => json!({"enabled": false}),
        }
    }

    /// Tokens of reasoning allowed on top of an output limit. OpenRouter maps
    /// an effort to a share of `max_tokens` (max and xhigh 95%, high 80%,
    /// medium 50%, low 20%, minimal 10%), so the visible output keeps `output`
    /// when `max_tokens` is `output / (1 - share)`. Never less than the
    /// documented minimum reasoning budget while reasoning is on.
    pub fn allowance(self, output: usize) -> usize {
        // (numerator, denominator) of share / (1 - share).
        let (numerator, denominator) = match self {
            Self::Effort(ReasoningEffort::None) | Self::Disabled => return 0,
            Self::Effort(ReasoningEffort::Max | ReasoningEffort::Xhigh) => (19, 1),
            Self::Effort(ReasoningEffort::High) => (4, 1),
            Self::Effort(ReasoningEffort::Medium) | Self::Enabled => (1, 1),
            Self::Effort(ReasoningEffort::Low) => (1, 4),
            Self::Effort(ReasoningEffort::Minimal) => (1, 9),
        };
        output
            .saturating_mul(numerator)
            .div_ceil(denominator)
            .max(MINIMUM_REASONING_ALLOWANCE)
    }
}

/// Public metadata contains no credential. It is retained beside the exact
/// definition and checked again before inference; wire caps enforce its prices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeOpenRouterObservation {
    pub schema_version: u32,
    pub base_url: String,
    pub model: String,
    pub endpoint_tag: String,
    pub provider_name: String,
    pub context_tokens: usize,
    pub max_output_tokens: usize,
    /// Dollars per million input tokens: the highest of the prompt, cache
    /// read and cache write prices at every price tier. Each input token is
    /// billed at one of them.
    pub prompt_price_per_million: String,
    /// Dollars per million output tokens, reasoning included: the highest of
    /// the completion and internal reasoning prices at every tier.
    pub completion_price_per_million: String,
    pub supported_parameters: Vec<String>,
    /// Schema 1 only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub non_reasoning_evidence: Option<String>,
    /// The endpoint's own prompt limit, when below the context window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_prompt_tokens: Option<usize>,
    /// Dollars per request, when the endpoint charges one (highest tier).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_price: Option<String>,
    /// The catalog's reasoning contract for a reasoning model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<NativeOpenRouterReasoning>,
    /// Priced features this endpoint offers that Axocoatl's request cannot
    /// start; see `UNREQUESTED_FEATURES`. Retained so a change re-reviews.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unrequested_priced_features: Vec<String>,
    pub observed_at_ms: u64,
    pub billing: String,
}

/// Bounds of one exact request, before it is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CallShape {
    /// Upper bound of the prompt tokens the provider can bill.
    pub prompt_tokens: usize,
    /// `max_tokens` on the wire: visible output plus reasoning.
    pub response_tokens: usize,
}

impl NativeOpenRouterObservation {
    pub fn validate(&self) -> Result<(), ProviderError> {
        endpoint(&self.base_url, "")?;
        if !matches!(self.schema_version, LEGACY_SCHEMA | SCHEMA)
            || self.model.starts_with("openrouter/")
            || self.model.split('/').count() != 2
            || self.model.ends_with(":online")
            || tier_variant(&self.model)
            || !valid_tag(&self.endpoint_tag)
            || service_tier_endpoint(&self.endpoint_tag)
            || self.provider_name.is_empty()
            || !(2048..=16 * 1024 * 1024).contains(&self.context_tokens)
            || self.max_output_tokens == 0
            || self.max_output_tokens > self.context_tokens
            || self
                .max_prompt_tokens
                .is_some_and(|limit| limit == 0 || limit > self.context_tokens)
            || self.observed_at_ms == 0
            || self.billing != "openrouter_credits"
            || !self.supports("max_tokens")
            || !self.supports("tools")
            || self
                .unrequested_priced_features
                .iter()
                .any(|feature| !UNREQUESTED_FEATURES.contains(&feature.as_str()))
        {
            return Err(invalid(
                "OpenRouter profile lacks exact text/tool endpoint bounds",
            ));
        }
        if self.schema_version == LEGACY_SCHEMA {
            if self.non_reasoning_evidence.as_deref() != Some(LEGACY_EVIDENCE)
                || !self.endpoint_tag.contains('/')
                || self.max_prompt_tokens.is_some()
                || self.request_price.is_some()
                || self.reasoning.is_some()
                || !self.unrequested_priced_features.is_empty()
                || self
                    .supported_parameters
                    .iter()
                    .any(|p| p.contains("reasoning"))
            {
                return Err(invalid(
                    "OpenRouter 1.2.0 profile lacks its exact non-reasoning text/tool bounds",
                ));
            }
        } else if self.non_reasoning_evidence.is_some()
            || (self.reasoning.is_some() && !self.supports("reasoning"))
        {
            return Err(invalid(
                "OpenRouter profile's reasoning contract does not match its endpoint parameters",
            ));
        }
        for price in [
            &self.prompt_price_per_million,
            &self.completion_price_per_million,
        ] {
            money::decimal_units(price)?;
        }
        if let Some(fee) = &self.request_price {
            money::decimal_units(fee)?;
        }
        Ok(())
    }

    fn supports(&self, parameter: &str) -> bool {
        self.supported_parameters.iter().any(|p| p == parameter)
    }

    /// The prompt tokens the provider can accept in one request.
    pub fn prompt_limit(&self) -> usize {
        self.max_prompt_tokens
            .unwrap_or(self.context_tokens)
            .min(self.context_tokens)
    }

    /// The reasoning setting to send for an Agent: its configured effort, or
    /// the model's own default. Refuses, by name, an effort the model does
    /// not accept and `none` for a model whose reasoning is mandatory.
    pub fn reasoning_request(
        &self,
        configured: Option<ReasoningEffort>,
    ) -> Result<Option<NativeOpenRouterReasoningRequest>, ProviderError> {
        reasoning_request_for(&self.model, self.reasoning.as_ref(), configured)
    }

    /// Whether some Agent setting resolves to `request` for this model.
    pub(super) fn accepts_reasoning(
        &self,
        request: Option<NativeOpenRouterReasoningRequest>,
    ) -> bool {
        const EFFORTS: [ReasoningEffort; 7] = [
            ReasoningEffort::Max,
            ReasoningEffort::Xhigh,
            ReasoningEffort::High,
            ReasoningEffort::Medium,
            ReasoningEffort::Low,
            ReasoningEffort::Minimal,
            ReasoningEffort::None,
        ];
        std::iter::once(None)
            .chain(EFFORTS.into_iter().map(Some))
            .any(|configured| {
                self.reasoning_request(configured)
                    .is_ok_and(|resolved| resolved == request)
            })
    }

    /// `max_tokens` for a call whose visible output may reach `output`: the
    /// output plus the reasoning allowance. It is never clamped to the
    /// endpoint's output limit, which would silently cut the visible output;
    /// an Agent whose allowance passes that limit is refused instead.
    pub fn response_allowance(
        &self,
        output: usize,
        reasoning: Option<NativeOpenRouterReasoningRequest>,
    ) -> usize {
        output.saturating_add(reasoning.map_or(0, |request| request.allowance(output)))
    }

    /// Why a call whose visible output may reach `output` cannot run on this
    /// endpoint, in plain words: its response allowance passes the
    /// endpoint's output limit, or streaming it can pass `response_bytes`.
    pub fn response_refusal(
        &self,
        output: usize,
        reasoning: Option<NativeOpenRouterReasoningRequest>,
        response_bytes: usize,
    ) -> Option<String> {
        let allowance = self.response_allowance(output, reasoning);
        let parts = if allowance > output {
            format!(
                "{output} of output plus {} of reasoning",
                allowance - output
            )
        } else {
            format!("{output} of output")
        };
        if allowance > self.max_output_tokens {
            return Some(format!(
                "each call to {} would need max_tokens {allowance} ({parts}), more than the \
                 {} output tokens it allows on OpenRouter endpoint {}",
                self.model, self.max_output_tokens, self.endpoint_tag
            ));
        }
        if allowance.saturating_mul(STREAMED_BYTES_PER_RESPONSE_TOKEN) > response_bytes {
            return Some(format!(
                "each call to {} may stream {allowance} tokens ({parts}), more than the \
                 {} KiB a Session keeps of one response ({STREAMED_BYTES_PER_RESPONSE_TOKEN} \
                 bytes per token)",
                self.model,
                response_bytes / 1024
            ));
        }
        None
    }

    /// Hard bounds of one call: its prompt bound plus its `max_tokens`, at the
    /// highest input and output rates, plus any per-request fee.
    pub(super) fn call_bounds(
        &self,
        shape: CallShape,
        response_bytes: usize,
    ) -> Result<ProviderExecutionBounds, ProviderError> {
        self.validate()?;
        if shape.response_tokens == 0
            || shape.response_tokens > self.max_output_tokens
            || shape.prompt_tokens > self.prompt_limit()
        {
            return Err(invalid("call exceeds the retained endpoint limits"));
        }
        let prompt = shape.prompt_tokens as u64;
        let response = shape.response_tokens as u64;
        Ok(ProviderExecutionBounds {
            token_limit: prompt
                .checked_add(response)
                .ok_or_else(|| invalid("token bound overflow"))?,
            cost_microunits: money::charge_bound(
                prompt,
                response,
                &self.prompt_price_per_million,
                &self.completion_price_per_million,
                self.request_price.as_deref(),
            )?,
            response_bytes,
        })
    }

    /// The bounds of the smallest call an Agent can make: no request bytes,
    /// only the template allowance, and its full response allowance. Larger
    /// prompts reserve more when they are sent.
    pub fn minimum_call_bounds(
        &self,
        output: usize,
        reasoning: Option<NativeOpenRouterReasoningRequest>,
        response_bytes: usize,
    ) -> Result<ProviderExecutionBounds, ProviderError> {
        if output == 0 || output > self.max_output_tokens {
            return Err(invalid("output exceeds retained endpoint ceiling"));
        }
        self.call_bounds(
            CallShape {
                prompt_tokens: PROMPT_TEMPLATE_ALLOWANCE.min(self.prompt_limit()),
                response_tokens: self.response_allowance(output, reasoning),
            },
            response_bytes,
        )
    }

    /// Whether a fresh observation still matches this retained profile.
    pub(super) fn same_contract(&self, observed: &Self) -> bool {
        let mut current = observed.clone();
        current.observed_at_ms = self.observed_at_ms;
        if self.schema_version == LEGACY_SCHEMA {
            return current.as_legacy().is_some_and(|legacy| legacy == *self);
        }
        current == *self
    }

    /// This observation in the 1.2.0 form, when that form can express it.
    fn as_legacy(&self) -> Option<Self> {
        if self.reasoning.is_some()
            || self.request_price.is_some()
            || !self.unrequested_priced_features.is_empty()
            || !self.endpoint_tag.contains('/')
            || self
                .supported_parameters
                .iter()
                .any(|p| p.contains("reasoning"))
        {
            return None;
        }
        Some(Self {
            schema_version: LEGACY_SCHEMA,
            non_reasoning_evidence: Some(LEGACY_EVIDENCE.into()),
            max_prompt_tokens: None,
            ..self.clone()
        })
    }

    fn price_order(&self) -> (u128, u128, u128) {
        let units = |text: &str| money::decimal_units(text).unwrap_or(u128::MAX);
        (
            units(&self.completion_price_per_million),
            units(&self.prompt_price_per_million),
            self.request_price.as_deref().map_or(0, units),
        )
    }
}

fn listed(efforts: &NativeOpenRouterEfforts) -> String {
    match efforts {
        NativeOpenRouterEfforts::Listed(efforts) => format!(" ({})", efforts.join(", ")),
        _ => String::new(),
    }
}

fn valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 256
        && !tag.starts_with('/')
        && !tag.ends_with('/')
        && tag
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"/_-.".contains(&c))
}

fn endpoint(base: &str, path: &str) -> Result<String, ProviderError> {
    let parsed =
        reqwest::Url::parse(base).map_err(|_| invalid("invalid OpenRouter metadata endpoint"))?;
    let official = parsed.scheme() == "https"
        && parsed.host_str() == Some("openrouter.ai")
        && parsed.path().trim_end_matches('/') == "/api/v1";
    #[cfg(any(test, feature = "test-loopback-endpoint"))]
    let official = official
        || (parsed.scheme() == "http"
            && matches!(parsed.host_str(), Some("127.0.0.1" | "localhost")));
    if !official {
        return Err(invalid(
            "native OpenRouter requires its exact first-party endpoint",
        ));
    }
    validated_endpoint(base, path, PROVIDER)
}
pub(super) async fn metadata(
    client: &reqwest::Client,
    base: &str,
    key: &str,
    path: &str,
) -> Result<Value, ProviderError> {
    let response = tokio::time::timeout(
        RESPONSE_TIMEOUT,
        client.get(endpoint(base, path)?).bearer_auth(key).send(),
    )
    .await
    .map_err(|_| ProviderError::Network("timed out: OpenRouter metadata did not answer".into()))?
    .map_err(|e| super::transport_error(&e, &[key]))?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let retry_after = super::retry_after_secs(response.headers());
        let detail = read_error_text(response, &[key]).await;
        return Err(ProviderError::ApiError {
            provider: PROVIDER.into(),
            status,
            message: super::with_retry_after(detail, retry_after),
        });
    }
    tokio::time::timeout(RESPONSE_TIMEOUT, read_json(response, PROVIDER))
        .await
        .map_err(|_| invalid("OpenRouter metadata body timed out"))?
}
fn strings(value: &Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|v| v.as_str().map(str::to_owned))
        .collect()
}
fn text_only(value: &Value) -> bool {
    strings(value).is_some_and(|items| items == ["text"])
}
fn integer(value: &Value) -> Option<usize> {
    usize::try_from(value.as_u64()?).ok()
}
fn price_text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.is_number().then(|| value.to_string()))
}
fn zero(value: &Value) -> bool {
    price_text(value).and_then(|text| money::decimal_units(&text).ok()) == Some(0)
}

/// The reasoning setting a native call to `model` sends, from the model's
/// catalog `contract`: the Agent's `configured` effort, or the model's own
/// default ([`NativeOpenRouterObservation::reasoning_request`]).
fn reasoning_request_for(
    model: &str,
    contract: Option<&NativeOpenRouterReasoning>,
    configured: Option<ReasoningEffort>,
) -> Result<Option<NativeOpenRouterReasoningRequest>, ProviderError> {
    let Some(contract) = contract else {
        return match configured {
            None => Ok(None),
            Some(effort) => Err(invalid(format!(
                "{model} is not a reasoning model in OpenRouter's catalog, so \
                 sampling.reasoning_effort: {effort} cannot be sent; remove it"
            ))),
        };
    };
    let Some(effort) = configured else {
        // The model's own default: off, its default effort, or plain on.
        if !contract.enabled_by_default {
            return Ok(None);
        }
        return match contract.default_effort.as_deref() {
            Some("none") => Ok(None),
            Some(name) => {
                let effort = ReasoningEffort::parse(name).ok_or_else(|| {
                    invalid(format!(
                        "{model}'s default reasoning effort `{name}` is not one Axocoatl \
                         knows; set sampling.reasoning_effort for this Agent"
                    ))
                })?;
                Ok(Some(NativeOpenRouterReasoningRequest::Effort(effort)))
            }
            None => Ok(Some(NativeOpenRouterReasoningRequest::Enabled)),
        };
    };
    if effort == ReasoningEffort::None && contract.mandatory {
        return Err(invalid(format!(
            "{model} requires reasoning, so sampling.reasoning_effort: none is refused; \
             choose an effort it accepts{}",
            listed(&contract.efforts)
        )));
    }
    let request = match &contract.efforts {
        NativeOpenRouterEfforts::Any => NativeOpenRouterReasoningRequest::Effort(effort),
        NativeOpenRouterEfforts::Listed(efforts)
            if efforts.iter().any(|name| name == effort.as_str()) =>
        {
            NativeOpenRouterReasoningRequest::Effort(effort)
        }
        // Off is still a choice where no `none` effort is listed.
        _ if effort == ReasoningEffort::None => NativeOpenRouterReasoningRequest::Disabled,
        NativeOpenRouterEfforts::Unavailable => {
            return Err(invalid(format!(
                "{model} does not let a request choose its reasoning effort; remove \
                 sampling.reasoning_effort: {effort}"
            )))
        }
        NativeOpenRouterEfforts::Listed(_) => {
            return Err(invalid(format!(
                "{model} does not accept reasoning effort {effort}{}",
                listed(&contract.efforts)
            )))
        }
    };
    Ok(Some(request))
}

/// The catalog's reasoning object. Absent or null means no reasoning.
fn reasoning_contract(value: Option<&Value>) -> Result<Option<NativeOpenRouterReasoning>, String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let object = value
        .as_object()
        .ok_or("the catalog's reasoning object is malformed")?;
    let flag = |name: &str| match object.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(format!("the catalog's reasoning.{name} is not a boolean")),
    };
    let mandatory = flag("mandatory")?.unwrap_or(false);
    let efforts = match object.get("supported_efforts") {
        None => NativeOpenRouterEfforts::Unavailable,
        Some(Value::Null) => NativeOpenRouterEfforts::Any,
        Some(list) => NativeOpenRouterEfforts::Listed(
            strings(list).ok_or("the catalog's reasoning.supported_efforts is malformed")?,
        ),
    };
    let default_effort = match object.get("default_effort") {
        None | Some(Value::Null) => None,
        Some(Value::String(effort)) => Some(effort.clone()),
        Some(_) => return Err("the catalog's reasoning.default_effort is malformed".into()),
    };
    let enabled_by_default = flag("default_enabled")?.unwrap_or(
        mandatory
            || default_effort
                .as_deref()
                .is_some_and(|effort| effort != "none"),
    );
    Ok(Some(NativeOpenRouterReasoning {
        mandatory,
        enabled_by_default,
        efforts,
        default_effort,
    }))
}

/// The highest price of each kind across the base prices and every tier.
#[derive(Default)]
struct Ceilings {
    input: u128,
    output: u128,
    request: u128,
    unrequested: std::collections::BTreeSet<String>,
}

impl Ceilings {
    fn add(&mut self, key: &str, value: &Value) -> Result<(), String> {
        if key == "discount" {
            // A discount lowers what is billed; the listed prices bound it.
            return match value.as_f64() {
                Some(share) if (0.0..=1.0).contains(&share) => Ok(()),
                _ => Err("the endpoint's discount is not a share between 0 and 1".into()),
            };
        }
        let input = key == "prompt" || key.starts_with("input_cache_");
        let output = key == "completion" || key == "internal_reasoning";
        let request = key == "request";
        let unrequested = UNREQUESTED_FEATURES.contains(&key);
        if !(input || output || request || unrequested) {
            return if zero(value) {
                Ok(())
            } else {
                Err(format!(
                    "the endpoint charges for `{key}`, which Axocoatl cannot bound"
                ))
            };
        }
        let text = price_text(value).ok_or_else(|| format!("the `{key}` price is malformed"))?;
        let units =
            money::ceiling_units(&text).map_err(|_| format!("the `{key}` price is malformed"))?;
        if units == 0 {
            return Ok(());
        }
        let slot = if input {
            &mut self.input
        } else if output {
            &mut self.output
        } else if request {
            &mut self.request
        } else {
            self.unrequested.insert(key.to_owned());
            return Ok(());
        };
        *slot = (*slot).max(units);
        Ok(())
    }
}

fn endpoint_ceilings(pricing: &serde_json::Map<String, Value>) -> Result<Ceilings, String> {
    let mut ceilings = Ceilings::default();
    let mut base_prompt = false;
    let mut base_completion = false;
    for (key, value) in pricing {
        match key.as_str() {
            "overrides" => {
                // Tiered and timed prices: every tier is a possible rate.
                for tier in value
                    .as_array()
                    .ok_or("the endpoint's price tiers are malformed")?
                {
                    let tier = tier
                        .as_object()
                        .ok_or("the endpoint's price tiers are malformed")?;
                    for (key, value) in tier {
                        if matches!(key.as_str(), "min_prompt_tokens" | "utc_start" | "utc_end") {
                            continue;
                        }
                        ceilings.add(key, value)?;
                    }
                }
            }
            key => {
                base_prompt |= key == "prompt";
                base_completion |= key == "completion";
                ceilings.add(key, value)?;
            }
        }
    }
    if !base_prompt || !base_completion {
        return Err("missing prompt or completion prices".into());
    }
    Ok(ceilings)
}

/// The smallest native call to one model, as OpenRouter's public model
/// catalog (`GET /models`, no key, no charge) shows it before a Session
/// exists: what Team & budget later checks against the endpoint it selects
/// ([`NativeOpenRouterObservation::minimum_call_bounds`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogCallFloor {
    /// The model's context window, `context_length` in the catalog.
    pub context_tokens: usize,
    /// The prompt the smallest call reserves: the template allowance, within
    /// the context. A call reserves its own request, not the context window.
    pub prompt_tokens: usize,
    /// The reasoning allowance each call adds to its output limit.
    pub reasoning_tokens: usize,
    /// The reasoning setting each call sends.
    pub reasoning: Option<NativeOpenRouterReasoningRequest>,
}

impl CatalogCallFloor {
    /// Tokens of the smallest call with `output` output tokens.
    pub fn tokens(&self, output: usize) -> usize {
        self.prompt_tokens
            .saturating_add(output)
            .saturating_add(self.reasoning_tokens)
    }
}

/// [`CatalogCallFloor`] of `model` in `catalog` (the body of `GET /models`)
/// for an Agent whose output limit is `output` and whose configured effort
/// is `effort`. `None` when the catalog cannot say: the model is not listed
/// exactly once, has no context, or its reasoning contract refuses `effort`.
/// The model's own observation, when its Session applies the Team, then
/// refuses it by name.
pub fn catalog_call_floor(
    catalog: &Value,
    model: &str,
    output: usize,
    effort: Option<ReasoningEffort>,
) -> Option<CatalogCallFloor> {
    let rows = catalog.get("data")?.as_array()?;
    let mut selected = rows
        .iter()
        .filter(|row| row.get("id").and_then(Value::as_str) == Some(model));
    let row = selected.next()?;
    if selected.next().is_some() {
        return None;
    }
    let context = integer(&row["context_length"])
        .or_else(|| integer(&row["top_provider"]["context_length"]))
        .filter(|context| *context > 0)?;
    let contract = reasoning_contract(row.get("reasoning")).ok()?;
    let reasoning = reasoning_request_for(model, contract.as_ref(), effort).ok()?;
    Some(CatalogCallFloor {
        context_tokens: context,
        prompt_tokens: PROMPT_TEMPLATE_ALLOWANCE.min(context),
        reasoning_tokens: reasoning.map_or(0, |request| request.allowance(output)),
        reasoning,
    })
}

pub async fn observe_native_openrouter_profiles(
    base_url: &str,
    api_key: &str,
    model: &str,
) -> Result<Vec<NativeOpenRouterObservation>, ProviderError> {
    if model.len() > 256
        || model.starts_with("openrouter/")
        || model.split('/').count() != 2
        || !model
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"/_-.:".contains(&c))
    {
        return Err(invalid("an exact static OpenRouter model id is required"));
    }
    if model.ends_with(":online") {
        return Err(invalid(
            "a :online model id turns on OpenRouter web search, which native Sessions \
             never request; use the model id without :online",
        ));
    }
    if tier_variant(model) {
        return Err(invalid(
            "a :nitro or :floor model id opts into an OpenRouter service tier; native \
             Sessions use the standard tier, so use the model id without the suffix",
        ));
    }
    let client = http_client();
    let catalog = metadata(&client, base_url, api_key, "models").await?;
    let rows = catalog
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("OpenRouter model catalog is unavailable"))?;
    let selected = rows
        .iter()
        .filter(|m| m.get("id").and_then(Value::as_str) == Some(model))
        .collect::<Vec<_>>();
    if selected.len() != 1 {
        return Err(invalid(format!(
            "{model} is missing or ambiguous in OpenRouter's model catalog"
        )));
    }
    let model_row = selected[0];
    let catalog_parameters = strings(&model_row["supported_parameters"])
        .ok_or_else(|| invalid(format!("{model} lacks declared parameter capabilities")))?;
    if !catalog_parameters.iter().any(|p| p == "tools") {
        return Err(invalid(format!(
            "{model} does not support tool calling on OpenRouter, which native Agents need"
        )));
    }
    if !strings(&model_row["architecture"]["input_modalities"])
        .is_some_and(|modalities| modalities.iter().any(|m| m == "text"))
        || !text_only(&model_row["architecture"]["output_modalities"])
    {
        return Err(invalid(format!(
            "{model} does not read and write text only; native Sessions support text models"
        )));
    }
    let reasoning = reasoning_contract(model_row.get("reasoning"))
        .map_err(|reason| invalid(format!("{model}: {reason}")))?;
    if reasoning.is_none() && catalog_parameters.iter().any(|p| p == "reasoning") {
        return Err(invalid(format!(
            "{model} accepts reasoning but OpenRouter's catalog does not describe how; \
             its reasoning spend cannot be bounded"
        )));
    }
    let data = metadata(
        &client,
        base_url,
        api_key,
        &format!("models/{model}/endpoints"),
    )
    .await?;
    if data["data"]["id"].as_str() != Some(model) {
        return Err(invalid("endpoint metadata changed model identity"));
    }
    let rows = data["data"]["endpoints"]
        .as_array()
        .ok_or_else(|| invalid("endpoint metadata is unavailable"))?;
    let tags = rows
        .iter()
        .filter_map(|row| row["tag"].as_str())
        .collect::<Vec<_>>();
    let observed_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .ok_or_else(|| invalid("observation clock unavailable"))?;
    let mut profiles = Vec::new();
    let mut rejected = std::collections::BTreeSet::new();
    for row in rows {
        let Some(tag) = row["tag"].as_str().filter(|tag| valid_tag(tag)) else {
            rejected.insert("an endpoint has no usable tag".to_owned());
            continue;
        };
        let reject = |reason: &str| format!("{tag}: {reason}");
        if service_tier_endpoint(tag) {
            rejected.insert(reject(
                "a service-tier endpoint a request must opt into; native Sessions use the \
                 standard tier",
            ));
            continue;
        }
        // `only: ["base"]` matches every `base/...` variant except service
        // tiers, which a base slug never selects; a base tag pins one
        // endpoint only when no other variant shares it.
        let shared = tags
            .iter()
            .filter(|other| {
                **other == tag
                    || (!tag.contains('/')
                        && !service_tier_endpoint(other)
                        && other.starts_with(&format!("{tag}/")))
            })
            .count();
        if shared != 1 {
            rejected.insert(reject("its tag also selects other endpoint variants"));
            continue;
        }
        if row["model_id"].as_str() != Some(model) || row["status"].as_i64() != Some(0) {
            rejected.insert(reject("model identity or endpoint availability differs"));
            continue;
        }
        let Some(context) = integer(&row["context_length"]) else {
            rejected.insert(reject("missing context limit"));
            continue;
        };
        let Some(output) = integer(&row["max_completion_tokens"]) else {
            rejected.insert(reject("missing output limit"));
            continue;
        };
        let max_prompt_tokens = match &row["max_prompt_tokens"] {
            Value::Null => None,
            value => match integer(value) {
                Some(limit) if limit < context => Some(limit),
                Some(_) => None,
                None => {
                    rejected.insert(reject("malformed prompt limit"));
                    continue;
                }
            },
        };
        let Some(mut supported) = strings(&row["supported_parameters"]) else {
            rejected.insert(reject("missing parameter capabilities"));
            continue;
        };
        supported.sort();
        supported.dedup();
        if !supported.iter().any(|p| p == "max_tokens") || !supported.iter().any(|p| p == "tools") {
            rejected.insert(reject("does not accept max_tokens and tools"));
            continue;
        }
        if reasoning.is_some() && !supported.iter().any(|p| p == "reasoning") {
            rejected.insert(reject("does not accept the reasoning parameter"));
            continue;
        }
        let Some(pricing) = row["pricing"].as_object() else {
            rejected.insert(reject("missing endpoint pricing"));
            continue;
        };
        let ceilings = match endpoint_ceilings(pricing) {
            Ok(ceilings) => ceilings,
            Err(reason) => {
                rejected.insert(reject(&reason));
                continue;
            }
        };
        let Some(provider_name) = row["provider_name"].as_str() else {
            rejected.insert(reject("missing provider identity"));
            continue;
        };
        let profile = NativeOpenRouterObservation {
            schema_version: SCHEMA,
            base_url: base_url.trim_end_matches('/').into(),
            model: model.into(),
            endpoint_tag: tag.into(),
            provider_name: provider_name.into(),
            context_tokens: context,
            max_output_tokens: output,
            prompt_price_per_million: money::per_million_text(ceilings.input)?,
            completion_price_per_million: money::per_million_text(ceilings.output)?,
            supported_parameters: supported,
            non_reasoning_evidence: None,
            max_prompt_tokens,
            request_price: (ceilings.request > 0).then(|| money::units_text(ceilings.request)),
            reasoning: reasoning.clone(),
            unrequested_priced_features: ceilings.unrequested.into_iter().collect(),
            observed_at_ms,
            billing: "openrouter_credits".into(),
        };
        match profile.validate() {
            Ok(()) => profiles.push(profile),
            Err(reason) => {
                rejected.insert(reject(&reason.to_string()));
            }
        }
    }
    // Cheapest first; the tag breaks ties so the order is stable.
    profiles.sort_by(|left, right| {
        left.price_order()
            .cmp(&right.price_order())
            .then_with(|| left.endpoint_tag.cmp(&right.endpoint_tag))
    });
    if profiles.is_empty() {
        return Err(invalid(format!(
            "no OpenRouter endpoint for {model} has exact text/tool limits and bounded pricing: {}",
            rejected.into_iter().collect::<Vec<_>>().join("; ")
        )));
    }
    Ok(profiles)
}
