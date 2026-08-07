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

pub struct TurnInput {
    pub captured_at_ms: i64,
    pub source: SourceKind,
    pub target: String,
    pub input: ModelInput,
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
            source_events.push(SourceEvent {
                id: EventId::new(),
                captured_at_ms: input.captured_at_ms,
                modality,
                source: input.source,
                target: input.target,
                payload,
                metadata: Default::default(),
            });

            match input.input {
                ModelInput::Text(text) => {
                    let processed = self
                        .run_stage(ProcessorStage::PrePrompt, text, &context, &mut trace)
                        .await?;
                    source_text_parts.push(processed.clone());
                    model_inputs.push(ModelInput::Text(processed));
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
        let translated_text = self
            .run_stage(
                ProcessorStage::PostTranslation,
                response.translated_text,
                &context,
                &mut trace,
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
        &self,
        stage: ProcessorStage,
        mut text: String,
        context: &ContextSnapshot,
        trace: &mut Vec<ProcessorTrace>,
    ) -> Result<String, EngineError> {
        for processor in &self.processors {
            if !processor.stages().contains(&stage) {
                continue;
            }
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
            });
        }
        Ok(text)
    }
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
                    },
                    TurnInput {
                        captured_at_ms: 2,
                        source: SourceKind::WindowCapture,
                        target: "42".into(),
                        input: ModelInput::Image {
                            media_type: "image/png".into(),
                            bytes: vec![1, 2],
                        },
                    },
                    TurnInput {
                        captured_at_ms: 2,
                        source: SourceKind::ApplicationAudio,
                        target: "42".into(),
                        input: ModelInput::Audio {
                            format: "wav".into(),
                            bytes: vec![3, 4],
                        },
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
}
