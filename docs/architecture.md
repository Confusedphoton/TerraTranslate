# Architecture

## Data flow

```text
XDG portal/PipeWire frames ─┐
Wine hook bridge text ──────┼─> sampling/processors ─> model provider ─> postprocessors
PipeWire application audio ─┘                              │
                                                          v
                                          SQLite commit DAG + blob store
                                                          │
                                          HUD / embedding adapter
```

`terratranslate-engine` is the transaction boundary. It resolves the branch head, stores all source
payloads, executes ordered processors, validates the selected model's modality/tool capabilities,
calls the provider, applies context changes, creates a content-addressed commit, and atomically
advances the branch. If the branch moves during inference, the commit remains preserved but is not
checked out.

## Crates

- `terratranslate-core`: stable domain values, commit hashing, context merge, processors, frame
  sampling, and voice segmentation.
- `terratranslate-store`: SQLite graph metadata, compare-and-swap branch refs, merge-base search,
  and content-addressed blobs.
- `terratranslate-provider`: provider capability negotiation and OpenAI-compatible multimodal/tool
  requests.
- `terratranslate-engine`: end-to-end versioned translation turns.
- `terratranslate-platform-linux`: desktop capability detection, ScreenCast and GlobalShortcuts
  portals, and the authenticated Unix-socket Wine bridge host.
- `terratranslate-plugin-api` and `terratranslate-plugin-runner`: versioned native processor ABI
  and out-of-process execution.
- `terratranslate-wine-protocol` and `terratranslate-wine-injector`: shared hook/replacement wire
  messages and same-prefix DLL injection.
- `terratranslate-app`: GTK4/Relm4 control surface and diagnostics CLI.

## History and merges

Translations and input events are immutable. A normal turn has one parent. Branching only creates
or moves a named ref. A manual merge finds the closest common ancestor, automatically applies
one-sided changes, and reports two-sided conflicts. The user supplies a resolved context snapshot;
the resulting commit has both heads as parents. Source turns on both sides remain accessible and
are not flattened into a synthetic chat transcript.

## Plugin ABI

Plugins export `terratranslate_plugin_v1` and exchange MessagePack domain requests through owned
buffers. The host never loads them into the GUI: `terratranslate-plugin-runner` loads one library,
validates ABI version and buffer bounds, and exposes framed stdin/stdout messages. The production
launcher should additionally apply process timeouts, memory limits, and a seccomp profile.

## Platform completion map

- Portal window permission, PipeWire node discovery, mapped raw video acquisition, PNG conversion,
  and shortcut binding are implemented.
- Per-application playback-node discovery, targeted PCM acquisition, stereo downmixing, and the
  model-facing VAD/segmentation path are implemented; the GTK target picker still needs to expose
  the discovered nodes.
- Wine injection is implemented; the injected DLL and GDI/Uniscribe/DirectWrite detours are still
  required before hook events can be emitted.
- The Wine protocol already models hook candidates, text events, safe replacement capacity,
  overflow policy, and overlay fallback.
- The GTK HUD is a standalone, resizable top-level window on both supported display servers. On
  Wayland it is the deliberate fallback when target-relative coordinates are unavailable: the
  user can move and resize it manually. If `gtk4-layer-shell` is installed and the compositor
  supports it, the HUD requests the overlay layer so it starts above normal application windows;
  the base Wayland protocol cannot guarantee that behavior for an ordinary top-level window.
- X11 passive-grab and target-relative overlay backends remain separate from the portal-based
  Wayland path.
