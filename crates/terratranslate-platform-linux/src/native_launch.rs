use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use terratranslate_wine_protocol::HookBridgeConfig;

pub const HOOK_CONFIG_ENV: &str = "TERRATRANSLATE_HOOK_CONFIG";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeLaunchRequest {
    pub executable: PathBuf,
    pub arguments: Vec<OsString>,
    pub working_directory: Option<PathBuf>,
    pub preload_library: PathBuf,
    pub hook_config: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeLaunchPlan {
    pub executable: PathBuf,
    pub arguments: Vec<OsString>,
    pub working_directory: PathBuf,
    pub preload: OsString,
    pub hook_config: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeHookAvailability {
    Available,
    Unavailable { reason: String },
}

#[derive(Debug, thiserror::Error)]
pub enum NativeLaunchError {
    #[error("native text hooks are unavailable: {0}")]
    Unavailable(String),
    #[error("could not inspect {path}: {source}")]
    Inspect {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{0} is not an executable regular file")]
    NotExecutable(PathBuf),
    #[error("{0} has the wrong ELF class for this TerraTranslate build")]
    WrongElfClass(PathBuf),
    #[error("{0} is statically linked; LD_PRELOAD cannot intercept it")]
    StaticExecutable(PathBuf),
    #[error("{0} uses secure-exec (setuid/setgid); LD_PRELOAD will be ignored")]
    SecureExec(PathBuf),
    #[error("working directory {0} is not a directory")]
    InvalidWorkingDirectory(PathBuf),
    #[error("native preload library is missing: {0}")]
    MissingPreload(PathBuf),
    #[error("hook configuration is missing: {0}")]
    MissingHookConfig(PathBuf),
    #[error("could not launch {path}: {source}")]
    Spawn {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not write hook configuration {path}: {source}")]
    WriteConfig {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("hook configuration path is not valid UTF-8: {0}")]
    NonUtf8Config(PathBuf),
    #[error("unterminated quote or trailing escape in arguments")]
    InvalidArguments,
}

/// Reports whether launch-time injection can be offered in this process environment.
pub fn native_hook_availability() -> NativeHookAvailability {
    if Path::new("/.flatpak-info").exists() || std::env::var_os("FLATPAK_ID").is_some() {
        return NativeHookAvailability::Unavailable {
            reason: "Flatpak cannot launch arbitrary host applications with LD_PRELOAD; use the host build of TerraTranslate. AT-SPI and window vision remain available.".into(),
        };
    }
    NativeHookAvailability::Available
}

/// User-facing limits that remain true even when launch-time interception is available.
pub fn native_hook_limitations() -> Vec<&'static str> {
    vec![
        "Native hooks must be enabled when TerraTranslate launches the application; already-running native processes cannot be attached.",
        "Setuid/setgid, statically linked, wrong-ELF-class, and sandbox-hidden applications cannot be intercepted.",
        "Custom GPU text renderers are not semantic text APIs; use window vision for those applications.",
    ]
}

pub fn write_native_hook_config(
    path: impl AsRef<Path>,
    socket_path: &Path,
    authentication_token: [u8; 32],
) -> Result<(), NativeLaunchError> {
    let path = path.as_ref();
    let socket_path = socket_path
        .to_str()
        .ok_or_else(|| NativeLaunchError::NonUtf8Config(socket_path.to_owned()))?;
    let config = HookBridgeConfig {
        socket_path: socket_path.into(),
        authentication_token_hex: authentication_token
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    };
    let bytes = serde_json::to_vec(&config).expect("serializing a fixed hook config cannot fail");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o600);
    std::io::Write::write_all(
        &mut options
            .open(path)
            .map_err(|source| NativeLaunchError::WriteConfig {
                path: path.to_owned(),
                source,
            })?,
        &bytes,
    )
    .map_err(|source| NativeLaunchError::WriteConfig {
        path: path.to_owned(),
        source,
    })
}

pub fn prepare_native_launch(
    request: &NativeLaunchRequest,
) -> Result<NativeLaunchPlan, NativeLaunchError> {
    if let NativeHookAvailability::Unavailable { reason } = native_hook_availability() {
        return Err(NativeLaunchError::Unavailable(reason));
    }
    validate_executable(&request.executable)?;
    let working_directory = request.working_directory.clone().unwrap_or_else(|| {
        request
            .executable
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_owned()
    });
    if !working_directory.is_dir() {
        return Err(NativeLaunchError::InvalidWorkingDirectory(
            working_directory,
        ));
    }
    let preload = combined_preload(
        request.preload_library.as_os_str(),
        std::env::var_os("LD_PRELOAD").as_deref(),
    );
    Ok(NativeLaunchPlan {
        executable: request.executable.clone(),
        arguments: request.arguments.clone(),
        working_directory,
        preload,
        hook_config: request.hook_config.clone(),
    })
}

fn combined_preload(library: &OsStr, existing: Option<&OsStr>) -> OsString {
    let mut preload = library.to_owned();
    if let Some(existing) = existing.filter(|value| !value.is_empty()) {
        preload.push(":");
        preload.push(existing);
    }
    preload
}

pub fn launch_native(request: &NativeLaunchRequest) -> Result<Child, NativeLaunchError> {
    if !request.preload_library.is_file() {
        return Err(NativeLaunchError::MissingPreload(
            request.preload_library.clone(),
        ));
    }
    if !request.hook_config.is_file() {
        return Err(NativeLaunchError::MissingHookConfig(
            request.hook_config.clone(),
        ));
    }
    let plan = prepare_native_launch(request)?;
    Command::new(&plan.executable)
        .args(&plan.arguments)
        .current_dir(&plan.working_directory)
        .env("LD_PRELOAD", &plan.preload)
        .env(HOOK_CONFIG_ENV, &plan.hook_config)
        .spawn()
        .map_err(|source| NativeLaunchError::Spawn {
            path: plan.executable,
            source,
        })
}

/// Produces a Steam launch option. `%command%` supplies the executable and exact arguments.
pub fn steam_launch_option(preload_library: &Path, hook_config: &Path) -> String {
    format!(
        "{}={} LD_PRELOAD={}${{LD_PRELOAD:+:$LD_PRELOAD}} %command%",
        HOOK_CONFIG_ENV,
        shell_quote(hook_config.as_os_str()),
        shell_quote(preload_library.as_os_str())
    )
}

/// Parses a GUI argument field without invoking a shell or performing expansions.
pub fn parse_native_arguments(input: &str) -> Result<Vec<OsString>, NativeLaunchError> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quote {
        None,
        Single,
        Double,
    }
    let mut result = Vec::new();
    let mut current = String::new();
    let mut quote = Quote::None;
    let mut started = false;
    let mut characters = input.chars();
    while let Some(character) = characters.next() {
        match (quote, character) {
            (Quote::None, '\'') => {
                quote = Quote::Single;
                started = true;
            }
            (Quote::None, '"') => {
                quote = Quote::Double;
                started = true;
            }
            (Quote::Single, '\'') => quote = Quote::None,
            (Quote::Double, '"') => quote = Quote::None,
            (Quote::None | Quote::Double, '\\') => {
                current.push(
                    characters
                        .next()
                        .ok_or(NativeLaunchError::InvalidArguments)?,
                );
                started = true;
            }
            (Quote::None, whitespace) if whitespace.is_whitespace() => {
                if started {
                    result.push(std::mem::take(&mut current).into());
                    started = false;
                }
            }
            (_, character) => {
                current.push(character);
                started = true;
            }
        }
    }
    if quote != Quote::None {
        return Err(NativeLaunchError::InvalidArguments);
    }
    if started {
        result.push(current.into());
    }
    Ok(result)
}

fn shell_quote(value: &OsStr) -> String {
    let bytes = value.as_bytes();
    let mut quoted = String::from("'");
    for &byte in bytes {
        if byte == b'\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(char::from(byte));
        }
    }
    quoted.push('\'');
    quoted
}

