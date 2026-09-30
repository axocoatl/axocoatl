//! Native resources from exact retained definitions and owned provider profiles.
//! Construction performs metadata verification only; the controller owns every
//! actual provider reservation, request, tool effect and terminal disposition.
use super::*;
use axocoatl_core::{AgentConfig, AgentRole, OverflowPolicy, ResponseFormat};
use axocoatl_llm::LlmProvider;
use axocoatl_llm_ollama::{NativeOllamaContextObservation, NativeOllamaProvider};
use axocoatl_session::control_authority::GrantLimits;
use axocoatl_session::execution_content::ExecutionContentStore;
use axocoatl_token::TokenCounter;
use axocoatl_tools::ToolExecutor;
use serde::{Deserialize, Serialize};

const NATIVE_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeOllamaRuntimeConfiguration {
    schema_version: u32,
    observation: NativeOllamaContextObservation,
    max_output_tokens: usize,
    max_response_bytes: usize,
    /// Evidence of the capacity used to select the initial ceiling. This value
    /// never replaces the current canonical grant or grants more authority.
    initial_limits: GrantLimits,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum NativeRuntimeConfiguration {
    Ollama(NativeOllamaRuntimeConfiguration),
    OpenRouter(NativeOpenRouterRuntimeConfiguration),
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeOpenRouterRuntimeConfiguration {
    schema_version: u32,
    openrouter_observation: axocoatl_llm_openai::NativeOpenRouterObservation,
    max_output_tokens: usize,
    max_response_bytes: usize,
    initial_limits: GrantLimits,
}
#[derive(Clone, Default)]
pub(crate) struct NativeProviderCredentials {
    pub(crate) ollama_base_url: Option<String>,
    pub(crate) openrouter_api_key: Option<String>,
    pub(crate) openrouter_credits_only: bool,
    pub(crate) openrouter_configuration_error: Option<String>,
}

pub(crate) struct NativeDefinitionPreparation {
    config: AgentConfig,
    profile: ExecutionProfile,
    definition_id: AgentDefinitionId,
    revision: u64,
    initial_limits: GrantLimits,
}

/// Opaque host result. References become executable only through a current
/// canonical manifest and the actual controller-owned content/authority stores.
/// The captured provider profile stays in content, keyed by the definition.
pub(crate) struct CapturedNativeDefinition {
    pub(crate) definition: DefinitionSnapshotRef,
    pub(crate) profile: ExecutionProfile,
}

fn validate_native_config(config: &AgentConfig) -> Result<()> {
    if !matches!(config.provider.as_str(), "ollama" | "openrouter")
        || config.model.trim().is_empty()
        || !matches!(
            config.role,
            AgentRole::Autonomous | AgentRole::Coordinator | AgentRole::Worker
        )
    {
        return Err(error(
            "native factory requires an exact configured bounded Ollama or OpenRouter actor",
        ));
    }
    match config.sampling.max_tokens {
        Some(0) => return Err(error("configured native output maximum must be positive")),
        Some(_) => {}
        None => {
            let budget = config.token_budget.as_ref().ok_or_else(|| error(
                "native output needs an explicit sampling maximum or an existing Abort token budget"))?;
            if budget.overflow_policy != OverflowPolicy::Abort
                || budget.per_call == 0
                || budget.per_execution == 0
            {
                return Err(error(
                    "native output cannot invent a hard maximum for Warn, absent or empty budgets",
                ));
            }
        }
    }
    Ok(())
}

fn native_output_limit(
    config: &AgentConfig,
    context: usize,
    limits: &GrantLimits,
) -> Result<usize> {
    validate_native_config(config)?;
    if !(2048..=16 * 1024 * 1024).contains(&context) {
        return Err(error(
            "observed context is outside the audited native profile",
        ));
    }
    // Ollama may repair JSON with a second inference pass. OpenRouter sends
    // one request with a native response format and reserves one whole call.
    let passes = if config.provider == "ollama"
        && config.sampling.response_format == Some(ResponseFormat::Json)
    {
        2u64
    } else {
        1
    };
    let native_maximum = context
        .checked_mul(10)
        .ok_or_else(|| error("native output capacity overflow"))?;
    let mut whole_call_capacity = limits.tokens;
    if let Some(budget) = config
        .token_budget
        .as_ref()
        .filter(|budget| budget.overflow_policy == OverflowPolicy::Abort)
    {
        whole_call_capacity = whole_call_capacity
            .min(budget.per_call as u64)
            .min(budget.per_execution as u64);
    }
    let pass_capacity = whole_call_capacity / passes;
    let allowed = pass_capacity.checked_sub(context as u64)
        .and_then(|output| usize::try_from(output).ok())
        .filter(|output| *output > 0)
        .ok_or_else(|| error("native context and response passes leave no output capacity in the existing hard budget"))?;
    let selected = match config.sampling.max_tokens {
        Some(explicit) if explicit <= allowed && explicit <= native_maximum => explicit,
        Some(_) => return Err(error("explicit sampling maximum does not fit the native whole-call budget; it was not clamped")),
        None => allowed.min(native_maximum),
    };
    Ok(selected)
}

impl NativeDefinitionPreparation {
    pub(crate) fn new(
        config: AgentConfig,
        definition_id: AgentDefinitionId,
        revision: u64,
        initial_limits: GrantLimits,
    ) -> Result<Self> {
        validate_native_config(&config)?;
        if revision == 0 {
            return Err(error("definition revision must be positive"));
        }
        if let Some(writes) = &config.writes {
            axocoatl_session::path_scope::validate_write_scope(writes).map_err(|reason| {
                error(format!(
                    "Agent '{}' has an invalid writes list: {reason}. Use repository paths such \
                     as lib/ or docs/*.md, or writes: [] for an Agent that changes nothing",
                    config.id.0
                ))
            })?;
        }
        let profile = ExecutionProfile {
            definition: definition_id.as_str().to_owned(),
            provider: config.provider.clone(),
            model: config.model.clone(),
            isolation: "in-process".into(),
            tools: config.tools.clone(),
            write_scope: config.writes.clone(),
        };
        Ok(Self {
            config,
            profile,
            definition_id,
            revision,
            initial_limits,
        })
    }

    pub(crate) fn model(&self) -> &str {
        &self.config.model
    }

    fn definition_content(&self) -> Result<ActivationEvidenceContent> {
        Ok(ActivationEvidenceContent::Definition {
            definition_id: self.definition_id.clone(),
            revision: self.revision,
            profile: self.profile.clone(),
            configuration: serde_json::to_string(&self.config).map_err(error)?,
        })
    }

    /// Called under the existing retained Session/store lock. Definition append
    /// is idempotent; an acknowledged provider profile is reused byte-for-byte.
    pub(crate) fn prepare_content(
        &self,
        canonical: &SessionExecutionStore,
        content: &mut ExecutionContentStore,
    ) -> Result<(DefinitionSnapshotRef, Option<NativeRuntimeConfiguration>)> {
        content
            .verify_provider_profile_owner(canonical)
            .map_err(error)?;
        let definition = content
            .retain_activation_evidence(self.definition_content()?)
            .map_err(error)?;
        let reference = DefinitionSnapshotRef {
            definition_id: self.definition_id.clone(),
            snapshot: definition.reference().clone(),
        };
        let retained = content
            .resolve_provider_profile(canonical, &reference.snapshot)
            .map_err(error)?;
        let runtime = retained
            .map(|(_, profile)| {
                if profile.provider() != self.config.provider {
                    return Err(error(
                        "retained provider configuration belongs to another backend",
                    ));
                }
                let runtime: NativeRuntimeConfiguration =
                    serde_json::from_str(profile.configuration()).map_err(error)?;
                runtime.validate(&self.config)?;
                Ok(runtime)
            })
            .transpose()?;
        Ok((reference, runtime))
    }

    pub(crate) fn observed(
        &self,
        observation: NativeOllamaContextObservation,
    ) -> Result<NativeRuntimeConfiguration> {
        let max_output_tokens = native_output_limit(
            &self.config,
            observation.context_tokens,
            &self.initial_limits,
        )?;
        let runtime = NativeRuntimeConfiguration::Ollama(NativeOllamaRuntimeConfiguration {
            schema_version: 1,
            observation,
            max_output_tokens,
            max_response_bytes: NATIVE_RESPONSE_BYTES,
            initial_limits: self.initial_limits.clone(),
        });
        runtime.validate(&self.config)?;
        Ok(runtime)
    }

    pub(crate) fn capture(
        &self,
        canonical: &SessionExecutionStore,
        content: &mut ExecutionContentStore,
        definition: &DefinitionSnapshotRef,
        runtime: &NativeRuntimeConfiguration,
    ) -> Result<CapturedNativeDefinition> {
        runtime.validate(&self.config)?;
        if definition.definition_id != self.definition_id
            || content
                .resolve_activation_evidence(&definition.snapshot)
                .map_err(error)?
                != &self.definition_content()?
        {
            return Err(error(
                "native capture differs from the exact retained definition",
            ));
        }
        content
            .retain_provider_profile(
                canonical,
                &definition.snapshot,
                &self.config.provider,
                serde_json::to_string(runtime).map_err(error)?,
            )
            .map_err(error)?;
        Ok(CapturedNativeDefinition {
            definition: definition.clone(),
            profile: self.profile.clone(),
        })
    }
}

impl NativeOllamaRuntimeConfiguration {
    fn validate(&self, config: &AgentConfig) -> Result<()> {
        if self.schema_version != 1
            || self.observation.requested_model != config.model
            || self.max_response_bytes != NATIVE_RESPONSE_BYTES
            || self.max_output_tokens
                != native_output_limit(
                    config,
                    self.observation.context_tokens,
                    &self.initial_limits,
                )?
        {
            return Err(error(
                "retained native runtime profile differs from its exact configured bounds",
            ));
        }
        Ok(())
    }
    pub(crate) async fn verify(
        &self,
        configured_base_url: Option<&str>,
    ) -> Result<Arc<dyn LlmProvider>> {
        if configured_base_url.is_some_and(|configured| configured != self.observation.base_url) {
            return Err(error(
                "configured Ollama endpoint differs from the admitted native profile",
            ));
        }
        let provider = NativeOllamaProvider::connect_observed(
            self.observation.clone(),
            self.max_output_tokens,
            self.max_response_bytes,
        )
        .await
        .map_err(error)?;
        Ok(Arc::new(provider))
    }
}

struct NativeActivationFactory {
    controller: SessionDispatchController,
    counter: Arc<dyn TokenCounter>,
    data_root: axocoatl_core::SecureDir,
    credentials: NativeProviderCredentials,
}

impl SessionDispatchController {
    #[cfg(test)]
    pub(crate) fn native_ollama_factory(
        &self,
        data_root: &axocoatl_core::SecureDir,
        configured_base_url: &str,
        counter: Arc<dyn TokenCounter>,
    ) -> Result<Arc<dyn AutonomousActivationFactory>> {
        self.native_provider_factory(
            data_root,
            NativeProviderCredentials {
                ollama_base_url: Some(configured_base_url.to_owned()),
                ..Default::default()
            },
            counter,
        )
    }
    pub(crate) fn native_provider_factory(
        &self,
        data_root: &axocoatl_core::SecureDir,
        credentials: NativeProviderCredentials,
        counter: Arc<dyn TokenCounter>,
    ) -> Result<Arc<dyn AutonomousActivationFactory>> {
        self.lock()?
            .canonical
            .verify_data_root(data_root)
            .map_err(error)?;
        Ok(Arc::new(NativeActivationFactory {
            controller: self.clone(),
            counter,
            data_root: data_root.clone(),
            credentials,
        }))
    }
}

struct ResolvedNativeResources {
    configuration: String,
    config: AgentConfig,
    profile: ExecutionProfile,
    runtime: NativeRuntimeConfiguration,
    provider_reference: EvidenceRef,
    provider_bytes: String,
}

impl NativeActivationFactory {
    fn resolve(&self, input: &ActivationInputManifest) -> Result<ResolvedNativeResources> {
        let state = self.controller.lock()?;
        state.execution_admission()?;
        state
            .canonical
            .verify_data_root(&self.data_root)
            .map_err(error)?;
        let snapshot = state.current(&input.activation)?;
        let resolved = state
            .content
            .validate_input(&snapshot, input)
            .map_err(error)?;
        let ActivationEvidenceContent::Definition {
            definition_id,
            profile,
            configuration,
            ..
        } = resolved.definition
        else {
            return Err(error("native input lacks an exact definition"));
        };
        let mut config: AgentConfig = serde_json::from_str(&configuration).map_err(error)?;
        validate_native_config(&config)?;
        if (config.id.0 != input.conversation_id.as_str()
            && state
                .native_child_origin(&input.activation.node_id)?
                .is_none())
            || profile.definition != definition_id.as_str()
            || profile.provider != config.provider
            || profile.model != config.model
            || profile.tools != config.tools
            || profile.write_scope != config.writes
            || profile.isolation != "in-process"
            || serde_json::to_string(&config).map_err(error)? != configuration
        {
            return Err(error(
                "native definition differs from the admitted actor configuration",
            ));
        }
        if matches!(input.repository, RepositoryInput::Unavailable) && !config.tools.is_empty() {
            return Err(error(
                "configured native tools require the exact supported repository resource",
            ));
        }
        let grant = input
            .grant
            .as_ref()
            .ok_or_else(|| error("native activation lacks an exact grant"))?;
        let current = state
            .authority
            .grant_policy(grant.grant_id.as_str())
            .map_err(error)?;
        if !state.captured_grant_is_current(resolved.grant.as_ref(), &resolved.budget, &current)? {
            return Err(error(
                "native activation budget or grant differs from current authority",
            ));
        }
        state
            .authority
            .validate_activation_grant(
                &input.activation,
                grant.grant_id.as_str(),
                &profile,
                now_ms()?,
            )
            .map_err(error)?;
        let (provider_reference, retained) = state
            .content
            .resolve_provider_profile(&state.canonical, &input.definition.snapshot)
            .map_err(error)?
            .ok_or_else(|| error("definition has no admitted native provider profile"))?;
        if retained.provider() != config.provider {
            return Err(error("native profile has another provider"));
        }
        let runtime: NativeRuntimeConfiguration =
            serde_json::from_str(retained.configuration()).map_err(error)?;
        runtime.validate(&config)?;
        if state
            .native_child_origin(&input.activation.node_id)?
            .is_some()
        {
            config.id = axocoatl_core::AgentId::new(input.conversation_id.as_str());
        }
        Ok(ResolvedNativeResources {
            configuration,
            config,
            profile,
            runtime,
            provider_reference: provider_reference.clone(),
            provider_bytes: retained.configuration().to_owned(),
        })
    }
}

#[async_trait]
impl AutonomousActivationFactory for NativeActivationFactory {
    async fn resources(
        &self,
        input: &ActivationInputManifest,
    ) -> std::result::Result<AutonomousActivationResources, String> {
        let before = self.resolve(input).map_err(|error| error.to_string())?;
        let provider = before
            .runtime
            .verify_credentials(&self.credentials)
            .await
            .map_err(|error| error.to_string())?;
        // Metadata verification yields. Stop, new epochs, changed grants and
        // replacement ownership must be checked again before returning resources.
        let after = self.resolve(input).map_err(|error| error.to_string())?;
        if before.configuration != after.configuration
            || before.profile != after.profile
            || before.provider_reference != after.provider_reference
            || before.provider_bytes != after.provider_bytes
        {
            return Err("native resources changed while resolving the admitted profile".into());
        }
        Ok(AutonomousActivationResources {
            config: after.config,
            profile: after.profile,
            provider,
            counter: self.counter.clone(),
            tools: Arc::new(ToolExecutor::new()),
        })
    }
}

#[cfg(test)]
#[path = "session_dispatch_native_tests.rs"]
mod tests;

include!("session_dispatch_native_openrouter.rs");
