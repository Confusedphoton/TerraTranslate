use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{ContextSnapshot, GameIdentity};

/// One text input exposed to a prompt template.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptText {
    /// One-based position in the current model turn.
    pub index: usize,
    #[serde(default)]
    pub hook_id: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    pub text: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub target: String,
}

/// Values available while rendering system and user prompt templates.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptData {
    #[serde(default)]
    pub game: Option<GameIdentity>,
    #[serde(default)]
    pub source_language: Option<String>,
    pub target_language: String,
    #[serde(default)]
    pub branch: String,
    pub context: ContextSnapshot,
    #[serde(default)]
    pub texts: Vec<PromptText>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptTemplate {
    source: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PromptTemplateError {
    #[error("prompt macro is not closed: {0}")]
    UnclosedMacro(String),
    #[error("prompt enumeration block is not closed")]
    UnclosedEnumeration,
    #[error("unexpected prompt enumeration terminator")]
    UnexpectedEnumerationEnd,
    #[error("unknown prompt macro: {0}")]
    UnknownMacro(String),
    #[error("unknown prompt format filter: {0}")]
    UnknownFilter(String),
}

impl PromptTemplate {
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
        }
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn render(&self, data: &PromptData) -> Result<String, PromptTemplateError> {
        render_enumeration_blocks(&self.source, data)
            .and_then(|rendered| render_fragment(&rendered, data, None))
    }
}

pub fn render_prompt(
    template: impl Into<String>,
    data: &PromptData,
) -> Result<String, PromptTemplateError> {
    PromptTemplate::new(template).render(data)
}

fn render_enumeration_blocks(
    source: &str,
    data: &PromptData,
) -> Result<String, PromptTemplateError> {
    let close = "{{/each}}";
    let mut rendered = source.to_owned();

    loop {
        let Some((open_start, open_len)) =
            ["{{#each texts}}", "{{#each hooks}}", "{{#each text_hooks}}"]
                .iter()
                .filter_map(|open| rendered.find(open).map(|start| (start, open.len())))
                .min_by_key(|(start, _)| *start)
        else {
            if rendered.contains(close) {
                return Err(PromptTemplateError::UnexpectedEnumerationEnd);
            }
            return Ok(rendered);
        };
        let body_start = open_start + open_len;
        let Some(close_relative) = rendered[body_start..].find(close) else {
            return Err(PromptTemplateError::UnclosedEnumeration);
        };
        let close_start = body_start + close_relative;
        let body = &rendered[body_start..close_start];
        let replacement = data
            .texts
            .iter()
            .map(|text| render_fragment(body, data, Some(text)))
            .collect::<Result<Vec<_>, _>>()?
            .join("");
        rendered.replace_range(open_start..close_start + close.len(), &replacement);
    }
}

fn render_fragment(
    source: &str,
    data: &PromptData,
    current_text: Option<&PromptText>,
) -> Result<String, PromptTemplateError> {
    let mut rendered = String::with_capacity(source.len());
    let mut cursor = 0;
    while let Some(relative_start) = source[cursor..].find("{{") {
        let start = cursor + relative_start;
        rendered.push_str(&source[cursor..start]);
        let token_start = start + 2;
        let Some(relative_end) = source[token_start..].find("}}") else {
            return Err(PromptTemplateError::UnclosedMacro(
                source[token_start..].trim().to_owned(),
            ));
        };
        let end = token_start + relative_end;
        let token = source[token_start..end].trim();
        if token.starts_with('#') || token == "/each" {
            return Err(if token == "/each" {
                PromptTemplateError::UnexpectedEnumerationEnd
            } else {
                PromptTemplateError::UnknownMacro(token.to_owned())
            });
        }
        rendered.push_str(&render_macro(token, data, current_text)?);
        cursor = end + 2;
    }
    rendered.push_str(&source[cursor..]);
    Ok(rendered)
}

