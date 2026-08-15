use std::collections::{BTreeMap, VecDeque};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::Parser;
use relm4::gtk;
use relm4::gtk::prelude::*;
use relm4::prelude::*;
use secrecy::SecretString;
use terratranslate_core::{
    ContextSnapshot, ModelMetadata, NormalizeWhitespace, ScratchpadAuthor, ScratchpadEdit,
    SourceKind, TranslationCommit,
};
use terratranslate_engine::{
    TextInputOptions, TextProcessingSelection, TranslationEngine, TurnInput, TurnRequest,
};
use terratranslate_platform_linux::{
    DesktopCapabilities, DisplayServer, NativeApplication, NativeLaunchRequest,
    NativeTextHookEvent, NativeTextHookService, PortalFrameReceiver, PortalShortcutSession,
    PortalStream, ShortcutBinding, VisionFrameCache, VisionFrameCacheConfig, WindowCaptureSession,
    WineArtifacts, WineHookEvent, WineHookService, WineTarget, attach_wine_target,
    discover_wine_targets, launch_native, list_native_applications, parse_native_arguments,
    register_shortcuts, select_window, steam_launch_option,
};
use terratranslate_provider::{ModelCapabilities, ModelInput, OpenAiCompatibleProvider};
use terratranslate_store::SessionStore;
use terratranslate_wine_protocol::{HookBridgeConfig, HookRuntime};
use uuid::Uuid;

mod hud;
mod text_hooks;

use hud::{HudAppearance, HudWindow, available_layer_shell_library, wayland_overlay_requested};
use text_hooks::{TextHookConfig, TextHookInit, TextHookRow, TextHookRowInput, TextHookRowOutput};

const LAYER_SHELL_PRELOAD_ATTEMPTED: &str = "TERRATRANSLATE_LAYER_SHELL_PRELOAD_ATTEMPTED";

#[derive(Parser)]
#[command(
    version,
    about = "Native multimodal translation harness for Linux and Wine/Proton"
)]
struct Arguments {
    /// Print detected desktop capabilities as JSON and exit.
    #[arg(long)]
    diagnostics: bool,
    /// Override the application data directory.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Reuse the last captured frame when its SSIM score reaches this value (0.0-1.0).
    #[arg(long, value_name = "THRESHOLD")]
    vision_similarity_threshold: Option<f32>,
}

struct AppInit {
    store: SessionStore,
    data_dir: PathBuf,
    capabilities: DesktopCapabilities,
    hud_appearance: HudAppearance,
    hud_appearance_path: PathBuf,
    wine_hook_service: WineHookService,
    wine_bridge_config_path: PathBuf,
    native_text_hook_service: NativeTextHookService,
    model_settings: ModelSettings,
    model_settings_path: PathBuf,
    text_hook_configs: BTreeMap<String, TextHookConfig>,
    text_hook_configs_path: PathBuf,
    native_preload_path: PathBuf,
    wine_artifacts: WineArtifacts,
    vision_cache_config: VisionFrameCacheConfig,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(default)]
struct ModelSettings {
    endpoint: String,
    model: String,
    source_language: String,
    target_language: String,
    system_prompt: String,
}

impl Default for ModelSettings {
    fn default() -> Self {
        Self {
            endpoint: "http://127.0.0.1:11434/v1".into(),
            model: String::new(),
            source_language: String::new(),
            target_language: "English".into(),
            system_prompt: "Translate faithfully while preserving character voice.".into(),
        }
    }
}

#[derive(Clone, Debug)]
struct PendingHookText {
    hook_id: String,
    captured_at_ms: i64,
    source: SourceKind,
    target: String,
    text: String,
    config: TextHookConfig,
}

struct AppModel {
    store: SessionStore,
    data_dir: PathBuf,
    status: String,
    branch_input: String,
    scratchpad_input: String,
    branches: String,
    active_branch: String,
    capture_streams: Vec<PortalStream>,
    capture_session: Option<WindowCaptureSession>,
    frame_receiver: Option<PortalFrameReceiver>,
    vision_cache: VisionFrameCache,
    shortcut_session: Option<PortalShortcutSession>,
    shortcut_registration_pending: bool,
    frame_encoding: bool,
    hud: HudWindow,
    hud_positioning: bool,
    hud_visible: bool,
    hud_appearance: HudAppearance,
    hud_appearance_path: PathBuf,
    wine_hook_service: WineHookService,
    wine_bridge_config_path: PathBuf,
    wine_hook_status: String,
    wine_attach_available: bool,
    wine_artifacts: WineArtifacts,
    wine_targets: Vec<WineTarget>,
    wine_target_index: String,
    wine_targets_display: String,
    native_text_hook_service: NativeTextHookService,
    native_application_id: String,
    native_hook_status: String,
    native_applications: String,
    native_launch_available: bool,
    native_preload_path: PathBuf,
    native_launch_executable: String,
    native_launch_arguments: String,
    native_launch_working_directory: String,
    native_launch_status: String,
    launched_native_processes: Vec<std::process::Child>,
    text_hooks: relm4::factory::FactoryVecDeque<TextHookRow>,
    text_hook_configs: BTreeMap<String, TextHookConfig>,
    text_hook_indices: BTreeMap<String, usize>,
    hook_routes: BTreeMap<String, (Uuid, Uuid)>,
    text_hook_configs_path: PathBuf,
    pending_hook_text: VecDeque<PendingHookText>,
    translation_pending: bool,
    model_settings: ModelSettings,
    model_settings_path: PathBuf,
}

#[derive(Debug)]
enum AppMsg {
    SelectWindow,
    RegisterShortcut,
    ToggleHudPositioning,
    ToggleHudVisibility,
    HudVisibilityChanged(bool),
    HudBackgroundColorChanged(String),
    HudTextColorChanged(String),
    HudOpacityChanged(f64),
    HudFontFamilyChanged(String),
    HudFontSizeChanged(f64),
    BranchInput(String),
    CreateBranch,
    ScratchpadInput(String),
    CommitScratchpad,
    PollFrame,
    PollWineHook,
    RefreshWineTargets,
    WineTargetIndex(String),
    AttachWineTarget,
    DetachHooks,
    NativeApplicationId(String),
    PollNativeTextHook,
    RefreshNativeApplications,
    NativeLaunchExecutable(String),
    NativeLaunchArguments(String),
    NativeLaunchWorkingDirectory(String),
    LaunchNative,
    TextHookConfigChanged(String, TextHookConfig),
    ForgetTextHook(String),
    ModelEndpoint(String),
    ModelName(String),
    SourceLanguage(String),
    TargetLanguage(String),
}

#[derive(Debug)]
enum CommandOutput {
    Capture(Result<(WindowCaptureSession, PortalFrameReceiver), String>),
    Shortcut(Result<PortalShortcutSession, String>),
    FrameEncoded {
        result: Result<EncodedFrame, String>,
        cache: VisionFrameCache,
    },
    NativeApplications(Result<Vec<NativeApplication>, String>),
    WineTargets(Result<Vec<WineTarget>, String>),
    WineAttached(Result<String, String>),
    Translation {
        hook_ids: Vec<String>,
        result: Box<Result<TranslationCommit, String>>,
    },
}

#[derive(Debug)]
struct EncodedFrame {
    width: u32,
    height: u32,
    format: String,
    png_bytes: usize,
}

#[relm4::component]
impl Component for AppModel {
    type Init = AppInit;
    type Input = AppMsg;
    type Output = ();
    type CommandOutput = CommandOutput;

