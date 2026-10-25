#[derive(Debug, thiserror::Error)]
pub enum IsolationError {
    #[error("OCI container setup failed: {0}")]
    OciSetupFailed(String),

    #[error("OCI container execution failed: {0}")]
    OciContainerFailed(String),

    #[error("Tool execution timed out after {0:?}")]
    Timeout(std::time::Duration),

    /// A Podman command did not answer within its bound: Podman is slow (a
    /// busy computer, another Podman command holding its storage), not
    /// failing, and may answer when asked again. `through`: the
    /// `CONTAINER_HOST=…` or `CONTAINER_CONNECTION=…` it was asked through.
    #[error("{}", not_answering(command, after, through.as_deref()))]
    PodmanNotAnswering {
        command: String,
        after: std::time::Duration,
        through: Option<String>,
    },

    /// `podman machine list` says the machine is still starting (Podman sets
    /// `Starting` from before the VM starts until its API is forwarded), and
    /// Podman refused a request meanwhile: it may answer once started.
    #[error(
        "the Podman machine `{machine}` is still starting (`podman machine list` says so), and \
         Podman refused a request meanwhile: {error}"
    )]
    PodmanMachineStarting { machine: String, error: String },

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
            through: None,
        }
    }

    /// This error, asked through `described` (`CONTAINER_HOST=…`), when it
    /// is [`IsolationError::PodmanNotAnswering`].
    pub fn through(self, described: Option<String>) -> Self {
        match self {
            Self::PodmanNotAnswering { command, after, .. } => Self::PodmanNotAnswering {
                command,
                after,
                through: described,
            },
            other => other,
        }
    }

    /// Whether Podman did not answer yet ([`IsolationError::PodmanNotAnswering`]
    /// or [`IsolationError::PodmanMachineStarting`]): asking again may
    /// succeed.
    pub fn is_podman_not_answering(&self) -> bool {
        matches!(
            self,
            Self::PodmanNotAnswering { .. } | Self::PodmanMachineStarting { .. }
        )
    }
}

/// What [`IsolationError::PodmanNotAnswering`] says.
fn not_answering(command: &str, after: &std::time::Duration, through: Option<&str>) -> String {
    let seconds = seconds(after);
    match through {
        Some(described) => format!(
            "{described} is set, and Podman did not answer `podman {command}` through it within \
             {seconds} s"
        ),
        None => format!("Podman did not answer `podman {command}` within {seconds} s"),
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
        // Asked through a connection the environment selects, it says so.
        let through = IsolationError::podman_not_answering("info", Duration::from_secs(15))
            .through(Some(
                "CONTAINER_HOST=unix:///run/user/2000/podman/podman.sock".into(),
            ));
        assert!(through.is_podman_not_answering());
        assert_eq!(
            through.to_string(),
            "CONTAINER_HOST=unix:///run/user/2000/podman/podman.sock is set, and Podman did not \
             answer `podman info` through it within 15 s"
        );
        // A machine still starting may answer later too.
        let starting = IsolationError::PodmanMachineStarting {
            machine: "podman-machine-default".into(),
            error: "podman info failed: connection refused".into(),
        };
        assert!(starting.is_podman_not_answering());
        assert_eq!(
            starting.to_string(),
            "the Podman machine `podman-machine-default` is still starting (`podman machine \
             list` says so), and Podman refused a request meanwhile: podman info failed: \
             connection refused"
        );
    }
}
