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
                self.openrouter_runtime(profiles)
            }
            _ => Err(error("unsupported native provider")),
        }
    }

    /// The first (cheapest) endpoint that fits this Agent. Selection never
    /// raises or clamps an explicitly approved maximum. Each usable endpoint
    /// is retained exactly and revalidated later.
    pub(crate) fn openrouter_runtime(
        &self,
        profiles: Vec<axocoatl_llm_openai::NativeOpenRouterObservation>,
    ) -> Result<NativeRuntimeConfiguration> {
        let mut refusals = std::collections::BTreeSet::new();
        for observation in profiles {
            // The reasoning contract is the model's, the same on every
            // endpoint; an effort it refuses is refused everywhere.
            let reasoning = observation
                .reasoning_request(self.config.sampling.reasoning_effort)
                .map_err(error)?;
            let runtime = openrouter_output_limit(
                &self.config,
                &observation,
                &self.initial_limits,
                reasoning,
            )
            .map(|max_output_tokens| NativeOpenRouterRuntimeConfiguration {
                schema_version: 2,
                max_output_tokens,
                openrouter_observation: observation,
                max_response_bytes: NATIVE_RESPONSE_BYTES,
                initial_limits: self.initial_limits.clone(),
                reasoning,
            });
            match runtime.and_then(|runtime| runtime.validate(&self.config).map(|()| runtime)) {
                Ok(runtime) => return Ok(NativeRuntimeConfiguration::OpenRouter(runtime)),
                Err(reason) => {
                    refusals.insert(reason.to_string());
                }
            }
        }
        Err(error(format!(
            "no OpenRouter endpoint for {} fits this Agent's approved budget: {}",
            self.model(),
            refusals.into_iter().collect::<Vec<_>>().join("; ")
        )))
    }
}

fn microdollars(amount: u64) -> String {
    format!("${}.{:06}", amount / 1_000_000, amount % 1_000_000)
}

/// Refuse, by name, a sampling parameter the Agent sends that this endpoint
/// does not list. OpenRouter would refuse every call (`require_parameters`).
fn openrouter_parameters(
    config: &AgentConfig,
    observation: &axocoatl_llm_openai::NativeOpenRouterObservation,
) -> Result<()> {
    for (parameter, set) in [
        ("temperature", config.sampling.temperature.is_some()),
        ("top_p", config.sampling.top_p.is_some()),
        ("response_format", config.sampling.response_format.is_some()),
    ] {
        if set
            && !observation
                .supported_parameters
                .iter()
                .any(|supported| supported == parameter)
        {
            return Err(error(format!(
                "{} on OpenRouter endpoint {} does not accept sampling.{parameter}; remove it \
                 from this Agent or choose a model whose endpoints accept it",
                observation.model, observation.endpoint_tag
            )));
        }
    }
    Ok(())
}