    view! {
        gtk::ApplicationWindow {
            set_title: Some("TerraTranslate"),
            set_default_size: (1100, 800),

            gtk::Box {
                set_orientation: gtk::Orientation::Vertical,
                set_spacing: 10,
                set_margin_all: 14,

                gtk::Box {
                    set_orientation: gtk::Orientation::Horizontal,
                    set_spacing: 8,

                    gtk::Label {
                        set_markup: "<span size='x-large' weight='bold'>TerraTranslate</span>",
                        set_hexpand: true,
                        set_halign: gtk::Align::Start,
                    },
                    gtk::Button {
                        set_label: "Select window",
                        connect_clicked => AppMsg::SelectWindow,
                    },
                    gtk::Button {
                        set_label: "Claim shortcut",
                        #[watch]
                        set_sensitive: !model.shortcut_registration_pending && model.shortcut_session.is_none(),
                        connect_clicked => AppMsg::RegisterShortcut,
                    },
                    gtk::Button {
                        #[watch]
                        set_label: if model.hud_positioning { "Use as overlay" } else { "Position HUD" },
                        #[watch]
                        set_sensitive: model.hud.supports_positioning(),
                        #[watch]
                        set_tooltip_text: if model.hud.supports_positioning() {
                            Some("Toggle the HUD frame and resize controls")
                        } else {
                            Some("Wayland layer surfaces are positioned by the compositor")
                        },
                        connect_clicked => AppMsg::ToggleHudPositioning,
                    },
                    gtk::Button {
                        #[watch]
                        set_label: if model.hud_visible { "Hide HUD" } else { "Show HUD" },
                        connect_clicked => AppMsg::ToggleHudVisibility,
                    },
                },

                gtk::Label {
                    #[watch]
                    set_label: &model.status,
                    set_wrap: true,
                    set_halign: gtk::Align::Start,
                    add_css_class: "dim-label",
                },

                gtk::Frame {
                    set_label: Some("HUD appearance"),
                    gtk::Box {
                        set_orientation: gtk::Orientation::Horizontal,
                        set_spacing: 8,
                        set_margin_all: 8,

                        gtk::Label {
                            set_label: "Background",
                        },
                        gtk::Entry {
                            set_width_chars: 8,
                            set_text: &model.hud_appearance.background_color,
                            set_tooltip_text: Some("Hex color, for example #1e1e2e"),
                            connect_changed[sender] => move |entry| {
                                sender.input(AppMsg::HudBackgroundColorChanged(entry.text().to_string()));
                            },
                        },
                        gtk::Label {
                            set_label: "Text",
                        },
                        gtk::Entry {
                            set_width_chars: 8,
                            set_text: &model.hud_appearance.text_color,
                            set_tooltip_text: Some("Hex color, for example #ffffff"),
                            connect_changed[sender] => move |entry| {
                                sender.input(AppMsg::HudTextColorChanged(entry.text().to_string()));
                            },
                        },
                        gtk::Label {
                            set_label: "Transparency",
                        },
                        gtk::Scale {
                            set_orientation: gtk::Orientation::Horizontal,
                            set_range: (0.0, 100.0),
                            set_value: (1.0 - model.hud_appearance.background_opacity) * 100.0,
                            set_digits: 0,
                            set_draw_value: true,
                            set_width_request: 150,
                            set_hexpand: true,
                            set_tooltip_text: Some("0% is opaque; 100% is fully transparent"),
                            connect_value_changed[sender] => move |scale| {
                                sender.input(AppMsg::HudOpacityChanged(1.0 - scale.value() / 100.0));
                            },
                        },
                        gtk::Label {
                            set_label: "Font",
                        },
                        gtk::Entry {
                            set_width_chars: 12,
                            set_text: &model.hud_appearance.font_family,
                            set_tooltip_text: Some("Installed font family, for example Sans"),
                            connect_changed[sender] => move |entry| {
                                sender.input(AppMsg::HudFontFamilyChanged(entry.text().to_string()));
                            },
                        },
                        gtk::Label {
                            set_label: "Size (pt)",
                        },
                        gtk::SpinButton {
                            set_range: (6.0, 96.0),
                            set_increments: (1.0, 4.0),
                            set_value: model.hud_appearance.font_size_pt,
                            set_numeric: true,
                            connect_value_changed[sender] => move |spin_button| {
                                sender.input(AppMsg::HudFontSizeChanged(spin_button.value()));
                            },
                        },
                    },
                },

                gtk::Paned {
                    set_orientation: gtk::Orientation::Horizontal,
                    set_position: 300,
                    set_vexpand: true,

                    #[wrap(Some)]
                    set_start_child = &gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        set_spacing: 8,
                        set_margin_end: 8,

                        gtk::Label {
                            set_markup: "<b>Translation history</b>",
                            set_halign: gtk::Align::Start,
                        },
                        gtk::Label {
                            #[watch]
                            set_label: &model.branches,
                            set_selectable: true,
                            set_halign: gtk::Align::Start,
                            set_valign: gtk::Align::Start,
                            set_vexpand: true,
                        },
                        gtk::Entry {
                            set_placeholder_text: Some("New branch name"),
                            connect_changed[sender] => move |entry| {
                                sender.input(AppMsg::BranchInput(entry.text().to_string()));
                            },
                        },
                        gtk::Button {
                            set_label: "Branch from current head",
                            connect_clicked => AppMsg::CreateBranch,
                        },
                    },

                    #[wrap(Some)]
                    set_end_child = &gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        set_spacing: 8,
                        set_margin_start: 8,

                        gtk::Label {
                            set_markup: "<b>Multimodal session</b>",
                            set_halign: gtk::Align::Start,
                        },
                        gtk::Frame {
                            set_label: Some("Model"),
                            gtk::Grid {
                                set_column_spacing: 8,
                                set_row_spacing: 6,
                                set_margin_all: 8,

                                attach[0, 0, 1, 1] = &gtk::Label {
                                    set_label: "Endpoint",
                                    set_halign: gtk::Align::End,
                                },
                                attach[1, 0, 1, 1] = &gtk::Entry {
                                    set_hexpand: true,
                                    set_text: &model.model_settings.endpoint,
                                    connect_changed[sender] => move |entry| {
                                        sender.input(AppMsg::ModelEndpoint(entry.text().to_string()));
                                    },
                                },
                                attach[0, 1, 1, 1] = &gtk::Label {
                                    set_label: "Model",
                                    set_halign: gtk::Align::End,
                                },
                                attach[1, 1, 1, 1] = &gtk::Entry {
                                    set_hexpand: true,
                                    set_placeholder_text: Some("Required model name"),
                                    set_text: &model.model_settings.model,
                                    connect_changed[sender] => move |entry| {
                                        sender.input(AppMsg::ModelName(entry.text().to_string()));
                                    },
                                },
                                attach[2, 0, 1, 1] = &gtk::Label {
                                    set_label: "Source language",
                                    set_halign: gtk::Align::End,
                                },
                                attach[3, 0, 1, 1] = &gtk::Entry {
                                    set_placeholder_text: Some("Source: auto-detect"),
                                    set_text: &model.model_settings.source_language,
                                    connect_changed[sender] => move |entry| {
                                        sender.input(AppMsg::SourceLanguage(entry.text().to_string()));
                                    },
                                },
                                attach[2, 1, 1, 1] = &gtk::Label {
                                    set_label: "Target language",
                                    set_halign: gtk::Align::End,
                                },
                                attach[3, 1, 1, 1] = &gtk::Entry {
                                    set_placeholder_text: Some("Target language"),
                                    set_text: &model.model_settings.target_language,
                                    connect_changed[sender] => move |entry| {
                                        sender.input(AppMsg::TargetLanguage(entry.text().to_string()));
                                    },
                                },
                            },
                        },
                        gtk::Frame {
                            set_label: Some("Source"),
                            set_vexpand: true,
                            gtk::Box {
                                set_orientation: gtk::Orientation::Vertical,
                                set_spacing: 6,
                                set_margin_all: 12,
                                gtk::Label {
                                    #[watch]
                                    set_label: &if model.capture_streams.is_empty() {
                                        "Choose a window to establish a direct PipeWire frame stream.".to_owned()
                                    } else {
                                        model.capture_streams.iter().map(|stream| format!(
                                            "PipeWire node {} — {:?} at {:?}", stream.pipewire_node_id, stream.size, stream.position
                                        )).collect::<Vec<_>>().join("\n")
                                    },
                                    set_wrap: true,
                                    set_halign: gtk::Align::Start,
                                },
                                gtk::Frame {
                                    set_label: Some("Launch native application with semantic hooks"),
                                    gtk::Grid {
                                        set_column_spacing: 8,
                                        set_row_spacing: 6,
                                        set_margin_all: 8,

                                        attach[0, 0, 1, 1] = &gtk::Label {
                                            set_label: "Executable",
                                            set_halign: gtk::Align::End,
                                        },
                                        attach[1, 0, 3, 1] = &gtk::Entry {
                                            set_hexpand: true,
                                            set_placeholder_text: Some("/path/to/application"),
                                            #[watch]
                                            set_sensitive: model.native_launch_available,
                                            connect_changed[sender] => move |entry| {
                                                sender.input(AppMsg::NativeLaunchExecutable(entry.text().to_string()));
                                            },
                                        },
                                        attach[0, 1, 1, 1] = &gtk::Label {
                                            set_label: "Arguments",
                                            set_halign: gtk::Align::End,
                                        },
                                        attach[1, 1, 3, 1] = &gtk::Entry {
                                            set_placeholder_text: Some("Quoted arguments are supported; no shell is invoked"),
                                            #[watch]
                                            set_sensitive: model.native_launch_available,
                                            connect_changed[sender] => move |entry| {
                                                sender.input(AppMsg::NativeLaunchArguments(entry.text().to_string()));
                                            },
                                        },
                                        attach[0, 2, 1, 1] = &gtk::Label {
                                            set_label: "Working directory",
                                            set_halign: gtk::Align::End,
                                        },
                                        attach[1, 2, 2, 1] = &gtk::Entry {
                                            set_placeholder_text: Some("Defaults to the executable directory"),
                                            #[watch]
                                            set_sensitive: model.native_launch_available,
                                            connect_changed[sender] => move |entry| {
                                                sender.input(AppMsg::NativeLaunchWorkingDirectory(entry.text().to_string()));
                                            },
                                        },
                                        attach[3, 2, 1, 1] = &gtk::Button {
                                            set_label: "Launch",
                                            #[watch]
                                            set_sensitive: model.native_launch_available && !model.native_launch_executable.trim().is_empty(),
                                            connect_clicked => AppMsg::LaunchNative,
                                        },
                                        attach[0, 3, 4, 1] = &gtk::Label {
                                            #[watch]
                                            set_label: &model.native_launch_status,
                                            set_selectable: true,
                                            set_wrap: true,
                                            set_halign: gtk::Align::Start,
                                            add_css_class: "dim-label",
                                        },
                                    },
                                },
                                gtk::Label {
                                    #[watch]
                                    set_label: &model.wine_hook_status,
                                    set_wrap: true,
                                    set_halign: gtk::Align::Start,
                                    add_css_class: "dim-label",
                                },
                                gtk::Box {
                                    set_orientation: gtk::Orientation::Horizontal,
                                    set_spacing: 8,
                                    gtk::Button {
                                        set_label: "Refresh Wine targets",
                                        #[watch]
                                        set_sensitive: model.wine_attach_available,
                                        connect_clicked => AppMsg::RefreshWineTargets,
                                    },
                                    gtk::Entry {
                                        set_width_chars: 5,
                                        set_placeholder_text: Some("Row"),
                                        set_tooltip_text: Some("One-based row number from the Wine target list"),
                                        #[watch]
                                        set_sensitive: model.wine_attach_available,
                                        connect_changed[sender] => move |entry| {
                                            sender.input(AppMsg::WineTargetIndex(entry.text().to_string()));
                                        },
                                    },
                                    gtk::Button {
                                        set_label: "Attach selected Wine target",
                                        #[watch]
                                        set_sensitive: model.wine_attach_available && !model.wine_targets.is_empty(),
                                        connect_clicked => AppMsg::AttachWineTarget,
                                    },
                                    gtk::Button {
                                        set_label: "Detach hooks",
                                        #[watch]
                                        set_sensitive: !model.hook_routes.is_empty(),
                                        connect_clicked => AppMsg::DetachHooks,
                                    },
                                },
                                gtk::Label {
                                    #[watch]
                                    set_label: &model.wine_targets_display,
                                    set_selectable: true,
                                    set_wrap: true,
                                    set_halign: gtk::Align::Start,
                                    add_css_class: "dim-label",
                                },
                                gtk::Entry {
                                    set_placeholder_text: Some("Native AT-SPI application ID"),
                                    set_tooltip_text: Some("The selected native application's AT-SPI unique bus name"),
                                    connect_changed[sender] => move |entry| {
                                        sender.input(AppMsg::NativeApplicationId(entry.text().to_string()));
                                    },
                                },
                                gtk::Button {
                                    set_label: "List native applications",
                                    connect_clicked => AppMsg::RefreshNativeApplications,
                                },
                                gtk::Label {
                                    #[watch]
                                    set_label: &model.native_applications,
                                    set_selectable: true,
                                    set_wrap: true,
                                    set_halign: gtk::Align::Start,
                                    add_css_class: "dim-label",
                                },
                                gtk::Label {
                                    #[watch]
                                    set_label: &model.native_hook_status,
                                    set_wrap: true,
                                    set_halign: gtk::Align::Start,
                                    add_css_class: "dim-label",
                                },
                                gtk::Label {
                                    set_markup: "<b>Discovered text hooks</b>",
                                    set_halign: gtk::Align::Start,
                                },
                                gtk::Label {
                                    set_label: "Enable any number of hooks. Labels and normalization are configured independently for each hook.",
                                    set_wrap: true,
                                    set_halign: gtk::Align::Start,
                                    add_css_class: "dim-label",
                                },
                                gtk::ScrolledWindow {
                                    set_min_content_height: 180,
                                    set_vexpand: true,
                                    set_hscrollbar_policy: gtk::PolicyType::Never,

                                    #[local_ref]
                                    text_hook_list -> gtk::ListBox {
                                        set_selection_mode: gtk::SelectionMode::None,
                                    },
                                },
                            },
                        },
                        gtk::Frame {
                            set_label: Some("Translation HUD preview"),
                            set_vexpand: true,
                            gtk::Label {
                                set_markup: "<span size='large'>Translations will stream here.</span>",
                                set_wrap: true,
                                set_margin_all: 12,
                            },
                        },
                        gtk::Label {
                            set_markup: "<b>Versioned scratchpad</b>",
                            set_halign: gtk::Align::Start,
                        },
                        gtk::Entry {
                            set_placeholder_text: Some("Model and user notes are committed to the DAG"),
                            connect_changed[sender] => move |entry| {
                                sender.input(AppMsg::ScratchpadInput(entry.text().to_string()));
                            },
                        },
                        gtk::Button {
                            set_label: "Commit user scratchpad edit",
                            connect_clicked => AppMsg::CommitScratchpad,
                        },
                    },
                },
            }
        }
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let display = format!("{:?}", init.capabilities.display_server);
        let native_launch_available = init.capabilities.native_preload_launch_possible;
        let wine_attach_available = init.capabilities.wine_attach_possible;
        let native_launch_status = if native_launch_available {
            format!(
                "Preload: {}\nSteam launch option: {}",
                init.native_preload_path.display(),
                steam_launch_option(&init.native_preload_path, &init.wine_bridge_config_path)
            )
        } else {
            "Native semantic hooks are unavailable in Flatpak. Use the host build; AT-SPI and window vision remain available.".into()
        };
        let hud = HudWindow::new(&root, &init.capabilities, &init.hud_appearance);
        let hud_positioning = hud.supports_positioning();
        let hud_visible = hud.is_visible();
        let visibility_input = sender.input_sender().clone();
        hud.connect_visible_changed(move |visible| {
            let _ = visibility_input.send(AppMsg::HudVisibilityChanged(visible));
        });
        let text_hooks = relm4::factory::FactoryVecDeque::builder()
            .launch_default()
            .forward(sender.input_sender(), |output| match output {
                TextHookRowOutput::ConfigChanged { id, config } => {
                    AppMsg::TextHookConfigChanged(id, config)
                }
                TextHookRowOutput::Forget(id) => AppMsg::ForgetTextHook(id),
            });
        let mut model = Self {
            store: init.store,
            data_dir: init.data_dir,
            status: format!("Desktop: {display}. Ready."),
            branch_input: String::new(),
            scratchpad_input: String::new(),
            branches: String::new(),
            active_branch: "main".into(),
            capture_streams: Vec::new(),
            capture_session: None,
            frame_receiver: None,
            vision_cache: VisionFrameCache::new(init.vision_cache_config),
            shortcut_session: None,
            shortcut_registration_pending: false,
            frame_encoding: false,
            hud,
            hud_positioning,
            hud_visible,
            hud_appearance: init.hud_appearance,
            hud_appearance_path: init.hud_appearance_path,
            wine_hook_service: init.wine_hook_service,
            wine_hook_status: format!(
                "Wine text hook listening. Configure the injected bridge with {}.",
                init.wine_bridge_config_path.display()
            ),
            wine_attach_available,
            wine_artifacts: init.wine_artifacts,
            wine_targets: Vec::new(),
            wine_target_index: String::new(),
            wine_targets_display: if wine_attach_available {
                "Refresh to discover active Wine/Proton prefixes and Windows processes.".into()
            } else {
                "Wine attachment is unavailable in Flatpak; use the host build.".into()
            },
            native_text_hook_service: init.native_text_hook_service,
            native_application_id: String::new(),
            native_hook_status: "Native text hook is connecting to AT-SPI. Enter an application ID to enable capture.".into(),
            native_applications: String::new(),
            native_launch_available,
            native_preload_path: init.native_preload_path,
            native_launch_executable: String::new(),
            native_launch_arguments: String::new(),
            native_launch_working_directory: String::new(),
            native_launch_status,
            launched_native_processes: Vec::new(),
            wine_bridge_config_path: init.wine_bridge_config_path,
            text_hooks,
            text_hook_configs: init.text_hook_configs,
            text_hook_indices: BTreeMap::new(),
            hook_routes: BTreeMap::new(),
            text_hook_configs_path: init.text_hook_configs_path,
            pending_hook_text: VecDeque::new(),
            translation_pending: false,
            model_settings: init.model_settings,
            model_settings_path: init.model_settings_path,
        };
        for (id, config) in model.text_hook_configs.clone() {
            let title = if config.title.is_empty() {
                id.clone()
            } else {
                config.title.clone()
            };
            let detail = if config.detail.is_empty() {
                "Saved hook is not connected".into()
            } else {
                config.detail.clone()
            };
            let index = model
                .text_hooks
                .guard()
                .push_back(TextHookInit {
                    id: id.clone(),
                    title,
                    detail,
                    sample: String::new(),
                    available: false,
                    config,
                })
                .current_index();
            model.text_hook_indices.insert(id, index);
        }
        model.refresh_branches();
        let text_hook_list = model.text_hooks.widget();
        let widgets = view_output!();
        let input = sender.input_sender().clone();
        gtk::glib::timeout_add_local(Duration::from_millis(250), move || {
            match input.send(AppMsg::PollFrame) {
                Ok(()) => gtk::glib::ControlFlow::Continue,
                Err(_) => gtk::glib::ControlFlow::Break,
            }
        });
        let input = sender.input_sender().clone();
        gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
            match input.send(AppMsg::PollWineHook) {
                Ok(()) => gtk::glib::ControlFlow::Continue,
                Err(_) => gtk::glib::ControlFlow::Break,
            }
        });
        let input = sender.input_sender().clone();
        gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
            match input.send(AppMsg::PollNativeTextHook) {
                Ok(()) => gtk::glib::ControlFlow::Continue,
                Err(_) => gtk::glib::ControlFlow::Break,
            }
        });
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>, _root: &Self::Root) {
        match msg {
            AppMsg::SelectWindow => {
                self.status = "Waiting for the desktop window picker…".into();
                sender.oneshot_command(async {
                    let result = async {
                        let session = select_window().await.map_err(|error| error.to_string())?;
                        let node_id = session
                            .streams()
                            .first()
                            .ok_or_else(|| "the portal returned no video node".to_owned())?
                            .pipewire_node_id;
                        let remote = session
                            .open_pipewire_remote()
                            .await
                            .map_err(|error| error.to_string())?;
                        let receiver = PortalFrameReceiver::spawn(remote, node_id)
                            .map_err(|error| error.to_string())?;
                        Ok((session, receiver))
                    }
                    .await;
                    CommandOutput::Capture(result)
                });
            }
            AppMsg::RegisterShortcut => {
                if self.shortcut_registration_pending || self.shortcut_session.is_some() {
                    return;
                }
                self.shortcut_registration_pending = true;
                self.status = "Waiting for compositor shortcut approval…".into();
                sender.oneshot_command(async {
                    let bindings = [ShortcutBinding {
                        id: "translate-current".into(),
                        description: "Translate the current target frame".into(),
                        preferred_trigger: "<Control><Shift>T".into(),
                    }];
                    CommandOutput::Shortcut(
                        register_shortcuts(&bindings)
                            .await
                            .map_err(|error| error.to_string()),
                    )
                });
            }
            AppMsg::ToggleHudPositioning => {
                let positioning = !self.hud_positioning;
                if self.hud.set_positioning(positioning) {
                    self.hud_positioning = positioning;
                }
            }
            AppMsg::ToggleHudVisibility => {
                if self.hud_visible {
                    self.hud.hide();
                } else {
                    self.hud.present();
                }
            }
            AppMsg::HudVisibilityChanged(visible) => self.hud_visible = visible,
            AppMsg::HudBackgroundColorChanged(value) => self.change_hud_appearance(|appearance| {
                appearance.background_color = value;
            }),
            AppMsg::HudTextColorChanged(value) => self.change_hud_appearance(|appearance| {
                appearance.text_color = value;
            }),
            AppMsg::HudOpacityChanged(value) => self.change_hud_appearance(|appearance| {
                appearance.background_opacity = value;
            }),
            AppMsg::HudFontFamilyChanged(value) => self.change_hud_appearance(|appearance| {
                appearance.font_family = value;
            }),
            AppMsg::HudFontSizeChanged(value) => self.change_hud_appearance(|appearance| {
                appearance.font_size_pt = value;
            }),
            AppMsg::BranchInput(value) => self.branch_input = value,
            AppMsg::CreateBranch => {
                let result = self.store.branch(&self.active_branch).and_then(|branch| {
                    self.store
                        .create_branch(&self.branch_input, &branch.head, now_ms())
                });
                self.status = match result {
                    Ok(branch) => format!(
                        "Created branch {} at {}",
                        branch.name,
                        short_id(&branch.head.0)
                    ),
                    Err(error) => format!("Could not create branch: {error}"),
                };
                self.refresh_branches();
            }
            AppMsg::ScratchpadInput(value) => self.scratchpad_input = value,
            AppMsg::CommitScratchpad => self.commit_scratchpad(),
            AppMsg::PollFrame => {
                self.launched_native_processes.retain_mut(|child| {
                    child
                        .try_wait()
                        .map(|status| status.is_none())
                        .unwrap_or(true)
                });
                if !self.frame_encoding
                    && let Some(receiver) = &self.frame_receiver
                    && let Ok(frame) = receiver.try_recv_latest()
                {
                    let cache = std::mem::take(&mut self.vision_cache);
                    self.frame_encoding = true;
                    sender.spawn_oneshot_command(move || {
                        let mut cache = cache;
                        let frame = cache.select(frame);
                        let width = frame.width;
                        let height = frame.height;
                        let format = format!("{:?}", frame.format);
                        let result = frame
                            .encode_png()
                            .map(|png| EncodedFrame {
                                width,
                                height,
                                format,
                                png_bytes: png.len(),
                            })
                            .map_err(|error| error.to_string());
                        CommandOutput::FrameEncoded { result, cache }
                    });
                }
            }
            AppMsg::PollWineHook => self.poll_wine_hook(&sender),
            AppMsg::RefreshWineTargets => {
                self.wine_hook_status = "Discovering active Wine/Proton processes…".into();
                sender.spawn_oneshot_command(move || {
                    CommandOutput::WineTargets(
                        discover_wine_targets().map_err(|error| error.to_string()),
                    )
                });
            }
            AppMsg::WineTargetIndex(value) => self.wine_target_index = value,
            AppMsg::AttachWineTarget => {
                let index = self
                    .wine_target_index
                    .trim()
                    .parse::<usize>()
                    .ok()
                    .and_then(|index| index.checked_sub(1));
                let Some(target) = index
                    .and_then(|index| self.wine_targets.get(index))
                    .cloned()
                else {
                    self.wine_hook_status = "Enter a valid Wine target row number.".into();
                    return;
                };
                self.wine_hook_status = format!(
                    "Attaching to {} (PID {})…",
                    target.executable, target.process_id
                );
                let artifacts = self.wine_artifacts.clone();
                let config = self.wine_bridge_config_path.clone();
                sender.spawn_oneshot_command(move || {
                    let description = format!("{} (PID {})", target.executable, target.process_id);
                    CommandOutput::WineAttached(
                        attach_wine_target(&target, &artifacts, &config)
                            .map(|()| description)
                            .map_err(|error| error.to_string()),
                    )
                });
            }
            AppMsg::DetachHooks => {
                let bridges = self
                    .hook_routes
                    .values()
                    .map(|(bridge_id, _)| *bridge_id)
                    .collect::<std::collections::BTreeSet<_>>();
                for bridge_id in bridges {
                    let _ = self.wine_hook_service.shutdown(bridge_id);
                }
                self.hook_routes.clear();
                for index in self.text_hook_indices.values().copied() {
                    self.text_hooks
                        .guard()
                        .send(index, TextHookRowInput::Availability(false));
                }
                self.wine_hook_status =
                    "Detached semantic hooks. Injected libraries are inert until process exit."
                        .into();
            }
            AppMsg::NativeApplicationId(application_id) => {
                self.native_application_id = application_id;
                self.native_text_hook_service
                    .select_application(Some(self.native_application_id.clone()));
                self.native_hook_status = if self.native_application_id.trim().is_empty() {
                    "Native text capture disabled until an AT-SPI application ID is selected."
                        .into()
                } else {
                    format!("Native text hook armed for {}.", self.native_application_id)
                };
            }
            AppMsg::PollNativeTextHook => self.poll_native_text_hook(&sender),
            AppMsg::RefreshNativeApplications => sender.oneshot_command(async {
                CommandOutput::NativeApplications(
                    list_native_applications()
                        .await
                        .map_err(|error| error.to_string()),
                )
            }),
            AppMsg::NativeLaunchExecutable(value) => self.native_launch_executable = value,
            AppMsg::NativeLaunchArguments(value) => self.native_launch_arguments = value,
            AppMsg::NativeLaunchWorkingDirectory(value) => {
                self.native_launch_working_directory = value
            }
            AppMsg::LaunchNative => {
                let result = parse_native_arguments(&self.native_launch_arguments)
                    .map_err(|error| error.to_string())
                    .and_then(|arguments| {
                        let request = NativeLaunchRequest {
                            executable: PathBuf::from(self.native_launch_executable.trim()),
                            arguments,
                            working_directory: (!self
                                .native_launch_working_directory
                                .trim()
                                .is_empty())
                            .then(|| PathBuf::from(self.native_launch_working_directory.trim())),
                            preload_library: self.native_preload_path.clone(),
                            hook_config: self.wine_bridge_config_path.clone(),
                        };
                        launch_native(&request).map_err(|error| error.to_string())
                    });
                match result {
                    Ok(child) => {
                        self.native_launch_status = format!(
                            "Launched {} with semantic hooks (PID {}).",
                            self.native_launch_executable,
                            child.id()
                        );
                        self.launched_native_processes.push(child);
                    }
                    Err(error) => {
                        self.native_launch_status = format!("Native launch unavailable: {error}");
                    }
                }
            }
            AppMsg::TextHookConfigChanged(id, config) => {
                if let Some((bridge_id, candidate_id)) = self.hook_routes.get(&id).copied() {
                    let control = if config.enabled {
                        self.wine_hook_service
                            .enable_candidate(bridge_id, candidate_id)
                    } else {
                        self.wine_hook_service
                            .disable_candidate(bridge_id, candidate_id)
                    };
                    if let Err(error) = control {
                        self.status = format!("Could not update hook producer: {error}");
                    }
                }
                self.text_hook_configs.insert(id, config);
                if let Err(error) = save_json(
                    &self.text_hook_configs_path,
                    &self.text_hook_configs,
                    "text hook settings",
                ) {
                    self.status = format!("Text hook settings could not be saved: {error}");
                }
            }
            AppMsg::ForgetTextHook(id) => {
                if let Some((bridge_id, candidate_id)) = self.hook_routes.remove(&id) {
                    let _ = self
                        .wine_hook_service
                        .disable_candidate(bridge_id, candidate_id);
                }
                self.text_hook_configs.remove(&id);
                if let Some(index) = self.text_hook_indices.remove(&id) {
                    self.text_hooks.guard().remove(index);
                    for current in self.text_hook_indices.values_mut() {
                        if *current > index {
                            *current -= 1;
                        }
                    }
                }
                if let Err(error) = save_json(
                    &self.text_hook_configs_path,
                    &self.text_hook_configs,
                    "text hook settings",
                ) {
                    self.status = format!("Text hook settings could not be saved: {error}");
                }
            }
            AppMsg::ModelEndpoint(value) => {
                self.model_settings.endpoint = value;
                self.save_model_settings();
            }
            AppMsg::ModelName(value) => {
                self.model_settings.model = value;
                self.save_model_settings();
            }
            AppMsg::SourceLanguage(value) => {
                self.model_settings.source_language = value;
                self.save_model_settings();
            }
            AppMsg::TargetLanguage(value) => {
                self.model_settings.target_language = value;
                self.save_model_settings();
            }
        }
    }

    fn update_cmd(
        &mut self,
        msg: Self::CommandOutput,
        _sender: ComponentSender<Self>,
        _root: &Self::Root,
    ) {
        match msg {
            CommandOutput::Capture(Ok((session, receiver))) => {
                let streams = session.streams().to_vec();
                self.status = format!(
                    "Capture permission active for {} PipeWire stream(s).",
                    streams.len()
                );
                self.hud.set_message(
                    "Translations will stream here.\n\nCapture permission is active; hook text and application audio will join the next translated turn.",
                );
                self.capture_streams = streams;
                self.capture_session = Some(session);
                self.frame_receiver = Some(receiver);
            }
            CommandOutput::Capture(Err(error)) => {
                self.status = format!("Window capture unavailable: {error}")
            }
            CommandOutput::Shortcut(Ok(session)) => {
                self.shortcut_registration_pending = false;
                self.status = format!("Claimed shortcuts: {}", session.accepted_ids().join(", "));
                self.shortcut_session = Some(session);
            }
            CommandOutput::Shortcut(Err(error)) => {
                self.shortcut_registration_pending = false;
                self.status = format!("Shortcut unavailable: {error}")
            }
            CommandOutput::FrameEncoded { result, cache } => {
                self.vision_cache = cache;
                self.frame_encoding = false;
                match result {
                    Ok(frame) => {
                        self.status = format!(
                            "Live capture: {}×{} {} frame encoded to {} bytes of PNG model input.",
                            frame.width, frame.height, frame.format, frame.png_bytes
                        );
                    }
                    Err(error) => {
                        self.status = format!("Could not encode captured frame: {error}");
                    }
                }
            }
            CommandOutput::NativeApplications(Ok(applications)) => {
                self.native_applications = if applications.is_empty() {
                    "No native applications are currently exposed through AT-SPI.".into()
                } else {
                    applications
                        .into_iter()
                        .map(|application| format!("{} — {}", application.name, application.id))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                self.status =
                    "Listed native AT-SPI applications. Copy an ID into the native hook field."
                        .into();
            }
            CommandOutput::NativeApplications(Err(error)) => {
                self.native_applications = format!("Could not list native applications: {error}");
                self.status = self.native_applications.clone();
            }
            CommandOutput::WineTargets(Ok(targets)) => {
                self.wine_targets_display = if targets.is_empty() {
                    "No active Windows processes were found in visible Wine/Proton prefixes.".into()
                } else {
                    targets
                        .iter()
                        .enumerate()
                        .map(|(index, target)| {
                            format!(
                                "{}. {} — PID {} — {} — {} — {}",
                                index + 1,
                                target.executable,
                                target.process_id,
                                target.architecture.as_str(),
                                target.runtime,
                                target.prefix.display()
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                self.wine_targets = targets;
                self.wine_hook_status = "Wine target discovery finished.".into();
            }
            CommandOutput::WineTargets(Err(error)) => {
                self.wine_targets.clear();
                self.wine_targets_display = format!("Wine discovery unavailable: {error}");
                self.wine_hook_status = self.wine_targets_display.clone();
            }
            CommandOutput::WineAttached(Ok(target)) => {
                self.wine_hook_status = format!(
                    "Attached to {target}. Candidate samples will appear below; enable only the desired rows."
                );
            }
            CommandOutput::WineAttached(Err(error)) => {
                self.wine_hook_status = format!("Wine attachment failed: {error}");
            }
            CommandOutput::Translation { hook_ids, result } => {
                self.translation_pending = false;
                match *result {
                    Ok(commit) => {
                        self.hud.set_message(&commit.translated_text);
                        self.status = format!(
                            "Translated {} selected text hook(s) in commit {}.",
                            hook_ids.len(),
                            short_id(&commit.id.0)
                        );
                        self.refresh_branches();
                    }
                    Err(error) => {
                        self.status = format!(
                            "Could not translate selected hook(s) {}: {error}",
                            hook_ids.join(", ")
                        );
                    }
                }
                self.start_next_translation(&_sender);
            }
        }
    }
}

impl AppModel {
    fn poll_native_text_hook(&mut self, sender: &ComponentSender<Self>) {
        while let Ok(event) = self.native_text_hook_service.try_recv() {
            match event {
                NativeTextHookEvent::Ready => {
                    self.native_hook_status = if self.native_application_id.trim().is_empty() {
                        "Native text hook ready. Enter an AT-SPI application ID to enable capture."
                            .into()
                    } else {
                        format!("Native text hook armed for {}.", self.native_application_id)
                    };
                }
                NativeTextHookEvent::Text(event) => {
                    let hook_id = format!("native:{}:{}", event.application_id, event.object_path);
                    self.native_hook_status = format!(
                        "Hooked native text from {} ({}) at {}.",
                        event.application_id, event.object_path, event.timestamp_ms
                    );
                    self.observe_text_hook(
                        hook_id.clone(),
                        "Native accessibility text".into(),
                        format!("{} — {}", event.application_id, event.object_path),
                        "AT-SPI".into(),
                        event.text.clone(),
                    );
                    self.queue_hook_text(PendingHookText {
                        hook_id,
                        captured_at_ms: event.timestamp_ms,
                        source: SourceKind::NativeHook,
                        target: format!("{}{}", event.application_id, event.object_path),
                        text: event.text,
                        config: TextHookConfig::default(),
                    });
                }
                NativeTextHookEvent::Error(error) => {
                    self.native_hook_status = format!("Native text hook unavailable: {error}");
                    self.status = self.native_hook_status.clone();
                }
            }
        }
        self.start_next_translation(sender);
    }

    fn poll_wine_hook(&mut self, sender: &ComponentSender<Self>) {
        while let Ok(event) = self.wine_hook_service.try_recv() {
            match event {
                WineHookEvent::Connected { bridge } => {
                    self.wine_hook_status = format!(
                        "Text hook attached to {} (PID {}, {:?}).",
                        bridge.executable.path, bridge.process_id, bridge.runtime
                    );
                    self.status = self.wine_hook_status.clone();
                }
                WineHookEvent::Candidate { bridge, candidate } => {
                    let hook_id = candidate.stable_key.to_string();
                    let executable = bridge.executable.path.clone();
                    let callsite = match (&candidate.caller_module, candidate.module_offset) {
                        (Some(module), Some(offset)) => format!("{module}+0x{offset:x}"),
                        (Some(module), None) => module.clone(),
                        (None, Some(offset)) => format!("module offset 0x{offset:x}"),
                        (None, None) => "unknown caller".into(),
                    };
                    self.hook_routes
                        .insert(hook_id.clone(), (bridge.bridge_id, candidate.candidate_id));
                    self.observe_text_hook(
                        hook_id.clone(),
                        format!("{} — {}", executable, candidate.adapter_id),
                        format!(
                            "{} via {} — {}",
                            candidate.api, candidate.adapter_id, callsite
                        ),
                        candidate.api.clone(),
                        candidate.sample,
                    );
                    if self
                        .text_hook_configs
                        .get(&hook_id)
                        .is_some_and(|config| config.enabled)
                    {
                        let _ = self
                            .wine_hook_service
                            .enable_candidate(bridge.bridge_id, candidate.candidate_id);
                    }
                    self.wine_hook_status = format!(
                        "Discovered a text hook from {executable} (PID {}). Select it below to send its text to the model.",
                        bridge.process_id
                    );
                }
                WineHookEvent::Text { bridge, event } => {
                    let hook_id = event.stable_key.to_string();
                    if self.hook_routes.get(&hook_id)
                        != Some(&(bridge.bridge_id, event.candidate_id))
                    {
                        continue;
                    }
                    let executable = bridge.executable.path.clone();
                    let speaker = event
                        .speaker
                        .as_deref()
                        .map(|speaker| format!("{speaker}: "))
                        .unwrap_or_default();
                    let text = format!("{speaker}{}", event.text);
                    self.wine_hook_status = format!(
                        "Hooked text from {executable} (PID {}), event {}.",
                        bridge.process_id, event.sequence
                    );
                    if let Some(index) = self.text_hook_indices.get(&hook_id).copied() {
                        let config = self
                            .text_hook_configs
                            .get(&hook_id)
                            .cloned()
                            .unwrap_or_default();
                        self.text_hooks.guard().send(
                            index,
                            TextHookRowInput::Observed {
                                title: config.title,
                                detail: config.detail,
                                sample: text.clone(),
                            },
                        );
                    }
                    self.queue_hook_text(PendingHookText {
                        hook_id,
                        captured_at_ms: event.timestamp_ms,
                        source: if matches!(bridge.runtime, HookRuntime::Native) {
                            SourceKind::NativeHook
                        } else {
                            SourceKind::WineHook
                        },
                        target: format!("{}:{}", executable, event.stable_key),
                        text,
                        config: TextHookConfig::default(),
                    });
                }
                WineHookEvent::Diagnostic {
                    bridge,
                    level,
                    message,
                } => {
                    self.status = format!(
                        "Hook bridge {} (PID {}) {level}: {message}",
                        bridge.executable.path, bridge.process_id
                    );
                }
                WineHookEvent::Disconnected { bridge } => {
                    let disconnected = self
                        .hook_routes
                        .iter()
                        .filter(|(_, (bridge_id, _))| *bridge_id == bridge.bridge_id)
                        .map(|(key, _)| key.clone())
                        .collect::<Vec<_>>();
                    for key in disconnected {
                        self.hook_routes.remove(&key);
                        if let Some(index) = self.text_hook_indices.get(&key).copied() {
                            self.text_hooks
                                .guard()
                                .send(index, TextHookRowInput::Availability(false));
                        }
                    }
                    self.wine_hook_status = format!(
                        "Text hook disconnected from {} (PID {}). Configure the bridge at {}.",
                        bridge.executable.path,
                        bridge.process_id,
                        self.wine_bridge_config_path.display()
                    );
                    self.status = self.wine_hook_status.clone();
                }
                WineHookEvent::Error(error) => {
                    self.status = format!("Wine text hook error: {error}")
                }
            }
        }
        self.start_next_translation(sender);
    }

    fn observe_text_hook(
        &mut self,
        id: String,
        title: String,
        detail: String,
        source_api: String,
        sample: String,
    ) {
        if let Some(index) = self.text_hook_indices.get(&id).copied() {
            self.text_hooks.guard().send(
                index,
                TextHookRowInput::Observed {
                    title: title.clone(),
                    detail: detail.clone(),
                    sample,
                },
            );
            if let Some(config) = self.text_hook_configs.get_mut(&id) {
                config.title = title;
                config.detail = detail;
                config.source_api = source_api;
            }
            let _ = save_json(
                &self.text_hook_configs_path,
                &self.text_hook_configs,
                "text hook settings",
            );
            return;
        }
        let mut config = self.text_hook_configs.get(&id).cloned().unwrap_or_default();
        config.title = title.clone();
        config.detail = detail.clone();
        config.source_api = source_api;
        self.text_hook_configs
            .entry(id.clone())
            .or_insert_with(|| config.clone());
        let index = self
            .text_hooks
            .guard()
            .push_back(TextHookInit {
                id: id.clone(),
                title,
                detail,
                sample,
                available: true,
                config,
            })
            .current_index();
        self.text_hook_indices.insert(id, index);
        let _ = save_json(
            &self.text_hook_configs_path,
            &self.text_hook_configs,
            "text hook settings",
        );
    }

    fn queue_hook_text(&mut self, mut pending: PendingHookText) {
        let Some(config) = self.text_hook_configs.get(&pending.hook_id).cloned() else {
            return;
        };
        if !config.enabled {
            return;
        }
        pending.config = config;
        const MAX_PENDING_HOOK_TEXT: usize = 256;
        if self.pending_hook_text.len() == MAX_PENDING_HOOK_TEXT {
            self.pending_hook_text.pop_front();
        }
        self.pending_hook_text.push_back(pending);
    }

    fn start_next_translation(&mut self, sender: &ComponentSender<Self>) {
        if self.translation_pending || self.pending_hook_text.is_empty() {
            return;
        }
        let oldest_timestamp = self
            .pending_hook_text
            .front()
            .expect("queue was checked above")
            .captured_at_ms;
        if now_ms().saturating_sub(oldest_timestamp) < 100 {
            return;
        }
        if self.model_settings.model.trim().is_empty() {
            self.pending_hook_text.clear();
            self.status =
                "Selected hook text was captured, but no model is configured in the Model panel."
                    .into();
            return;
        }
        if self.model_settings.target_language.trim().is_empty() {
            self.pending_hook_text.clear();
            self.status =
                "Selected hook text was captured, but the target language is blank.".into();
            return;
        }

        let first_post_processors = self
            .pending_hook_text
            .front()
            .expect("queue was checked above")
            .config
            .post_processors();
        let first_timestamp = self
            .pending_hook_text
            .front()
            .expect("queue was checked above")
            .captured_at_ms;
        let queued = self.pending_hook_text.len();
        let mut batch = Vec::new();
        for _ in 0..queued {
            let pending = self
                .pending_hook_text
                .pop_front()
                .expect("queue length is stable during grouping");
            if pending.config.post_processors() == first_post_processors
                && pending.captured_at_ms.abs_diff(first_timestamp) <= 100
            {
                batch.push(pending);
            } else {
                self.pending_hook_text.push_back(pending);
            }
        }
        let hook_ids = batch
            .iter()
            .map(|pending| pending.hook_id.clone())
            .collect::<Vec<_>>();
        self.translation_pending = true;
        self.status = format!("Translating {} selected text hook(s)…", batch.len());
        let data_dir = self.data_dir.clone();
        let settings = self.model_settings.clone();
        let branch = self.active_branch.clone();
        sender.oneshot_command(async move {
            CommandOutput::Translation {
                hook_ids,
                result: Box::new(
                    translate_hook_batch(data_dir, branch, settings, batch)
                        .await
                        .map_err(|error| format!("{error:#}")),
                ),
            }
        });
    }

    fn save_model_settings(&mut self) {
        if let Err(error) = save_json(
            &self.model_settings_path,
            &self.model_settings,
            "model settings",
        ) {
            self.status = format!("Model settings could not be saved: {error}");
        }
    }

    fn change_hud_appearance(&mut self, change: impl FnOnce(&mut HudAppearance)) {
        let mut appearance = self.hud_appearance.clone();
        change(&mut appearance);
        if let Err(error) = self.hud.set_appearance(&appearance) {
            self.status = format!("HUD appearance was not changed: {error}");
            return;
        }
        if let Err(error) = save_hud_appearance(&self.hud_appearance_path, &appearance) {
            self.status = format!("HUD appearance changed, but could not be saved: {error}");
        } else {
            self.status = "HUD appearance updated.".into();
        }
        self.hud_appearance = appearance;
    }

    fn refresh_branches(&mut self) {
        self.branches = match self.store.list_branches() {
            Ok(branches) => branches
                .iter()
                .map(|branch| {
                    let marker = if branch.name == self.active_branch {
                        "●"
                    } else {
                        "○"
                    };
                    format!("{marker} {}  {}", branch.name, short_id(&branch.head.0))
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Err(error) => format!("History unavailable: {error}"),
        };
    }

    fn commit_scratchpad(&mut self) {
        let result = (|| -> Result<()> {
            let branch = self.store.branch(&self.active_branch)?;
            let parent = self.store.get_commit(&branch.head)?;
            let previous_digest = blake3::hash(parent.context.scratchpad.as_bytes())
                .to_hex()
                .to_string();
            let new_digest = blake3::hash(self.scratchpad_input.as_bytes())
                .to_hex()
                .to_string();
            let mut context = parent.context;
            context.scratchpad.clone_from(&self.scratchpad_input);
            let commit = TranslationCommit::create(
                vec![branch.head.clone()],
                now_ms(),
                vec![],
                String::new(),
                String::new(),
                context,
                vec![ScratchpadEdit {
                    author: ScratchpadAuthor::User,
                    at_ms: now_ms(),
                    previous_digest,
                    new_digest,
                }],
                vec![],
                ModelMetadata::default(),
                "Update scratchpad".into(),
            )?;
            self.store.put_commit(&commit)?;
            if !self.store.advance_branch(
                &self.active_branch,
                &branch.head,
                &commit.id,
                now_ms(),
            )? {
                anyhow::bail!("branch moved while committing; retry the edit");
            }
            Ok(())
        })();
        self.status = match result {
            Ok(()) => "Committed user scratchpad edit.".into(),
            Err(error) => format!("Could not commit scratchpad: {error}"),
        };
        self.refresh_branches();
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn short_id(id: &str) -> &str {
    &id[..id.len().min(10)]
}

fn native_preload_library_path() -> PathBuf {
    if let Some(path) = env::var_os("TERRATRANSLATE_NATIVE_HOOK_LIBRARY") {
        return PathBuf::from(path);
    }
    let candidates = [
        PathBuf::from("/usr/lib/terratranslate/libterratranslate_native_hook.so"),
        PathBuf::from("/usr/lib64/terratranslate/libterratranslate_native_hook.so"),
        PathBuf::from("target/release/libterratranslate_native_hook.so"),
        PathBuf::from("target/debug/libterratranslate_native_hook.so"),
    ];
    candidates
        .iter()
        .find(|path| path.is_file())
        .cloned()
        .unwrap_or_else(|| candidates[0].clone())
}

async fn translate_hook_batch(
    data_dir: PathBuf,
    branch: String,
    settings: ModelSettings,
    batch: Vec<PendingHookText>,
) -> Result<TranslationCommit> {
    let store = SessionStore::open(data_dir.join("sessions.db"), data_dir.join("blobs"))?;
    let api_key = env::var("TERRATRANSLATE_API_KEY")
        .ok()
        .filter(|value| !value.is_empty())
        .map(SecretString::from);
    let provider = OpenAiCompatibleProvider::new(
        settings.endpoint.trim(),
        api_key,
        settings.model.trim(),
        ModelCapabilities {
            text: true,
            tools: true,
            json_schema: true,
            ..Default::default()
        },
    )?;
    let mut engine = TranslationEngine::new(store, Arc::new(provider));
    engine.add_processor(Arc::new(NormalizeWhitespace));
    let created_at_ms = batch
        .iter()
        .map(|pending| pending.captured_at_ms)
        .max()
        .unwrap_or_else(now_ms);
    let multiple_hooks = batch.len() > 1;
    let inputs = batch
        .into_iter()
        .map(|pending| TurnInput {
            captured_at_ms: pending.captured_at_ms,
            source: pending.source,
            target: pending.target,
            input: ModelInput::Text(pending.text),
            text_options: Some(TextInputOptions {
                stable_hook_key: Some(pending.hook_id),
                label: pending.config.label(),
                processing: TextProcessingSelection {
                    pre_prompt: pending.config.pre_processors(),
                    post_translation: pending.config.post_processors(),
                },
            }),
        })
        .collect();
    let mut system_prompt = settings.system_prompt;
    if multiple_hooks {
        system_prompt.push_str(
            " Translate every text-hook input in the same order. Keep distinct labeled hooks clearly separated in the result.",
        );
    }
    engine
        .translate_turn(TurnRequest {
            branch,
            created_at_ms,
            system_prompt,
            source_language: (!settings.source_language.trim().is_empty())
                .then(|| settings.source_language.trim().to_owned()),
            target_language: settings.target_language.trim().to_owned(),
            inputs,
        })
        .await
        .map_err(Into::into)
}

fn initialize_store(data_dir: &Path) -> Result<SessionStore> {
    fs::create_dir_all(data_dir).context("create application data directory")?;
    let mut store = SessionStore::open(data_dir.join("sessions.db"), data_dir.join("blobs"))?;
    if store.list_branches()?.is_empty() {
        let root = TranslationCommit::create(
            vec![],
            now_ms(),
            vec![],
            String::new(),
            String::new(),
            ContextSnapshot::default(),
            vec![],
            vec![],
            ModelMetadata::default(),
            "Initialize translation session".into(),
        )?;
        store.put_commit(&root)?;
        store.create_branch("main", &root.id, now_ms())?;
    }
    Ok(store)
}

fn load_hud_appearance(path: &Path) -> Result<HudAppearance> {
    match fs::read(path) {
        Ok(contents) => {
            let appearance: HudAppearance =
                serde_json::from_slice(&contents).context("parse HUD appearance")?;
            appearance
                .validate()
                .map_err(anyhow::Error::msg)
                .context("validate HUD appearance")?;
            Ok(appearance)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(HudAppearance::default()),
        Err(error) => Err(error).context("read HUD appearance"),
    }
}

fn save_hud_appearance(path: &Path, appearance: &HudAppearance) -> Result<()> {
    let contents = serde_json::to_vec_pretty(appearance).context("serialize HUD appearance")?;
    fs::write(path, contents).context("write HUD appearance")
}

fn load_json_or_default<T>(path: &Path, description: &str) -> Result<T>
where
    T: serde::de::DeserializeOwned + Default,
{
    match fs::read(path) {
        Ok(contents) => serde_json::from_slice(&contents)
            .with_context(|| format!("parse {description} from {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => {
            Err(error).with_context(|| format!("read {description} from {}", path.display()))
        }
    }
}

fn save_json<T>(path: &Path, value: &T, description: &str) -> Result<()>
where
    T: serde::Serialize,
{
    let contents =
        serde_json::to_vec_pretty(value).with_context(|| format!("serialize {description}"))?;
    fs::write(path, contents).with_context(|| format!("write {description} to {}", path.display()))
}

fn start_wine_hook(data_dir: &Path) -> Result<(WineHookService, PathBuf)> {
    let socket_path = data_dir.join("wine-bridge.sock");
    let config_path = data_dir.join("wine-bridge.json");
    clear_stale_wine_socket(&socket_path)?;
    let mut authentication_token = [0_u8; 32];
    fs::File::open("/dev/urandom")
        .context("open system random source for Wine bridge authentication")?
        .read_exact(&mut authentication_token)
        .context("read Wine bridge authentication token")?;
    let service = WineHookService::bind(&socket_path, authentication_token)
        .with_context(|| format!("listen for Wine hooks at {}", socket_path.display()))?;
    let socket_path_string = service.socket_path().display().to_string();
    let config = HookBridgeConfig {
        socket_path: socket_path_string,
        authentication_token_hex: authentication_token
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    };
    let mut config_file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&config_path)
        .with_context(|| format!("create Wine bridge configuration {}", config_path.display()))?;
    config_file
        .write_all(&serde_json::to_vec_pretty(&config)?)
        .with_context(|| format!("write Wine bridge configuration {}", config_path.display()))?;
    fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600)).with_context(|| {
        format!(
            "restrict Wine bridge configuration {}",
            config_path.display()
        )
    })?;
    Ok((service, config_path))
}

fn clear_stale_wine_socket(socket_path: &Path) -> Result<()> {
    match UnixStream::connect(socket_path) {
        Ok(_) => anyhow::bail!(
            "another TerraTranslate instance is already listening for Wine hooks at {}",
            socket_path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
            fs::remove_file(socket_path)
                .with_context(|| format!("remove stale Wine hook socket {}", socket_path.display()))
        }
        Err(error) => {
            Err(error).with_context(|| format!("check Wine hook socket {}", socket_path.display()))
        }
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let arguments = Arguments::parse();
    let capabilities = DesktopCapabilities::detect();
    if arguments.diagnostics {
        println!("{}", serde_json::to_string_pretty(&capabilities)?);
        return Ok(());
    }
    if restart_with_layer_shell_preloaded(&capabilities)? {
        return Ok(());
    }
    let vision_cache_config = VisionFrameCacheConfig {
        similarity_threshold: arguments.vision_similarity_threshold,
    };
    let data_dir = arguments.data_dir.unwrap_or_else(|| {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("terratranslate")
    });
    let store = initialize_store(&data_dir)?;
    let (wine_hook_service, wine_bridge_config_path) = start_wine_hook(&data_dir)?;
    let native_text_hook_service = NativeTextHookService::start();
    let hud_appearance_path = data_dir.join("hud-appearance.json");
    let hud_appearance = match load_hud_appearance(&hud_appearance_path) {
        Ok(appearance) => appearance,
        Err(error) => {
            tracing::warn!("could not load HUD appearance; using defaults: {error:#}");
            HudAppearance::default()
        }
    };
    let model_settings_path = data_dir.join("model-settings.json");
    let mut model_settings =
        match load_json_or_default::<ModelSettings>(&model_settings_path, "model settings") {
            Ok(settings) => settings,
            Err(error) => {
                tracing::warn!("could not load model settings; using defaults: {error:#}");
                ModelSettings::default()
            }
        };
    if model_settings.model.is_empty()
        && let Ok(model) = env::var("TERRATRANSLATE_MODEL")
    {
        model_settings.model = model;
    }
    if let Ok(endpoint) = env::var("TERRATRANSLATE_ENDPOINT") {
        model_settings.endpoint = endpoint;
    }
    let text_hook_configs_path = data_dir.join("text-hooks.json");
    let text_hook_configs = match load_json_or_default::<BTreeMap<String, TextHookConfig>>(
        &text_hook_configs_path,
        "text hook settings",
    ) {
        Ok(configs) => configs,
        Err(error) => {
            tracing::warn!("could not load text hook settings; using defaults: {error:#}");
            BTreeMap::new()
        }
    };
    let native_preload_path = native_preload_library_path();
    let wine_artifacts = WineArtifacts::host_defaults();
    let app = RelmApp::new("io.github.confusedphoton.TerraTranslate");
    app.run::<AppModel>(AppInit {
        store,
        data_dir,
        capabilities,
        hud_appearance,
        hud_appearance_path,
        wine_hook_service,
        wine_bridge_config_path,
        native_text_hook_service,
        model_settings,
        model_settings_path,
        text_hook_configs,
        text_hook_configs_path,
        native_preload_path,
        wine_artifacts,
        vision_cache_config,
    });
    Ok(())
}

/// gtk4-layer-shell interposes Wayland client requests, so it must be loaded before
/// libwayland-client. Re-executing here keeps it optional while satisfying that ordering.
fn restart_with_layer_shell_preloaded(capabilities: &DesktopCapabilities) -> Result<bool> {
    if capabilities.display_server != DisplayServer::Wayland
        || !wayland_overlay_requested()
        || env::var_os(LAYER_SHELL_PRELOAD_ATTEMPTED).is_some()
    {
        return Ok(false);
    }
    let Some(layer_shell_library) = available_layer_shell_library() else {
        return Ok(false);
    };

    let mut preload = OsString::from(layer_shell_library);
    if let Some(existing_preload) = env::var_os("LD_PRELOAD") {
        preload.push(":");
        preload.push(existing_preload);
    }

    let error = Command::new(env::current_exe().context("resolve the TerraTranslate executable")?)
        .args(env::args_os().skip(1))
        .env("LD_PRELOAD", preload)
        .env(LAYER_SHELL_PRELOAD_ATTEMPTED, "1")
        .exec();
    Err(error).context("restart TerraTranslate with gtk4-layer-shell preloaded")
}

#[cfg(test)]
mod wine_hook_startup_tests {
    use std::os::unix::net::UnixListener;

    use super::*;

    #[test]
    fn removes_a_refused_wine_hook_socket() {
        let socket_path = std::env::temp_dir().join(format!(
            "terratranslate-stale-{}-{}.sock",
            std::process::id(),
            now_ms()
        ));
        let listener = UnixListener::bind(&socket_path).unwrap();
        drop(listener);
        clear_stale_wine_socket(&socket_path).unwrap();
        assert!(!socket_path.exists());
    }

    #[test]
    fn parses_native_arguments_without_a_shell() {
        assert_eq!(
            parse_native_arguments(r#"--name "two words" 'literal $HOME' """#).unwrap(),
            ["--name", "two words", "literal $HOME", ""]
        );
        assert!(parse_native_arguments("'unterminated").is_err());
    }
}
