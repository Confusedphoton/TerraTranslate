use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use secrecy::SecretString;
use terratranslate_core::{
    CommitId, ContextSnapshot, GameIdentity, ModelMetadata, NormalizeWhitespace, SourceKind,
    TranslationCommit,
};
use terratranslate_engine::{ContextMode, TranslationEngine, TurnInput, TurnRequest};
use terratranslate_platform_linux::list_application_audio_targets;
use terratranslate_provider::{ModelCapabilities, ModelInput, OpenAiCompatibleProvider};
use terratranslate_store::{SessionStore, StoreError};

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
    Branches {
        /// Stable identity key used when listing one game's history.
        #[arg(long)]
        game: Option<String>,
    },
    /// List registered game identities.
    Games,
    /// List live application playback nodes available for target-audio capture.
    AudioTargets,
    /// Create or reset a branch from another branch or full commit ID.
    Branch {
        name: String,
        #[arg(long, default_value = "main")]
        from: String,
        #[arg(long)]
        game: Option<String>,
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
        /// Stable identity key used to select an independent per-game history.
        #[arg(long)]
        game: Option<String>,
        #[arg(long, default_value = "CLI game")]
        game_name: String,
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
        #[arg(
            long,
            default_value = "Translate every text input into {{target_language}} in order.\n\n{{texts|enumerate}}"
        )]
        user_prompt: String,
        /// Send the complete main-branch context history for this request.
        #[arg(long)]
        endless_context: bool,
        /// Reinsert the current branch scratchpad into this endless-context request.
        #[arg(long = "endless-context-scratchpad", requires = "endless_context")]
        endless_context_scratchpad: bool,
    },
    /// Produce the automatic portion and explicit conflicts of a manual merge.
    MergePlan {
        left: String,
        right: String,
        #[arg(long)]
        game: Option<String>,
    },
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
        #[arg(long)]
        game: Option<String>,
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
        Command::Branches { game } => {
            let branches = match game.as_deref() {
                Some(game) => {
                    let identity = ensure_cli_game(&mut store, game)?;
                    store.list_game_branches(&identity.id)?
                }
                None => store.list_branches()?,
            };
            for branch in branches {
                println!("{}\t{}", branch.name, branch.head);
            }
        }
        Command::Games => {
            for game in store.list_games()? {
                println!("{}\t{}\t{}", game.id, game.name, game.executable_path);
            }
        }
        Command::AudioTargets => unreachable!("handled before opening the session store"),
        Command::Branch { name, from, game } => {
            let game_identity = game
                .as_deref()
                .map(|game| ensure_cli_game(&mut store, game))
                .transpose()?;
            let head = resolve_ref_for_game(&store, game.as_deref(), &from)?;
            let branch = match game_identity.as_ref() {
                Some(game) => store.create_game_branch(&game.id, &name, &head, now_ms())?,
                None => store.create_branch(&name, &head, now_ms())?,
            };
            println!("{}\t{}", branch.name, branch.head);
        }
        Command::Translate {
            branch,
            endpoint,
            model,
            target_language,
            source_language,
            game,
            game_name,
            text,
            image,
            audio,
            system_prompt,
            user_prompt,
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
            if let Some(game) = game {
                engine.set_game(GameIdentity::from_stable_key(
                    &game,
                    game_name,
                    game.clone(),
                    None,
                    "cli",
                    "cli",
                ));
            }
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
                    user_prompt,
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
        Command::MergePlan { left, right, game } => {
            let _game_identity = game
                .as_deref()
                .map(|game| ensure_cli_game(&mut store, game))
                .transpose()?;
            let left = resolve_ref_for_game(&store, game.as_deref(), &left)?;
            let right = resolve_ref_for_game(&store, game.as_deref(), &right)?;
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
            game,
        } => {
            let game_identity = game
                .as_deref()
                .map(|game| ensure_cli_game(&mut store, game))
                .transpose()?;
            let left = resolve_ref_for_game(&store, game.as_deref(), &left)?;
            let right = resolve_ref_for_game(&store, game.as_deref(), &right)?;
            let context: ContextSnapshot = serde_json::from_slice(
                &fs::read(&context).with_context(|| format!("read {}", context.display()))?,
            )?;
            let commit = match game_identity.as_ref() {
                Some(game) => store.create_game_merge_commit(
                    &game.id,
                    left,
                    right,
                    context,
                    now_ms(),
                    message,
                )?,
                None => store.create_merge_commit(left, right, context, now_ms(), message)?,
            };
            match game_identity.as_ref() {
                Some(game) => {
                    store.create_game_branch(&game.id, &target_branch, &commit.id, now_ms())?;
                }
                None => {
                    store.create_branch(&target_branch, &commit.id, now_ms())?;
                }
            }
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

fn cli_game_identity(stable_key: &str) -> GameIdentity {
    GameIdentity::from_stable_key(stable_key, "CLI game", stable_key, None, "cli", "cli")
}

fn ensure_cli_game(store: &mut SessionStore, stable_key: &str) -> Result<GameIdentity> {
    let identity = cli_game_identity(stable_key);
    match store.game(&identity.id) {
        Ok(existing) => Ok(existing),
        Err(StoreError::GameNotFound(_)) => {
            store.ensure_game(&identity, now_ms())?;
            Ok(identity)
        }
        Err(error) => Err(error.into()),
    }
}

fn resolve_ref_for_game(
    store: &SessionStore,
    game: Option<&str>,
    reference: &str,
) -> Result<CommitId> {
    if let Some(game) = game {
        let identity = cli_game_identity(game);
        if let Ok(branch) = store.game_branch(&identity.id, reference) {
            return Ok(branch.head);
        }
        let id = CommitId(reference.to_owned());
        store.get_commit(&id)?;
        return Ok(id);
    }
    resolve_ref(store, reference)
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
