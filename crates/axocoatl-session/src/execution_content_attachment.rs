//! Immutable attachment bytes and the exact presentation selected at Begin.
//! The digest describes source bytes; extracted text is a distinct projection.

use super::*;
use axocoatl_core::AgentAttachment;
use base64::{engine::general_purpose::STANDARD, Engine};

// The existing browser upload ceiling. Aggregate content/checkpoint/provider
// representation limits can still reject a set of otherwise valid attachments.
const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;
const MAX_ENCODED_BYTES: usize = MAX_ATTACHMENT_BYTES.div_ceil(3) * 4;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedBinaryAttachment {
    reference_id: String,
    name: String,
    media_type: String,
    source_sha256: String,
    byte_len: u64,
    bytes_base64: String,
    extracted_text: Option<String>,
}

impl ActivationEvidenceContent {
    pub fn from_attachment(source: &AgentAttachment) -> Result<Self, ExecutionContentError> {
        if source.bytes.len() > MAX_ATTACHMENT_BYTES || source.size != source.bytes.len() as u64 {
            return Err(ExecutionContentError::Invalid(
                "attachment source size differs or exceeds its byte limit",
            ));
        }
        let attachment = RetainedBinaryAttachment {
            reference_id: source.id.clone(),
            name: source.name.clone(),
            media_type: source.mime.clone(),
            source_sha256: sha256(&source.bytes),
            byte_len: source.size,
            bytes_base64: STANDARD.encode(&source.bytes),
            extracted_text: source.extracted_text.clone(),
        };
        attachment.validate()?;
        Ok(Self::BinaryAttachment { attachment })
    }
}

impl RetainedBinaryAttachment {
    pub fn reference_id(&self) -> &str {
        &self.reference_id
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn media_type(&self) -> &str {
        &self.media_type
    }
    pub fn source_sha256(&self) -> &str {
        &self.source_sha256
    }
    pub fn byte_len(&self) -> u64 {
        self.byte_len
    }
    pub fn extracted_text(&self) -> Option<&str> {
        self.extracted_text.as_deref()
    }

    fn bytes(&self) -> Result<Vec<u8>, ExecutionContentError> {
        if self.byte_len > MAX_ATTACHMENT_BYTES as u64
            || self.bytes_base64.len() > MAX_ENCODED_BYTES
        {
            return Err(ExecutionContentError::Capacity);
        }
        let bytes = STANDARD
            .decode(&self.bytes_base64)
            .map_err(|_| ExecutionContentError::Invalid("attachment source encoding is invalid"))?;
        if bytes.len() as u64 != self.byte_len || sha256(&bytes) != self.source_sha256 {
            return Err(ExecutionContentError::Invalid(
                "attachment source digest or size differs",
            ));
        }
        Ok(bytes)
    }

    pub(super) fn validate(&self) -> Result<(), ExecutionContentError> {
        bounded_name(&self.reference_id)?;
        bounded_name(&self.media_type)?;
        if self.name.is_empty()
            || self.name.len() > 512
            || self.name.chars().any(char::is_control)
            || self
                .extracted_text
                .as_ref()
                .is_some_and(|text| text.len() > MAX_TEXT)
        {
            return Err(ExecutionContentError::Invalid(
                "attachment presentation exceeds its limits",
            ));
        }
        self.bytes()?;
        Ok(())
    }

    pub fn to_attachment(&self) -> Result<AgentAttachment, ExecutionContentError> {
        self.validate()?;
        Ok(AgentAttachment {
            id: self.reference_id.clone(),
            name: self.name.clone(),
            mime: self.media_type.clone(),
            size: self.byte_len,
            bytes: self.bytes()?,
            extracted_text: self.extracted_text.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> AgentAttachment {
        AgentAttachment {
            id: "session-source".into(),
            name: "screenshot.png".into(),
            mime: "image/png".into(),
            bytes: vec![0x89, b'P', b'N', b'G', 0xff, 0],
            size: 6,
            extracted_text: Some("OCR is distinct".into()),
        }
    }

    #[test]
    fn retained_binary_round_trips_exact_bytes_and_refuses_changed_source() {
        let source = source();
        let evidence = ActivationEvidenceContent::from_attachment(&source).unwrap();
        let encoded = serde_json::to_vec(&evidence).unwrap();
        let decoded: ActivationEvidenceContent = serde_json::from_slice(&encoded).unwrap();
        validate_activation_evidence(&decoded).unwrap();
        let ActivationEvidenceContent::BinaryAttachment { mut attachment } = decoded else {
            panic!()
        };
        let restored = attachment.to_attachment().unwrap();
        assert_eq!(restored.bytes, source.bytes);
        assert_eq!(restored.id, source.id);
        assert_eq!(restored.mime, source.mime);
        assert_eq!(restored.extracted_text, source.extracted_text);
        attachment.bytes_base64 = STANDARD.encode(b"forged");
        assert!(attachment.to_attachment().is_err());
        attachment.bytes_base64 = STANDARD.encode(&source.bytes);
        attachment.byte_len += 1;
        assert!(attachment.validate().is_err());
    }

    #[test]
    fn source_size_and_retained_representation_are_bounded() {
        let mut source = source();
        source.size += 1;
        assert!(ActivationEvidenceContent::from_attachment(&source).is_err());
        source.size -= 1;
        source.extracted_text = Some("x".repeat(MAX_TEXT + 1));
        assert!(ActivationEvidenceContent::from_attachment(&source).is_err());
    }
}
