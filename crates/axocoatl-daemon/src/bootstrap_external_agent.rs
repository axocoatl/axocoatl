//! External agent activations on the live daemon: run the program in the
//! Session container as the writer user under `--harden`, with the run's
//! routes, and record its parsed output as the activation's evidence.
//! Owner: workstream `agents`.
//!
//! The activation path itself runs through the Session controller: the
//! factory from `native_session_activation_factory` gives an external
//! definition's activation a provider whose one model call is the program
//! run (`session_dispatch::external_port`). [`AxocoatlDaemon::
//! run_external_activation`] is the same run, addressed by Session, turn and
//! node. Also here: the secret store and recipe images as the daemon sees
//! them (`{data root}/secrets`, `{data root}/recipes`), for core's run
//! admission and Session start.
use super::*;
use crate::external_agent::recipe_images::{self, RecipeImageRecord};
use crate::external_agent::{ExternalActivationRequest, ExternalActivationResult};

impl AxocoatlDaemon {
    /// Run the external program of the running activation of
    /// `request.node_id` in `request.turn_id` of `request.session_id`, and
    /// return its parsed output. The activation's retained definition must
    /// name `request.runtime` and `request.model` and its write scope must be
    /// `request.writes`; the program runs only while the activation's
    /// admitted model call is open (its reservation is what the run may
    /// spend), which is how the activation's own provider calls it.
    pub async fn run_external_activation(
        &self,
        request: ExternalActivationRequest,
    ) -> Result<ExternalActivationResult, DaemonError> {
        let token = self
            .session_dispatch_lifecycles
            .session_team_token(&request.session_id)?;
        let controller = self
            .session_dispatch_lifecycles
            .with_session_team_controller(&token, |controller| Ok(controller.clone()))?;
        let snapshot = controller
            .snapshot()
            .map_err(|error| DaemonError::Session(error.to_string()))?;
        let activation = snapshot
            .contract()
            .activations()
            .iter()
            .find(|item| {
                item.activation.turn_id.as_str() == request.turn_id
                    && item.activation.node_id.as_str() == request.node_id
                    && item.state == axocoatl_session::turn_contract::ActivationState::Running
            })
            .map(|item| item.activation.clone())
            .ok_or_else(|| {
                DaemonError::Session(format!(
                    "node {} of turn {} has no running activation",
                    request.node_id, request.turn_id
                ))
            })?;
        let writes = snapshot
            .contract()
            .activations()
            .iter()
            .find(|item| item.activation == activation)
            .map(|item| item.input.definition.clone())
            .and_then(|definition| {
                self.session_dispatch_lifecycles
                    .with_session_team_stores(&token, |_, content, _| {
                        match content
                            .resolve_activation_evidence(&definition.snapshot)
                            .map_err(|error| DaemonError::Session(error.to_string()))?
                        {
                            axocoatl_session::execution_content::ActivationEvidenceContent::Definition {
                                profile,
                                ..
                            } => Ok(profile.write_scope.clone()),
                            _ => Err(DaemonError::Session(
                                "the activation's definition is not retained".into(),
                            )),
                        }
                    })
                    .ok()
            });
        if writes.as_ref() != Some(&request.writes) {
            return Err(DaemonError::Session(
                "the request's write scope differs from the activation's admitted definition"
                    .into(),
            ));
        }
        let outcome = controller
            .run_external_request(
                &activation,
                request.runtime,
                &request.model,
                request.prompt,
                (request.timeout_ms > 0).then_some(request.timeout_ms),
                &crate::session_dispatch::ExternalSettings::default(),
            )
            .await
            .map_err(|error| DaemonError::Session(error.to_string()))?;
        Ok(outcome.result)
    }

    /// The data root's secret store directory (`{data root}/secrets`), for
    /// `loadout::egress::loadout_policy` and `secret_store::credential_source`.
    pub fn secret_store_dir(&self) -> std::path::PathBuf {
        self.data_root.path().join(crate::secret_store::SECRETS_DIR)
    }

    /// The image built from exactly `recipes` on this computer (a loadout's
    /// `environment.recipes`): run its Session on the record's `image_id`.
    /// Refused, naming the `axocoatl recipe build` command, when none is.
    pub fn recipe_image(&self, recipes: &[String]) -> Result<RecipeImageRecord, DaemonError> {
        recipe_images::image_for_recipes(&self.data_root, recipes)
            .map_err(|error| DaemonError::Session(error.to_string()))
    }

    /// Whether `image` is exactly the image id of a recipe build recorded in
    /// this data root (see `external_agent::recipe_images`).
    pub fn trusted_recipe_image(&self, image: &str) -> bool {
        recipe_images::is_trusted_image(&self.data_root, image)
    }

    /// Whether a Session may start from `image`: the configured
    /// `sandbox.allow_untrusted_images`, or a recorded recipe image id. Core
    /// passes this where it passes `sandbox.allow_untrusted_images` for a
    /// Session's image (the Session sandbox policy and the image preflight),
    /// so a recipe image is trusted by its exact id the way the browser image
    /// is, and nothing else changes.
    pub fn session_image_trusted(&self, image: Option<&str>) -> bool {
        self.config.sandbox.allow_untrusted_images
            || image.is_some_and(|image| self.trusted_recipe_image(image))
    }
}

#[cfg(test)]
#[path = "bootstrap_external_turn_tests.rs"]
mod turn_tests;
