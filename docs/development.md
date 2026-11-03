# Building and testing humibird

For using the app, read [usage.md](usage.md). The working agreement — how changes
are sized, committed, and verified — is in [AGENTS.md](../AGENTS.md); this page
is the mechanics.

## Requirements

- **macOS on Apple Silicon.** The app is developed and tested here. GPUI runs on
  Linux and the profile layout is plain paths, but nothing is packaged or
  verified off macOS yet.
- **Rust 1.98.1**, pinned by [`rust-toolchain.toml`](../rust-toolchain.toml).
  `rustup` installs it on first build.
- **A C toolchain** (Xcode or the Command Line Tools) for the native
  dependencies, including a bundled SQLite.
- **Network on the first build.** GPUI, the Markdown renderer, the grammars and
  the theme crates are pinned Git dependencies of the Zed repository, not
  crates.io releases. Several of them are patched to Zed's own forks in
  [`Cargo.toml`](../Cargo.toml); those patches have to stay in lockstep with the
  pinned revision.

## Build and run

```sh
cargo run
```

The first build compiles GPUI and takes a while; later builds are incremental.
Shaders are compiled during the build when the Metal compiler is available and
at launch otherwise (see [Packaging](#packaging)).

## Checks

Every change has to pass all three before it is committed:

```sh
cargo fmt --all --check
cargo clippy --offline --all-targets -- -D warnings
cargo test --offline
```

`--offline` is not required once the dependencies are in `~/.cargo`, but it keeps
a build from surprising you with a network fetch. Tests that touch the UI are
GPUI tests: they open a real window in a headless test harness, so they run with
the rest of `cargo test`.

Unit tests should assert behaviour — what a user or a caller can observe — not
the shape of the code. Where a change fixes a bug, the test should fail on the
commit before it.

## Packaging

```sh
./script/bundle-mac   # target/dmg/Humibird-0.1.0-arm64.dmg
```

The script builds the release binary for the host (Apple Silicon), assembles
`Humibird.app`, signs it ad-hoc — what arm64 needs in order to launch at all —
and wraps it in a DMG with an `/Applications` shortcut. It is **not notarized**,
so the first launch has to be allowed by hand: System Settings → Privacy &
Security → **Open Anyway**. Signing, notarization, and auto-updates are unsolved
(the app has no updater).

Xcode 26 and newer ship the Metal compiler as a separate download. Without it
the script falls back to compiling GPUI's shaders on first launch, which is
slower to start; `xcodebuild -downloadComponent MetalToolchain` restores the
build-time path and the script picks it up by itself.

## Assets

The app icon is a hummingbird. `crates/humibird/resources/icon.png` is the
1024×1024 master — the artwork's rounded tile at 824×824 on Apple's grid, in a
transparent margin — and `icon-source.png` is the artwork as delivered. The
master is that artwork's tile cut out of its own soft shadow, because macOS draws
the shadow itself; redo that cut by hand if the artwork is replaced.

`script/make-icon` regenerates both committed derivatives from the master:

- `crates/humibird/resources/Humibird.icns` for the bundle;
- `crates/humibird_ui/resources/mark.png`, the sidebar brand mark, cropped to the
  tile so the bird fills the square it is drawn in.

Nothing in a normal build needs the script — run it only after the artwork
changes.

### Icons

The window's icons are Zed's, copied from the pinned checkout's `assets/icons/`
into `crates/humibird_ui/resources/icons/` with that directory's `LICENSES`
beside them (Lucide's ISC, and Zed's own). `ui` names an icon
(`IconName::ChevronDown`) and resolves it to `icons/chevron_down.svg`, so the app
needs an `AssetSource` that answers with the bytes: `humibird_ui::Assets` does,
and `application().with_assets(humibird_ui::Assets)` installs it. The files are
embedded with `include_bytes!`, like the sidebar mark, so a bundle carries them
and a debug build behaves like a release one. Without a source, `ui` skips every
icon in silence — that is how the transcript came to draw no chevrons at all.

Adding an icon is a file plus a line in `assets.rs`;
`timeline::tests::every_icon_the_window_asks_for_is_vendored` fails until both
are there.

## Repository layout

| Crate | Owns |
|---|---|
| `humibird` | the binary: window, actions, keymap, menu bar |
| `humibird_ui` | the window root, the projection, Markdown, timeline, cards, settings window |
| `humibird_agent` | the conversation, the turn loop, events, compaction |
| `humibird_prompt` | system-prompt assembly; pure, no I/O |
| `humibird_providers` | chat types, SSE decoders, provider presets and adapters |
| `humibird_tools` | the fourteen tools and the workspace boundary |
| `humibird_mcp` | the context server (MCP) client: stdio child processes and streamable HTTP, JSON-RPC both ways |
| `humibird_memory` | the two curated memory files |
| `humibird_skills` | skill import and the `SKILL.md` index |
| `humibird_store` | SQLite: sessions, messages, tool calls, usage, compactions |
| `humibird_profile` | profile paths, `config.toml`, `.env` |

The layering rules that keep these honest: only `humibird_ui` touches GPUI, only
`humibird_profile` resolves profile paths or secrets, and the turn loop never
learns a provider's wire format.

## Upgrading the Zed pin

Every Zed crate in [`Cargo.toml`](../Cargo.toml) points at one revision; keep it
that way, or the crates drift apart. An upgrade is a quarterly-sized job:

1. Bump every `rev =` in the workspace dependencies together.
2. Re-run `cargo test`; GPUI is pre-1.0 and its APIs move.
3. Re-check the `[patch.crates-io]` forks still point at revisions the new Zed
   revision expects.

Keep the surface of Zed internals small — the UI layer should look like an
application built on GPUI, not a fork of Zed's editor.
