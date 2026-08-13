use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::Parser;
use relm4::gtk;
use relm4::gtk::prelude::*;
use relm4::prelude::*;
use terratranslate_core::{
    ContextSnapshot, ModelMetadata, ScratchpadAuthor, ScratchpadEdit, TranslationCommit,
};
use terratranslate_platform_linux::{
    DesktopCapabilities, DisplayServer, NativeApplication, NativeTextHookEvent,
    NativeTextHookService, PortalFrameReceiver, PortalShortcutSession, PortalStream,
    ShortcutBinding, WindowCaptureSession, WineHookEvent, WineHookService,
    list_native_applications, register_shortcuts, select_window,
};
use terratranslate_store::SessionStore;

mod hud;

use hud::{HudAppearance, HudWindow, available_layer_shell_library, wayland_overlay_requested};

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
}

struct AppInit {
    store: SessionStore,
    capabilities: DesktopCapabilities,
    hud_appearance: HudAppearance,
    hud_appearance_path: PathBuf,
    wine_hook_service: WineHookService,
    wine_bridge_config_path: PathBuf,
    native_text_hook_service: NativeTextHookService,
}

struct AppModel {
    store: SessionStore,
    status: String,
    branch_input: String,
    scratchpad_input: String,
    branches: String,
    active_branch: String,
    capture_streams: Vec<PortalStream>,
    capture_session: Option<WindowCaptureSession>,
    frame_receiver: Option<PortalFrameReceiver>,
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
    native_text_hook_service: NativeTextHookService,
    native_application_id: String,
    native_hook_status: String,
    native_applications: String,
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
    NativeApplicationId(String),
    PollNativeTextHook,
    RefreshNativeApplications,
}

#[derive(Debug)]
enum CommandOutput {
    Capture(Result<(WindowCaptureSession, PortalFrameReceiver), String>),
    Shortcut(Result<PortalShortcutSession, String>),
    FrameEncoded(Result<EncodedFrame, String>),
    NativeApplications(Result<Vec<NativeApplication>, String>),
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
            set_default_size: (920, 640),

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
                                gtk::Label {
                                    #[watch]
                                    set_label: &model.wine_hook_status,
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
        let hud = HudWindow::new(&root, &init.capabilities, &init.hud_appearance);
        let hud_positioning = hud.supports_positioning();
        let hud_visible = hud.is_visible();
        let visibility_input = sender.input_sender().clone();
        hud.connect_visible_changed(move |visible| {
            let _ = visibility_input.send(AppMsg::HudVisibilityChanged(visible));
        });
        let mut model = Self {
            store: init.store,
            status: format!("Desktop: {display}. Ready."),
            branch_input: String::new(),
            scratchpad_input: String::new(),
            branches: String::new(),
            active_branch: "main".into(),
            capture_streams: Vec::new(),
            capture_session: None,
            frame_receiver: None,
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
            native_text_hook_service: init.native_text_hook_service,
            native_application_id: String::new(),
            native_hook_status: "Native text hook is connecting to AT-SPI. Enter an application ID to enable capture.".into(),
            native_applications: String::new(),
            wine_bridge_config_path: init.wine_bridge_config_path,
        };
        model.refresh_branches();
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
                if !self.frame_encoding
                    && let Some(receiver) = &self.frame_receiver
                    && let Ok(frame) = receiver.try_recv_latest()
                {
                    self.frame_encoding = true;
                    sender.spawn_oneshot_command(move || {
                        let width = frame.width;
                        let height = frame.height;
                        let format = format!("{:?}", frame.format);
                        CommandOutput::FrameEncoded(
                            frame
                                .encode_png()
                                .map(|png| EncodedFrame {
                                    width,
                                    height,
                                    format,
                                    png_bytes: png.len(),
                                })
                                .map_err(|error| error.to_string()),
                        )
                    });
                }
            }
            AppMsg::PollWineHook => self.poll_wine_hook(),
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
            AppMsg::PollNativeTextHook => self.poll_native_text_hook(),
            AppMsg::RefreshNativeApplications => sender.oneshot_command(async {
                CommandOutput::NativeApplications(
                    list_native_applications()
                        .await
                        .map_err(|error| error.to_string()),
                )
            }),
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
            CommandOutput::FrameEncoded(Ok(frame)) => {
                self.frame_encoding = false;
                self.status = format!(
                    "Live capture: {}×{} {} frame encoded to {} bytes of PNG model input.",
                    frame.width, frame.height, frame.format, frame.png_bytes
                );
            }
            CommandOutput::FrameEncoded(Err(error)) => {
                self.frame_encoding = false;
                self.status = format!("Could not encode captured frame: {error}");
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
        }
    }
}

