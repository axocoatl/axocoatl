use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("Config parse error in {path:?}: {reason}\nSuggestion: {suggestion}")]
    ParseError {
        path: PathBuf,
        reason: String,
        suggestion: String,
    },

    #[error("Invalid value for field '{field}': {value}\nReason: {reason}\nFix: {suggestion}")]
    InvalidField {
        field: String,
        value: String,
        reason: String,
        suggestion: String,
    },

    #[error("Duplicate ID '{id}' in {field}.\nFix: Each {field} must have a unique 'id' value.")]
    DuplicateId { field: String, id: String },

    #[error("Unknown provider: '{provider}'.\nValid providers: openai, anthropic, gemini, ollama, mistral.\nFix: Change provider to one of the valid values above.")]
    UnknownProvider { provider: String },

    #[error("IO error reading config: {0}")]
    Io(#[from] std::io::Error),
}
