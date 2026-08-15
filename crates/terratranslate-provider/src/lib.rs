//! Capability-checked model provider interfaces and an OpenAI-compatible implementation.

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use terratranslate_core::{ContextHistoryEntry, ContextSnapshot};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCapabilities {
    pub text: bool,
    pub vision: bool,
    pub audio: bool,
    pub tools: bool,
    pub json_schema: bool,
    pub streaming: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelInput {
    Text(String),
    Image { media_type: String, bytes: Vec<u8> },
    Audio { format: String, bytes: Vec<u8> },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TranslationRequest {
    pub system_prompt: String,
    pub inputs: Vec<ModelInput>,
    pub source_language: Option<String>,
    pub target_language: String,
    pub context: ContextSnapshot,
    /// Optional oldest-first branch history used by endless-context requests.
    #[serde(default)]
    pub context_history: Vec<ContextHistoryEntry>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ContextPatch {
    pub summary: Option<String>,
    pub glossary: Option<std::collections::BTreeMap<String, String>>,
    pub entities: Option<std::collections::BTreeMap<String, terratranslate_core::Entity>>,
    pub style: Option<String>,
    pub scratchpad: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TranslationResponse {
    pub translated_text: String,
    pub context_patch: ContextPatch,
    pub request_id: Option<String>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("model does not support required modality: {0}")]
    UnsupportedModality(&'static str),
    #[error("model does not support tool calls required for context updates")]
    ToolsUnsupported,
    #[error("provider request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("provider returned an invalid response: {0}")]
    InvalidResponse(String),
}

#[async_trait]
pub trait ModelProvider: Send + Sync {
    fn id(&self) -> &str;
    fn model(&self) -> &str;
    fn capabilities(&self) -> ModelCapabilities;
    async fn translate(
        &self,
        request: TranslationRequest,
    ) -> Result<TranslationResponse, ProviderError>;
}

pub fn validate_request(
    capabilities: ModelCapabilities,
    request: &TranslationRequest,
) -> Result<(), ProviderError> {
    for input in &request.inputs {
        match input {
            ModelInput::Text(_) if !capabilities.text => {
                return Err(ProviderError::UnsupportedModality("text"));
            }
            ModelInput::Image { .. } if !capabilities.vision => {
                return Err(ProviderError::UnsupportedModality("vision"));
            }
            ModelInput::Audio { .. } if !capabilities.audio => {
                return Err(ProviderError::UnsupportedModality("audio"));
            }
            _ => {}
        }
    }
    if !capabilities.tools {
        return Err(ProviderError::ToolsUnsupported);
    }
    Ok(())
}

pub struct OpenAiCompatibleProvider {
    client: reqwest::Client,
    endpoint: String,
    api_key: Option<SecretString>,
    model: String,
    capabilities: ModelCapabilities,
}

impl OpenAiCompatibleProvider {
    pub fn new(
        endpoint: impl Into<String>,
        api_key: Option<SecretString>,
        model: impl Into<String>,
        capabilities: ModelCapabilities,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            client: reqwest::Client::builder().build()?,
            endpoint: endpoint.into().trim_end_matches('/').to_owned(),
            api_key,
            model: model.into(),
            capabilities,
        })
    }

    fn body(&self, request: &TranslationRequest) -> Value {
        let mut content = Vec::new();
        for input in &request.inputs {
            match input {
                ModelInput::Text(text) => content.push(json!({"type": "text", "text": text})),
                ModelInput::Image { media_type, bytes } => content.push(json!({
                    "type": "image_url",
                    "image_url": {"url": format!("data:{media_type};base64,{}", STANDARD.encode(bytes))}
                })),
                ModelInput::Audio { format, bytes } => content.push(json!({
                    "type": "input_audio",
                    "input_audio": {"data": STANDARD.encode(bytes), "format": format}
                })),
            }
        }

        let context_json = serde_json::to_string(&request.context).expect("context serializes");
        let history_json =
            serde_json::to_string(&request.context_history).expect("context history serializes");
        content.insert(
            0,
            json!({
                "type": "text",
                "text": format!(
                    "Translate into {}. Source language: {}. Current versioned context: {}. Complete main-branch context history (oldest first): {}",
                    request.target_language,
                    request.source_language.as_deref().unwrap_or("auto-detect"),
                    context_json,
                    history_json
                )
            }),
        );

        json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": request.system_prompt},
                {"role": "user", "content": content}
            ],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "submit_translation",
                    "description": "Return the translation and any context changes needed for subsequent turns.",
                    "parameters": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["translated_text", "context_patch"],
                        "properties": {
                            "translated_text": {"type": "string"},
                            "context_patch": {
                                "type": "object",
                                "additionalProperties": false,
                                "properties": {
                                    "summary": {"type": "string"},
                                    "glossary": {"type": "object", "additionalProperties": {"type": "string"}},
                                    "entities": {"type": "object"},
                                    "style": {"type": "string"},
                                    "scratchpad": {"type": "string"}
                                }
                            }
                        }
                    }
                }
            }],
            "tool_choice": {"type": "function", "function": {"name": "submit_translation"}},
            "stream": false
        })
    }

    fn parse_response(value: Value) -> Result<TranslationResponse, ProviderError> {
        let request_id = value.get("id").and_then(Value::as_str).map(str::to_owned);
        let prompt_tokens = value
            .pointer("/usage/prompt_tokens")
            .and_then(Value::as_u64);
        let completion_tokens = value
            .pointer("/usage/completion_tokens")
            .and_then(Value::as_u64);
        let arguments = value
            .pointer("/choices/0/message/tool_calls/0/function/arguments")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ProviderError::InvalidResponse("missing submit_translation tool call".into())
            })?;
        #[derive(Deserialize)]
        struct ToolResult {
            translated_text: String,
            #[serde(default)]
            context_patch: ContextPatch,
        }
        let result: ToolResult = serde_json::from_str(arguments)
            .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
        Ok(TranslationResponse {
            translated_text: result.translated_text,
            context_patch: result.context_patch,
            request_id,
            prompt_tokens,
            completion_tokens,
        })
    }
}

