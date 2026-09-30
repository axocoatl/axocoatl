impl NativeDefinitionPreparation {
    pub(crate) async fn observe_configured(
        &self,
        credentials: &NativeProviderCredentials,
    ) -> Result<NativeRuntimeConfiguration> {
        match self.config.provider.as_str() {
            "ollama" => {
                let base = credentials
                    .ollama_base_url
                    .as_deref()
                    .ok_or_else(|| error("Ollama provider is not configured"))?;
                let observation =
                    axocoatl_llm_ollama::observe_native_ollama_context(base, self.model())
                        .await
                        .map_err(error)?;
                self.observed(observation)
            }
            "openrouter" => {
                let key = credentials.openrouter_key()?;
                let profiles = axocoatl_llm_openai::observe_native_openrouter_profiles(
                    "https://openrouter.ai/api/v1",
                    key,
                    self.model(),
                )
                .await
                .map_err(error)?;
                // Selection never raises or clamps an explicitly approved maximum.
                // Each usable endpoint is retained exactly and revalidated later.
                for observation in profiles {
                    let Ok(max_output_tokens) =
                        openrouter_output_limit(&self.config, &observation, &self.initial_limits)
                    else {
                        continue;
                    };
                    let runtime = NativeOpenRouterRuntimeConfiguration {
                        schema_version: 2,
                        max_output_tokens,
                        openrouter_observation: observation,
                        max_response_bytes: NATIVE_RESPONSE_BYTES,
                        initial_limits: self.initial_limits.clone(),
                    };
                    if runtime.validate(&self.config).is_ok() {
                        return Ok(NativeRuntimeConfiguration::OpenRouter(runtime));
                    }
                }
                Err(error("no exact OpenRouter endpoint fits the approved whole-call token and monetary budget"))
            }
            _ => Err(error("unsupported native provider")),
        }
    }
}
fn openrouter_output_limit(
    config: &AgentConfig,
    observation: &axocoatl_llm_openai::NativeOpenRouterObservation,
    limits: &GrantLimits,
) -> Result<usize> {
    let selected = native_output_limit(config, observation.context_tokens, limits)?;
    if config.sampling.max_tokens.is_some()
        && (selected > observation.max_output_tokens || selected >= observation.context_tokens)
    {
        return Err(error(
            "explicit output maximum exceeds the retained OpenRouter endpoint or leaves no input context",
        ));
    }
    Ok(selected
        .min(observation.max_output_tokens)
        .min(observation.context_tokens - 1))
}
impl NativeProviderCredentials {
    fn openrouter_key(&self) -> Result<&str> {
        if let Some(reason) = &self.openrouter_configuration_error {
            return Err(error(reason));
        }
        if !self.openrouter_credits_only {
            return Err(error("native OpenRouter supports OpenRouter credits only; confirm providers.openrouter_billing: credits in the active configuration. BYOK support is TODO; no inference was sent"));
        }
        self.openrouter_api_key
            .as_deref()
            .filter(|key| !key.is_empty())
            .ok_or_else(|| error("OpenRouter API key is not configured"))
    }
}
impl NativeOpenRouterRuntimeConfiguration {
    fn validate(&self, config: &AgentConfig) -> Result<()> {
        let selected =
            openrouter_output_limit(config, &self.openrouter_observation, &self.initial_limits)?;
        // A derived maximum is a ceiling, not permission to enlarge a retained
        // request. Older JSON profiles used a stricter two-pass allowance;
        // retain their finite output cap when reopening the same definition.
        let output_matches = if config.sampling.max_tokens.is_some() {
            self.max_output_tokens == selected
        } else {
            self.max_output_tokens > 0 && self.max_output_tokens <= selected
        };
        if self.schema_version != 2
            || config.provider != "openrouter"
            || self.openrouter_observation.model != config.model
            || self.max_response_bytes != NATIVE_RESPONSE_BYTES
            || !output_matches
        {
            return Err(error(
                "retained OpenRouter profile differs from its admitted definition",
            ));
        }
        let bounds = self
            .openrouter_observation
            .execution_bounds(self.max_output_tokens, self.max_response_bytes)
            .map_err(error)?;
        if bounds.cost_microunits > self.initial_limits.cost_microunits {
            return Err(error(
                "the exact OpenRouter request price ceiling exceeds the approved cost budget",
            ));
        }
        Ok(())
    }
}
impl NativeRuntimeConfiguration {
    fn validate(&self, config: &AgentConfig) -> Result<()> {
        match self {
            Self::Ollama(runtime) => {
                if config.provider != "ollama" {
                    return Err(error("Ollama profile cannot authorize another provider"));
                }
                runtime.validate(config)
            }
            Self::OpenRouter(runtime) => runtime.validate(config),
        }
    }
    pub(crate) async fn verify_credentials(
        &self,
        credentials: &NativeProviderCredentials,
    ) -> Result<Arc<dyn LlmProvider>> {
        match self {
            Self::Ollama(runtime) => runtime.verify(credentials.ollama_base_url.as_deref()).await,
            Self::OpenRouter(runtime) => {
                let key = credentials.openrouter_key()?;
                let provider = axocoatl_llm_openai::NativeOpenRouterProvider::connect_observed(
                    runtime.openrouter_observation.clone(),
                    key,
                    runtime.max_output_tokens,
                    runtime.max_response_bytes,
                )
                .await
                .map_err(error)?;
                Ok(Arc::new(provider))
            }
        }
    }
}
