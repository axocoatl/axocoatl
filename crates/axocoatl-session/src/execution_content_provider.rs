//! Definition-bound runtime configuration retained before canonical use. These
//! bytes are configuration evidence, never a provider lease or budget authority.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedProviderProfile {
    schema_version: u32,
    definition: EvidenceRef,
    provider: String,
    configuration: String,
    captured_after_records: usize,
}
impl RetainedProviderProfile {
    pub fn definition(&self) -> &EvidenceRef {
        &self.definition
    }
    pub fn provider(&self) -> &str {
        &self.provider
    }
    pub fn configuration(&self) -> &str {
        &self.configuration
    }
}

fn first_definition_use(
    canonical: &SessionExecutionStore,
    definition: &EvidenceRef,
) -> Result<Option<usize>, ExecutionContentError> {
    canonical
        .first_definition_use(definition)
        .map_err(|error| ExecutionContentError::Io(io::Error::other(error)))
}

impl ExecutionContentStore {
    /// Verify the actual held canonical namespace before host preparation can
    /// append a definition or a backend-profile binding.
    pub fn verify_provider_profile_owner(
        &self,
        canonical: &SessionExecutionStore,
    ) -> Result<(), ExecutionContentError> {
        self.verify_canonical_owner(canonical)?;
        if !matches!(&self.storage, Storage::Owned(_)) {
            return Err(ExecutionContentError::Invalid(
                "native provider capture requires held canonical content ownership",
            ));
        }
        Ok(())
    }

    /// The exact existing definition is the key. Retrying identical capture
    /// returns its original receipt; a different observation cannot replace it.
    /// New binding must precede the definition's first canonical graph use.
    pub fn retain_provider_profile(
        &mut self,
        canonical: &SessionExecutionStore,
        definition: &EvidenceRef,
        provider: &str,
        configuration: String,
    ) -> Result<DurableActivationEvidence, ExecutionContentError> {
        self.verify_provider_profile_owner(canonical)?;
        if let Some((reference, existing)) = self.resolve_provider_profile(canonical, definition)? {
            if existing.provider != provider || existing.configuration != configuration {
                return Err(ExecutionContentError::Conflict);
            }
            return Ok(DurableActivationEvidence {
                identity: self.identity.clone(),
                reference: reference.clone(),
            });
        }
        if first_definition_use(canonical, definition)?.is_some() {
            return Err(ExecutionContentError::Invalid(
                "provider configuration was not captured before definition admission",
            ));
        }
        let captured_after_records = canonical.record_count() as usize;
        let reference = self.append(Body::ProviderProfile(RetainedProviderProfile {
            schema_version: 1,
            definition: definition.clone(),
            provider: provider.to_owned(),
            configuration,
            captured_after_records,
        }))?;
        Ok(DurableActivationEvidence {
            identity: self.identity.clone(),
            reference,
        })
    }

    /// Only a read from the actual held store establishes this binding. A
    /// deserialized copy of RetainedProviderProfile is not accepted as a receipt.
    pub fn resolve_provider_profile(
        &self,
        canonical: &SessionExecutionStore,
        definition: &EvidenceRef,
    ) -> Result<Option<(EvidenceRef, RetainedProviderProfile)>, ExecutionContentError> {
        self.verify_provider_profile_owner(canonical)?;
        let selected = self
            .keyed(&definition_key(definition))?
            .iter()
            .find_map(|record| match &record.body {
                Body::ProviderProfile(profile) if &profile.definition == definition => {
                    Some((record.reference.clone(), profile.clone()))
                }
                _ => None,
            });
        if selected.is_none() && first_definition_use(canonical, definition)?.is_some() {
            return Err(ExecutionContentError::Invalid(
                "definition was admitted without a retained native provider profile",
            ));
        }
        if let Some((_, profile)) = &selected {
            if profile.captured_after_records > canonical.record_count() as usize
                || first_definition_use(canonical, definition)?
                    .is_some_and(|first| profile.captured_after_records > first)
            {
                return Err(ExecutionContentError::Invalid(
                    "provider capture follows canonical definition admission",
                ));
            }
        }
        Ok(selected)
    }
}

pub(super) fn validate_provider_profile(
    profile: &RetainedProviderProfile,
) -> Result<(), ExecutionContentError> {
    if profile.schema_version != 1
        || !matches!(profile.provider.as_str(), "ollama" | "openrouter")
        || profile.configuration.is_empty()
    {
        return Err(ExecutionContentError::Invalid(
            "unsupported retained native provider profile",
        ));
    }
    encode_bounded(profile, MAX_TEXT)?;
    let configuration: serde_json::Value = serde_json::from_str(&profile.configuration)?;
    if !configuration.is_object() {
        return Err(ExecutionContentError::Invalid(
            "native provider configuration must be an object",
        ));
    }
    Ok(())
}

pub(super) fn validate_provider_next(
    records: &[Record],
    profile: &RetainedProviderProfile,
) -> Result<(), ExecutionContentError> {
    if records.iter().any(|record| {
        matches!(&record.body,
        Body::ProviderProfile(existing) if existing.definition == profile.definition)
    }) {
        return Err(ExecutionContentError::Conflict);
    }
    if !records.iter().any(|record| {
        record.reference == profile.definition
            && matches!(&record.body,
        Body::ActivationEvidence(ActivationEvidenceContent::Definition { profile:definition, .. })
            if definition.provider == profile.provider)
    }) {
        return Err(ExecutionContentError::Invalid(
            "native provider profile lacks its exact retained definition",
        ));
    }
    Ok(())
}

fn definition_key(definition: &EvidenceRef) -> String {
    format!("provider:{}", definition.as_str())
}

/// The key a provider profile is found by: its definition.
pub(super) fn profile_key(profile: &RetainedProviderProfile) -> String {
    definition_key(&profile.definition)
}

/// What `validate_provider_next` may consult: profiles of the same
/// definition and the definition itself.
pub(super) fn profile_dependency_keys(profile: &RetainedProviderProfile) -> Vec<String> {
    vec![
        definition_key(&profile.definition),
        segments::reference_key(&profile.definition),
    ]
}
