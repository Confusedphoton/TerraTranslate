//! End-to-end turn orchestration: persist inputs, process text, call a multimodal model, and
//! atomically advance a branch.

use std::sync::Arc;
use std::time::Instant;

use terratranslate_core::{
    ContextSnapshot, EventId, Modality, ModelMetadata, PayloadRef, ProcessorRequest,
    ProcessorStage, ProcessorTrace, ScratchpadAuthor, ScratchpadEdit, SourceEvent, SourceKind,
    TextProcessor, TranslationCommit,
};
use terratranslate_provider::{
    ModelInput, ModelProvider, ProviderError, TranslationRequest, validate_request,
};
use terratranslate_store::{SessionStore, StoreError};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextProcessingSelection {
    /// Processor IDs to run, in this order, before this text is sent to the model.
    pub pre_prompt: Vec<String>,
    /// Processor IDs to run, in this order, on the translation produced for this text.
    pub post_translation: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextInputOptions {
    /// Stable producer identity, independent of a connection UUID, PID, or ASLR.
    pub stable_hook_key: Option<String>,
    /// An optional user-facing name that is included with this input in the model request.
    pub label: Option<String>,
    pub processing: TextProcessingSelection,
}

pub struct TurnInput {
    pub captured_at_ms: i64,
    pub source: SourceKind,
    pub target: String,
    pub input: ModelInput,
    /// `None` retains the legacy behavior of running every registered processor.
    pub text_options: Option<TextInputOptions>,
}

pub struct TurnRequest {
    pub branch: String,
    pub created_at_ms: i64,
    pub system_prompt: String,
    pub source_language: Option<String>,
    pub target_language: String,
    pub inputs: Vec<TurnInput>,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("processor {id} failed: {message}")]
    Processor { id: String, message: String },
    #[error("commit could not be constructed: {0}")]
    Commit(String),
    #[error(
        "branch moved while a translation was running; result was preserved but not checked out"
    )]
    BranchMoved,
    #[error("translation turn contains no inputs")]
    EmptyTurn,
    #[error("text inputs in one model turn selected different post-processing pipelines")]
    IncompatiblePostProcessing,
}

pub struct TranslationEngine {
    store: SessionStore,
    provider: Arc<dyn ModelProvider>,
    processors: Vec<Arc<dyn TextProcessor>>,
}

impl TranslationEngine {
    pub fn new(store: SessionStore, provider: Arc<dyn ModelProvider>) -> Self {
        Self {
            store,
            provider,
            processors: Vec::new(),
        }
    }

    pub fn add_processor(&mut self, processor: Arc<dyn TextProcessor>) {
        self.processors.push(processor);
    }

    pub fn store(&self) -> &SessionStore {
        &self.store
    }

