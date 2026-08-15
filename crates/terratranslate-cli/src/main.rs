use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use secrecy::SecretString;
use terratranslate_core::{
    CommitId, ContextSnapshot, ModelMetadata, NormalizeWhitespace, SourceKind, TranslationCommit,
};
use terratranslate_engine::{ContextMode, TranslationEngine, TurnInput, TurnRequest};
use terratranslate_platform_linux::list_application_audio_targets;
use terratranslate_provider::{ModelCapabilities, ModelInput, OpenAiCompatibleProvider};
use terratranslate_store::SessionStore;

#[derive(Parser)]
#[command(
    version,
    about = "Headless TerraTranslate session and translation client"
)]
struct Arguments {
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Initialize an empty session database and main branch.
    Init,
    /// List branch heads.
    Branches,
    /// List live application playback nodes available for target-audio capture.
    AudioTargets,
    /// Create or reset a branch from another branch or full commit ID.
    Branch {
        name: String,
        #[arg(long, default_value = "main")]
        from: String,
    },
    /// Submit a versioned text/image/audio translation turn.
    Translate {
        #[arg(long, default_value = "main")]
        branch: String,
        #[arg(long, default_value = "http://127.0.0.1:11434/v1")]
        endpoint: String,
        #[arg(long)]
        model: String,
        #[arg(long, default_value = "English")]
        target_language: String,
        #[arg(long)]
        source_language: Option<String>,
        #[arg(long)]
        text: Vec<String>,
        #[arg(long)]
        image: Vec<PathBuf>,
        #[arg(long)]
        audio: Vec<PathBuf>,
        #[arg(
            long,
            default_value = "Translate faithfully while preserving character voice."
        )]
        system_prompt: String,
        /// Send the complete main-branch context history for this request.
        #[arg(long)]
        endless_context: bool,
        /// Reinsert the current branch scratchpad into this endless-context request.
        #[arg(long = "endless-context-scratchpad", requires = "endless_context")]
        endless_context_scratchpad: bool,
    },
    /// Produce the automatic portion and explicit conflicts of a manual merge.
    MergePlan { left: String, right: String },
    /// Create a two-parent merge using a manually resolved ContextSnapshot JSON file.
    Merge {
        left: String,
        right: String,
        #[arg(long)]
        context: PathBuf,
        #[arg(long, default_value = "main")]
        target_branch: String,
        #[arg(long, default_value = "Merge translation context")]
        message: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let arguments = Arguments::parse();
    if matches!(&arguments.command, Command::AudioTargets) {
        println!(
            "{}",
            serde_json::to_string_pretty(&list_application_audio_targets()?)?
        );
        return Ok(());
    }
    let data_dir = arguments.data_dir.unwrap_or_else(default_data_dir);
    let mut store = open_store(&data_dir)?;
    match arguments.command {
        Command::Init => println!("Initialized {}", data_dir.display()),
        Command::Branches => {
            for branch in store.list_branches()? {
                println!("{}\t{}", branch.name, branch.head);
            }
        }
        Command::AudioTargets => unreachable!("handled before opening the session store"),
        Command::Branch { name, from } => {
            let head = resolve_ref(&store, &from)?;
            let branch = store.create_branch(&name, &head, now_ms())?;
            println!("{}\t{}", branch.name, branch.head);
        }
        Command::Translate {
            branch,
            endpoint,
            model,
            target_language,
            source_language,
            text,
            image,
            audio,
            system_prompt,
            endless_context,
            endless_context_scratchpad,
        } => {
            if text.is_empty() && image.is_empty() && audio.is_empty() {
                bail!("provide at least one --text, --image, or --audio input");
            }
            let key = std::env::var("TERRATRANSLATE_API_KEY")
                .ok()
                .filter(|value| !value.is_empty())
                .map(SecretString::from);
            let provider = OpenAiCompatibleProvider::new(
                endpoint,
                key,
                model,
                ModelCapabilities {
                    text: true,
                    vision: true,
                    audio: true,
                    tools: true,
                    json_schema: true,
                    streaming: false,
                },
            )?;
            let mut engine = TranslationEngine::new(store, Arc::new(provider));
            engine.add_processor(Arc::new(NormalizeWhitespace));
            let captured_at_ms = now_ms();
            let mut inputs = text
                .into_iter()
                .map(|text| TurnInput {
                    captured_at_ms,
                    source: SourceKind::Manual,
                    target: "cli".into(),
                    input: ModelInput::Text(text),
                    text_options: None,
                })
                .collect::<Vec<_>>();
            for path in image {
                inputs.push(TurnInput {
                    captured_at_ms,
                    source: SourceKind::Import,
                    target: path.display().to_string(),
                    input: ModelInput::Image {
                        media_type: image_media_type(&path)?.into(),
                        bytes: fs::read(&path)
                            .with_context(|| format!("read image {}", path.display()))?,
                    },
                    text_options: None,
                });
            }
            for path in audio {
                inputs.push(TurnInput {
                    captured_at_ms,
                    source: SourceKind::Import,
                    target: path.display().to_string(),
                    input: ModelInput::Audio {
                        format: audio_format(&path)?.into(),
                        bytes: fs::read(&path)
                            .with_context(|| format!("read audio {}", path.display()))?,
                    },
                    text_options: None,
                });
            }
            let commit = engine
                .translate_turn(TurnRequest {
                    branch,
                    created_at_ms: captured_at_ms,
                    system_prompt,
                    source_language,
                    target_language,
                    context_mode: if endless_context {
                        ContextMode::Endless {
                            include_scratchpad: endless_context_scratchpad,
                        }
                    } else {
                        ContextMode::Current
                    },
                    inputs,
                })
                .await?;
            println!("{}", commit.translated_text);
            eprintln!("commit {}", commit.id);
        }
        Command::MergePlan { left, right } => {
            let left = resolve_ref(&store, &left)?;
            let right = resolve_ref(&store, &right)?;
            let (base, plan) = store.plan_merge(&left, &right)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "base": base,
                    "left": left,
                    "right": right,
                    "auto_merged": plan.auto_merged,
                    "conflicts": plan.conflicts,
                }))?
            );
        }
        Command::Merge {
            left,
            right,
            context,
            target_branch,
            message,
        } => {
            let left = resolve_ref(&store, &left)?;
            let right = resolve_ref(&store, &right)?;
            let context: ContextSnapshot = serde_json::from_slice(
                &fs::read(&context).with_context(|| format!("read {}", context.display()))?,
            )?;
            let commit = store.create_merge_commit(left, right, context, now_ms(), message)?;
            store.create_branch(&target_branch, &commit.id, now_ms())?;
            println!("{}", commit.id);
        }
    }
    Ok(())
}

