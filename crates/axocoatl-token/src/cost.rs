/// Per-provider token pricing (USD per million tokens).
#[derive(Debug, Clone)]
pub struct ProviderPricing {
    pub input_per_million: f64,
    pub output_per_million: f64,
}

/// Get pricing for a provider/model combination.
/// Prices as of March 2026 — update when providers change pricing.
pub fn get_pricing(provider: &str, model: &str) -> ProviderPricing {
    match (provider, model) {
        ("openai", m) if m.contains("gpt-4o") => ProviderPricing {
            input_per_million: 2.50,
            output_per_million: 10.00,
        },
        ("openai", m) if m.contains("gpt-4o-mini") => ProviderPricing {
            input_per_million: 0.15,
            output_per_million: 0.60,
        },
        ("anthropic", m) if m.contains("claude-sonnet") => ProviderPricing {
            input_per_million: 3.00,
            output_per_million: 15.00,
        },
        ("anthropic", m) if m.contains("claude-haiku") => ProviderPricing {
            input_per_million: 0.25,
            output_per_million: 1.25,
        },
        ("anthropic", m) if m.contains("claude-opus") => ProviderPricing {
            input_per_million: 15.00,
            output_per_million: 75.00,
        },
        ("ollama", _) => ProviderPricing {
            input_per_million: 0.0,
            output_per_million: 0.0,
        },
        _ => ProviderPricing {
            input_per_million: 1.0,
            output_per_million: 1.0,
        },
    }
}

/// Calculate the USD cost for a given token usage.
pub fn calculate_cost(pricing: &ProviderPricing, input_tokens: usize, output_tokens: usize) -> f64 {
    (input_tokens as f64 / 1_000_000.0) * pricing.input_per_million
        + (output_tokens as f64 / 1_000_000.0) * pricing.output_per_million
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_gpt4o_pricing() {
        let pricing = get_pricing("openai", "gpt-4o");
        assert!((pricing.input_per_million - 2.50).abs() < f64::EPSILON);
    }

    #[test]
    fn ollama_is_free() {
        let pricing = get_pricing("ollama", "llama3");
        assert!((pricing.input_per_million).abs() < f64::EPSILON);
        assert!((pricing.output_per_million).abs() < f64::EPSILON);
    }

    #[test]
    fn calculate_cost_basic() {
        let pricing = ProviderPricing {
            input_per_million: 2.50,
            output_per_million: 10.00,
        };
        let cost = calculate_cost(&pricing, 1_000_000, 1_000_000);
        assert!((cost - 12.50).abs() < 0.001);
    }

    #[test]
    fn calculate_cost_zero_tokens() {
        let pricing = get_pricing("openai", "gpt-4o");
        let cost = calculate_cost(&pricing, 0, 0);
        assert!((cost).abs() < f64::EPSILON);
    }

    #[test]
    fn unknown_provider_has_fallback_pricing() {
        let pricing = get_pricing("unknown", "some-model");
        assert!((pricing.input_per_million - 1.0).abs() < f64::EPSILON);
    }
}