fn render_macro(
    token: &str,
    data: &PromptData,
    current_text: Option<&PromptText>,
) -> Result<String, PromptTemplateError> {
    let (name, filter) = token
        .split_once('|')
        .or_else(|| token.split_once(':'))
        .map(|(name, filter)| (name.trim(), Some(filter.trim())))
        .unwrap_or((token.trim(), None));
    let value = match name {
        "game.id" | "game_id" => data.game.as_ref().map(|game| game.id.0.clone()),
        "game.name" | "game_name" => data.game.as_ref().map(|game| game.name.clone()),
        "game.executable" | "game.executable_path" | "game.path" | "game_path" => {
            data.game.as_ref().map(|game| game.executable_path.clone())
        }
        "game.image_id" => data.game.as_ref().and_then(|game| game.image_id.clone()),
        "game.platform" | "game_platform" => data.game.as_ref().map(|game| game.platform.clone()),
        "game.runtime" | "game_runtime" => data.game.as_ref().map(|game| game.runtime.clone()),
        "game" => data.game.as_ref().map(|game| {
            format!(
                "{} ({}, {}, {})",
                game.name, game.platform, game.runtime, game.executable_path
            )
        }),
        "source_language" | "source.language" | "source_lang" => data.source_language.clone(),
        "target_language" | "target.language" | "target_lang" => Some(data.target_language.clone()),
        "branch" => Some(data.branch.clone()),
        "context.summary" => Some(data.context.summary.clone()),
        "context.style" => Some(data.context.style.clone()),
        "context.scratchpad" => Some(data.context.scratchpad.clone()),
        "context.glossary" => Some(json_string(&data.context.glossary)),
        "context.entities" => Some(json_string(&data.context.entities)),
        "context" | "context.json" => Some(json_string(&data.context)),
        "text" => Some(
            current_text
                .map(|text| text.text.clone())
                .unwrap_or_else(|| join_texts(data)),
        ),
        "texts" | "hooks" => Some(join_texts(data)),
        "texts.enumerate"
        | "texts.enumerated"
        | "hooks.enumerate"
        | "text_hooks"
        | "text_hooks.enumerate" => Some(enumerate_texts(data)),
        "texts.count" | "hooks.count" | "hook_count" => Some(data.texts.len().to_string()),
        "index" | "number" | "@index" | "text.index" | "this.index" => {
            current_text.map(|text| text.index.to_string())
        }
        "label" | "text.label" | "this.label" => current_text.and_then(|text| text.label.clone()),
        "hook_id" | "text.hook_id" | "this.hook_id" => {
            current_text.and_then(|text| text.hook_id.clone())
        }
        "source" | "text.source" | "this.source" => current_text.map(|text| text.source.clone()),
        "target" | "text.target" | "this.target" => current_text.map(|text| text.target.clone()),
        "this.text" => current_text.map(|text| text.text.clone()),
        _ => return Err(PromptTemplateError::UnknownMacro(name.to_owned())),
    }
    .unwrap_or_default();

    match filter {
        None => Ok(value),
        Some("trim") => Ok(value.trim().to_owned()),
        Some("upper") => Ok(value.to_uppercase()),
        Some("lower") => Ok(value.to_lowercase()),
        Some("json") => Ok(json_string(&value)),
        Some("enumerate") if matches!(name, "texts" | "hooks" | "text_hooks") => {
            Ok(enumerate_texts(data))
        }
        Some(filter) => Err(PromptTemplateError::UnknownFilter(filter.to_owned())),
    }
}

fn join_texts(data: &PromptData) -> String {
    data.texts
        .iter()
        .map(|text| text.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn enumerate_texts(data: &PromptData) -> String {
    data.texts
        .iter()
        .map(|text| {
            let label = text
                .label
                .as_deref()
                .or(text.hook_id.as_deref())
                .unwrap_or("text hook");
            format!("[{}] {label}:\n{}", text.index, text.text)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn json_string<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("prompt values must serialize")
}

impl fmt::Display for PromptTemplate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data() -> PromptData {
        PromptData {
            game: Some(GameIdentity::from_stable_key(
                "story.exe",
                "Story",
                "C:/Games/story.exe",
                Some("sha256:story".into()),
                "windows",
                "wine",
            )),
            source_language: Some("Japanese".into()),
            target_language: "English".into(),
            branch: "main".into(),
            context: ContextSnapshot {
                summary: "The heroine is at the station.".into(),
                ..Default::default()
            },
            texts: vec![
                PromptText {
                    index: 1,
                    hook_id: Some("dialogue".into()),
                    label: Some("Dialogue".into()),
                    text: "こんにちは".into(),
                    source: "wine_hook".into(),
                    target: "story.exe".into(),
                },
                PromptText {
                    index: 2,
                    hook_id: Some("choice".into()),
                    label: Some("Choice".into()),
                    text: "はい".into(),
                    source: "wine_hook".into(),
                    target: "story.exe".into(),
                },
            ],
        }
    }

    #[test]
    fn renders_game_context_and_enumerates_every_text_hook() {
        let rendered = PromptTemplate::new(
            "{{game.name}} -> {{target_language}}\n{{#each texts}}{{number}} {{label}} = {{text}}; {{/each}}",
        )
        .render(&data())
        .unwrap();
        assert_eq!(
            rendered,
            "Story -> English\n1 Dialogue = こんにちは; 2 Choice = はい; "
        );
    }

    #[test]
    fn renders_enumeration_macro_and_json_filter() {
        let rendered = PromptTemplate::new("{{texts|enumerate}} {{game.id|json}}")
            .render(&data())
            .unwrap();
        assert!(rendered.contains("[1] Dialogue:\nこんにちは"));
        assert!(rendered.contains('"'));
    }

    #[test]
    fn rejects_unknown_macros() {
        assert!(matches!(
            PromptTemplate::new("{{game.title}}").render(&data()),
            Err(PromptTemplateError::UnknownMacro(_))
        ));
    }
}
