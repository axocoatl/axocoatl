#[derive(Debug, thiserror::Error)]
pub enum IsolationError {
    #[error("OCI container setup failed: {0}")]
    OciSetupFailed(String),

    #[error("OCI container execution failed: {0}")]
    OciContainerFailed(String),

    #[error("Tool execution timed out after {0:?}")]
    Timeout(std::time::Duration),

    /// A Podman command did not answer within its bound: Podman is slow (a
    /// busy computer, a machine still starting, another Podman command
    /// holding its storage), not failing, and may answer when asked again.
    #[error("Podman did not answer `podman {command}` within {} s", seconds(.after))]
    PodmanNotAnswering {
        command: String,
        after: std::time::Duration,
    },

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("E2B sandbox error: {0}")]
    E2b(String),
}

impl IsolationError {
    /// `podman {command}` did not answer within `after`.
    pub fn podman_not_answering(command: &str, after: std::time::Duration) -> Self {
        Self::PodmanNotAnswering {
            command: command.to_string(),
            after,
        }
    }

    /// Whether this is [`IsolationError::PodmanNotAnswering`]: asking again
    /// may succeed.
    pub fn is_podman_not_answering(&self) -> bool {
        matches!(self, Self::PodmanNotAnswering { .. })
    }
}

/// `duration` in seconds, to a tenth: `10`, `0.5`.
fn seconds(duration: &std::time::Duration) -> f64 {
    (duration.as_secs_f64() * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A Podman command that did not answer names itself and its bound in
    /// seconds, never "Tool execution timed out".
    #[test]
    fn a_podman_that_did_not_answer_names_the_command_and_its_bound() {
        let error = IsolationError::podman_not_answering("ps", Duration::from_secs(10));
        assert!(error.is_podman_not_answering());
        assert_eq!(
            error.to_string(),
            "Podman did not answer `podman ps` within 10 s"
        );
        let error =
            IsolationError::podman_not_answering("container exists", Duration::from_millis(1500));
        assert_eq!(
            error.to_string(),
            "Podman did not answer `podman container exists` within 1.5 s"
        );
        assert!(!IsolationError::Timeout(Duration::from_secs(10)).is_podman_not_answering());
    }
}
