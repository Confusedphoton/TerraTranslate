# Repository Guidelines

## Project Structure & Module Organization

This is a Rust 2024 workspace. Production code lives in `crates/`, split by responsibility:
`terratranslate-core` contains domain values and processors; `store` handles SQLite and blobs;
`provider` handles model requests; `engine` coordinates translation turns; `platform-linux`
contains portal, PipeWire, and Wine integration; and `app`/`cli` provide user interfaces.
Plugin and Wine protocol crates define their respective process boundaries. Architecture notes are
in `docs/architecture.md`; desktop metadata and packaging files are in `packaging/`.

## Build, Test, and Development Commands

Run commands from the repository root:

- `cargo fmt --all -- --check` verifies formatting; use `cargo fmt --all` to apply it.
- `cargo check --workspace` performs a fast workspace compilation check.
- `cargo build --workspace` builds all crates.
- `cargo test --workspace` runs the inline unit tests across the workspace.
- `cargo run -p terratranslate-app --bin terratranslate -- --diagnostics` prints Linux capability
  diagnostics without opening the GUI.
- `cargo run -p terratranslate-cli -- branches` exercises the headless client and local history.

GTK 4, PipeWire, and the XDG desktop portal are required for the full Linux application. Use
`RUST_LOG=debug` when investigating runtime behavior.

## Coding Style & Naming Conventions

Use idiomatic Rust with four-space indentation and the repository’s `rustfmt.toml` settings.
Name modules, functions, and variables in `snake_case`; types and traits in `PascalCase`; and
constants in `SCREAMING_SNAKE_CASE`. Keep responsibilities within the existing crate boundaries,
prefer explicit error types at public boundaries, and run formatting before submitting changes.

## Testing Guidelines

Tests are colocated with implementation in `#[cfg(test)]` modules. Name tests after observable
behavior, such as `merge_reports_two_sided_conflict`. Add regression coverage for domain, storage,
provider, protocol, and platform changes where practical. No repository-wide coverage threshold is
currently defined; at minimum, run `cargo test --workspace` and the relevant package tests locally.

## Commit & Pull Request Guidelines

Git history currently contains only `Initial commit`, so no project-specific convention is
established. Use short, imperative subjects (for example, `Add provider capability validation`)
and keep unrelated changes separate. Pull requests should explain the behavior and affected crates,
list validation commands and platform prerequisites, link related issues, and include screenshots or
recordings for GUI changes.

## Security & Configuration Tips

Never commit provider credentials, captured media, or local runtime state. API credentials belong in
`TERRATRANSLATE_API_KEY`; data defaults to `$XDG_DATA_HOME/terratranslate` and may be redirected
with `--data-dir`. Treat native plugins as trusted code and review configured model endpoints before
running captures.
