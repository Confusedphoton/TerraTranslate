use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type TimestampMillis = i64;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CommitId(pub String);

impl fmt::Display for CommitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventId(pub Uuid);

impl EventId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for EventId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    Text,
    Image,
    Audio,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    WineHook,
    NativeHook,
    WindowCapture,
    ApplicationAudio,
    Manual,
    Import,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayloadRef {
    /// BLAKE3 digest of the payload bytes.
    pub digest: String,
    pub media_type: String,
    pub byte_len: u64,
}

impl PayloadRef {
    pub fn from_bytes(media_type: impl Into<String>, bytes: &[u8]) -> Self {
        Self {
            digest: blake3::hash(bytes).to_hex().to_string(),
            media_type: media_type.into(),
            byte_len: bytes.len() as u64,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceEvent {
    pub id: EventId,
    pub captured_at_ms: TimestampMillis,
    pub modality: Modality,
    pub source: SourceKind,
    /// Stable target identity such as a Wine process id or portal stream id.
    pub target: String,
    pub payload: PayloadRef,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entity {
    pub display_name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub facts: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextSnapshot {
    pub summary: String,
    #[serde(default)]
    pub glossary: BTreeMap<String, String>,
    #[serde(default)]
    pub entities: BTreeMap<String, Entity>,
    pub style: String,
    pub scratchpad: String,
}

/// A model-facing entry from a branch's accumulated context history.
///
/// The snapshot is retained at every point in the history so an endless-context
/// request can reconstruct the full evolution of the branch without exposing the
/// storage commit envelope to a provider.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextHistoryEntry {
    pub source_text: String,
    pub translated_text: String,
    pub context: ContextSnapshot,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScratchpadAuthor {
    Model,
    User,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScratchpadEdit {
    pub author: ScratchpadAuthor,
    pub at_ms: TimestampMillis,
    pub previous_digest: String,
    pub new_digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessorTrace {
    pub processor_id: String,
    pub processor_version: String,
    pub input_digest: String,
    pub output_digest: String,
    pub elapsed_micros: u64,
    /// Source-selection context for this invocation. This keeps per-hook processor
    /// choices auditable without changing processor implementations.
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelMetadata {
    pub provider: String,
    pub model: String,
    pub request_id: Option<String>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranslationCommit {
    pub id: CommitId,
    /// Zero parents for a root, one for an ordinary turn, two for a merge.
    pub parents: Vec<CommitId>,
    pub created_at_ms: TimestampMillis,
    #[serde(default)]
    pub source_events: Vec<SourceEvent>,
    pub source_text: String,
    pub translated_text: String,
    pub context: ContextSnapshot,
    #[serde(default)]
    pub scratchpad_edits: Vec<ScratchpadEdit>,
    #[serde(default)]
    pub processor_trace: Vec<ProcessorTrace>,
    pub model: ModelMetadata,
    #[serde(default)]
    pub message: String,
}

#[derive(Serialize)]
struct CommitContent<'a> {
    parents: &'a [CommitId],
    created_at_ms: TimestampMillis,
    source_events: &'a [SourceEvent],
    source_text: &'a str,
    translated_text: &'a str,
    context: &'a ContextSnapshot,
    scratchpad_edits: &'a [ScratchpadEdit],
    processor_trace: &'a [ProcessorTrace],
    model: &'a ModelMetadata,
    message: &'a str,
}

#[derive(Debug, thiserror::Error)]
pub enum CommitError {
    #[error("a commit may have at most two parents")]
    TooManyParents,
    #[error("a commit cannot list the same parent twice")]
    DuplicateParent,
    #[error("failed to encode commit: {0}")]
    Encoding(#[from] serde_json::Error),
}

impl TranslationCommit {
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        parents: Vec<CommitId>,
        created_at_ms: TimestampMillis,
        source_events: Vec<SourceEvent>,
        source_text: String,
        translated_text: String,
        context: ContextSnapshot,
        scratchpad_edits: Vec<ScratchpadEdit>,
        processor_trace: Vec<ProcessorTrace>,
        model: ModelMetadata,
        message: String,
    ) -> Result<Self, CommitError> {
        if parents.len() > 2 {
            return Err(CommitError::TooManyParents);
        }
        if parents.len() == 2 && parents[0] == parents[1] {
            return Err(CommitError::DuplicateParent);
        }

        let content = CommitContent {
            parents: &parents,
            created_at_ms,
            source_events: &source_events,
            source_text: &source_text,
            translated_text: &translated_text,
            context: &context,
            scratchpad_edits: &scratchpad_edits,
            processor_trace: &processor_trace,
            model: &model,
            message: &message,
        };
        let canonical = serde_json::to_vec(&content)?;
        let id = CommitId(blake3::hash(&canonical).to_hex().to_string());

        Ok(Self {
            id,
            parents,
            created_at_ms,
            source_events,
            source_text,
            translated_text,
            context,
            scratchpad_edits,
            processor_trace,
            model,
            message,
        })
    }

    pub fn verify_id(&self) -> Result<bool, CommitError> {
        let rebuilt = Self::create(
            self.parents.clone(),
            self.created_at_ms,
            self.source_events.clone(),
            self.source_text.clone(),
            self.translated_text.clone(),
            self.context.clone(),
            self.scratchpad_edits.clone(),
            self.processor_trace.clone(),
            self.model.clone(),
            self.message.clone(),
        )?;
        Ok(rebuilt.id == self.id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchRef {
    pub name: String,
    pub head: CommitId,
    pub updated_at_ms: TimestampMillis,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_ids_are_content_addressed() {
        let make = || {
            TranslationCommit::create(
                vec![],
                42,
                vec![],
                "こんにちは".into(),
                "Hello".into(),
                ContextSnapshot::default(),
                vec![],
                vec![],
                ModelMetadata::default(),
                "root".into(),
            )
            .unwrap()
        };
        let first = make();
        let second = make();
        assert_eq!(first.id, second.id);
        assert!(first.verify_id().unwrap());
    }

    #[test]
    fn rejects_duplicate_merge_parent() {
        let parent = CommitId("parent".into());
        let result = TranslationCommit::create(
            vec![parent.clone(), parent],
            0,
            vec![],
            String::new(),
            String::new(),
            ContextSnapshot::default(),
            vec![],
            vec![],
            ModelMetadata::default(),
            String::new(),
        );
        assert!(matches!(result, Err(CommitError::DuplicateParent)));
    }
}
