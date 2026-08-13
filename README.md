# TerraTranslate

TerraTranslate is a Rust-first multimodal translation harness for Linux applications and visual
novels running through Wine/Proton. It is designed around direct window frames, hooked text, and
application audio rather than an OCR subsystem.

This repository currently contains the first working architectural slice. The session engine,
branching/merge store, OpenAI-compatible provider, portal selection, shortcut registration,
plugin runner, Wine injection helper, and GTK/Relm4 control surface compile and are covered by
tests. PipeWire frame/audio decoding and the injected text API adapters are the next integration
layer; they are not represented as finished in the UI.

## Build and run

Requirements on Arch Linux are Rust, GTK 4, PipeWire, and the XDG desktop portal for the running
desktop. Equivalent development packages are required on other distributions.

```sh
cargo build --workspace
cargo test --workspace
cargo run -p terratranslate-app --bin terratranslate
```

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
  On Wayland, it normally remains a movable toplevel; set `TERRATRANSLATE_WAYLAND_OVERLAY=1` to
  explicitly request a compositor-managed layer surface through gtk4-layer-shell instead.
- Authenticated local Wine bridge protocol and a Windows injector intended to run inside the
  selected Wine prefix.
- GTK4/Relm4 control surface for capture selection, branch creation, capability diagnostics, and
  versioned user scratchpad edits.

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