fn validate_executable(path: &Path) -> Result<(), NativeLaunchError> {
    let metadata = fs::metadata(path).map_err(|source| NativeLaunchError::Inspect {
        path: path.to_owned(),
        source,
    })?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(NativeLaunchError::NotExecutable(path.to_owned()));
    }
    if metadata.mode() & 0o6000 != 0 {
        return Err(NativeLaunchError::SecureExec(path.to_owned()));
    }
    let bytes = fs::read(path).map_err(|source| NativeLaunchError::Inspect {
        path: path.to_owned(),
        source,
    })?;
    if bytes.starts_with(b"#!") {
        return Ok(());
    }
    let elf =
        ElfIdentity::read(&bytes).ok_or_else(|| NativeLaunchError::NotExecutable(path.into()))?;
    if elf.class
        != if cfg!(target_pointer_width = "64") {
            2
        } else {
            1
        }
    {
        return Err(NativeLaunchError::WrongElfClass(path.into()));
    }
    if !elf.has_interpreter {
        return Err(NativeLaunchError::StaticExecutable(path.into()));
    }
    Ok(())
}

struct ElfIdentity {
    class: u8,
    has_interpreter: bool,
}

impl ElfIdentity {
    fn read(bytes: &[u8]) -> Option<Self> {
        if bytes.get(..4)? != b"\x7fELF" || !matches!(bytes[4], 1 | 2) {
            return None;
        }
        let class = bytes[4];
        let little = bytes[5] == 1;
        let read_u16 = |offset: usize| {
            let raw: [u8; 2] = bytes.get(offset..offset + 2)?.try_into().ok()?;
            Some(if little {
                u16::from_le_bytes(raw)
            } else {
                u16::from_be_bytes(raw)
            })
        };
        let read_u32 = |offset: usize| {
            let raw: [u8; 4] = bytes.get(offset..offset + 4)?.try_into().ok()?;
            Some(if little {
                u32::from_le_bytes(raw)
            } else {
                u32::from_be_bytes(raw)
            })
        };
        let read_u64 = |offset: usize| {
            let raw: [u8; 8] = bytes.get(offset..offset + 8)?.try_into().ok()?;
            Some(if little {
                u64::from_le_bytes(raw)
            } else {
                u64::from_be_bytes(raw)
            })
        };
        let (program_offset, entry_size, count) = if class == 2 {
            (
                read_u64(32)? as usize,
                read_u16(54)? as usize,
                read_u16(56)? as usize,
            )
        } else {
            (
                read_u32(28)? as usize,
                read_u16(42)? as usize,
                read_u16(44)? as usize,
            )
        };
        let has_interpreter = (0..count).any(|index| {
            let offset = program_offset.saturating_add(index.saturating_mul(entry_size));
            read_u32(offset) == Some(3) // PT_INTERP
        });
        Some(Self {
            class,
            has_interpreter,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_remain_separate_and_preload_is_preserved() {
        let request = NativeLaunchRequest {
            executable: PathBuf::from("/bin/sh"),
            arguments: vec!["two words".into(), "$(never-expanded)".into()],
            working_directory: Some(std::env::temp_dir()),
            preload_library: PathBuf::from("/opt/Terra Translate/hook.so"),
            hook_config: PathBuf::from("/tmp/hook.json"),
        };
        // Mutating process environment is unsafe in Rust 2024, so preservation is exercised by
        // integration callers; exact argv and cwd are still deterministic here.
        let plan = prepare_native_launch(&request).unwrap();
        assert_eq!(plan.arguments, request.arguments);
        assert_eq!(plan.working_directory, std::env::temp_dir());
        assert!(
            plan.preload
                .as_bytes()
                .starts_with(b"/opt/Terra Translate/hook.so")
        );
        assert_eq!(
            combined_preload(OsStr::new("new.so"), Some(OsStr::new("one.so:two.so"))),
            OsStr::new("new.so:one.so:two.so")
        );
    }

    #[test]
    fn steam_option_quotes_paths_and_preserves_existing_preload() {
        let option = steam_launch_option(
            Path::new("/opt/Terra Translate/libhook.so"),
            Path::new("/tmp/player's hook.json"),
        );
        assert!(option.contains("'/opt/Terra Translate/libhook.so'"));
        assert!(option.contains("'/tmp/player'\\''s hook.json'"));
        assert!(option.contains("${LD_PRELOAD:+:$LD_PRELOAD}"));
        assert!(option.ends_with("%command%"));
    }

    #[test]
    fn config_is_private_and_round_trips() {
        let path = std::env::temp_dir().join(format!(
            "terratranslate-native-config-{}.json",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        write_native_hook_config(&path, Path::new("/tmp/tt.sock"), [0xab; 32]).unwrap();
        let parsed: HookBridgeConfig = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(parsed.socket_path, "/tmp/tt.sock");
        assert_eq!(parsed.authentication_token_hex, "ab".repeat(32));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn argument_parser_has_no_shell_expansion() {
        assert_eq!(
            parse_native_arguments(r#"--name "two words" 'literal $HOME' escaped\ space """#)
                .unwrap(),
            vec![
                OsString::from("--name"),
                OsString::from("two words"),
                OsString::from("literal $HOME"),
                OsString::from("escaped space"),
                OsString::from(""),
            ]
        );
        assert!(parse_native_arguments("'unterminated").is_err());
        assert!(parse_native_arguments("trailing\\").is_err());
    }

    #[test]
    fn limitations_explain_non_hookable_renderers_and_processes() {
        let limitations = native_hook_limitations().join(" ");
        assert!(limitations.contains("already-running"));
        assert!(limitations.contains("wrong-ELF-class"));
        assert!(limitations.contains("Custom GPU"));
    }
}
