//! The models of the pinned external programs: how many tokens one model
//! call of the program can use, and the list prices Axocoatl prices a
//! program's reported tokens at when the program reports no cost. Owner:
//! workstream `agents`.
//!
//! Sources, read on [`PINNED_ON`]:
//!
//! - Codex 0.160.1's bundled model catalog
//!   (`codex-rs/models-manager/models.json` at tag `rust-v0.160.1`): the
//!   models it lists for API use and the context window it keeps for each
//!   (272,000 tokens; it compacts the conversation before passing it, so no
//!   request reaches the 272K-input long-context price).
//! - OpenAI's model pages (`https://developers.openai.com/api/docs/models/<model>`):
//!   each model's output limit, context window and Standard prices for
//!   prompts of at most 272K input tokens: input, cached input, cache
//!   writes (1.25 times input where the page prices them; billed as input
//!   where it does not) and output, reasoning included. GPT-5.6 Sol's price
//!   is OpenAI's promotional price, which its page says runs at least
//!   through 2026-11-21. `codex exec` sends no service tier, so a run is
//!   billed at Standard rates.
//! - Anthropic's model overview: each Claude model's context window (input
//!   and output) and output limit. Claude Code reports its own cost
//!   (`total_cost_usd`), so no Claude price is pinned.
//!
//! A model the table does not list runs as before: admission warns that its
//! context is not checked and that its cost is computed only from a
//! `pricing` entry of the configuration; without one it is not known.

use axocoatl_config::loadout::AgentRuntime;
use axocoatl_config::ModelPriceYaml;

/// The day the table was read from its sources.
pub const PINNED_ON: &str = "2026-10-08";

/// Dollars per million tokens, in micro-dollars (`$1.75` is `1_750_000`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListPrice {
    /// Uncached input.
    pub input: u64,
    /// Input read from the prompt cache.
    pub cached_input: u64,
    /// Input written to the prompt cache.
    pub cache_write: u64,
    /// Output, reasoning included.
    pub output: u64,
}

impl ListPrice {
    /// Every input token at the input rate: a price that names only input
    /// and output (a configuration's `pricing` entry).
    pub const fn flat(input: u64, output: u64) -> Self {
        Self {
            input,
            cached_input: input,
            cache_write: input,
            output,
        }
    }

    /// A configuration `pricing` entry (dollars per million tokens), when
    /// both rates are finite and not negative.
    pub fn from_configured(price: &ModelPriceYaml) -> Option<Self> {
        let micro = |dollars: f64| {
            (dollars.is_finite() && (0.0..=1_000_000.0).contains(&dollars))
                .then(|| (dollars * 1_000_000.0).round() as u64)
        };
        Some(Self::flat(
            micro(price.input_per_mtok)?,
            micro(price.output_per_mtok)?,
        ))
    }

    /// What `usage` costs at this price, in micro-dollars, rounded up.
    pub fn cost_microunits(&self, usage: &ExternalUsage) -> u64 {
        let uncached = usage
            .input_tokens
            .saturating_sub(usage.cached_input_tokens)
            .saturating_sub(usage.cache_write_tokens);
        let cached = usage.cached_input_tokens.min(usage.input_tokens);
        let written = usage
            .cache_write_tokens
            .min(usage.input_tokens.saturating_sub(cached));
        let total: u128 = [
            (uncached, self.input),
            (cached, self.cached_input),
            (written, self.cache_write),
            (usage.output_tokens, self.output),
        ]
        .iter()
        .map(|(tokens, rate)| u128::from(*tokens) * u128::from(*rate))
        .sum();
        u64::try_from(total.div_ceil(1_000_000)).unwrap_or(u64::MAX)
    }
}

/// The token counts a program reported for its whole run. Cached input and
/// cache writes are parts of the input; reasoning is part of the output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExternalUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
}

/// One model of a pinned program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinnedModel {
    pub runtime: AgentRuntime,
    /// The model name the program takes.
    pub model: &'static str,
    /// The context the program keeps for the model: what it fills before
    /// it compacts the conversation.
    pub context_tokens: u64,
    /// The model's output limit per call.
    pub max_output_tokens: u64,
    /// The model's whole context window, input and output.
    pub window_tokens: u64,
    /// List prices, for a program that reports no cost of its own.
    pub price: Option<ListPrice>,
}

