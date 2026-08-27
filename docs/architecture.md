# Architecture

## Data flow

```text
XDG portal/PipeWire frames ─┐
Native/Wine semantic text ──┼─> sampling/processors ─> model provider ─> postprocessors
PipeWire application audio ─┘                              │
                                                          v
                                          SQLite per-game commit DAG + blob store
                                                          │
                                          HUD / embedding adapter
```

`terratranslate-engine` is the transaction boundary. It resolves the active game's branch head,
stores all source payloads, executes ordered processors, renders the configured system and user
prompt templates, validates the selected model's modality/tool capabilities, calls the provider,
applies context changes, creates a content-addressed commit, and atomically advances the branch.
If the branch moves during inference, the commit remains preserved but is not checked out.

## Crates

- `terratranslate-core`: stable domain values, commit hashing, context merge, processors, frame
  sampling, and voice segmentation.
- `terratranslate-store`: SQLite graph metadata, per-game compare-and-swap branch refs,
  merge-base search, legacy-session migration, and content-addressed blobs.
- `terratranslate-provider`: provider capability negotiation and OpenAI-compatible multimodal/tool
  requests.
- `terratranslate-engine`: end-to-end versioned translation turns.
- `terratranslate-platform-linux`: desktop capability detection, portals, native launcher, Wine
  process discovery, and the authenticated hook host. Native clients use a Unix socket; Wine
  clients use an ephemeral loopback TCP listener because Winsock under Wine lacks reliable
  `AF_UNIX` support.
- `terratranslate-plugin-api` and `terratranslate-plugin-runner`: versioned native processor ABI
  and out-of-process execution.
- `terratranslate-wine-protocol`, `terratranslate-wine-hook`, and
  `terratranslate-wine-injector`: shared hook messages, GDI/Uniscribe/DirectWrite interception,
  module-agnostic HarfBuzz interception for custom GPU renderers, and architecture-checked
  same-prefix DLL injection.
- `terratranslate-native-hook`: launch-time Pango, SDL_ttf 2/3, and Cairo interception.
- `terratranslate-app`: GTK4/Relm4 control surface and diagnostics CLI.

## History and merges

Translations and input events are immutable. A normal turn has one parent. Each registered game has
its own `main` ref and named branch namespace; the same branch name can therefore exist for every
game without sharing its head. Branching only creates or moves a named ref. A manual merge finds
the closest common ancestor, automatically applies one-sided changes, and reports two-sided
conflicts. The user supplies a resolved context snapshot; the resulting commit has both heads as
parents. Source turns on both sides remain accessible and are not flattened into a synthetic chat
transcript. Existing databases are copied into a `default` game namespace on first open.

### Prompt templates

The engine renders both configured prompts against one `PromptData` value after text processors have
run. Game identity fields are stable and do not include a PID. Supported scalar macros include
`{{game.id}}`, `{{game.name}}`, `{{game.executable}}`, `{{game.image_id}}`, `{{game.platform}}`,
`{{game.runtime}}`, `{{source_language}}`, `{{target_language}}`, `{{branch}}`,
`{{context.summary}}`, `{{context.style}}`, `{{context.scratchpad}}`, `{{context.glossary}}`, and
`{{context.entities}}`. `{{texts}}` joins text inputs, while `{{texts|enumerate}}` labels each one
with its one-based position. `{{updated_texts}}`, `{{updated_texts|enumerate}}`, and
`{{updated_hook_count}}` expose only newly observed inputs. For custom formatting,
`{{#each texts}}...{{/each}}` exposes `{{number}}`, `{{label}}`, `{{text}}`, `{{hook_id}}`,
`{{source}}`, `{{target}}`, and `{{updated}}` inside the loop. Conditional blocks use
`{{#if updated_hooks}}...{{else}}...{{/if}}` or `{{#unless updated_hooks}}...{{/unless}}`; inside
a hook loop, `{{#if updated}}...{{/if}}` checks the current hook. The `trim`, `upper`, `lower`,
and `json` filters can be applied with `|` (or `:`), and `{{texts|default("fallback")}}` supplies
a configurable fallback for an empty value.

An endless-context request is an explicit one-shot exception at the model-request boundary. The
engine walks every commit reachable from the active game's `main` in oldest-first order and sends the source,
translation, and context snapshot from each commit alongside the current request. Historical
scratchpads are removed from that replay. The caller may reinsert the current request's scratchpad;
otherwise the scratchpad is omitted from both the provider request and the resulting context.

## Plugin ABI

Plugins export `terratranslate_plugin_v1` and exchange MessagePack domain requests through owned
buffers. The host never loads them into the GUI: `terratranslate-plugin-runner` loads one library,
validates ABI version and buffer bounds, and exposes framed stdin/stdout messages. The production
launcher should additionally apply process timeouts, memory limits, and a seccomp profile.

## Platform completion map

- Portal window permission, PipeWire node discovery, mapped raw video acquisition, PNG conversion,
  optional SSIM-based reuse of cached vision frames, and shortcut binding are implemented.
- Per-application playback-node discovery, targeted PCM acquisition, stereo downmixing, and the
  model-facing VAD/segmentation path are implemented; the GTK target picker still needs to expose
  the discovered nodes.
- Host hooks use stable candidate identities while connection-local UUIDs route explicit producer
  enable/disable commands. Candidate samples aid discovery, but disabled text never reaches the
  model. Disconnect removes runtime routing without deleting saved configuration.
- The GTK app exposes semantic candidates and supplemental native AT-SPI text objects as
  independently selectable model inputs, including optional labels and ordered per-hook pre/post
  processors. By default, a turn contains only hooks that reported new text; an optional latest-
  value mode can add the most recently observed value from every enabled hook. Contemporaneous
  hooks with compatible postprocessing are coalesced into one turn.
- The GTK HUD can switch at runtime between a decorated positioning mode and a frameless overlay
  mode, and can be hidden or shown from the control window. The ordinary Wayland toplevel is the
  default because the compositor can move and resize it while retaining its position when the
  decorations are removed. Setting `TERRATRANSLATE_WAYLAND_OVERLAY=1` explicitly requests the
  overlay layer through `gtk4-layer-shell`; such layer surfaces are compositor-positioned and
  therefore cannot enter the manual positioning mode.
- X11 passive-grab and target-relative overlay backends remain separate from the portal-based
  Wayland path.
