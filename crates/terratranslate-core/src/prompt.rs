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
    /// Whether this text was newly observed for the current model turn.
    #[serde(default)]
    pub updated: bool,
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
    #[error("prompt block is not closed: {0}")]
    UnclosedBlock(String),
    #[error("unexpected prompt block terminator: {0}")]
    UnexpectedBlockEnd(String),
    #[error("unexpected prompt else branch")]
    UnexpectedElse,
    #[error("prompt block has more than one else branch")]
    MultipleElse,
    #[error("unknown prompt block: {0}")]
    UnknownBlock(String),
    #[error("unknown prompt condition: {0}")]
    UnknownCondition(String),
    #[error("invalid prompt filter argument: {0}")]
    InvalidFilterArgument(String),
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
        render_template_fragment(&self.source, data)
    }
}

pub fn render_prompt(
    template: impl Into<String>,
    data: &PromptData,
) -> Result<String, PromptTemplateError> {
    PromptTemplate::new(template).render(data)
}

struct BlockParts<'a> {
    body: &'a str,
    alternate: Option<&'a str>,
    end: usize,
}

fn render_template_fragment(
    source: &str,
    data: &PromptData,
) -> Result<String, PromptTemplateError> {
    render_template_fragment_for_text(source, data, None)
}

fn render_template_fragment_for_text(
    source: &str,
    data: &PromptData,
    current_text: Option<&PromptText>,
) -> Result<String, PromptTemplateError> {
    let mut rendered = String::with_capacity(source.len());
    let mut cursor = 0;
    loop {
        let Some(relative_start) = source[cursor..].find("{{#") else {
            rendered.push_str(&render_fragment(&source[cursor..], data, current_text)?);
            return Ok(rendered);
        };
        let open_start = cursor + relative_start;
        rendered.push_str(&render_fragment(
            &source[cursor..open_start],
            data,
            current_text,
        )?);
        let token_start = open_start + 2;
        let Some(relative_end) = source[token_start..].find("}}") else {
            return Err(PromptTemplateError::UnclosedMacro(
                source[token_start..].trim().to_owned(),
            ));
        };
        let token_end = token_start + relative_end;
        let block = source[token_start + 1..token_end].trim();
        let (block_name, argument) = split_block_tag(block);
        let parts = find_block_parts(source, token_end + 2, block_name)?;
        let rendered_block = match block_name {
            "each" => {
                if !matches!(argument, "texts" | "hooks" | "text_hooks") {
                    return Err(PromptTemplateError::UnknownBlock(block.to_owned()));
                }
                if parts.alternate.is_some() {
                    return Err(PromptTemplateError::UnexpectedElse);
                }
                data.texts
                    .iter()
                    .map(|text| render_template_fragment_for_text(parts.body, data, Some(text)))
                    .collect::<Result<Vec<_>, _>>()?
                    .join("")
            }
            "if" | "unless" => {
                let condition = evaluate_condition(argument, data, current_text)?;
                let include_body = if block_name == "if" {
                    condition
                } else {
                    !condition
                };
                let selected = if include_body {
                    parts.body
                } else {
                    parts.alternate.unwrap_or_default()
                };
                render_template_fragment_for_text(selected, data, current_text)?
            }
            _ => return Err(PromptTemplateError::UnknownBlock(block.to_owned())),
        };
        rendered.push_str(&rendered_block);
        cursor = parts.end;
    }
}

fn split_block_tag(block: &str) -> (&str, &str) {
    let mut parts = block.splitn(2, char::is_whitespace);
    (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default().trim(),
    )
}

fn find_block_parts<'a>(
    source: &'a str,
    body_start: usize,
    expected: &str,
) -> Result<BlockParts<'a>, PromptTemplateError> {
    let mut stack = vec![expected.to_owned()];
    let mut alternate_marker = None;
    let mut alternate_start = None;
    let mut cursor = body_start;
    loop {
        let Some(relative_start) = source[cursor..].find("{{") else {
            return Err(if expected == "each" {
                PromptTemplateError::UnclosedEnumeration
            } else {
                PromptTemplateError::UnclosedBlock(expected.to_owned())
            });
        };
        let start = cursor + relative_start;
        let token_start = start + 2;
        let Some(relative_end) = source[token_start..].find("}}") else {
            return Err(PromptTemplateError::UnclosedMacro(
                source[token_start..].trim().to_owned(),
            ));
        };
        let token_end = token_start + relative_end;
        let token = source[token_start..token_end].trim();
        if let Some(nested) = token.strip_prefix('#') {
            let (nested_name, _) = split_block_tag(nested.trim());
            stack.push(nested_name.to_owned());
        } else if let Some(close) = token.strip_prefix('/') {
            let close = close.trim();
            if stack.last().map(String::as_str) != Some(close) {
                return Err(PromptTemplateError::UnexpectedBlockEnd(close.to_owned()));
            }
            stack.pop();
            if stack.is_empty() {
                let first_end = alternate_marker.unwrap_or(start);
                return Ok(BlockParts {
                    body: &source[body_start..first_end],
                    alternate: alternate_start.map(|alternate| &source[alternate..start]),
                    end: token_end + 2,
                });
            }
        } else if token == "else" && stack.len() == 1 {
            if alternate_start.is_some() {
                return Err(PromptTemplateError::MultipleElse);
            }
            alternate_marker = Some(start);
            alternate_start = Some(token_end + 2);
        }
        cursor = token_end + 2;
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
                PromptTemplateError::UnknownBlock(token.to_owned())
            });
        }
        if token.starts_with('/') {
            return Err(PromptTemplateError::UnexpectedBlockEnd(token.to_owned()));
        }
        if token == "else" {
            return Err(PromptTemplateError::UnexpectedElse);
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
        "updated_texts" | "updated_hooks" => Some(join_updated_texts(data)),
        "texts.enumerate"
        | "texts.enumerated"
        | "hooks.enumerate"
        | "text_hooks"
        | "text_hooks.enumerate" => Some(enumerate_texts(data, false)),
        "updated_texts.enumerate" | "updated_texts.enumerated" | "updated_hooks.enumerate" => {
            Some(enumerate_texts(data, true))
        }
        "texts.count" | "hooks.count" | "hook_count" => Some(data.texts.len().to_string()),
        "updated_texts.count" | "updated_hooks.count" | "updated_hook_count" => {
            Some(updated_text_count(data).to_string())
        }
        "updated" | "text.updated" | "this.updated" => Some(
            current_text
                .map(|text| text.updated.to_string())
                .unwrap_or_else(|| has_updated_text(data).to_string()),
        ),
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
            Ok(enumerate_texts(data, false))
        }
        Some("enumerate") if matches!(name, "updated_texts" | "updated_hooks") => {
            Ok(enumerate_texts(data, true))
        }
        Some(filter) if filter.starts_with("default(") && filter.ends_with(')') => {
            let fallback = parse_default_argument(&filter["default(".len()..filter.len() - 1])?;
            if value.trim().is_empty() {
                Ok(fallback)
            } else {
                Ok(value)
            }
        }
        Some(filter) if filter.starts_with("default:") => {
            let fallback = parse_default_argument(&filter["default:".len()..])?;
            if value.trim().is_empty() {
                Ok(fallback)
            } else {
                Ok(value)
            }
        }
        Some(filter) => Err(PromptTemplateError::UnknownFilter(filter.to_owned())),
    }
}

