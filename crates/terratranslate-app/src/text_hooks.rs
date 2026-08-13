use relm4::gtk;
use relm4::gtk::prelude::*;
use relm4::prelude::*;
use serde::{Deserialize, Deserializer, Serialize};

pub const NORMALIZE_WHITESPACE_PROCESSOR: &str = "builtin.normalize_whitespace";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct TextHookConfig {
    pub enabled: bool,
    pub label: String,
    pub pre_processors: Vec<String>,
    pub post_processors: Vec<String>,
    /// Last known presentation metadata lets unavailable saved hooks remain identifiable.
    pub title: String,
    pub detail: String,
    pub source_api: String,
}

impl TextHookConfig {
    pub fn pre_processors(&self) -> Vec<String> {
        self.pre_processors.clone()
    }

    pub fn post_processors(&self) -> Vec<String> {
        self.post_processors.clone()
    }

    pub fn label(&self) -> Option<String> {
        let label = self.label.trim();
        (!label.is_empty()).then(|| label.to_owned())
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct TextHookConfigWire {
    enabled: bool,
    label: String,
    pre_processors: Option<Vec<String>>,
    post_processors: Option<Vec<String>>,
    normalize_before_model: Option<bool>,
    normalize_after_model: Option<bool>,
    title: String,
    detail: String,
    source_api: String,
}

impl<'de> Deserialize<'de> for TextHookConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = TextHookConfigWire::deserialize(deserializer)?;
        let legacy = |enabled: Option<bool>| {
            enabled
                .unwrap_or(false)
                .then(|| NORMALIZE_WHITESPACE_PROCESSOR.to_owned())
                .into_iter()
                .collect()
        };
        Ok(Self {
            enabled: wire.enabled,
            label: wire.label,
            pre_processors: wire
                .pre_processors
                .unwrap_or_else(|| legacy(wire.normalize_before_model)),
            post_processors: wire
                .post_processors
                .unwrap_or_else(|| legacy(wire.normalize_after_model)),
            title: wire.title,
            detail: wire.detail,
            source_api: wire.source_api,
        })
    }
}

#[derive(Clone, Debug)]
pub struct TextHookInit {
    pub id: String,
    pub title: String,
    pub detail: String,
    pub sample: String,
    pub available: bool,
    pub config: TextHookConfig,
}

#[derive(Debug)]
pub struct TextHookRow {
    id: String,
    title: String,
    detail: String,
    sample: String,
    available: bool,
    config: TextHookConfig,
}

#[derive(Clone, Debug)]
pub enum TextHookRowInput {
    SetEnabled(bool),
    SetLabel(String),
    SetNormalizeBefore(bool),
    SetNormalizeAfter(bool),
    Observed {
        title: String,
        detail: String,
        sample: String,
    },
    Availability(bool),
    Forget,
}

#[derive(Debug)]
pub enum TextHookRowOutput {
    ConfigChanged { id: String, config: TextHookConfig },
    Forget(String),
}

#[relm4::factory(pub)]
impl FactoryComponent for TextHookRow {
    type Init = TextHookInit;
    type Input = TextHookRowInput;
    type Output = TextHookRowOutput;
    type CommandOutput = ();
    type ParentWidget = gtk::ListBox;

    view! {
        gtk::Box {
            set_orientation: gtk::Orientation::Vertical,
            set_spacing: 5,
            set_margin_all: 8,

            gtk::Box {
                set_orientation: gtk::Orientation::Horizontal,
                set_spacing: 8,

                gtk::CheckButton {
                    set_active: self.config.enabled,
                    set_tooltip_text: Some("Pass new text from this hook to the model"),
                    connect_toggled[sender] => move |button| {
                        sender.input(TextHookRowInput::SetEnabled(button.is_active()));
                    },
                },
                gtk::Label {
                    #[watch]
                    set_label: &if self.available {
                        self.title.clone()
                    } else {
                        format!("{} — unavailable", self.title)
                    },
                    set_hexpand: true,
                    set_halign: gtk::Align::Start,
                    add_css_class: "heading",
                },
                gtk::Entry {
                    set_width_chars: 16,
                    set_placeholder_text: Some("Optional model label"),
                    set_text: &self.config.label,
                    connect_changed[sender] => move |entry| {
                        sender.input(TextHookRowInput::SetLabel(entry.text().to_string()));
                    },
                },
                gtk::Button {
                    set_label: "Forget",
                    set_tooltip_text: Some("Remove this saved hook configuration"),
                    connect_clicked => TextHookRowInput::Forget,
                },
            },

            gtk::Label {
                #[watch]
                set_label: &self.detail,
                set_halign: gtk::Align::Start,
                set_ellipsize: gtk::pango::EllipsizeMode::Middle,
                add_css_class: "dim-label",
            },

            gtk::Box {
                set_orientation: gtk::Orientation::Horizontal,
                set_spacing: 12,

                gtk::CheckButton {
                    set_label: Some("Normalize before model"),
                    set_active: self.config.pre_processors.iter().any(|id| id == NORMALIZE_WHITESPACE_PROCESSOR),
                    connect_toggled[sender] => move |button| {
                        sender.input(TextHookRowInput::SetNormalizeBefore(button.is_active()));
                    },
                },
                gtk::CheckButton {
                    set_label: Some("Normalize after model"),
                    set_active: self.config.post_processors.iter().any(|id| id == NORMALIZE_WHITESPACE_PROCESSOR),
                    connect_toggled[sender] => move |button| {
                        sender.input(TextHookRowInput::SetNormalizeAfter(button.is_active()));
                    },
                },
            },

            gtk::Label {
                #[watch]
                set_label: &if self.sample.is_empty() {
                    "Waiting for text…".to_owned()
                } else {
                    format!("Latest: {}", one_line_preview(&self.sample))
                },
                set_halign: gtk::Align::Start,
                set_ellipsize: gtk::pango::EllipsizeMode::End,
                add_css_class: "dim-label",
            },
        }
    }