fn default_data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("terratranslate")
}

fn open_store(data_dir: &Path) -> Result<SessionStore> {
    fs::create_dir_all(data_dir)?;
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

fn resolve_ref(store: &SessionStore, reference: &str) -> Result<CommitId> {
    if let Ok(branch) = store.branch(reference) {
        return Ok(branch.head);
    }
    let id = CommitId(reference.to_owned());
    store.get_commit(&id)?;
    Ok(id)
}

fn image_media_type(path: &Path) -> Result<&'static str> {
    match extension(path).as_deref() {
        Some("png") => Ok("image/png"),
        Some("jpg" | "jpeg") => Ok("image/jpeg"),
        Some("webp") => Ok("image/webp"),
        Some("gif") => Ok("image/gif"),
        _ => bail!("unsupported image type for {}", path.display()),
    }
}

fn audio_format(path: &Path) -> Result<&'static str> {
    match extension(path).as_deref() {
        Some("wav") => Ok("wav"),
        Some("mp3") => Ok("mp3"),
        Some("flac") => Ok("flac"),
        Some("ogg") => Ok("ogg"),
        _ => bail!("unsupported audio type for {}", path.display()),
    }
}

fn extension(path: &Path) -> Option<String> {
    path.extension()?.to_str().map(str::to_ascii_lowercase)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
