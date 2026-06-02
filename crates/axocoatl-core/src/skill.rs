//! Skill system — reusable prompt templates that agents can invoke.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// A skill is a named, parameterized prompt template.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub template: String,
    #[serde(default)]
    pub parameters: Vec<SkillParameter>,
}

/// A parameter that a skill accepts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillParameter {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub required: bool,
    pub default: Option<String>,
}

impl Skill {
    /// Render the skill template with the given parameter values.
    pub fn render(&self, params: &HashMap<String, String>) -> Result<String, SkillError> {
        // Check required parameters
        for param in &self.parameters {
            if param.required && !params.contains_key(&param.name) && param.default.is_none() {
                return Err(SkillError::MissingParameter {
                    skill: self.name.clone(),
                    parameter: param.name.clone(),
                });
            }
        }

        // Simple template rendering: replace {{param_name}} with values
        let mut result = self.template.clone();
        for param in &self.parameters {
            let placeholder = format!("{{{{{}}}}}", param.name);
            let empty = String::new();
            let value = params
                .get(&param.name)
                .or(param.default.as_ref())
                .unwrap_or(&empty);
            result = result.replace(&placeholder, value);
        }

        // Also replace any ad-hoc parameters from the params map
        for (key, value) in params {
            let placeholder = format!("{{{{{key}}}}}");
            result = result.replace(&placeholder, value);
        }

        Ok(result)
    }
}

/// Registry of available skills.
#[derive(Debug, Default)]
pub struct SkillRegistry {
    skills: HashMap<String, Skill>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self {
            skills: HashMap::new(),
        }
    }

    /// Register a skill.
    pub fn register(&mut self, skill: Skill) {
        self.skills.insert(skill.name.clone(), skill);
    }

    /// Get a skill by name.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    /// List all skill names.
    pub fn names(&self) -> Vec<String> {
        self.skills.keys().cloned().collect()
    }

    /// Number of registered skills.
    pub fn count(&self) -> usize {
        self.skills.len()
    }

    /// Register built-in skills.
    pub fn register_builtins(&mut self) {
        self.register(Skill {
            name: "summarize".to_string(),
            description: "Summarize the given text concisely".to_string(),
            template: "Please summarize the following text concisely:\n\n{{text}}".to_string(),
            parameters: vec![SkillParameter {
                name: "text".to_string(),
                description: "Text to summarize".to_string(),
                required: true,
                default: None,
            }],
        });

        self.register(Skill {
            name: "code_review".to_string(),
            description: "Review code for bugs, style, and improvements".to_string(),
            template: "Review the following {{language}} code for bugs, style issues, and potential improvements:\n\n```{{language}}\n{{code}}\n```".to_string(),
            parameters: vec![
                SkillParameter {
                    name: "code".to_string(),
                    description: "Code to review".to_string(),
                    required: true,
                    default: None,
                },
                SkillParameter {
                    name: "language".to_string(),
                    description: "Programming language".to_string(),
                    required: false,
                    default: Some("rust".to_string()),
                },
            ],
        });

        self.register(Skill {
            name: "research".to_string(),
            description: "Research a topic and provide a comprehensive answer".to_string(),
            template: "Research the following topic thoroughly and provide a comprehensive answer with sources where possible:\n\nTopic: {{topic}}\n\nFocus areas: {{focus}}".to_string(),
            parameters: vec![
                SkillParameter {
                    name: "topic".to_string(),
                    description: "Research topic".to_string(),
                    required: true,
                    default: None,
                },
                SkillParameter {
                    name: "focus".to_string(),
                    description: "Specific areas to focus on".to_string(),
                    required: false,
                    default: Some("general overview".to_string()),
                },
            ],
        });

        self.register(Skill {
            name: "translate".to_string(),
            description: "Translate text to a target language".to_string(),
            template: "Translate the following text to {{target_language}}. Preserve the original meaning, tone, and formatting:\n\n{{text}}".to_string(),
            parameters: vec![
                SkillParameter {
                    name: "text".to_string(),
                    description: "Text to translate".to_string(),
                    required: true,
                    default: None,
                },
                SkillParameter {
                    name: "target_language".to_string(),
                    description: "Target language".to_string(),
                    required: true,
                    default: None,
                },
            ],
        });
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    #[error("Missing required parameter '{parameter}' for skill '{skill}'")]
    MissingParameter { skill: String, parameter: String },

    #[error("Skill not found: {0}")]
    NotFound(String),

    #[error("Skill rendering failed: {0}")]
    RenderFailed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_simple_template() {
        let skill = Skill {
            name: "greet".to_string(),
            description: "Greet someone".to_string(),
            template: "Hello, {{name}}!".to_string(),
            parameters: vec![SkillParameter {
                name: "name".to_string(),
                description: "Name".to_string(),
                required: true,
                default: None,
            }],
        };

        let mut params = HashMap::new();
        params.insert("name".to_string(), "World".to_string());

        let result = skill.render(&params).unwrap();
        assert_eq!(result, "Hello, World!");
    }

    #[test]
    fn render_with_default() {
        let skill = Skill {
            name: "greet".to_string(),
            description: "Greet".to_string(),
            template: "Hello, {{name}}!".to_string(),
            parameters: vec![SkillParameter {
                name: "name".to_string(),
                description: "Name".to_string(),
                required: false,
                default: Some("World".to_string()),
            }],
        };

        let result = skill.render(&HashMap::new()).unwrap();
        assert_eq!(result, "Hello, World!");
    }

    #[test]
    fn render_missing_required() {
        let skill = Skill {
            name: "greet".to_string(),
            description: "Greet".to_string(),
            template: "Hello, {{name}}!".to_string(),
            parameters: vec![SkillParameter {
                name: "name".to_string(),
                description: "Name".to_string(),
                required: true,
                default: None,
            }],
        };

        let result = skill.render(&HashMap::new());
        assert!(result.is_err());
    }

    #[test]
    fn registry_builtins() {
        let mut reg = SkillRegistry::new();
        reg.register_builtins();

        assert_eq!(reg.count(), 4);
        assert!(reg.get("summarize").is_some());
        assert!(reg.get("code_review").is_some());
        assert!(reg.get("research").is_some());
        assert!(reg.get("translate").is_some());
    }

    #[test]
    fn code_review_skill_renders() {
        let mut reg = SkillRegistry::new();
        reg.register_builtins();

        let skill = reg.get("code_review").unwrap();
        let mut params = HashMap::new();
        params.insert("code".to_string(), "fn main() {}".to_string());

        let result = skill.render(&params).unwrap();
        assert!(result.contains("fn main() {}"));
        assert!(result.contains("rust")); // default language
    }
}
