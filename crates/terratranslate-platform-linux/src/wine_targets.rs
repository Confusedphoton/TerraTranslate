//! Discovery and attachment planning for Wine/Proton guest processes.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WineProcessArchitecture {
    X86,
    X86_64,
}

impl WineProcessArchitecture {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::X86 => "x86",
            Self::X86_64 => "x86_64",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WineTarget {
    pub process_id: u32,
    pub executable: String,
    pub architecture: WineProcessArchitecture,
    pub prefix: PathBuf,
    pub runtime: String,
    pub runtime_command: PathBuf,
}

#[derive(Clone, Debug)]
pub struct WineArtifacts {
    pub injector_x86: PathBuf,
    pub injector_x86_64: PathBuf,
    pub hook_x86: PathBuf,
    pub hook_x86_64: PathBuf,
}

impl WineArtifacts {
    pub fn host_defaults() -> Self {
        let lib_override = std::env::var_os("TERRATRANSLATE_WINE_LIB_DIR").map(PathBuf::from);
        let libexec_override =
            std::env::var_os("TERRATRANSLATE_WINE_LIBEXEC_DIR").map(PathBuf::from);
        let candidates = default_artifact_candidates();
        Self {
            injector_x86: resolve_artifact(
                libexec_override
                    .as_deref()
                    .map(|root| root.join("i686/terratranslate-wine-injector.exe")),
                candidates
                    .iter()
                    .map(|artifacts| artifacts.injector_x86.clone()),
            ),
            injector_x86_64: resolve_artifact(
                libexec_override
                    .as_deref()
                    .map(|root| root.join("x86_64/terratranslate-wine-injector.exe")),
                candidates
                    .iter()
                    .map(|artifacts| artifacts.injector_x86_64.clone()),
            ),
            hook_x86: resolve_artifact(
                lib_override
                    .as_deref()
                    .map(|root| root.join("i686/terratranslate_wine_hook.dll")),
                candidates
                    .iter()
                    .map(|artifacts| artifacts.hook_x86.clone()),
            ),
            hook_x86_64: resolve_artifact(
                lib_override
                    .as_deref()
                    .map(|root| root.join("x86_64/terratranslate_wine_hook.dll")),
                candidates
                    .iter()
                    .map(|artifacts| artifacts.hook_x86_64.clone()),
            ),
        }
    }

    fn for_architecture(&self, architecture: WineProcessArchitecture) -> (&Path, &Path) {
        match architecture {
            WineProcessArchitecture::X86 => (&self.injector_x86, &self.hook_x86),
            WineProcessArchitecture::X86_64 => (&self.injector_x86_64, &self.hook_x86_64),
        }
    }
}

fn default_artifact_candidates() -> Vec<WineArtifacts> {
    let mut candidates = Vec::new();
    if let Some((target_dir, profile)) = development_target_location() {
        let profiles = if profile == "debug" {
            ["debug", "release"]
        } else {
            ["release", "debug"]
        };
        candidates.extend(
            profiles
                .into_iter()
                .map(|profile| development_artifacts(&target_dir, profile)),
        );
    }
    if let Some(prefix) = executable_prefix() {
        candidates.push(installed_artifacts(&prefix));
    }
    candidates.push(installed_artifacts(Path::new("/usr")));
    candidates
}

fn resolve_artifact(
    explicit_path: Option<PathBuf>,
    candidates: impl IntoIterator<Item = PathBuf>,
) -> PathBuf {
    if let Some(path) = explicit_path {
        return path;
    }
    let candidates = candidates.into_iter().collect::<Vec<_>>();
    candidates
        .iter()
        .find(|path| path.is_file())
        .cloned()
        .or_else(|| candidates.into_iter().next())
        .expect("Wine artifact candidate list cannot be empty")
}

fn development_target_location() -> Option<(PathBuf, String)> {
    let executable = std::env::current_exe().ok()?;
    let profile_dir = executable.parent()?;
    let profile = profile_dir.file_name()?.to_str()?;
    if !matches!(profile, "debug" | "release") {
        return None;
    }
    Some((profile_dir.parent()?.to_path_buf(), profile.to_owned()))
}

fn development_artifacts(target_dir: &Path, profile: &str) -> WineArtifacts {
    let target_artifacts = |target: &str| target_dir.join(target).join(profile);
    WineArtifacts {
        injector_x86: target_artifacts("i686-pc-windows-gnu")
            .join("terratranslate-wine-injector.exe"),
        injector_x86_64: target_artifacts("x86_64-pc-windows-gnu")
            .join("terratranslate-wine-injector.exe"),
        hook_x86: target_artifacts("i686-pc-windows-gnu").join("terratranslate_wine_hook.dll"),
        hook_x86_64: target_artifacts("x86_64-pc-windows-gnu").join("terratranslate_wine_hook.dll"),
    }
}

fn executable_prefix() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let bin_dir = executable.parent()?;
    (bin_dir.file_name() == Some(OsStr::new("bin")))
        .then(|| bin_dir.parent().map(Path::to_path_buf))
        .flatten()
}

