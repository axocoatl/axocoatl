//! Native Agent definitions and activation factories built from the daemon's
//! actual configuration and the Session's retained stores.
use super::*;
use crate::session_dispatch::{
    AutonomousActivationFactory, CapturedNativeDefinition, NativeDefinitionPreparation,
    NativeProviderCredentials, SessionDispatchController,
};
use axocoatl_core::AgentConfig;
use axocoatl_session::control_authority::GrantLimits;
use axocoatl_session::turn_contract::AgentDefinitionId;

fn native_error(error: impl std::fmt::Display) -> DaemonError {
    DaemonError::Provider(error.to_string())
}

fn native_agent_config(
    host: &AxocoatlConfig,
    registry: &ProviderRegistry,
    mut agent: AgentConfig,
) -> Result<AgentConfig, DaemonError> {
    let configured =
        AxocoatlDaemon::resolve_base_provider(host, registry, &agent.provider, Some(&agent.model))?;
    // Registry adapters are shared across models. Their default is only a
    // fallback for an unspecified model, never the identity of a selected one.
    if agent.model.trim().is_empty() {
        agent.model = configured.model_id().to_owned();
    }
    Ok(agent)
}

impl AxocoatlDaemon {
    /// Resolve and capture against the same exact retained team owner on both
    /// sides of provider observation, for first turns and closed successors.
    pub(crate) async fn prepare_native_session_team_definition(
        &self,
        token: &session_dispatch::SessionTeamToken,
        config: AgentConfig,
        definition_id: AgentDefinitionId,
        revision: u64,
        initial_limits: GrantLimits,
    ) -> Result<CapturedNativeDefinition, DaemonError> {
        crate::session_dispatch::validate_repository_tools(&config.tools).map_err(native_error)?;
        // A listed host tool (web_search, web_fetch, browser, browser_check)
        // must be available before the team is admitted.
        if let Some(reason) = self.host_tool_refusal(&config, &definition_id) {
            return Err(DaemonError::Session(reason));
        }
        let credentials = self.configured_native_provider_credentials();
        let config = native_agent_config(&self.config, &self.provider_registry, config)?;
        let preparation =
            NativeDefinitionPreparation::new(config, definition_id, revision, initial_limits)
                .map_err(native_error)?;
        prepare_retained_native_definition_with_credentials(
            &self.session_dispatch_lifecycles,
            token,
            &self.data_root,
            &credentials,
            preparation,
        )
        .await
    }

    /// The host tools native Session controllers register: the web tools
    /// and, when `browser:` is configured, `browser` and `browser_check`.
    pub(crate) fn host_invocation_tools(
        &self,
    ) -> Vec<Arc<dyn crate::session_dispatch::HostInvocationTool>> {
        let mut tools = self.web_tools.host_tools();
        if let Some(browser) = &self.browser_service {
            tools.push(Arc::new(
                crate::session_dispatch_browser::BrowserHostTool::browser(browser.clone()),
            ));
            tools.push(Arc::new(
                crate::session_dispatch_browser::BrowserHostTool::browser_check(browser.clone()),
            ));
        }
        tools
    }

    /// Why an Agent with `config` cannot have a host tool it lists.
    pub(crate) fn host_tool_refusal(
        &self,
        config: &AgentConfig,
        definition_id: &AgentDefinitionId,
    ) -> Option<String> {
        if !config
            .tools
            .iter()
            .any(|tool| crate::session_dispatch::is_host_invocation_tool(tool))
        {
            return None;
        }
        // The refusal names the Agent the person configured; the definition
        // id is an internal identity.
        let label = if config.name.trim().is_empty() {
            definition_id.as_str().to_string()
        } else {
            config.name.clone()
        };
        if self.browser_service.is_none() {
            if let Some(tool) = config
                .tools
                .iter()
                .find(|tool| matches!(tool.as_str(), "browser" | "browser_check"))
            {
                return Some(format!(
                    "{tool} is listed for {label} but the browser block is not configured; add `browser:` to the config and run `axocoatl browser install`"
                ));
            }
        }
        let registered = self
            .host_invocation_tools()
            .into_iter()
            .map(|tool| (tool.name(), tool))
            .collect();
        let profile = axocoatl_session::control_authority::ExecutionProfile {
            definition: label,
            provider: config.provider.clone(),
            model: config.model.clone(),
            isolation: "in-process".into(),
            tools: config.tools.clone(),
            write_scope: config.writes.clone(),
        };
        crate::session_dispatch::host_tool_refusal(&registered, &profile)
    }

