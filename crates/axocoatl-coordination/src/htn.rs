use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// A task in the HTN hierarchy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HtnTask {
    pub name: String,
    pub parameters: HashMap<String, serde_json::Value>,
    pub task_type: HtnTaskType,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum HtnTaskType {
    /// Directly executable (maps to a tool or agent action).
    Primitive,
    /// Must be decomposed into sub-tasks.
    Compound,
}

/// A decomposition method: how to break a compound task into primitives.
#[derive(Debug, Clone)]
pub struct DecompositionMethod {
    pub task_pattern: String,
    pub preconditions: Vec<Condition>,
    pub subtasks: Vec<HtnTask>,
}

/// A simple precondition check.
#[derive(Debug, Clone)]
pub struct Condition {
    pub key: String,
    pub expected: serde_json::Value,
}

impl Condition {
    pub fn satisfied(&self, state: &HashMap<String, serde_json::Value>) -> bool {
        state.get(&self.key) == Some(&self.expected)
    }
}

/// Result of HTN planning.
pub struct HtnPlan {
    /// Tasks that can be executed directly.
    pub primitives: Vec<HtnTask>,
    /// Tasks that need LLM involvement to decompose further.
    pub llm_frontiers: Vec<HtnTask>,
}

impl HtnPlan {
    pub fn new() -> Self {
        Self {
            primitives: Vec::new(),
            llm_frontiers: Vec::new(),
        }
    }
}

impl Default for HtnPlan {
    fn default() -> Self {
        Self::new()
    }
}

/// The symbolic planner — resolves tasks without LLM calls when methods are available.
pub struct HtnPlanner {
    methods: Vec<DecompositionMethod>,
    state: HashMap<String, serde_json::Value>,
}

impl HtnPlanner {
    pub fn new() -> Self {
        Self {
            methods: Vec::new(),
            state: HashMap::new(),
        }
    }

    /// Register a decomposition method.
    pub fn add_method(&mut self, method: DecompositionMethod) {
        self.methods.push(method);
    }

    /// Update world state.
    pub fn set_state(&mut self, key: impl Into<String>, value: serde_json::Value) {
        self.state.insert(key.into(), value);
    }

    /// Attempt to decompose a task symbolically.
    /// Returns None if no method applies — caller should invoke LLM.
    pub fn decompose(&self, task: &HtnTask) -> Option<Vec<HtnTask>> {
        for method in &self.methods {
            if method.task_pattern == task.name
                && method
                    .preconditions
                    .iter()
                    .all(|c| c.satisfied(&self.state))
            {
                return Some(method.subtasks.clone());
            }
        }
        None
    }

    /// Recursively plan: decompose compound tasks, collect primitives and LLM frontiers.
    pub fn plan(&self, root: HtnTask) -> HtnPlan {
        let mut plan = HtnPlan::new();
        self.plan_recursive(root, &mut plan);
        plan
    }

    fn plan_recursive(&self, task: HtnTask, plan: &mut HtnPlan) {
        match task.task_type {
            HtnTaskType::Primitive => {
                plan.primitives.push(task);
            }
            HtnTaskType::Compound => match self.decompose(&task) {
                Some(subtasks) => {
                    for subtask in subtasks {
                        self.plan_recursive(subtask, plan);
                    }
                }
                None => {
                    plan.llm_frontiers.push(task);
                }
            },
        }
    }
}

impl Default for HtnPlanner {
    fn default() -> Self {
        Self::new()
    }
}

/// Trait for LLM-based task decomposition at frontiers.
#[async_trait::async_trait]
pub trait FrontierResolver: Send + Sync {
    /// Decompose a frontier task using LLM.
    /// Returns a list of subtasks (name, type, parameters).
    async fn resolve(
        &self,
        task: &HtnTask,
        state: &HashMap<String, serde_json::Value>,
    ) -> Result<Vec<HtnTask>, String>;
}

impl HtnPlanner {
    /// Resolve all frontier tasks using an LLM resolver, then re-plan.
    /// Returns a fully-resolved plan with no remaining frontiers (or errors if LLM fails).
    pub async fn resolve_frontiers(
        &mut self,
        root: HtnTask,
        resolver: &dyn FrontierResolver,
        max_rounds: usize,
    ) -> Result<HtnPlan, String> {
        let mut current_plan = self.plan(root.clone());
        let mut round = 0;

        while !current_plan.llm_frontiers.is_empty() && round < max_rounds {
            round += 1;
            tracing::debug!(
                round,
                frontiers = current_plan.llm_frontiers.len(),
                "Resolving LLM frontiers"
            );

            // Resolve each frontier task and register as new methods
            let frontiers: Vec<HtnTask> = current_plan.llm_frontiers.drain(..).collect();
            for frontier_task in frontiers {
                let subtasks = resolver.resolve(&frontier_task, &self.state).await?;

                self.add_method(DecompositionMethod {
                    task_pattern: frontier_task.name.clone(),
                    preconditions: vec![],
                    subtasks,
                });
            }

            // Re-plan from scratch with newly registered methods
            current_plan = self.plan(root.clone());
        }

        if !current_plan.llm_frontiers.is_empty() {
            tracing::warn!(
                remaining = current_plan.llm_frontiers.len(),
                "Some frontiers remain unresolved after {} rounds",
                max_rounds
            );
        }

        Ok(current_plan)
    }
}

