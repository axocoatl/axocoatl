use super::*;
use crate::execution_content::{RepositoryCheckDefinition, REPOSITORY_SNAPSHOT_COMMAND};

/// The existing foreground command lifetime/capture ceilings apply. Arming
/// selects exact argv; this adds no tool capability, cost, or token grant.
pub fn standing_check_definitions(
    checks: &[Vec<String>],
) -> Result<Vec<RepositoryCheckDefinition>, TeamWorkError> {
    if checks.is_empty() {
        return Ok(Vec::new());
    }
    if checks.len().saturating_add(3) > crate::turn_contract::MAX_COMPLETION_CONDITIONS {
        return Err(TeamWorkError::Capacity);
    }
    let capture = RepositoryCheckDefinition {
        argv: vec!["sh".into(), "-c".into(), REPOSITORY_SNAPSHOT_COMMAND.into()],
        timeout_ms: 180_000,
        stdout_bytes: 768 * 1024,
        stderr_bytes: 256 * 1024,
    };
    let mut definitions = vec![capture.clone()];
    for argv in checks {
        crate::execution_content::standing_check_command(argv)
            .map_err(|error| TeamWorkError::Invalid(error.to_string()))?;
        definitions.push(RepositoryCheckDefinition {
            argv: argv.clone(),
            timeout_ms: 180_000,
            stdout_bytes: 768 * 1024,
            stderr_bytes: 256 * 1024,
        });
    }
    definitions.push(capture);
    Ok(definitions)
}
pub fn standing_condition_id(receipt: &str, index: usize) -> String {
    format!("standing:{receipt}:{index}")
}
pub fn standing_readiness_text(receipt: &str, checks: &[Vec<String>]) -> String {
    serde_json::json!({"kind":"standing_candidate_readiness","receipt":receipt,"required_checks":checks,"rule":"all exact checks pass and the captured repository tree remains unchanged"}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn standing_capture_and_commands_fit_the_actual_condition_store_bound() {
        use crate::execution_content::ExecutionContentStore;
        use crate::execution_namespace::ExecutionComponent;
        use crate::execution_ownership::LegacyFormatOwnership;
        use crate::execution_store::{ExecutionStoreOwner, SessionExecutionStore};
        let root = tempfile::tempdir().unwrap();
        let canonical = SessionExecutionStore::open(
            std::sync::Arc::new(
                LegacyFormatOwnership::acquire(root.path())
                    .unwrap()
                    .upgrade()
                    .unwrap(),
            ),
            ExecutionStoreOwner {
                workspace_id: "checks".into(),
                session_id: crate::turn_contract::SessionId::new("session").unwrap(),
            },
        )
        .unwrap();
        let mut content = ExecutionContentStore::open_owned(
            canonical
                .component_namespace(ExecutionComponent::ExecutionContent)
                .unwrap(),
        )
        .unwrap();
        let commands = vec![vec!["cargo".into(), "test".into(), "--quiet".into()]];
        let definitions = standing_check_definitions(&commands).unwrap();
        assert_eq!(definitions.len(), 3);
        assert_eq!(definitions[1].argv, commands[0]);
        for definition in definitions {
            assert_eq!(
                definition.stdout_bytes + definition.stderr_bytes,
                1024 * 1024
            );
            // Full 512 KiB patch in base64, 8 KiB manifest prefix and metadata.
            assert!(definition.stdout_bytes >= (512 * 1024 * 4 / 3) + 16384);
            content
                .retain_repository_check_definition(definition)
                .unwrap();
        }
    }
}