    pub(crate) fn native_session_activation_factory(
        &self,
        controller: &SessionDispatchController,
    ) -> Result<Arc<dyn AutonomousActivationFactory>, DaemonError> {
        let workspace_id = controller.knowledge_workspace_id().map_err(native_error)?;
        controller
            .attach_workspace_knowledge(self.workspace_knowledge_store(&workspace_id)?)
            .map_err(native_error)?;
        for tool in self.host_invocation_tools() {
            controller
                .register_host_invocation_tool(tool)
                .map_err(native_error)?;
        }
        controller
            .native_provider_factory(
                &self.data_root,
                self.configured_native_provider_credentials(),
                self.counter.clone(),
            )
            .map_err(native_error)
    }

    fn configured_native_provider_credentials(&self) -> NativeProviderCredentials {
        let invalid_openrouter = self.config.providers.openrouter.as_ref().is_some_and(|p| {
            p.fallback.is_some()
                || p.base_url.as_deref().is_some_and(|base| {
                    base.trim_end_matches('/') != "https://openrouter.ai/api/v1"
                })
        });
        NativeProviderCredentials {
            ollama_base_url: self
                .config
                .providers
                .ollama
                .as_ref()
                .map(|p| p.base_url.clone()),
            openrouter_api_key: self
                .config
                .providers
                .openrouter
                .as_ref()
                .map(|p| p.api_key.expose_secret().to_owned()),
            openrouter_credits_only: self.config.providers.openrouter_billing
                == Some(axocoatl_config::OpenRouterBilling::Credits),
            openrouter_configuration_error: invalid_openrouter.then(|| {
                "native OpenRouter requires the official endpoint and no configured fallback".into()
            }),
        }
    }
}

/// One implementation of the retained asynchronous preparation join. The
/// daemon resolves effective configuration before entering this function.
async fn prepare_retained_native_definition_with_credentials(
    registry: &session_dispatch::SessionDispatchRegistry,
    token: &session_dispatch::SessionTeamToken,
    data_root: &SecureDir,
    credentials: &NativeProviderCredentials,
    preparation: NativeDefinitionPreparation,
) -> Result<CapturedNativeDefinition, DaemonError> {
    let (definition, retained) =
        registry.with_session_team_stores(token, |canonical, content, _| {
            canonical
                .verify_data_root(data_root)
                .map_err(native_error)?;
            preparation
                .prepare_content(canonical, content)
                .map_err(native_error)
        })?;
    let runtime = match retained {
        Some(runtime) => runtime,
        None => preparation
            .observe_configured(credentials)
            .await
            .map_err(native_error)?,
    };
    // Verification does not generate output or load another model. Keep the
    // configured endpoint exact; changed credentials cannot retarget replay.
    runtime
        .verify_credentials(credentials)
        .await
        .map_err(native_error)?;
    registry.with_session_team_stores(token, |canonical, content, _| {
        canonical
            .verify_data_root(data_root)
            .map_err(native_error)?;
        preparation
            .capture(canonical, content, &definition, &runtime)
            .map_err(native_error)
    })
}

#[cfg(test)]
async fn prepare_retained_native_definition(
    registry: &session_dispatch::SessionDispatchRegistry,
    token: &session_dispatch::SessionTeamToken,
    data_root: &SecureDir,
    base_url: &str,
    preparation: NativeDefinitionPreparation,
) -> Result<CapturedNativeDefinition, DaemonError> {
    prepare_retained_native_definition_with_credentials(
        registry,
        token,
        data_root,
        &NativeProviderCredentials {
            ollama_base_url: Some(base_url.into()),
            ..Default::default()
        },
        preparation,
    )
    .await
}

#[cfg(test)]
#[path = "bootstrap_native_activation_tests.rs"]
mod tests;
