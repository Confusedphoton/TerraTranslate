# Architecture

## Data flow

```text
XDG portal/PipeWire frames ─┐
Native/Wine semantic text ──┼─> sampling/processors ─> model provider ─> postprocessors
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
- `terratranslate-platform-linux`: desktop capability detection, portals, native launcher, Wine
  process discovery, and the authenticated platform-neutral Unix-socket hook host.
- `terratranslate-plugin-api` and `terratranslate-plugin-runner`: versioned native processor ABI
  and out-of-process execution.
- `terratranslate-wine-protocol`, `terratranslate-wine-hook`, and
  `terratranslate-wine-injector`: shared hook messages, GDI/Uniscribe/DirectWrite interception,
  and architecture-checked same-prefix DLL injection.
- `terratranslate-native-hook`: launch-time Pango, SDL_ttf 2/3, and Cairo interception.
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
- Host hooks use stable candidate identities while connection-local UUIDs route explicit producer
  enable/disable commands. Candidate samples aid discovery, but disabled text never reaches the
  model. Disconnect removes runtime routing without deleting saved configuration.
- The GTK app exposes semantic candidates and supplemental native AT-SPI text objects as
  independently selectable model inputs, including optional labels and ordered per-hook pre/post
  processors. Contemporaneous hooks with compatible postprocessing are coalesced into one turn.
- The GTK HUD can switch at runtime between a decorated positioning mode and a frameless overlay
  mode, and can be hidden or shown from the control window. The ordinary Wayland toplevel is the
  default because the compositor can move and resize it while retaining its position when the
  decorations are removed. Setting `TERRATRANSLATE_WAYLAND_OVERLAY=1` explicitly requests the
  overlay layer through `gtk4-layer-shell`; such layer surfaces are compositor-positioned and
  therefore cannot enter the manual positioning mode.
- X11 passive-grab and target-relative overlay backends remain separate from the portal-based
  Wayland path.