    pub async fn translate_turn(
        &mut self,
        turn: TurnRequest,
    ) -> Result<TranslationCommit, EngineError> {
        if turn.inputs.is_empty() {
            return Err(EngineError::EmptyTurn);
        }
        let branch = self.store.branch(&turn.branch)?;
        let parent = self.store.get_commit(&branch.head)?;
        let mut context = parent.context;
        let mut source_events = Vec::new();
        let mut model_inputs = Vec::new();
        let mut source_text_parts = Vec::new();
        let mut trace = Vec::new();
        let mut post_translation_selection: Option<Option<Vec<String>>> = None;
        let processors = self.processors.clone();

        for input in turn.inputs {
            let (modality, media_type, bytes) = match &input.input {
                ModelInput::Text(text) => (
                    Modality::Text,
                    "text/plain; charset=utf-8".to_owned(),
                    text.as_bytes().to_vec(),
                ),
                ModelInput::Image { media_type, bytes } => {
                    (Modality::Image, media_type.clone(), bytes.clone())
                }
                ModelInput::Audio { format, bytes } => {
                    (Modality::Audio, format!("audio/{format}"), bytes.clone())
                }
            };
            let digest = self.store.put_blob(&bytes)?;
            let payload = PayloadRef {
                digest,
                media_type,
                byte_len: bytes.len() as u64,
            };
            let mut metadata = std::collections::BTreeMap::new();
            if let Some(options) = &input.text_options {
                if let Some(stable_hook_key) = options
                    .stable_hook_key
                    .as_deref()
                    .map(str::trim)
                    .filter(|key| !key.is_empty())
                {
                    metadata.insert("stable_hook_key".into(), stable_hook_key.to_owned());
                }
                if let Some(label) = options
                    .label
                    .as_deref()
                    .map(str::trim)
                    .filter(|label| !label.is_empty())
                {
                    metadata.insert("text_hook_label".into(), label.to_owned());
                }
                metadata.insert(
                    "pre_prompt_processors".into(),
                    options.processing.pre_prompt.join(","),
                );
                metadata.insert(
                    "post_translation_processors".into(),
                    options.processing.post_translation.join(","),
                );
            }
            source_events.push(SourceEvent {
                id: EventId::new(),
                captured_at_ms: input.captured_at_ms,
                modality,
                source: input.source,
                target: input.target,
                payload,
                metadata,
            });

            match input.input {
                ModelInput::Text(text) => {
                    let selected_pre_processors = input
                        .text_options
                        .as_ref()
                        .map(|options| options.processing.pre_prompt.as_slice());
                    let processed = Self::run_stage(
                        &processors,
                        ProcessorStage::PrePrompt,
                        text,
                        &context,
                        &mut trace,
                        selected_pre_processors,
                        processor_metadata(input.text_options.as_ref()),
                    )
                    .await?;
                    source_text_parts.push(processed.clone());
                    model_inputs.push(ModelInput::Text(label_text_input(
                        input
                            .text_options
                            .as_ref()
                            .and_then(|options| options.label.as_deref()),
                        processed,
                    )));

                    let selected_post_processors = input
                        .text_options
                        .as_ref()
                        .map(|options| options.processing.post_translation.clone());
                    match &post_translation_selection {
                        Some(existing) if existing != &selected_post_processors => {
                            return Err(EngineError::IncompatiblePostProcessing);
                        }
                        None => post_translation_selection = Some(selected_post_processors),
                        _ => {}
                    }
                }
                other => model_inputs.push(other),
            }
        }

        let model_request = TranslationRequest {
            system_prompt: turn.system_prompt,
            inputs: model_inputs,
            source_language: turn.source_language,
            target_language: turn.target_language,
            context: context.clone(),
        };
        validate_request(self.provider.capabilities(), &model_request)?;
        let response = self.provider.translate(model_request).await?;
        let translated_text = Self::run_stage(
            &processors,
            ProcessorStage::PostTranslation,
            response.translated_text,
            &context,
            &mut trace,
            post_translation_selection
                .as_ref()
                .and_then(|selection| selection.as_deref()),
            aggregate_processor_metadata(&source_events),
        )
        .await?;

        let previous_scratchpad = context.scratchpad.clone();
        apply_patch(&mut context, response.context_patch);
        let scratchpad_edits = if context.scratchpad != previous_scratchpad {
            vec![ScratchpadEdit {
                author: ScratchpadAuthor::Model,
                at_ms: turn.created_at_ms,
                previous_digest: blake3::hash(previous_scratchpad.as_bytes())
                    .to_hex()
                    .to_string(),
                new_digest: blake3::hash(context.scratchpad.as_bytes())
                    .to_hex()
                    .to_string(),
            }]
        } else {
            vec![]
        };
        let commit = TranslationCommit::create(
            vec![branch.head.clone()],
            turn.created_at_ms,
            source_events,
            source_text_parts.join("\n"),
            translated_text,
            context,
            scratchpad_edits,
            trace,
            ModelMetadata {
                provider: self.provider.id().to_owned(),
                model: self.provider.model().to_owned(),
                request_id: response.request_id,
                prompt_tokens: response.prompt_tokens,
                completion_tokens: response.completion_tokens,
            },
            "Translate multimodal turn".into(),
        )
        .map_err(|error| EngineError::Commit(error.to_string()))?;
        self.store.put_commit(&commit)?;
        if !self
            .store
            .advance_branch(&turn.branch, &branch.head, &commit.id, turn.created_at_ms)?
        {
            return Err(EngineError::BranchMoved);
        }
        Ok(commit)
    }