impl PinnedModel {
    /// The most tokens one model call of the program can use: the context
    /// it keeps plus the model's output limit, within the model's window.
    pub fn call_tokens(&self) -> u64 {
        self.context_tokens
            .saturating_add(self.max_output_tokens)
            .min(self.window_tokens)
    }

    /// [`Self::call_tokens`] in words, for the admission refusal.
    pub fn call_words(&self) -> String {
        let program = program_name(self.runtime);
        if self.call_tokens() == self.window_tokens {
            format!(
                "{}'s whole {}-token context window, input and output, which one call of {program} \
                 can fill",
                self.model, self.window_tokens
            )
        } else {
            format!(
                "the {}-token context {program} keeps for {} plus the model's {} output tokens",
                self.context_tokens, self.model, self.max_output_tokens
            )
        }
    }
}

/// The program and its pinned version, such as `Codex 0.160.1`.
pub fn program_name(runtime: AgentRuntime) -> String {
    match runtime {
        AgentRuntime::ClaudeCode => {
            format!("Claude Code {}", super::claude_code::CLAUDE_CODE_VERSION)
        }
        AgentRuntime::Codex => format!("Codex {}", super::codex::CODEX_VERSION),
        AgentRuntime::Native => "Axocoatl's tool loop".into(),
    }
}

/// Dollars per million tokens to micro-dollars, for the table.
const fn usd(dollars: u64, cents_millis: u64) -> u64 {
    dollars * 1_000_000 + cents_millis * 1_000
}

/// An OpenAI Standard price: input, cached input, cache writes, output.
const fn openai(input: u64, cached_input: u64, cache_write: u64, output: u64) -> Option<ListPrice> {
    Some(ListPrice {
        input,
        cached_input,
        cache_write,
        output,
    })
}

const CODEX_CONTEXT: u64 = 272_000;
const OPENAI_OUTPUT: u64 = 128_000;
const OPENAI_WINDOW: u64 = 1_050_000;

const fn codex(model: &'static str, price: Option<ListPrice>) -> PinnedModel {
    PinnedModel {
        runtime: AgentRuntime::Codex,
        model,
        context_tokens: CODEX_CONTEXT,
        max_output_tokens: OPENAI_OUTPUT,
        window_tokens: OPENAI_WINDOW,
        price,
    }
}

const fn claude(model: &'static str, window: u64, output: u64) -> PinnedModel {
    PinnedModel {
        runtime: AgentRuntime::ClaudeCode,
        model,
        context_tokens: window,
        max_output_tokens: output,
        window_tokens: window,
        price: None,
    }
}

/// The table. Prices are `usd(dollars, thousandths of a dollar)` per
/// million tokens: input, cached input, cache writes, output.
pub const PINNED_MODELS: [PinnedModel; 20] = [
    // Codex 0.160.1's API models, OpenAI Standard prices.
    codex(
        "gpt-6-astra",
        openai(usd(10, 0), usd(1, 0), usd(12, 500), usd(50, 0)),
    ),
    codex(
        "gpt-6.1-sol",
        openai(usd(2, 0), usd(0, 100), usd(2, 500), usd(10, 0)),
    ),
    codex(
        "gpt-6-sol",
        openai(usd(2, 0), usd(0, 200), usd(2, 500), usd(10, 0)),
    ),
    codex(
        "gpt-6-luna",
        openai(usd(0, 100), usd(0, 10), usd(0, 125), usd(0, 500)),
    ),
    codex(
        "gpt-5.6-sol",
        openai(usd(4, 0), usd(0, 400), usd(5, 0), usd(20, 0)),
    ),
    codex(
        "gpt-5.6-terra",
        openai(usd(2, 0), usd(0, 200), usd(2, 500), usd(12, 0)),
    ),
    codex(
        "gpt-5.6-luna",
        openai(usd(0, 200), usd(0, 20), usd(0, 250), usd(1, 200)),
    ),
    // GPT-5.5's page prices no cache writes: they are input.
    codex(
        "gpt-5.5",
        openai(usd(5, 0), usd(0, 500), usd(5, 0), usd(30, 0)),
    ),
    // Claude Code 2.1.292's models: it reports its own cost.
    claude("claude-fable-5-1", 1_000_000, 128_000),
    claude("claude-fable-5", 1_000_000, 128_000),
    claude("claude-opus-5-5", 1_000_000, 128_000),
    claude("claude-opus-5", 1_000_000, 128_000),
    claude("claude-opus-4-8", 1_000_000, 128_000),
    claude("claude-opus-4-7", 1_000_000, 128_000),
    claude("claude-opus-4-6", 1_000_000, 128_000),
    claude("claude-sonnet-5-5", 1_000_000, 128_000),
    claude("claude-sonnet-5", 1_000_000, 128_000),
    claude("claude-sonnet-4-6", 1_000_000, 128_000),
    claude("claude-haiku-5-5", 1_000_000, 128_000),
    claude("claude-haiku-4-5", 200_000, 64_000),
];