fn evaluate_condition(
    condition: &str,
    data: &PromptData,
    current_text: Option<&PromptText>,
) -> Result<bool, PromptTemplateError> {
    let condition = condition.trim();
    let (negated, condition) = condition
        .strip_prefix('!')
        .map(|condition| (true, condition.trim()))
        .unwrap_or((false, condition));
    let value = match condition {
        "true" => true,
        "false" => false,
        "texts" | "hooks" | "text_hooks" => !data.texts.is_empty(),
        "updated_texts" | "updated_hooks" | "has_updated_hook" => has_updated_text(data),
        "updated" | "text.updated" | "this.updated" => {
            current_text.is_some_and(|text| text.updated)
        }
        _ => return Err(PromptTemplateError::UnknownCondition(condition.to_owned())),
    };
    Ok(if negated { !value } else { value })
}

fn parse_default_argument(argument: &str) -> Result<String, PromptTemplateError> {
    let argument = argument.trim();
    if argument.len() >= 2
        && ((argument.starts_with('"') && argument.ends_with('"'))
            || (argument.starts_with('\'') && argument.ends_with('\'')))
    {
        if argument.starts_with('"') {
            serde_json::from_str(argument)
                .map_err(|_| PromptTemplateError::InvalidFilterArgument(argument.to_owned()))
        } else {
            Ok(argument[1..argument.len() - 1].replace("\\'", "'"))
        }
    } else if argument.is_empty() {
        Err(PromptTemplateError::InvalidFilterArgument(
            argument.to_owned(),
        ))
    } else {
        Ok(argument.to_owned())
    }
}

fn join_texts(data: &PromptData) -> String {
    data.texts
        .iter()
        .map(|text| text.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn join_updated_texts(data: &PromptData) -> String {
    data.texts
        .iter()
        .filter(|text| text.updated)
        .map(|text| text.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn updated_text_count(data: &PromptData) -> usize {
    data.texts.iter().filter(|text| text.updated).count()
}

fn has_updated_text(data: &PromptData) -> bool {
    data.texts.iter().any(|text| text.updated)
}

fn enumerate_texts(data: &PromptData, updated_only: bool) -> String {
    data.texts
        .iter()
        .filter(|text| !updated_only || text.updated)
        .enumerate()
        .map(|(index, text)| {
            let label = text
                .label
                .as_deref()
                .or(text.hook_id.as_deref())
                .unwrap_or("text hook");
            let number = if updated_only { index + 1 } else { text.index };
            format!("[{number}] {label}:\n{}", text.text)
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
                    updated: true,
                },
                PromptText {
                    index: 2,
                    hook_id: Some("choice".into()),
                    label: Some("Choice".into()),
                    text: "はい".into(),
                    source: "wine_hook".into(),
                    target: "story.exe".into(),
                    updated: false,
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
    fn renders_updated_conditionals_nested_in_hook_loops() {
        let rendered = PromptTemplate::new(
            "{{#if updated_hooks}}Updated: {{updated_texts|enumerate}}{{else}}No updates{{/if}}|{{#each texts}}{{#if updated}}new{{else}}old{{/if}}={{text}};{{/each}}",
        )
        .render(&data())
        .unwrap();
        assert_eq!(
            rendered,
            "Updated: [1] Dialogue:\nこんにちは|new=こんにちは;old=はい;"
        );
    }

    #[test]
    fn renders_configurable_defaults_and_else_branches() {
        let empty = PromptData::default();
        let rendered = PromptTemplate::new(
            "{{#unless updated_hooks}}No updated hook{{else}}Updated{{/unless}} / {{texts|default(\"fallback message\")}}",
        )
        .render(&empty)
        .unwrap();
        assert_eq!(rendered, "No updated hook / fallback message");
    }

    #[test]
    fn rejects_unknown_macros() {
        assert!(matches!(
            PromptTemplate::new("{{game.title}}").render(&data()),
            Err(PromptTemplateError::UnknownMacro(_))
        ));
    }
}
