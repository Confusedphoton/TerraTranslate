use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::ContextSnapshot;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessorStage {
    PrePrompt,
    PostTranslation,
    PreEmbedding,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessorRequest {
    pub stage: ProcessorStage,
    pub text: String,
    pub context: ContextSnapshot,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessorResponse {
    pub text: String,
    #[serde(default)]
    pub diagnostics: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProcessorError {
    #[error("processor rejected input: {0}")]
    Rejected(String),
    #[error("processor failed: {0}")]
    Failed(String),
}

#[async_trait]
pub trait TextProcessor: Send + Sync {
    fn id(&self) -> &str;
    fn version(&self) -> &str;
    fn stages(&self) -> &[ProcessorStage];
    async fn process(&self, request: ProcessorRequest)
    -> Result<ProcessorResponse, ProcessorError>;
}

#[derive(Default)]
pub struct NormalizeWhitespace;

const ALL_TEXT_STAGES: &[ProcessorStage] = &[
    ProcessorStage::PrePrompt,
    ProcessorStage::PostTranslation,
    ProcessorStage::PreEmbedding,
];

#[async_trait]
impl TextProcessor for NormalizeWhitespace {
    fn id(&self) -> &str {
        "builtin.normalize_whitespace"
    }

    fn version(&self) -> &str {
        "1"
    }

    fn stages(&self) -> &[ProcessorStage] {
        ALL_TEXT_STAGES
    }

    async fn process(
        &self,
        mut request: ProcessorRequest,
    ) -> Result<ProcessorResponse, ProcessorError> {
        request.text = request
            .text
            .lines()
            .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_owned();
        Ok(ProcessorResponse {
            text: request.text,
            diagnostics: vec![],
        })
    }
}

pub struct RegexReplacement {
    id: String,
    version: String,
    regex: Regex,
    replacement: String,
    stages: Vec<ProcessorStage>,
}

impl RegexReplacement {
    pub fn new(
        id: impl Into<String>,
        pattern: &str,
        replacement: impl Into<String>,
        stages: Vec<ProcessorStage>,
    ) -> Result<Self, regex::Error> {
        Ok(Self {
            id: id.into(),
            version: "1".into(),
            regex: Regex::new(pattern)?,
            replacement: replacement.into(),
            stages,
        })
    }
}

#[async_trait]
impl TextProcessor for RegexReplacement {
    fn id(&self) -> &str {
        &self.id
    }
    fn version(&self) -> &str {
        &self.version
    }
    fn stages(&self) -> &[ProcessorStage] {
        &self.stages
    }

    async fn process(
        &self,
        request: ProcessorRequest,
    ) -> Result<ProcessorResponse, ProcessorError> {
        Ok(ProcessorResponse {
            text: self
                .regex
                .replace_all(&request.text, self.replacement.as_str())
                .into_owned(),
            diagnostics: vec![],
        })
    }
}

pub struct RepeatSuppressor {
    id: String,
    capacity: usize,
    recent: Mutex<VecDeque<String>>,
}

impl RepeatSuppressor {
    pub fn new(id: impl Into<String>, capacity: usize) -> Self {
        Self {
            id: id.into(),
            capacity,
            recent: Mutex::new(VecDeque::with_capacity(capacity)),
        }
    }
}

#[async_trait]
impl TextProcessor for RepeatSuppressor {
    fn id(&self) -> &str {
        &self.id
    }
    fn version(&self) -> &str {
        "1"
    }
    fn stages(&self) -> &[ProcessorStage] {
        &[ProcessorStage::PrePrompt]
    }

    async fn process(
        &self,
        request: ProcessorRequest,
    ) -> Result<ProcessorResponse, ProcessorError> {
        let mut recent = self
            .recent
            .lock()
            .map_err(|_| ProcessorError::Failed("deduplication state was poisoned".into()))?;
        if recent.contains(&request.text) {
            return Err(ProcessorError::Rejected("duplicate line".into()));
        }
        if self.capacity > 0 {
            if recent.len() == self.capacity {
                recent.pop_front();
            }
            recent.push_back(request.text.clone());
        }
        Ok(ProcessorResponse {
            text: request.text,
            diagnostics: vec![],
        })
    }
}

#[cfg(test)]
mod processor_tests {
    use super::*;

    #[tokio::test]
    async fn regex_processor_is_orderable_and_unicode_safe() {
        let processor = RegexReplacement::new(
            "strip-tags",
            r"<[^>]+>",
            "",
            vec![ProcessorStage::PrePrompt],
        )
        .unwrap();
        let response = processor
            .process(ProcessorRequest {
                stage: ProcessorStage::PrePrompt,
                text: "<name>栞</name>".into(),
                context: ContextSnapshot::default(),
            })
            .await
            .unwrap();
        assert_eq!(response.text, "栞");
    }
}