#[async_trait]
impl ModelProvider for OpenAiCompatibleProvider {
    fn id(&self) -> &str {
        "openai-compatible"
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn capabilities(&self) -> ModelCapabilities {
        self.capabilities
    }

    async fn translate(
        &self,
        request: TranslationRequest,
    ) -> Result<TranslationResponse, ProviderError> {
        validate_request(self.capabilities, &request)?;
        let mut builder = self
            .client
            .post(format!("{}/chat/completions", self.endpoint))
            .json(&self.body(&request));
        if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key.expose_secret());
        }
        let response = builder.send().await?.error_for_status()?;
        let value = response.json::<Value>().await?;
        Self::parse_response(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(input: ModelInput) -> TranslationRequest {
        TranslationRequest {
            system_prompt: "Translate faithfully".into(),
            inputs: vec![input],
            source_language: None,
            target_language: "English".into(),
            context: ContextSnapshot::default(),
            context_history: vec![],
        }
    }

    #[test]
    fn capability_check_does_not_silently_drop_vision() {
        let capabilities = ModelCapabilities {
            text: true,
            tools: true,
            ..Default::default()
        };
        let result = validate_request(
            capabilities,
            &request(ModelInput::Image {
                media_type: "image/png".into(),
                bytes: vec![1, 2, 3],
            }),
        );
        assert!(matches!(
            result,
            Err(ProviderError::UnsupportedModality("vision"))
        ));
    }

    #[test]
    fn body_includes_complete_context_history() {
        let provider = OpenAiCompatibleProvider::new(
            "http://localhost/v1",
            None,
            "model",
            ModelCapabilities {
                text: true,
                tools: true,
                ..Default::default()
            },
        )
        .unwrap();
        let mut request = request(ModelInput::Text("current".into()));
        request.context_history = vec![ContextHistoryEntry {
            source_text: "previous source".into(),
            translated_text: "previous translation".into(),
            context: ContextSnapshot {
                summary: "previous summary".into(),
                ..Default::default()
            },
        }];

        let body = provider.body(&request);
        let prompt = body["messages"][1]["content"][0]["text"]
            .as_str()
            .expect("context prompt is text");
        assert!(prompt.contains("Complete main-branch context history (oldest first)"));
        assert!(prompt.contains("previous source"));
        assert!(prompt.contains("previous translation"));
        assert!(prompt.contains("previous summary"));
    }

    #[test]
    fn parses_context_tool_call() {
        let response = json!({
            "id": "req-1",
            "choices": [{"message": {"tool_calls": [{"function": {
                "arguments": "{\"translated_text\":\"Hello\",\"context_patch\":{\"scratchpad\":\"A greeted B\"}}"
            }}]}}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 4}
        });
        let parsed = OpenAiCompatibleProvider::parse_response(response).unwrap();
        assert_eq!(parsed.translated_text, "Hello");
        assert_eq!(
            parsed.context_patch.scratchpad.as_deref(),
            Some("A greeted B")
        );
    }
}
