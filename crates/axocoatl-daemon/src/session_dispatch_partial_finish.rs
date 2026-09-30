//! Human-only partial finalization over the existing closing/settlement owner.
use super::*;
use axocoatl_session::control_command::FinishMode;

pub(super) fn partial_finish_selection(
    contract: &TurnContract,
) -> Result<HumanPartialFinishSelection> {
    let graph = contract
        .graph()
        .ok_or_else(|| error("turn graph is unavailable"))?;
    Ok(HumanPartialFinishSelection {
        selected_activations: vec![],
        stop_activations: contract
            .activations()
            .iter()
            .filter(|item| item.state == ActivationState::Running)
            .map(|item| item.activation.clone())
            .collect(),
        missing_conditions: graph
            .conditions
            .iter()
            .filter(|condition| !contract.condition_satisfied(&condition.condition_id))
            .map(|condition| condition.condition_id.clone())
            .collect(),
        unrun_nodes: graph
            .nodes
            .iter()
            .filter(|node| {
                !contract.activations().iter().any(|item| {
                    item.activation.node_id == node.node_id
                        && item.state != ActivationState::Unstarted
                })
            })
            .map(|node| node.node_id.clone())
            .collect(),
        confirmed: true,
    })
}

pub(super) fn validate_partial_finish_selection(
    selected: &HumanPartialFinishSelection,
    offered: &HumanPartialFinishSelection,
) -> Result<()> {
    if !selected.confirmed
        || selected.stop_activations != offered.stop_activations
        || selected.missing_conditions != offered.missing_conditions
        || selected.unrun_nodes != offered.unrun_nodes
    {
        return Err(error(
            "partial Finish review changed; refresh the exact work and checks before confirming",
        ));
    }
    Ok(())
}

impl DispatchState {
    pub(in super::super) fn validate_partial_finish_control(
        &self,
        view: &CommandReceiptView,
        preview: bool,
    ) -> Result<()> {
        let ControlParameters::FinishTurn {
            mode: FinishMode::ForcePartial { approval, .. },
        } = &view.request.parameters
        else {
            return Err(error("partial Finish parameters are absent"));
        };
        let CommandSourceRecord::Human {
            request_evidence, ..
        } = &view.source
        else {
            return Err(error(
                "only an authenticated human may finish a partial result",
            ));
        };
        if self.is_isolated_ways()? {
            return Err(error(
                "Explored ways are finalized through Keep this one or no selection",
            ));
        }
        if request_evidence != approval {
            return Err(error(
                "partial Finish approval must be its exact human request",
            ));
        }
        if !preview {
            let ActivationEvidenceContent::Guidance { text } = self
                .content
                .resolve_activation_evidence(approval)
                .map_err(error)?
            else {
                return Err(error("partial Finish approval has the wrong evidence role"));
            };
            let original = HumanControlActionRequest::decode(text.as_bytes())?;
            let selection = original
                .partial_finish
                .as_ref()
                .ok_or_else(|| error("partial Finish has no explicit human selection"))?;
            let snapshot = self.canonical.snapshot(&self.turn_id).map_err(error)?;
            validate_partial_finish_selection(
                selection,
                &partial_finish_selection(snapshot.contract())?,
            )?;
            let rebuilt = self.extended_human_parameters(&original, approval, None, false)?;
            if original.command_id != view.request.command_id
                || original.session_id != view.request.session_id
                || original.turn_id != view.request.turn_id
                || original.execution_epoch_id != view.request.execution_epoch_id
                || original.expected_turn_revision != view.request.expected_turn_revision
                || original.expected_graph_revision != view.request.expected_graph_revision
                || rebuilt != view.request.parameters
            {
                return Err(error(
                    "partial Finish differs from its retained human approval",
                ));
            }
        }
        Ok(())
    }
}