/// The pinned entry of `runtime`'s `model`: the same model id, a snapshot
/// date or `-latest` left out ([`axocoatl_core::same_model`]).
pub fn pinned_model(runtime: AgentRuntime, model: &str) -> Option<&'static PinnedModel> {
    let provider = runtime.model_provider()?;
    PINNED_MODELS.iter().find(|entry| {
        entry.runtime == runtime
            && axocoatl_core::same_model(provider, entry.model, provider, model)
    })
}

/// Where a computed cost's price came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceSource {
    /// The configuration's `pricing` entry for the model.
    Configured,
    /// [`PINNED_MODELS`].
    Pinned,
}

/// The price Axocoatl computes `runtime`'s cost at for `model`, for a
/// program that reports tokens but no cost of its own: the configuration's
/// `pricing` entry for exactly that model name when there is one, else the
/// pinned list price. `None` for a program that reports its own cost
/// (Claude Code), and for a model neither names.
pub fn computed_price(
    runtime: AgentRuntime,
    model: &str,
    configured: Option<&ModelPriceYaml>,
) -> Option<(ListPrice, PriceSource)> {
    if runtime == AgentRuntime::Native || super::reports_cost(runtime) {
        return None;
    }
    if let Some(price) = configured.and_then(ListPrice::from_configured) {
        return Some((price, PriceSource::Configured));
    }
    pinned_model(runtime, model)
        .and_then(|entry| entry.price)
        .map(|price| (price, PriceSource::Pinned))
}

/// The code of the run warning [`unpinned_warning`] words.
pub const UNPINNED_MODEL: &str = "external_model_not_pinned";

/// Why admission cannot check or price `runtime`'s `model`: it is not in
/// [`PINNED_MODELS`]. `priced` is whether the configuration's `pricing`
/// names it.
pub fn unpinned_warning(runtime: AgentRuntime, model: &str, priced: bool) -> Option<String> {
    if runtime == AgentRuntime::Native || pinned_model(runtime, model).is_some() {
        return None;
    }
    let program = program_name(runtime);
    let cost = if super::reports_cost(runtime) {
        String::new()
    } else if priced {
        " Its cost is computed from the tokens it reports at the configuration's `pricing` \
         entry for it."
            .into()
    } else {
        " It reports tokens but no cost, and the configuration's `pricing` does not name it, so \
         the run's cost will not be known (reserved up to what its grant allows)."
            .into()
    };
    Some(format!(
        "{program}'s model {model} is not in Axocoatl's pinned model table, so admission did \
         not check that its tokens budget holds one model call.{cost}"
    ))
}

