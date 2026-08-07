use std::fs;
use std::path::PathBuf;
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
    DesktopCapabilities, PortalFrameReceiver, PortalShortcutSession, PortalStream, ShortcutBinding,
    WindowCaptureSession, register_shortcuts, select_window,
};
use terratranslate_store::SessionStore;

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
}

#[derive(Debug)]
enum AppMsg {
    SelectWindow,
    RegisterShortcut,
    BranchInput(String),
    CreateBranch,
    ScratchpadInput(String),
    CommitScratchpad,
    PollFrame,
}

#[derive(Debug)]
enum CommandOutput {
    Capture(Result<(WindowCaptureSession, PortalFrameReceiver), String>),
    Shortcut(Result<PortalShortcutSession, String>),
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
                        connect_clicked => AppMsg::RegisterShortcut,
                    },
                },

                gtk::Label {
                    #[watch]
                    set_label: &model.status,
                    set_wrap: true,
                    set_halign: gtk::Align::Start,
                    add_css_class: "dim-label",
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
                            gtk::Label {
                                #[watch]
                                set_label: &if model.capture_streams.is_empty() {
                                    "Choose a window to establish a direct PipeWire frame stream.\nHook text and application audio will join the same timestamped turn.".to_owned()
                                } else {
                                    model.capture_streams.iter().map(|stream| format!(
                                        "PipeWire node {} — {:?} at {:?}", stream.pipewire_node_id, stream.size, stream.position
                                    )).collect::<Vec<_>>().join("\n")
                                },
                                set_wrap: true,
                                set_halign: gtk::Align::Start,
                                set_valign: gtk::Align::Start,
                                set_margin_all: 12,
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
                if let Some(receiver) = &self.frame_receiver
                    && let Ok(frame) = receiver.try_recv()
                {
                    self.status = match frame.encode_png() {
                        Ok(png) => format!(
                            "Live capture: {}×{} {:?} frame encoded to {} bytes of PNG model input.",
                            frame.width,
                            frame.height,
                            frame.format,
                            png.len()
                        ),
                        Err(error) => format!("Could not encode captured frame: {error}"),
                    };
                }
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
                self.capture_streams = streams;
                self.capture_session = Some(session);
                self.frame_receiver = Some(receiver);
            }
            CommandOutput::Capture(Err(error)) => {
                self.status = format!("Window capture unavailable: {error}")
            }
            CommandOutput::Shortcut(Ok(session)) => {
                self.status = format!("Claimed shortcuts: {}", session.accepted_ids().join(", "));
                self.shortcut_session = Some(session);
            }
            CommandOutput::Shortcut(Err(error)) => {
                self.status = format!("Shortcut unavailable: {error}")
            }
        }
    }
}

impl AppModel {
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
    let data_dir = arguments.data_dir.unwrap_or_else(|| {
        dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("terratranslate")
    });
    let store = initialize_store(&data_dir)?;
    let app = RelmApp::new("io.github.confusedphoton.TerraTranslate");
    app.run::<AppModel>(AppInit {
        store,
        capabilities,
    });
    Ok(())
}