/// Orchestration plan linking HTN output to worker assignments.
#[derive(Debug)]
pub struct OrchestrationPlan {
    /// Primitives assigned to workers: (worker_id, task).
    pub assignments: Vec<(String, HtnTask)>,
    /// Unassigned tasks (no suitable worker found).
    pub unassigned: Vec<HtnTask>,
}

impl OrchestrationPlan {
    /// Create from an HTN plan, assigning primitives to available workers.
    pub fn from_plan(plan: HtnPlan, available_workers: &[String]) -> Self {
        let mut assignments = Vec::new();
        let mut unassigned = Vec::new();

        for (i, task) in plan.primitives.into_iter().enumerate() {
            if available_workers.is_empty() {
                unassigned.push(task);
            } else {
                // Round-robin assignment
                let worker = &available_workers[i % available_workers.len()];
                assignments.push((worker.clone(), task));
            }
        }

        // Frontier tasks are unassigned (need further decomposition)
        unassigned.extend(plan.llm_frontiers);

        Self {
            assignments,
            unassigned,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn primitive(name: &str) -> HtnTask {
        HtnTask {
            name: name.to_string(),
            parameters: HashMap::new(),
            task_type: HtnTaskType::Primitive,
        }
    }

    fn compound(name: &str) -> HtnTask {
        HtnTask {
            name: name.to_string(),
            parameters: HashMap::new(),
            task_type: HtnTaskType::Compound,
        }
    }

    #[test]
    fn primitive_task_goes_to_plan_directly() {
        let planner = HtnPlanner::new();
        let plan = planner.plan(primitive("do_thing"));
        assert_eq!(plan.primitives.len(), 1);
        assert!(plan.llm_frontiers.is_empty());
    }

    #[test]
    fn compound_without_method_goes_to_frontier() {
        let planner = HtnPlanner::new();
        let plan = planner.plan(compound("unknown_task"));
        assert!(plan.primitives.is_empty());
        assert_eq!(plan.llm_frontiers.len(), 1);
    }

    #[test]
    fn compound_with_method_decomposes() {
        let mut planner = HtnPlanner::new();
        planner.add_method(DecompositionMethod {
            task_pattern: "research".to_string(),
            preconditions: vec![],
            subtasks: vec![primitive("search"), primitive("summarize")],
        });

        let plan = planner.plan(compound("research"));
        assert_eq!(plan.primitives.len(), 2);
        assert_eq!(plan.primitives[0].name, "search");
        assert_eq!(plan.primitives[1].name, "summarize");
        assert!(plan.llm_frontiers.is_empty());
    }

    #[test]
    fn precondition_must_be_satisfied() {
        let mut planner = HtnPlanner::new();
        planner.add_method(DecompositionMethod {
            task_pattern: "deploy".to_string(),
            preconditions: vec![Condition {
                key: "tests_passing".to_string(),
                expected: serde_json::json!(true),
            }],
            subtasks: vec![primitive("push"), primitive("notify")],
        });

        // Without state — precondition fails, goes to frontier
        let plan = planner.plan(compound("deploy"));
        assert_eq!(plan.llm_frontiers.len(), 1);

        // With state — precondition passes, decomposes
        planner.set_state("tests_passing", serde_json::json!(true));
        let plan = planner.plan(compound("deploy"));
        assert_eq!(plan.primitives.len(), 2);
        assert!(plan.llm_frontiers.is_empty());
    }

    #[test]
    fn nested_decomposition() {
        let mut planner = HtnPlanner::new();
        planner.add_method(DecompositionMethod {
            task_pattern: "build_report".to_string(),
            preconditions: vec![],
            subtasks: vec![compound("gather_data"), primitive("format_output")],
        });
        planner.add_method(DecompositionMethod {
            task_pattern: "gather_data".to_string(),
            preconditions: vec![],
            subtasks: vec![primitive("query_db"), primitive("fetch_api")],
        });

        let plan = planner.plan(compound("build_report"));
        assert_eq!(plan.primitives.len(), 3); // query_db, fetch_api, format_output
        assert!(plan.llm_frontiers.is_empty());
    }

    #[test]
    fn mixed_decomposition_and_frontier() {
        let mut planner = HtnPlanner::new();
        planner.add_method(DecompositionMethod {
            task_pattern: "analyze".to_string(),
            preconditions: vec![],
            subtasks: vec![primitive("extract"), compound("interpret")],
        });
        // No method for "interpret" — it goes to LLM frontier

        let plan = planner.plan(compound("analyze"));
        assert_eq!(plan.primitives.len(), 1); // extract
        assert_eq!(plan.llm_frontiers.len(), 1); // interpret
    }
}