fn installed_artifacts(prefix: &Path) -> WineArtifacts {
    let lib = prefix.join("lib/terratranslate/wine");
    let libexec = prefix.join("libexec/terratranslate/wine");
    WineArtifacts {
        injector_x86: libexec.join("i686/terratranslate-wine-injector.exe"),
        injector_x86_64: libexec.join("x86_64/terratranslate-wine-injector.exe"),
        hook_x86: lib.join("i686/terratranslate_wine_hook.dll"),
        hook_x86_64: lib.join("x86_64/terratranslate_wine_hook.dll"),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WineTargetError {
    #[error("Wine process discovery is unavailable in Flatpak; install and run the host build")]
    Flatpak,
    #[error("read process information: {0}")]
    Io(#[from] std::io::Error),
    #[error("{kind} artifact is missing: {path}")]
    MissingArtifact { kind: &'static str, path: PathBuf },
    #[error("Wine injector failed with {status}: {diagnostic}")]
    Injector {
        status: ExitStatus,
        diagnostic: String,
    },
}

/// Finds Wine/Proton processes visible through `/proc`. Each result has a
/// guest PE image found in the process mappings, avoiding PID and host-loader
/// guesses for x86-versus-x86-64 selection.
pub fn discover_wine_targets() -> Result<Vec<WineTarget>, WineTargetError> {
    if Path::new("/.flatpak-info").exists() || std::env::var_os("FLATPAK_ID").is_some() {
        return Err(WineTargetError::Flatpak);
    }
    discover_wine_targets_in(Path::new("/proc"))
}

fn discover_wine_targets_in(proc_root: &Path) -> Result<Vec<WineTarget>, WineTargetError> {
    let mut targets = Vec::new();
    for entry in fs::read_dir(proc_root)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let Some(process_id) = entry
            .file_name()
            .to_str()
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };
        let directory = entry.path();
        // Wine consumes WINEPREFIX and Proton's Steam variables before it
        // creates the guest process, so they are often absent from the
        // process environment exposed by /proc. Keep mapping discovery
        // independent from that optional metadata.
        let environment = fs::read(directory.join("environ"))
            .map(|bytes| parse_nul_pairs(&bytes))
            .unwrap_or_default();
        let command_line = fs::read(directory.join("cmdline")).unwrap_or_default();
        let arguments = parse_nul_values(&command_line);
        let mapped_paths_on_process = fs::read_to_string(directory.join("maps"))
            .ok()
            .map(|maps| mapped_paths(&maps).collect::<Vec<_>>())
            .unwrap_or_default();
        let prefix = prefix_from_environment(&environment).or_else(|| {
            mapped_paths_on_process
                .iter()
                .find_map(|path| prefix_from_mapped_path(path))
        });
        let Some(prefix) = prefix else {
            continue;
        };
        let runtime_command = environment
            .iter()
            .find(|(key, _)| key == "WINELOADER")
            .map(|(_, value)| PathBuf::from(value))
            .or_else(|| {
                arguments.first().and_then(|argument| {
                    argument
                        .to_ascii_lowercase()
                        .contains("wine")
                        .then(|| PathBuf::from(argument))
                })
            })
            .or_else(|| process_runtime_command(&directory))
            .unwrap_or_else(|| PathBuf::from("wine"));
        let Some((executable, architecture)) = mapped_paths_on_process
            .iter()
            .filter(|path| is_pe_executable(path) && !is_wine_infrastructure_executable(path))
            .find_map(|path| pe_architecture(path).map(|architecture| (path.clone(), architecture)))
            .or_else(|| {
                arguments.iter().find_map(|argument| {
                    let path = Path::new(argument);
                    pe_architecture(path).map(|architecture| (path.to_path_buf(), architecture))
                })
            })
        else {
            continue;
        };
        let runtime = if environment
            .iter()
            .any(|(key, value)| key == "STEAM_COMPAT_DATA_PATH" && !value.is_empty())
            || is_proton_prefix(&prefix)
        {
            "proton"
        } else {
            "wine"
        };
        targets.push(WineTarget {
            process_id,
            executable: executable.to_string_lossy().into_owned(),
            architecture,
            prefix,
            runtime: runtime.into(),
            runtime_command,
        });
    }
    targets.sort_by_key(|target| target.process_id);
    Ok(targets)
}

/// Runs the architecture-matched injector inside the selected prefix. Paths
/// are passed as separate arguments and Wine converts host absolute paths
/// through its `Z:` mapping; no shell is involved. The target's Linux PID is
/// deliberately not passed to the injector: Wine exposes a different,
/// Windows-side PID to Windows APIs. The injector resolves the target by its
/// executable path inside the prefix instead.
pub fn attach_wine_target(
    target: &WineTarget,
    artifacts: &WineArtifacts,
    config: &Path,
) -> Result<(), WineTargetError> {
    let (injector, hook) = artifacts.for_architecture(target.architecture);
    for (kind, path) in [
        ("injector", injector),
        ("hook DLL", hook),
        ("config", config),
    ] {
        if !path.is_file() {
            return Err(WineTargetError::MissingArtifact {
                kind,
                path: path.to_owned(),
            });
        }
    }
    let wine = std::env::var_os("TERRATRANSLATE_WINE_COMMAND")
        .unwrap_or_else(|| OsString::from(&target.runtime_command));
    let output = Command::new(wine)
        .env("WINEPREFIX", &target.prefix)
        .arg(injector)
        .arg("--executable")
        .arg(&target.executable)
        .arg("--dll")
        .arg(hook)
        .arg("--config")
        .arg(config)
        .output()?;
    if !output.status.success() {
        let diagnostic = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let diagnostic = if diagnostic.is_empty() {
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        } else {
            diagnostic
        };
        let diagnostic = if diagnostic.is_empty() {
            "no diagnostic output".to_owned()
        } else {
            diagnostic
        };
        return Err(WineTargetError::Injector {
            status: output.status,
            diagnostic,
        });
    }
    Ok(())
}

fn prefix_from_environment(environment: &[(String, String)]) -> Option<PathBuf> {
    environment
        .iter()
        .find(|(key, value)| key == "WINEPREFIX" && !value.is_empty())
        .map(|(_, value)| PathBuf::from(value))
        .or_else(|| {
            environment
                .iter()
                .find(|(key, value)| key == "STEAM_COMPAT_DATA_PATH" && !value.is_empty())
                .map(|(_, value)| PathBuf::from(value).join("pfx"))
        })
}

fn prefix_from_mapped_path(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|ancestor| ancestor.file_name() == Some(OsStr::new("drive_c")))
        .and_then(Path::parent)
        .map(Path::to_path_buf)
}

fn is_proton_prefix(prefix: &Path) -> bool {
    prefix.file_name() == Some(OsStr::new("pfx"))
        && prefix
            .components()
            .any(|component| component.as_os_str() == OsStr::new("compatdata"))
}

fn process_runtime_command(directory: &Path) -> Option<PathBuf> {
    let executable = fs::read_link(directory.join("exe")).ok()?;
    let name = executable
        .file_name()?
        .to_string_lossy()
        .to_ascii_lowercase();
    let preloader_name = name.strip_suffix("-preloader")?;
    let loader = executable.with_file_name(preloader_name);
    loader.is_file().then_some(loader)
}

fn parse_nul_pairs(bytes: &[u8]) -> Vec<(String, String)> {
    parse_nul_values(bytes)
        .into_iter()
        .filter_map(|value| {
            let (key, value) = value.split_once('=')?;
            Some((key.to_owned(), value.to_owned()))
        })
        .collect()
}

fn parse_nul_values(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|value| !value.is_empty())
        .map(|value| String::from_utf8_lossy(value).into_owned())
        .collect()
}