/// The Agent's visible output limit per call on this endpoint. The response
/// allowance is that output plus the reasoning allowance; with the template
/// allowance for the smallest prompt it must fit one whole call, within the
/// endpoint's output limit and the response bytes a Session keeps. Without
/// `sampling.max_tokens`, the response allowance takes at most half of the
/// whole-call budget so the other half is left for prompts.
fn openrouter_output_limit(
    config: &AgentConfig,
    observation: &axocoatl_llm_openai::NativeOpenRouterObservation,
    limits: &GrantLimits,
    reasoning: Option<axocoatl_llm_openai::NativeOpenRouterReasoningRequest>,
) -> Result<usize> {
    validate_native_config(config)?;
    openrouter_parameters(config, observation)?;
    let model = &observation.model;
    let tag = &observation.endpoint_tag;
    let capacity = whole_call_capacity(config, limits);
    let minimum_prompt = axocoatl_llm_openai::PROMPT_TEMPLATE_ALLOWANCE
        .min(observation.prompt_limit()) as u64;
    let response = |output: usize| observation.response_allowance(output, reasoning);
    let effort = match reasoning {
        Some(axocoatl_llm_openai::NativeOpenRouterReasoningRequest::Effort(effort)) => {
            format!(" at reasoning effort {effort}")
        }
        Some(_) => " with reasoning".to_owned(),
        None => String::new(),
    };
    if let Some(explicit) = config.sampling.max_tokens {
        if explicit > observation.max_output_tokens {
            return Err(error(format!(
                "sampling.max_tokens {explicit} exceeds the {} output tokens {model} allows on \
                 OpenRouter endpoint {tag}",
                observation.max_output_tokens
            )));
        }
        if let Some(refusal) =
            observation.response_refusal(explicit, reasoning, NATIVE_RESPONSE_BYTES)
        {
            return Err(error(format!(
                "sampling.max_tokens {explicit}{effort}: {refusal}; lower sampling.max_tokens \
                 or sampling.reasoning_effort"
            )));
        }
        let allowance = response(explicit);
        if allowance >= observation.context_tokens {
            return Err(error(format!(
                "a {allowance}-token response leaves no input context in {model}'s \
                 {}-token window on {tag}",
                observation.context_tokens
            )));
        }
        if minimum_prompt + allowance as u64 > capacity {
            return Err(error(format!(
                "each call to {model} may produce {allowance} tokens ({explicit} of output plus \
                 {} of reasoning{effort}) and its prompt takes at least {minimum_prompt}, more \
                 than the {capacity}-token whole-call budget; raise the budget or lower \
                 sampling.max_tokens or sampling.reasoning_effort",
                allowance - explicit
            )));
        }
        return Ok(explicit);
    }
    let fits = |output: usize| {
        let allowance = response(output) as u64;
        allowance <= capacity / 2
            && minimum_prompt + allowance <= capacity
            && (allowance as usize) < observation.context_tokens
            && observation
                .response_refusal(output, reasoning, NATIVE_RESPONSE_BYTES)
                .is_none()
    };
    if let Some(refusal) = observation.response_refusal(1, reasoning, NATIVE_RESPONSE_BYTES) {
        return Err(error(format!(
            "{refusal}; choose a lower sampling.reasoning_effort"
        )));
    }
    if !fits(1) {
        return Err(error(format!(
            "the {capacity}-token whole-call budget leaves no room for a {model} response of \
             {} tokens{effort}; raise the budget or set sampling.max_tokens",
            response(1)
        )));
    }
    // The response allowance only grows with the output limit.
    let (mut low, mut high) = (1usize, observation.max_output_tokens);
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        if fits(middle) {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    Ok(low)
}
impl NativeProviderCredentials {
    fn openrouter_key(&self) -> Result<&str> {
        if let Some(reason) = &self.openrouter_configuration_error {
            return Err(error(reason));
        }
        if !self.openrouter_credits_only {
            return Err(error("native OpenRouter supports OpenRouter credits only, not BYOK provider keys; set providers.openrouter_billing: credits in the active configuration. No inference was sent"));
        }
        self.openrouter_api_key
            .as_deref()
            .filter(|key| !key.is_empty())
            .ok_or_else(|| error("OpenRouter API key is not configured"))
    }
}
impl NativeOpenRouterRuntimeConfiguration {
    fn validate(&self, config: &AgentConfig) -> Result<()> {
        let observation = &self.openrouter_observation;
        let reasoning = observation
            .reasoning_request(config.sampling.reasoning_effort)
            .map_err(error)?;
        let selected =
            openrouter_output_limit(config, observation, &self.initial_limits, reasoning)?;
        // A derived maximum is a ceiling, not permission to enlarge a retained
        // request. Older JSON profiles used a stricter allowance; retain their
        // finite output cap when reopening the same definition.
        let output_matches = if config.sampling.max_tokens.is_some() {
            self.max_output_tokens == selected
        } else {
            self.max_output_tokens > 0 && self.max_output_tokens <= selected
        };
        if self.schema_version != 2
            || config.provider != "openrouter"
            || observation.model != config.model
            || self.max_response_bytes != NATIVE_RESPONSE_BYTES
            || self.reasoning != reasoning
            || !output_matches
        {
            return Err(error(
                "retained OpenRouter profile differs from its admitted definition",
            ));
        }
        // Each call reserves its own prompt bound and response allowance at
        // dispatch. Admission checks that the smallest call fits the money.
        let bounds = observation
            .minimum_call_bounds(self.max_output_tokens, reasoning, self.max_response_bytes)
            .map_err(error)?;
        if bounds.cost_microunits > self.initial_limits.cost_microunits {
            return Err(error(format!(
                "the smallest call to {} on {} can cost up to {} ({} response tokens at \
                 ${}/M plus the prompt at ${}/M), more than the approved {} cost budget",
                observation.model,
                observation.endpoint_tag,
                microdollars(bounds.cost_microunits),
                observation.response_allowance(self.max_output_tokens, reasoning),
                observation.completion_price_per_million,
                observation.prompt_price_per_million,
                microdollars(self.initial_limits.cost_microunits),
            )));
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
                    runtime.reasoning,
                )
                .await
                .map_err(error)?;
                Ok(Arc::new(provider))
            }
        }
    }
}
#[cfg(test)]
impl NativeRuntimeConfiguration {
    /// The OpenRouter endpoint this profile pins, for test evidence.
    pub(crate) fn openrouter_endpoint(&self) -> Option<&str> {
        match self {
            Self::OpenRouter(runtime) => Some(&runtime.openrouter_observation.endpoint_tag),
            Self::Ollama(_) => None,
        }
    }
}