    fn init_model(init: Self::Init, _index: &DynamicIndex, _sender: FactorySender<Self>) -> Self {
        Self {
            id: init.id,
            title: init.title,
            detail: init.detail,
            sample: init.sample,
            available: init.available,
            config: init.config,
        }
    }

    fn update(&mut self, message: Self::Input, sender: FactorySender<Self>) {
        let config_changed = match message {
            TextHookRowInput::SetEnabled(enabled) => {
                self.config.enabled = enabled;
                true
            }
            TextHookRowInput::SetLabel(label) => {
                self.config.label = label;
                true
            }
            TextHookRowInput::SetNormalizeBefore(enabled) => {
                set_processor_enabled(
                    &mut self.config.pre_processors,
                    NORMALIZE_WHITESPACE_PROCESSOR,
                    enabled,
                );
                true
            }
            TextHookRowInput::SetNormalizeAfter(enabled) => {
                set_processor_enabled(
                    &mut self.config.post_processors,
                    NORMALIZE_WHITESPACE_PROCESSOR,
                    enabled,
                );
                true
            }
            TextHookRowInput::Observed {
                title,
                detail,
                sample,
            } => {
                self.title = title;
                self.detail = detail;
                self.sample = sample;
                self.available = true;
                false
            }
            TextHookRowInput::Availability(available) => {
                self.available = available;
                false
            }
            TextHookRowInput::Forget => {
                let _ = sender.output(TextHookRowOutput::Forget(self.id.clone()));
                false
            }
        };
        if config_changed {
            let _ = sender.output(TextHookRowOutput::ConfigChanged {
                id: self.id.clone(),
                config: self.config.clone(),
            });
        }
    }
}

fn set_processor_enabled(processors: &mut Vec<String>, processor_id: &str, enabled: bool) {
    if enabled {
        if !processors.iter().any(|id| id == processor_id) {
            processors.push(processor_id.to_owned());
        }
    } else {
        processors.retain(|id| id != processor_id);
    }
}

fn one_line_preview(text: &str) -> String {
    const MAX_CHARS: usize = 120;
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut characters = one_line.chars();
    let preview = characters.by_ref().take(MAX_CHARS).collect::<String>();
    if characters.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn processor_selection_is_independent_per_stage() {
        let config = TextHookConfig {
            pre_processors: vec![NORMALIZE_WHITESPACE_PROCESSOR.into()],
            ..Default::default()
        };
        assert_eq!(
            config.pre_processors(),
            vec![NORMALIZE_WHITESPACE_PROCESSOR]
        );
        assert!(config.post_processors().is_empty());
    }

    #[test]
    fn migrates_legacy_normalization_booleans_to_ordered_processor_lists() {
        let config: TextHookConfig = serde_json::from_str(
            r#"{"enabled":true,"normalize_before_model":true,"normalize_after_model":false}"#,
        )
        .unwrap();
        assert_eq!(config.pre_processors, vec![NORMALIZE_WHITESPACE_PROCESSOR]);
        assert!(config.post_processors.is_empty());
        let serialized = serde_json::to_string(&config).unwrap();
        assert!(!serialized.contains("normalize_before_model"));
    }

    #[test]
    fn blank_labels_are_not_passed_to_the_model() {
        let config = TextHookConfig {
            label: "  \t ".into(),
            ..Default::default()
        };
        assert_eq!(config.label(), None);
    }
}