fn mapped_paths(maps: &str) -> impl Iterator<Item = PathBuf> + '_ {
    maps.lines().filter_map(|line| {
        let path = mapped_path(line)?;
        let path = path.strip_suffix(" (deleted)").unwrap_or(path);
        Some(PathBuf::from(path))
    })
}

fn mapped_path(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    let mut index = 0;
    for _ in 0..5 {
        while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
            index += 1;
        }
        if index == bytes.len() {
            return None;
        }
        while bytes
            .get(index)
            .is_some_and(|byte| !byte.is_ascii_whitespace())
        {
            index += 1;
        }
    }
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    (index < bytes.len()).then(|| &line[index..])
}

fn is_pe_executable(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
}

fn is_wine_infrastructure_executable(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| {
            [
                "winedevice.exe",
                "wineboot.exe",
                "winemenubuilder.exe",
                "services.exe",
                "plugplay.exe",
                "rpcss.exe",
                "svchost.exe",
                "explorer.exe",
            ]
            .iter()
            .any(|infrastructure| name.eq_ignore_ascii_case(infrastructure))
        })
}

fn pe_architecture(path: &Path) -> Option<WineProcessArchitecture> {
    let bytes = fs::read(path).ok()?;
    if bytes.get(..2)? != b"MZ" {
        return None;
    }
    let offset = u32::from_le_bytes(bytes.get(0x3c..0x40)?.try_into().ok()?) as usize;
    if bytes.get(offset..offset + 4)? != b"PE\0\0" {
        return None;
    }
    match u16::from_le_bytes(bytes.get(offset + 4..offset + 6)?.try_into().ok()?) {
        0x014c => Some(WineProcessArchitecture::X86),
        0x8664 => Some(WineProcessArchitecture::X86_64),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn discovers_prefix_executable_and_architecture() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("tt-wine-target-{nonce}"));
        let process = root.join("42");
        fs::create_dir_all(&process).unwrap();
        let image = root.join("game.exe");
        let mut pe = vec![0; 0x86];
        pe[..2].copy_from_slice(b"MZ");
        pe[0x3c..0x40].copy_from_slice(&0x80_u32.to_le_bytes());
        pe[0x80..0x84].copy_from_slice(b"PE\0\0");
        pe[0x84..0x86].copy_from_slice(&0x8664_u16.to_le_bytes());
        fs::write(&image, pe).unwrap();
        fs::write(process.join("environ"), b"WINEPREFIX=/games/prefix\0").unwrap();
        fs::write(process.join("cmdline"), b"wine64\0game.exe\0").unwrap();
        fs::write(
            process.join("maps"),
            format!("00400000-00500000 r-xp 0 00:00 0 {}\n", image.display()),
        )
        .unwrap();
        // A non-numeric entry must be ignored.
        symlink(&process, root.join("self")).unwrap();

        let targets = discover_wine_targets_in(&root).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].process_id, 42);
        assert_eq!(targets[0].architecture, WineProcessArchitecture::X86_64);
        assert_eq!(targets[0].prefix, Path::new("/games/prefix"));
        assert_eq!(targets[0].runtime_command, Path::new("wine64"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn discovers_guest_when_wine_metadata_is_not_inherited() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("tt-wine-target-no-env-{nonce}"));
        let process = root.join("314");
        let prefix = root.join("steam/steamapps/compatdata/123/pfx");
        let prefix_image = prefix.join("drive_c/windows/system32/user32.dll");
        let image = root.join("Games/Pokemon Academy Life Forever/game.exe");
        fs::create_dir_all(&process).unwrap();
        fs::create_dir_all(image.parent().unwrap()).unwrap();
        fs::create_dir_all(prefix_image.parent().unwrap()).unwrap();

        let mut pe = vec![0; 0x86];
        pe[..2].copy_from_slice(b"MZ");
        pe[0x3c..0x40].copy_from_slice(&0x80_u32.to_le_bytes());
        pe[0x80..0x84].copy_from_slice(b"PE\0\0");
        pe[0x84..0x86].copy_from_slice(&0x014c_u16.to_le_bytes());
        fs::write(&image, pe).unwrap();
        fs::write(process.join("environ"), b"HOME=/home/test\0").unwrap();
        fs::write(
            process.join("cmdline"),
            b"Z:\\Games\\Pokemon Academy Life Forever\\game.exe\0",
        )
        .unwrap();
        fs::write(
            process.join("maps"),
            format!(
                "00400000-00500000 r-xp 0 00:00 0 {}\n7b000000-7b001000 r--p 0 00:00 0 {}\n",
                image.display(),
                prefix_image.display()
            ),
        )
        .unwrap();

        let targets = discover_wine_targets_in(&root).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].process_id, 314);
        assert_eq!(targets[0].executable, image.to_string_lossy());
        assert_eq!(targets[0].architecture, WineProcessArchitecture::X86);
        assert_eq!(targets[0].prefix, prefix);
        assert_eq!(targets[0].runtime, "proton");
        assert_eq!(targets[0].runtime_command, Path::new("wine"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parses_mapped_paths_with_spaces_and_deleted_suffixes() {
        let paths = mapped_paths(
            "00400000-00500000 r-xp 0 00:00 0 /games/Pokemon Academy Life Forever.exe (deleted)\n",
        )
        .collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec![PathBuf::from("/games/Pokemon Academy Life Forever.exe")]
        );
    }

    #[test]
    fn ignores_wine_infrastructure_process_images() {
        assert!(is_wine_infrastructure_executable(Path::new(
            "/usr/lib/wine/x86_64-windows/winedevice.exe"
        )));
        assert!(!is_wine_infrastructure_executable(Path::new(
            "/games/Pokemon Academy Life Forever/game.exe"
        )));
    }

    #[test]
    fn requires_a_real_pe_image() {
        assert_eq!(pe_architecture(Path::new("/does/not/exist")), None);
    }

    #[test]
    fn development_artifacts_follow_cargo_target_layout() {
        let artifacts = development_artifacts(Path::new("/build/terratranslate/target"), "debug");
        assert_eq!(
            artifacts.injector_x86_64,
            Path::new(
                "/build/terratranslate/target/x86_64-pc-windows-gnu/debug/terratranslate-wine-injector.exe"
            )
        );
        assert_eq!(
            artifacts.hook_x86,
            Path::new(
                "/build/terratranslate/target/i686-pc-windows-gnu/debug/terratranslate_wine_hook.dll"
            )
        );
    }

    #[test]
    fn installed_artifacts_follow_prefix_layout() {
        let artifacts = installed_artifacts(Path::new("/opt/terratranslate"));
        assert_eq!(
            artifacts.injector_x86,
            Path::new(
                "/opt/terratranslate/libexec/terratranslate/wine/i686/terratranslate-wine-injector.exe"
            )
        );
        assert_eq!(
            artifacts.hook_x86_64,
            Path::new(
                "/opt/terratranslate/lib/terratranslate/wine/x86_64/terratranslate_wine_hook.dll"
            )
        );
    }
}
