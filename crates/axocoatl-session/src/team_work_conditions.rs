use super::*;
use crate::execution_content::{ExecutionContentError, RepositoryCheckDefinition};
use crate::turn_checks::CheckGroup;

/// Standing work's capture, commands and capture; see
/// [`crate::turn_checks::check_definitions`].
pub fn standing_check_definitions(
    checks: &[Vec<String>],
) -> Result<Vec<RepositoryCheckDefinition>, TeamWorkError> {
    crate::turn_checks::check_definitions(checks).map_err(|error| match error {
        ExecutionContentError::Capacity => TeamWorkError::Capacity,
        error => TeamWorkError::Invalid(error.to_string()),
    })
}
pub fn standing_condition_id(receipt: &str, index: usize) -> String {
    CheckGroup::standing(receipt).condition_id(index)
}
pub fn standing_readiness_text(receipt: &str, checks: &[Vec<String>]) -> String {
    serde_json::json!({"kind":"standing_candidate_readiness","receipt":receipt,"required_checks":checks,"rule":"all exact checks pass and the captured repository tree remains unchanged"}).to_string()
}