    async fn run_stage(
        processors: &[Arc<dyn TextProcessor>],
        stage: ProcessorStage,
        mut text: String,
        context: &ContextSnapshot,
        trace: &mut Vec<ProcessorTrace>,
        selected_processor_ids: Option<&[String]>,
        metadata: std::collections::BTreeMap<String, String>,
    ) -> Result<String, EngineError> {
        let selected = match selected_processor_ids {
            Some(ids) => ids
                .iter()
                .filter_map(|id| processors.iter().find(|processor| processor.id() == id))
                .filter(|processor| processor.stages().contains(&stage))
                .collect::<Vec<_>>(),
            None => processors
                .iter()
                .filter(|processor| processor.stages().contains(&stage))
                .collect::<Vec<_>>(),
        };
        for processor in selected {
            let input_digest = blake3::hash(text.as_bytes()).to_hex().to_string();
            let started = Instant::now();
            let response = processor
                .process(ProcessorRequest {
                    stage,
                    text,
                    context: context.clone(),
                })
                .await
                .map_err(|error| EngineError::Processor {
                    id: processor.id().to_owned(),
                    message: error.to_string(),
                })?;
            text = response.text;
            trace.push(ProcessorTrace {
                processor_id: processor.id().to_owned(),
                processor_version: processor.version().to_owned(),
                input_digest,
                output_digest: blake3::hash(text.as_bytes()).to_hex().to_string(),
                elapsed_micros: started.elapsed().as_micros() as u64,
                metadata: metadata.clone(),
            });
        }
        Ok(text)
    }
}

fn processor_metadata(
    options: Option<&TextInputOptions>,
) -> std::collections::BTreeMap<String, String> {
    let mut metadata = std::collections::BTreeMap::new();
    let Some(options) = options else {
        return metadata;
    };
    if let Some(key) = options.stable_hook_key.as_deref() {
        metadata.insert("stable_hook_key".into(), key.to_owned());
    }
    if let Some(label) = options.label.as_deref() {
        metadata.insert("text_hook_label".into(), label.to_owned());
    }
    metadata.insert(
        "pre_prompt_processors".into(),
        options.processing.pre_prompt.join(","),
    );
    metadata.insert(
        "post_translation_processors".into(),
        options.processing.post_translation.join(","),
    );
    metadata
}

fn aggregate_processor_metadata(
    source_events: &[SourceEvent],
) -> std::collections::BTreeMap<String, String> {
    let mut metadata = std::collections::BTreeMap::new();
    for (source_name, event_name) in [
        ("stable_hook_key", "stable_hook_keys"),
        ("text_hook_label", "text_hook_labels"),
        ("pre_prompt_processors", "pre_prompt_processors"),
        ("post_translation_processors", "post_translation_processors"),
    ] {
        let values = source_events
            .iter()
            .filter_map(|event| event.metadata.get(source_name))
            .filter(|value| !value.is_empty())
            .cloned()
            .collect::<Vec<_>>();
        if !values.is_empty() {
            metadata.insert(event_name.into(), values.join(","));
        }
    }
    metadata
}

fn label_text_input(label: Option<&str>, text: String) -> String {
    let Some(label) = label.map(str::trim).filter(|label| !label.is_empty()) else {
        return text;
    };
    serde_json::json!({
        "text_hook_label": label,
        "text": text,
    })
    .to_string()
}