/// The [`unpinned_warning`] of each external writer of `resolved`, as the
/// run's warnings (`external_model_not_pinned`, on the writer's `model`
/// field). `pricing` is the configuration's.
pub fn unpinned_warnings(
    resolved: &axocoatl_config::loadout::ResolvedLoadout,
    pricing: &std::collections::HashMap<String, ModelPriceYaml>,
) -> Vec<axocoatl_config::loadout::LoadoutWarning> {
    resolved
        .loadout
        .file
        .agents
        .iter()
        .filter_map(|agent| {
            let model = resolved.agent_models.get(&agent.id)?;
            let message = unpinned_warning(
                agent.runtime,
                &model.model,
                pricing.contains_key(&model.model),
            )?;
            Some(axocoatl_config::loadout::LoadoutWarning {
                code: UNPINNED_MODEL.into(),
                field: format!("agents.{}.model", agent.id),
                message,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_lists_each_model_of_its_program_once() {
        for entry in &PINNED_MODELS {
            assert_eq!(
                PINNED_MODELS
                    .iter()
                    .filter(|other| other.runtime == entry.runtime && other.model == entry.model)
                    .count(),
                1,
                "{}",
                entry.model
            );
            assert_eq!(pinned_model(entry.runtime, entry.model), Some(entry));
            // A program that reports its own cost has no pinned price.
            assert_eq!(
                entry.price.is_some(),
                !crate::external_agent::reports_cost(entry.runtime)
            );
        }
        // Snapshot dates and case do not change the model.
        assert_eq!(
            pinned_model(AgentRuntime::ClaudeCode, "claude-haiku-4-5-20251001")
                .unwrap()
                .model,
            "claude-haiku-4-5"
        );
        assert_eq!(
            pinned_model(AgentRuntime::Codex, "GPT-5.5").unwrap().model,
            "gpt-5.5"
        );
        assert!(pinned_model(AgentRuntime::Codex, "claude-haiku-4-5").is_none());
        assert!(pinned_model(AgentRuntime::Codex, "gpt-7").is_none());
        assert!(pinned_model(AgentRuntime::Native, "gpt-5.5").is_none());
    }

    #[test]
    fn one_call_is_the_programs_context_plus_output_within_the_window() {
        let codex = pinned_model(AgentRuntime::Codex, "gpt-5.5").unwrap();
        assert_eq!(codex.call_tokens(), 400_000);
        assert_eq!(
            codex.call_words(),
            "the 272000-token context Codex 0.160.1 keeps for gpt-5.5 plus the model's 128000 \
             output tokens"
        );
        let sonnet = pinned_model(AgentRuntime::ClaudeCode, "claude-sonnet-5-5").unwrap();
        assert_eq!(sonnet.call_tokens(), 1_000_000);
        assert_eq!(
            sonnet.call_words(),
            "claude-sonnet-5-5's whole 1000000-token context window, input and output, which one \
             call of Claude Code 2.1.292 can fill"
        );
        let haiku = pinned_model(AgentRuntime::ClaudeCode, "claude-haiku-4-5").unwrap();
        assert_eq!(haiku.call_tokens(), 200_000);
    }

    /// The Codex 0.160.1 capture's turn (`fixtures/codex-0.160.1-exec.jsonl`):
    /// 4,107 input tokens of which 1,000 cached, 60 output, on gpt-5.5 at
    /// $5 input, $0.50 cached input and $30 output per million tokens.
    #[test]
    fn a_cost_is_each_part_of_the_usage_at_its_rate_rounded_up() {
        let (price, source) = computed_price(AgentRuntime::Codex, "gpt-5.5", None).unwrap();
        assert_eq!(source, PriceSource::Pinned);
        let usage = ExternalUsage {
            input_tokens: 4107,
            cached_input_tokens: 1000,
            cache_write_tokens: 0,
            output_tokens: 60,
        };
        // 3107 × 5 + 1000 × 0.5 + 60 × 30 = 15535 + 500 + 1800 = 17835 µ$.
        assert_eq!(price.cost_microunits(&usage), 17_835);
        // Cache writes at their own rate, as part of the input.
        let (sol, _) = computed_price(AgentRuntime::Codex, "gpt-5.6-sol", None).unwrap();
        let written = ExternalUsage {
            input_tokens: 100,
            cached_input_tokens: 40,
            cache_write_tokens: 60,
            output_tokens: 0,
        };
        // 40 × 0.40 + 60 × 5.00 = 16 + 300 µ$.
        assert_eq!(sol.cost_microunits(&written), 316);
        // A fraction of a micro-dollar is charged as one.
        let one = ExternalUsage {
            input_tokens: 1,
            ..ExternalUsage::default()
        };
        assert_eq!(sol.cost_microunits(&one), 4);
        let luna = computed_price(AgentRuntime::Codex, "gpt-6-luna", None)
            .unwrap()
            .0;
        assert_eq!(luna.cost_microunits(&one), 1);
        // Counts that do not add up never charge below zero.
        let odd = ExternalUsage {
            input_tokens: 10,
            cached_input_tokens: 50,
            cache_write_tokens: 50,
            output_tokens: 0,
        };
        assert_eq!(sol.cost_microunits(&odd), 4);
    }

    #[test]
    fn a_configured_price_wins_and_claude_code_is_never_priced() {
        let configured = ModelPriceYaml {
            input_per_mtok: 1.25,
            output_per_mtok: 10.0,
        };
        let (price, source) =
            computed_price(AgentRuntime::Codex, "gpt-5.5", Some(&configured)).unwrap();
        assert_eq!(source, PriceSource::Configured);
        assert_eq!(price, ListPrice::flat(1_250_000, 10_000_000));
        // A model the table does not list is priced only by the
        // configuration.
        assert!(computed_price(AgentRuntime::Codex, "gpt-7", None).is_none());
        assert_eq!(
            computed_price(AgentRuntime::Codex, "gpt-7", Some(&configured))
                .unwrap()
                .1,
            PriceSource::Configured
        );
        let negative = ModelPriceYaml {
            input_per_mtok: -1.0,
            output_per_mtok: 1.0,
        };
        assert_eq!(
            computed_price(AgentRuntime::Codex, "gpt-5.5", Some(&negative))
                .unwrap()
                .1,
            PriceSource::Pinned
        );
        // Claude Code reports its own cost.
        assert!(computed_price(
            AgentRuntime::ClaudeCode,
            "claude-haiku-4-5",
            Some(&configured)
        )
        .is_none());
        assert!(computed_price(AgentRuntime::Native, "gpt-5.5", Some(&configured)).is_none());
    }

    /// Admission adds the warning to the run's for each external writer on
    /// a model the table does not list, and for nothing else.
    #[test]
    fn a_runs_unpinned_external_writer_is_warned_about() {
        use axocoatl_config::loadout::{builtin_loadouts, resolve_loadout, ParamValues};
        let mut fix = builtin_loadouts()
            .into_iter()
            .map(Result::unwrap)
            .find(|loadout| loadout.file.id == "fix")
            .unwrap();
        fix.file.agents[0].runtime = AgentRuntime::Codex;
        let resolve = |writer: &str| {
            let mut params = ParamValues::new();
            params.insert("writer_model".into(), writer.into());
            params.insert("reviewer_model".into(), "ollama:gpt-oss:120b".into());
            resolve_loadout(&fix, &params, "fix it", "/repo").unwrap()
        };
        let none = std::collections::HashMap::new();
        assert!(unpinned_warnings(&resolve("openai:gpt-5.5"), &none).is_empty());
        let warnings = unpinned_warnings(&resolve("openai:gpt-7"), &none);
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].code, "external_model_not_pinned");
        assert_eq!(warnings[0].field, "agents.writer.model");
        assert!(warnings[0].message.contains("will not be known"));
        let priced = std::collections::HashMap::from([(
            "gpt-7".to_string(),
            ModelPriceYaml {
                input_per_mtok: 1.0,
                output_per_mtok: 2.0,
            },
        )]);
        assert!(unpinned_warnings(&resolve("openai:gpt-7"), &priced)[0]
            .message
            .contains("`pricing` entry for it"));
    }

    #[test]
    fn an_unpinned_model_is_warned_about() {
        assert_eq!(
            unpinned_warning(AgentRuntime::Codex, "gpt-5.5", false),
            None
        );
        assert_eq!(
            unpinned_warning(AgentRuntime::Codex, "gpt-7", false).unwrap(),
            "Codex 0.160.1's model gpt-7 is not in Axocoatl's pinned model table, so admission \
             did not check that its tokens budget holds one model call. It reports tokens but no \
             cost, and the configuration's `pricing` does not name it, so the run's cost will not \
             be known (reserved up to what its grant allows)."
        );
        assert!(unpinned_warning(AgentRuntime::Codex, "gpt-7", true)
            .unwrap()
            .ends_with("at the configuration's `pricing` entry for it."));
        assert_eq!(
            unpinned_warning(AgentRuntime::ClaudeCode, "sonnet", false).unwrap(),
            "Claude Code 2.1.292's model sonnet is not in Axocoatl's pinned model table, so \
             admission did not check that its tokens budget holds one model call."
        );
    }
}
