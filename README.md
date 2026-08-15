# TerraTranslate

TerraTranslate is a Rust-first multimodal translation harness for Linux applications and visual
novels running through Wine/Proton. It is designed around direct window frames, hooked text, and
application audio rather than an OCR subsystem.

This repository currently contains the first working architectural slice. The session engine,
branching/merge store, OpenAI-compatible provider, portal selection, shortcut registration,
plugin runner, generic hook bridge, semantic Linux/Wine text adapters, and GTK/Relm4 control
surface compile and are covered by tests.

## Build and run

Requirements on Arch Linux are Rust, GTK 4, PipeWire, and the XDG desktop portal for the running
desktop. Equivalent development packages are required on other distributions.

```sh
cargo build --workspace
cargo test --workspace
cargo run -p terratranslate-app --bin terratranslate
```

Wine/Proton attachment additionally needs the matching Windows artifacts. Install the MinGW Rust
targets and build both architectures; when the app is run from this checkout it automatically
checks the corresponding `target/<triple>/<profile>` directories before packaged locations:

```sh
rustup target add i686-pc-windows-gnu x86_64-pc-windows-gnu
cargo build --target i686-pc-windows-gnu -p terratranslate-wine-hook -p terratranslate-wine-injector
cargo build --target x86_64-pc-windows-gnu -p terratranslate-wine-hook -p terratranslate-wine-injector
```

The host package installs these files with `packaging/install-host-artifacts.sh`. Custom artifact
locations can be selected with `TERRATRANSLATE_WINE_LIB_DIR` and
`TERRATRANSLATE_WINE_LIBEXEC_DIR`.

Print the capability report without opening a window:

```sh
cargo run -p terratranslate-app --bin terratranslate -- --diagnostics
```

The headless client can drive real provider calls and history operations. It defaults to an
Ollama-style local endpoint; set `TERRATRANSLATE_API_KEY` for endpoints that require a token.

```sh
cargo run -p terratranslate-cli -- translate \
  --model your-multimodal-model \
  --text 'こんにちは' \
  --image frame.png \
  --audio dialogue.wav

cargo run -p terratranslate-cli -- branches
cargo run -p terratranslate-cli -- audio-targets
cargo run -p terratranslate-cli -- merge-plan branch-a branch-b
```

Set `RUST_LOG=debug` for diagnostic logging. Runtime state defaults to
`$XDG_DATA_HOME/terratranslate` and can be redirected with `--data-dir`.

## Implemented foundations

- Content-addressed translation commits with zero, one, or two parents.
- Named branches, atomic branch advancement, merge-base discovery, and manual three-way context
  merge plans for summaries, glossaries, entities, style, and scratchpad.
- SQLite metadata and content-addressed local frame/audio/text blobs.
- OpenAI-compatible text, image, audio, and typed context-tool requests with strict capability
  checks.
- End-to-end turn orchestration that preprocesses inputs, calls a provider, versions model notes,
  stores every modality, and advances the active branch.
- Ordered built-in processors plus a crash-isolated native plugin ABI and runner.
- XDG portal window selection, mapped PipeWire frame acquisition, raw RGB conversion to PNG model
  input, application-audio target discovery/capture, and global-shortcut registration.
- A translation HUD with runtime positioning and frameless overlay modes plus Show/Hide controls.
  Its background color, text color, background transparency, font family, and font size are
  configurable live from the control surface and persist in `hud-appearance.json` under the
  application data directory.
  On Wayland, it normally remains a movable toplevel; set `TERRATRANSLATE_WAYLAND_OVERLAY=1` to
  explicitly request a compositor-managed layer surface through gtk4-layer-shell instead.
- Authenticated, bounded hook protocol shared by the native preload library and Wine DLL. Native
  launch intercepts Pango, SDL_ttf 2/3, and Cairo semantic text APIs; Wine interception covers
  GDI, Uniscribe, and DirectWrite with matching 32-bit/64-bit injector artifacts.
- GTK4/Relm4 control surface for capture selection, branch creation, capability diagnostics, and
  versioned user scratchpad edits. The main app discovers individual Wine hook candidates and
  native AT-SPI text objects, allows any number to be routed to the model, and persists optional
  per-hook labels plus independent pre-model and post-translation normalization choices.

Semantic hooks are capture-only and cover common text APIs, not every renderer. Glyph-only
FreeType/Cairo calls, custom GPU atlases, static or secure-exec native binaries, and other custom
renderers remain supported through direct window vision. Native processes must be launched from
TerraTranslate (or with the displayed Steam option); attaching an already-running native process
is not supported. Flatpak intentionally exposes only portal vision and supplemental AT-SPI—use a
host package for `LD_PRELOAD` launch and Wine attachment. Development builds may override the
preload artifact with `TERRATRANSLATE_NATIVE_HOOK_LIBRARY`.

See [the architecture document](docs/architecture.md) for component boundaries and remaining
platform integration work.

## Security model

Captured content is stored locally by digest. The only network destination used by the core is the
configured model endpoint. Native processor plugins execute in a separate runner process but are
still trusted native code; sandbox/resource enforcement beyond message limits and process
isolation remains packaging-specific. The Wine bridge requires a fresh 256-bit authentication
token and rejects protocol-version mismatches.

## License

GPL-3.0-only. The repository's license text is currently stored as `LISCENCE` for compatibility
with the initial repository state.
