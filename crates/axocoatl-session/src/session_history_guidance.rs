//! Read-only guidance receipts joined to their exact canonical amendment.
use sha2::{Digest, Sha256};

use crate::control_command::{
    CommandReceiptView, CommandSourceRecord, ControlCommandState, ControlParameters,
    ControlTransition, SteerMode,
};
use crate::execution_content::{
    ContentResolution, ExecutionActivationView, ExecutionTurnView, GuidanceDelivery,
};
use crate::execution_store::SessionExecutionStore;
use crate::turn_contract::{
    ActivationGuidance, EvidenceRef, TurnContractEnvelope, TurnContractEvent,
};

pub(super) fn join_delivery(
    canonical: &SessionExecutionStore,
    view: &mut ExecutionTurnView,
    receipts: Option<&[CommandReceiptView]>,
) {
    let identity = canonical.identity();
    let records = canonical.records();
    for activation in &mut view.activations {
        for item in &mut activation.guidance {
            item.delivery = match (&identity, &records, receipts) {
                (Ok(identity), Ok(records), Some(receipts)) => {
                    delivery(&item.amendment, identity.journal_id(), records, receipts)
                }
                _ => unknown(
                    "The exact owned command receipt is unavailable; actor delivery is unknown.",
                ),
            };
        }
    }
}

fn unknown(reason: &str) -> GuidanceDelivery {
    GuidanceDelivery::Unknown {
        reason: reason.into(),
    }
}

/// Match the host's versioned evidence format, including the canonical journal
/// incarnation. IDs and a terminal label alone cannot establish this delivery.
fn evidence(kind: &str, journal_id: &str, value: &impl serde::Serialize) -> Option<EvidenceRef> {
    let bytes = serde_json::to_vec(&("control-evidence-v1", journal_id, kind, value)).ok()?;
    EvidenceRef::new(format!("control-{kind}-{:x}", Sha256::digest(bytes))).ok()
}

fn delivery(
    amendment: &ActivationGuidance,
    journal_id: &str,
    records: &[TurnContractEnvelope],
    receipts: &[CommandReceiptView],
) -> GuidanceDelivery {
    let Some(receipt) = receipts
        .iter()
        .find(|view| view.request.command_id == amendment.control_command_id)
    else {
        return unknown("The amendment's command receipt is missing; actor delivery is unknown.");
    };
    let exact = &amendment.activation;
    let matching_source = matches!(&receipt.source, CommandSourceRecord::Human { session_id, turn_id, .. }
        if session_id == &exact.session_id && turn_id == &exact.turn_id)
        || matches!(&receipt.source, CommandSourceRecord::Agent { activation, .. } if activation.session_id == exact.session_id && activation.turn_id == exact.turn_id);
    let matching_action = matches!(&receipt.request.parameters,
        ControlParameters::SteerActivation { activation, instruction, mode: SteerMode::NextSafeBoundary }
        if activation == exact && instruction == &amendment.instruction);
    let request_evidence = evidence(
        "request",
        journal_id,
        &serde_json::json!({"request": receipt.request, "source": receipt.source}),
    );
    if !matching_source
        || !matching_action
        || receipt.request.session_id != exact.session_id
        || receipt.request.turn_id != exact.turn_id
        || receipt.request.execution_epoch_id != exact.execution_epoch_id
        || request_evidence.as_ref() != Some(&amendment.request)
    {
        return unknown(
            "The command does not match this exact canonical amendment; actor delivery is unknown.",
        );
    }
    let Some(event) = records.iter().find(|event| {
        event.session_id == exact.session_id
            && event.turn_id == exact.turn_id
            && matches!(&event.event, TurnContractEvent::ApplyGuidance {
            activation, control_command_id, instruction, request,
        } if activation == exact && control_command_id == &amendment.control_command_id
            && instruction == &amendment.instruction && request == &amendment.request)
    }) else {
        return unknown(
            "The exact canonical guidance event is unavailable; actor delivery is unknown.",
        );
    };
    let transition_evidence = evidence("canonical", journal_id, event);
    match (&receipt.state, &receipt.last_transition) {
        (ControlCommandState::Settled, Some(ControlTransition::Settled { result }))
            if transition_evidence.as_ref() == Some(result) =>
        {
            GuidanceDelivery::Delivered {
                receipt_revision: receipt.revision,
            }
        }
        (
            ControlCommandState::Applied,
            Some(ControlTransition::Applied {
                state_transition, ..
            }),
        ) if transition_evidence.as_ref() == Some(state_transition) => {
            GuidanceDelivery::HandoffRecorded
        }
        _ => unknown(
            "The retained receipt does not confirm actor delivery; the handoff remains recorded.",
        ),
    }
}

/// Shared by the Session export and the daemon's versioned Markdown consumer.
pub fn append_guidance_markdown(markdown: &mut String, activation: &ExecutionActivationView) {
    for item in &activation.guidance {
        markdown.push_str(&format!("Safe-boundary guidance (recorded handoff):\n\nCommand: `{}`; instruction: `{}`; request evidence: `{}`.\n\n",
            item.amendment.control_command_id.as_str(), item.amendment.instruction.as_str(), item.amendment.request.as_str()));
        match &item.instruction {
            ContentResolution::Available { content, .. } if content.is_empty() => {
                markdown.push_str("Recorded empty instruction.\n\n")
            }
            ContentResolution::Available { content, .. } => {
                if let Some(instruction) = presented_instruction(content) {
                    markdown.push_str(&format!("{instruction}\n\n<details><summary>Retained instruction and context</summary>\n\n```json\n{content}\n```\n\n</details>\n\n"));
                } else {
                    markdown.push_str(&format!("{content}\n\n"));
                }
            }
            ContentResolution::Missing { reference } => markdown.push_str(&format!(
                "Instruction body is missing: `{}`.\n\n",
                reference.as_str()
            )),
            ContentResolution::NotRecorded => {
                markdown.push_str("Instruction body was not recorded.\n\n")
            }
        }
        match &item.delivery {
            GuidanceDelivery::HandoffRecorded => markdown.push_str("Handoff recorded; actor input append has not been confirmed.\n\n"),
            GuidanceDelivery::Delivered { receipt_revision } => markdown.push_str(&format!("Actor input append acknowledged (receipt revision {receipt_revision}). This does not establish provider consumption.\n\n")),
            GuidanceDelivery::Unknown { reason } => markdown.push_str(&format!("Delivery unknown: {reason}\n\n")),
        }
    }
}

fn presented_instruction(content: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(content).ok()?;
    let object = value.as_object()?;
    if object.len() != 5
        || value.get("kind")?.as_str()? != "authenticated_control_context_v1"
        || !value.get("references")?.is_array()
        || !value.get("attachments")?.is_array()
        || !value.get("original")?.get("references")?.is_array()
        || !value.get("original")?.get("attachment_ids")?.is_array()
    {
        return None;
    }
    Some(value.get("instruction")?.as_str()?.to_owned())
}

#[cfg(test)]
mod presentation_tests {
    #[test]
    fn only_exact_typed_instruction_marker_is_presented() {
        let body = serde_json::json!({"kind":"authenticated_control_context_v1","instruction":"Review this image", "original":{"references":[],"attachment_ids":["image"]},"references":[],"attachments":["retained-image"]});
        assert_eq!(
            super::presented_instruction(&body.to_string()).as_deref(),
            Some("Review this image")
        );
        assert_eq!(
            super::presented_instruction("{\"instruction\":\"ordinary user JSON\"}"),
            None
        );
        let mut future = body;
        future["kind"] = "future".into();
        assert_eq!(super::presented_instruction(&future.to_string()), None);
    }
}
