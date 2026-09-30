use super::*;

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
    pub prompt_price_per_million: String,
    pub completion_price_per_million: String,
    pub supported_parameters: Vec<String>,
    pub non_reasoning_evidence: String,
    pub observed_at_ms: u64,
    pub billing: String,
}
impl NativeOpenRouterObservation {
    pub fn validate(&self) -> Result<(), ProviderError> {
        endpoint(&self.base_url, "")?;
        if self.schema_version != 1
            || self.model.starts_with("openrouter/")
            || self.model.split('/').count() != 2
            || !self.endpoint_tag.contains('/')
            || self.endpoint_tag.len() > 256
            || self.provider_name.is_empty()
            || !(2048..=16 * 1024 * 1024).contains(&self.context_tokens)
            || self.max_output_tokens == 0
            || self.max_output_tokens > self.context_tokens
            || self.non_reasoning_evidence != "openrouter-model-catalog:no-reasoning-contract-v1"
            || self.observed_at_ms == 0
            || self.billing != "openrouter_credits"
            || !self.supported_parameters.iter().any(|p| p == "max_tokens")
            || !self.supported_parameters.iter().any(|p| p == "tools")
            || self
                .supported_parameters
                .iter()
                .any(|p| p.contains("reasoning") || p == "include_reasoning")
        {
            return Err(invalid(
                "OpenRouter profile lacks exact non-reasoning text/tool endpoint bounds",
            ));
        }
        for price in [
            &self.prompt_price_per_million,
            &self.completion_price_per_million,
        ] {
            money::decimal_units(price)?;
        }
        Ok(())
    }
    pub fn execution_bounds(
        &self,
        output: usize,
        response_bytes: usize,
    ) -> Result<ProviderExecutionBounds, ProviderError> {
        self.validate()?;
        if output == 0 || output > self.max_output_tokens {
            return Err(invalid("output exceeds retained endpoint ceiling"));
        }
        Ok(ProviderExecutionBounds {
            token_limit: (self.context_tokens as u64)
                .checked_add(output as u64)
                .ok_or_else(|| invalid("token bound overflow"))?,
            cost_microunits: money::charge_bound(
                self.context_tokens,
                output,
                &self.prompt_price_per_million,
                &self.completion_price_per_million,
            )?,
            response_bytes,
        })
    }
    pub(super) fn same_contract(&self, other: &Self) -> bool {
        let mut left = self.clone();
        left.observed_at_ms = other.observed_at_ms;
        left == *other
    }
}
fn endpoint(base: &str, path: &str) -> Result<String, ProviderError> {
    let parsed =
        reqwest::Url::parse(base).map_err(|_| invalid("invalid OpenRouter metadata endpoint"))?;
    let official = parsed.scheme() == "https"
        && parsed.host_str() == Some("openrouter.ai")
        && parsed.path().trim_end_matches('/') == "/api/v1";
    #[cfg(test)]
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
    .map_err(|_| invalid("OpenRouter metadata timed out"))?
    .map_err(|e| network_error(&e, &[key]))?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let detail = read_error_text(response, &[key]).await;
        return Err(ProviderError::ApiError {
            provider: PROVIDER.into(),
            status,
            message: detail,
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
fn zero(value: &Value) -> bool {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.is_number().then(|| value.to_string()))
        .and_then(|text| money::decimal_units(&text).ok())
        == Some(0)
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
        return Err(invalid(
            "exact model is missing or ambiguous in OpenRouter catalog",
        ));
    }
    let model_row = selected[0];
    let catalog_parameters = strings(&model_row["supported_parameters"])
        .ok_or_else(|| invalid("model lacks declared parameter capabilities"))?;
    // OpenRouter's model contract documents an omitted reasoning object for
    // non-reasoning static models. Router aliases are rejected separately.
    if model_row.get("reasoning").is_some_and(|v| !v.is_null())
        || catalog_parameters.iter().any(|p| p.contains("reasoning"))
        || !strings(&model_row["architecture"]["input_modalities"])
            .is_some_and(|modalities| modalities.iter().any(|m| m == "text"))
        || !text_only(&model_row["architecture"]["output_modalities"])
    {
        return Err(invalid("model lacks an audited non-reasoning text-only contract; reasoning/image fees are not guessed"));
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
    let observed_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .ok_or_else(|| invalid("observation clock unavailable"))?;
    let mut profiles = Vec::new();
    let mut rejected = std::collections::BTreeSet::new();
    for row in rows {
        let Some(tag) = row["tag"].as_str().filter(|tag| tag.contains('/')) else {
            rejected.insert("no exact endpoint variant".to_owned());
            continue;
        };
        if row["model_id"].as_str() != Some(model) || row["status"].as_i64() != Some(0) {
            rejected.insert("model identity or endpoint availability differs".to_owned());
            continue;
        }
        let Some(context) = integer(&row["context_length"]) else {
            rejected.insert("missing context limit".to_owned());
            continue;
        };
        let Some(output) = integer(&row["max_completion_tokens"]) else {
            rejected.insert("missing output limit".to_owned());
            continue;
        };
        let Some(mut supported) = strings(&row["supported_parameters"]) else {
            rejected.insert("missing parameter capabilities".to_owned());
            continue;
        };
        supported.sort();
        supported.dedup();
        let Some(pricing) = row["pricing"].as_object() else {
            rejected.insert("missing endpoint pricing".to_owned());
            continue;
        };
        // Unknown priced SKUs and premium cache writes cannot inherit text
        // pricing bounds. Discounted reads are safely covered by full input.
        if pricing.iter().any(|(key, value)| {
            !matches!(
                key.as_str(),
                "prompt" | "completion" | "input_cache_read" | "discount"
            ) && !zero(value)
        }) {
            rejected.insert("unbounded additional price component".to_owned());
            continue;
        }
        let (Some(prompt), Some(completion)) = (
            pricing.get("prompt").and_then(Value::as_str),
            pricing.get("completion").and_then(Value::as_str),
        ) else {
            rejected.insert("missing prompt or completion prices".to_owned());
            continue;
        };
        if let Some(cache) = pricing.get("input_cache_read") {
            let Some(cache) = cache.as_str() else {
                rejected.insert("missing cache read price".to_owned());
                continue;
            };
            if money::decimal_units(cache)? > money::decimal_units(prompt)? {
                rejected.insert("cache price exceeds the input ceiling".to_owned());
                continue;
            }
        }
        let Some(provider_name) = row["provider_name"].as_str() else {
            rejected.insert("missing provider identity".to_owned());
            continue;
        };
        let profile = NativeOpenRouterObservation {
            schema_version: 1,
            base_url: base_url.trim_end_matches('/').into(),
            model: model.into(),
            endpoint_tag: tag.into(),
            provider_name: provider_name.into(),
            context_tokens: context,
            max_output_tokens: output,
            prompt_price_per_million: money::per_million_ceiling(prompt)?,
            completion_price_per_million: money::per_million_ceiling(completion)?,
            supported_parameters: supported,
            non_reasoning_evidence: "openrouter-model-catalog:no-reasoning-contract-v1".into(),
            observed_at_ms,
            billing: "openrouter_credits".into(),
        };
        match profile.validate() {
            Ok(()) => profiles.push(profile),
            Err(reason) => {
                rejected.insert(reason.to_string());
            }
        }
    }
    profiles.sort_by(|left, right| left.endpoint_tag.cmp(&right.endpoint_tag));
    if profiles.is_empty() {
        return Err(invalid(format!("no exact OpenRouter endpoint proves non-reasoning text/tool tokens and complete bounded pricing for {model}: {}", rejected.into_iter().collect::<Vec<_>>().join("; "))));
    }
    Ok(profiles)
}