fn apply_patch(context: &mut ContextSnapshot, patch: terratranslate_provider::ContextPatch) {
    if let Some(summary) = patch.summary {
        context.summary = summary;
    }
    if let Some(glossary) = patch.glossary {
        context.glossary = glossary;
    }
    if let Some(entities) = patch.entities {
        context.entities = entities;
    }
    if let Some(style) = patch.style {
        context.style = style;
    }
    if let Some(scratchpad) = patch.scratchpad {
        context.scratchpad = scratchpad;
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use terratranslate_core::{ModelMetadata, NormalizeWhitespace};
    use terratranslate_provider::{ContextPatch, ModelCapabilities, TranslationResponse};

    use super::*;

    struct MockProvider;

    struct LabeledProvider;

    #[async_trait]
    impl ModelProvider for MockProvider {
        fn id(&self) -> &str {
            "mock"
        }
        fn model(&self) -> &str {
            "mock-multimodal"
        }
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                text: true,
                vision: true,
                audio: true,
                tools: true,
                ..Default::default()
            }
        }
        async fn translate(
            &self,
            request: TranslationRequest,
        ) -> Result<TranslationResponse, ProviderError> {
            assert_eq!(request.inputs.len(), 3);
            Ok(TranslationResponse {
                translated_text: "  Hello, Shiori.  ".into(),
                context_patch: ContextPatch {
                    scratchpad: Some("Shiori greeted the player.".into()),
                    ..Default::default()
                },
                request_id: Some("mock-1".into()),
                prompt_tokens: Some(10),
                completion_tokens: Some(4),
            })
        }
    }

    #[async_trait]
    impl ModelProvider for LabeledProvider {
        fn id(&self) -> &str {
            "mock"
        }

        fn model(&self) -> &str {
            "mock-text"
        }

        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                text: true,
                tools: true,
                ..Default::default()
            }
        }

        async fn translate(
            &self,
            request: TranslationRequest,
        ) -> Result<TranslationResponse, ProviderError> {
            let ModelInput::Text(text) = &request.inputs[0] else {
                panic!("expected a text input");
            };
            let value: serde_json::Value = serde_json::from_str(text).unwrap();
            assert_eq!(value["text_hook_label"], "Dialogue");
            assert_eq!(value["text"], "こんにちは 世界");
            Ok(TranslationResponse {
                translated_text: "  Hello world.  ".into(),
                context_patch: ContextPatch::default(),
                request_id: None,
                prompt_tokens: None,
                completion_tokens: None,
            })
        }
    }

    fn initialized_store() -> SessionStore {
        let blobs = std::env::temp_dir().join(format!(
            "terratranslate-engine-test-{}",
            uuid::Uuid::new_v4()
        ));
        let mut store = SessionStore::in_memory(blobs).unwrap();
        let root = TranslationCommit::create(
            vec![],
            1,
            vec![],
            String::new(),
            String::new(),
            ContextSnapshot::default(),
            vec![],
            vec![],
            ModelMetadata::default(),
            "root".into(),
        )
        .unwrap();
        store.put_commit(&root).unwrap();
        store.create_branch("main", &root.id, 1).unwrap();
        store
    }

    #[tokio::test]
    async fn commits_a_multimodal_turn_and_model_scratchpad() {
        let mut engine = TranslationEngine::new(initialized_store(), Arc::new(MockProvider));
        engine.add_processor(Arc::new(NormalizeWhitespace));
        let commit = engine
            .translate_turn(TurnRequest {
                branch: "main".into(),
                created_at_ms: 2,
                system_prompt: "Translate faithfully".into(),
                source_language: Some("Japanese".into()),
                target_language: "English".into(),
                inputs: vec![
                    TurnInput {
                        captured_at_ms: 2,
                        source: SourceKind::WineHook,
                        target: "game.exe".into(),
                        input: ModelInput::Text("  こんにちは  ".into()),
                        text_options: None,
                    },
                    TurnInput {
                        captured_at_ms: 2,
                        source: SourceKind::WindowCapture,
                        target: "42".into(),
                        input: ModelInput::Image {
                            media_type: "image/png".into(),
                            bytes: vec![1, 2],
                        },
                        text_options: None,
                    },
                    TurnInput {
                        captured_at_ms: 2,
                        source: SourceKind::ApplicationAudio,
                        target: "42".into(),
                        input: ModelInput::Audio {
                            format: "wav".into(),
                            bytes: vec![3, 4],
                        },
                        text_options: None,
                    },
                ],
            })
            .await
            .unwrap();
        assert_eq!(commit.source_text, "こんにちは");
        assert_eq!(commit.translated_text, "Hello, Shiori.");
        assert_eq!(commit.context.scratchpad, "Shiori greeted the player.");
        assert_eq!(commit.scratchpad_edits[0].author, ScratchpadAuthor::Model);
        assert_eq!(engine.store().branch("main").unwrap().head, commit.id);
    }

    #[tokio::test]
    async fn applies_and_records_processor_selection_for_a_labeled_text_hook() {
        let mut engine = TranslationEngine::new(initialized_store(), Arc::new(LabeledProvider));
        engine.add_processor(Arc::new(NormalizeWhitespace));
        let commit = engine
            .translate_turn(TurnRequest {
                branch: "main".into(),
                created_at_ms: 2,
                system_prompt: "Translate".into(),
                source_language: None,
                target_language: "English".into(),
                inputs: vec![TurnInput {
                    captured_at_ms: 2,
                    source: SourceKind::WineHook,
                    target: "wine:42:hook".into(),
                    input: ModelInput::Text("  こんにちは   世界  ".into()),
                    text_options: Some(TextInputOptions {
                        stable_hook_key: Some("wine|game.exe|gdi|dialog".into()),
                        label: Some(" Dialogue ".into()),
                        processing: TextProcessingSelection {
                            pre_prompt: vec!["builtin.normalize_whitespace".into()],
                            post_translation: vec![],
                        },
                    }),
                }],
            })
            .await
            .unwrap();

        assert_eq!(commit.source_text, "こんにちは 世界");
        assert_eq!(commit.translated_text, "  Hello world.  ");
        assert_eq!(
            commit.source_events[0].metadata["text_hook_label"],
            "Dialogue"
        );
        assert_eq!(
            commit.source_events[0].metadata["stable_hook_key"],
            "wine|game.exe|gdi|dialog"
        );
        assert_eq!(commit.processor_trace.len(), 1);
        assert_eq!(
            commit.processor_trace[0].metadata["stable_hook_key"],
            "wine|game.exe|gdi|dialog"
        );
    }

    #[tokio::test]
    async fn rejects_mixed_post_processing_in_one_model_turn() {
        let mut engine = TranslationEngine::new(initialized_store(), Arc::new(MockProvider));
        engine.add_processor(Arc::new(NormalizeWhitespace));
        let input = |post_translation| TurnInput {
            captured_at_ms: 2,
            source: SourceKind::WineHook,
            target: "hook".into(),
            input: ModelInput::Text("text".into()),
            text_options: Some(TextInputOptions {
                stable_hook_key: Some("hook".into()),
                label: None,
                processing: TextProcessingSelection {
                    pre_prompt: vec![],
                    post_translation,
                },
            }),
        };
        let result = engine
            .translate_turn(TurnRequest {
                branch: "main".into(),
                created_at_ms: 2,
                system_prompt: "Translate".into(),
                source_language: None,
                target_language: "English".into(),
                inputs: vec![
                    input(vec![]),
                    input(vec!["builtin.normalize_whitespace".into()]),
                ],
            })
            .await;
        assert!(matches!(
            result,
            Err(EngineError::IncompatiblePostProcessing)
        ));
    }
}