impl AppModel {
    fn poll_native_text_hook(&mut self) {
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
                    self.native_hook_status = format!(
                        "Hooked native text from {} ({}) at {}.",
                        event.application_id, event.object_path, event.timestamp_ms
                    );
                    self.status = format!("{} {}", self.native_hook_status, event.text);
                    self.hud.set_message(&event.text);
                }
                NativeTextHookEvent::Error(error) => {
                    self.native_hook_status = format!("Native text hook unavailable: {error}");
                    self.status = self.native_hook_status.clone();
                }
            }
        }
    }

    fn poll_wine_hook(&mut self) {
        while let Ok(event) = self.wine_hook_service.try_recv() {
            match event {
                WineHookEvent::Connected {
                    process_id,
                    executable,
                } => {
                    self.wine_hook_status =
                        format!("Wine text hook attached to {executable} (PID {process_id}).");
                    self.status = self.wine_hook_status.clone();
                }
                WineHookEvent::Text {
                    process_id,
                    executable,
                    event,
                } => {
                    let speaker = event
                        .speaker
                        .as_deref()
                        .map(|speaker| format!("{speaker}: "))
                        .unwrap_or_default();
                    self.wine_hook_status = format!(
                        "Hooked text from {executable} (PID {process_id}), event {}.",
                        event.sequence
                    );
                    self.status = format!("{} {speaker}{}", self.wine_hook_status, event.text);
                    self.hud.set_message(&format!("{speaker}{}", event.text));
                }
                WineHookEvent::Diagnostic {
                    process_id,
                    executable,
                    level,
                    message,
                } => {
                    self.status =
                        format!("Wine bridge {executable} (PID {process_id}) {level}: {message}");
                }
                WineHookEvent::Disconnected {
                    process_id,
                    executable,
                } => {
                    self.wine_hook_status = format!(
                        "Wine text hook disconnected from {executable} (PID {process_id}). Configure the bridge at {}.",
                        self.wine_bridge_config_path.display()
                    );
                    self.status = self.wine_hook_status.clone();
                }
                WineHookEvent::Error(error) => {
                    self.status = format!("Wine text hook error: {error}")
                }
            }
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

fn initialize_store(data_dir: &PathBuf) -> Result<SessionStore> {
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

fn load_hud_appearance(path: &PathBuf) -> Result<HudAppearance> {
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

fn save_hud_appearance(path: &PathBuf, appearance: &HudAppearance) -> Result<()> {
    let contents = serde_json::to_vec_pretty(appearance).context("serialize HUD appearance")?;
    fs::write(path, contents).context("write HUD appearance")
}

#[derive(serde::Serialize)]
struct WineBridgeConfig<'a> {
    socket_path: &'a str,
    authentication_token_hex: String,
}

fn start_wine_hook(data_dir: &PathBuf) -> Result<(WineHookService, PathBuf)> {
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
    let config = WineBridgeConfig {
        socket_path: &socket_path_string,
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

fn clear_stale_wine_socket(socket_path: &PathBuf) -> Result<()> {
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
    let app = RelmApp::new("io.github.confusedphoton.TerraTranslate");
    app.run::<AppModel>(AppInit {
        store,
        capabilities,
        hud_appearance,
        hud_appearance_path,
        wine_hook_service,
        wine_bridge_config_path,
        native_text_hook_service,
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
