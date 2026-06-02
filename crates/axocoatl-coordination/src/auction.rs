use axocoatl_core::{AgentConfig, AgentId};

/// A bid from an agent for a task.
#[derive(Debug, Clone)]
pub struct AgentBid {
    pub agent_id: AgentId,
    pub score: f32,
}

/// Score an agent for a task based on deterministic heuristics (no LLM call).
pub fn compute_bid(
    agent: &AgentConfig,
    required_tools: &[String],
    current_load: usize,
    token_budget_remaining: usize,
) -> AgentBid {
    let mut score = 0.0f32;

    // Factor 1: Capability match (does agent have required tools?)
    let has_all_tools = required_tools.iter().all(|t| agent.tools.contains(t));
    if has_all_tools {
        score += 0.4;
    } else {
        return AgentBid {
            agent_id: agent.id.clone(),
            score: 0.0,
        };
    }

    // Factor 2: Current load (prefer less busy agents)
    let load_penalty = (current_load as f32 / 10.0).min(0.3);
    score += 0.3 - load_penalty;

    // Factor 3: Token budget availability
    if token_budget_remaining > 1000 {
        score += 0.3;
    } else if token_budget_remaining > 100 {
        score += 0.15;
    }

    AgentBid {
        agent_id: agent.id.clone(),
        score,
    }
}

/// Run an auction: return the winning agent (highest score > 0).
pub fn run_auction(bids: Vec<AgentBid>) -> Option<AgentId> {
    bids.into_iter()
        .filter(|b| b.score > 0.0)
        .max_by(|a, b| {
            a.score
                .partial_cmp(&b.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|b| b.agent_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_with_tools(id: &str, tools: Vec<&str>) -> AgentConfig {
        AgentConfig {
            id: AgentId::new(id),
            name: id.to_string(),
            tools: tools.into_iter().map(String::from).collect(),
            ..AgentConfig::default()
        }
    }

    #[test]
    fn agent_with_matching_tools_scores_high() {
        let agent = agent_with_tools("a", vec!["web_search", "read_file"]);
        let bid = compute_bid(&agent, &["web_search".to_string()], 0, 5000);
        assert!(bid.score > 0.9); // 0.4 + 0.3 + 0.3 = 1.0
    }

    #[test]
    fn agent_missing_tools_scores_zero() {
        let agent = agent_with_tools("a", vec!["read_file"]);
        let bid = compute_bid(&agent, &["web_search".to_string()], 0, 5000);
        assert_eq!(bid.score, 0.0);
    }

    #[test]
    fn high_load_reduces_score() {
        let agent = agent_with_tools("a", vec!["web_search"]);
        let low_load = compute_bid(&agent, &["web_search".to_string()], 0, 5000);
        let high_load = compute_bid(&agent, &["web_search".to_string()], 10, 5000);
        assert!(low_load.score > high_load.score);
    }

    #[test]
    fn low_budget_reduces_score() {
        let agent = agent_with_tools("a", vec!["web_search"]);
        let high_budget = compute_bid(&agent, &["web_search".to_string()], 0, 5000);
        let low_budget = compute_bid(&agent, &["web_search".to_string()], 0, 50);
        assert!(high_budget.score > low_budget.score);
    }

    #[test]
    fn auction_picks_highest_scorer() {
        let bids = vec![
            AgentBid {
                agent_id: AgentId::new("low"),
                score: 0.3,
            },
            AgentBid {
                agent_id: AgentId::new("high"),
                score: 0.9,
            },
            AgentBid {
                agent_id: AgentId::new("mid"),
                score: 0.6,
            },
        ];
        let winner = run_auction(bids).unwrap();
        assert_eq!(winner, AgentId::new("high"));
    }

    #[test]
    fn auction_with_all_zero_returns_none() {
        let bids = vec![
            AgentBid {
                agent_id: AgentId::new("a"),
                score: 0.0,
            },
            AgentBid {
                agent_id: AgentId::new("b"),
                score: 0.0,
            },
        ];
        assert!(run_auction(bids).is_none());
    }

    #[test]
    fn auction_empty_returns_none() {
        assert!(run_auction(vec![]).is_none());
    }

    #[test]
    fn no_required_tools_still_scores() {
        let agent = agent_with_tools("a", vec![]);
        let bid = compute_bid(&agent, &[], 0, 5000);
        assert!(bid.score > 0.0); // No tools required, so capability match passes
    }
}
