# humibird

English · [简体中文](README.zh-CN.md)

**A tiny desktop agent that just works.**

A native macOS window, built on [GPUI](https://gpui.rs).


## Quick start

macOS on Apple Silicon, with Rust 1.98.1 pinned by `rust-toolchain.toml`. The Zed
crates come from Git, so the first build needs the network and a few minutes.

Clone the repository, then:

```sh
cd humibird
cargo run
```

Press `⌘,`, pick a provider, paste a key, and press **Save** — the key goes into
the profile's `.env`. Choose a workspace in the header, or keep the default, and
type.

Checks, packaging and the Metal toolchain: [docs/development.md](docs/development.md).

## Documentation

| | |
|---|---|
| Use the app | [docs/usage.md](docs/usage.md) |
| Build and test | [docs/development.md](docs/development.md) |
| Send a change | [CONTRIBUTING.md](CONTRIBUTING.md) |
| Report a vulnerability | [SECURITY.md](SECURITY.md) |

## License

GPL-3.0-or-later: the UI links GPL-3.0 crates from Zed (`markdown`, `ui`,
`theme`). GPUI itself is Apache-2.0, and the Zed crates are pinned in
[`Cargo.toml`](Cargo.toml).
