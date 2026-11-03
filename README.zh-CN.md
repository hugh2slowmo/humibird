# humibird

[English](README.md) · 简体中文

**一款小巧、开箱即用的桌面智能体。**

humibird 是一个原生 macOS 应用，界面基于 [GPUI](https://gpui.rs)。


## 快速开始

需要 Apple Silicon 的 Mac，以及 Rust 1.98.1（仓库里的 `rust-toolchain.toml` 已经指定好版本，`rustup` 会在第一次构建时自动装上）。因为要从 Git 拉 Zed 的几个 crate，第一次构建需要联网，也会比较耗时。

克隆仓库，然后运行：

```sh
cd humibird
cargo run
```

第一次打开时还没有 key。按 `⌘,` 进入设置，选一个服务商，粘贴 key，点 **Save**——key 会写进 humibird 配置文件夹里的 `.env`。然后在窗口顶部选一个工作区，或者直接用默认的，就可以开始聊了。

自己编译、跑测试、打 app 包、配置 Metal 工具链：见 [docs/development.md](docs/development.md)。

## 文档

| 想做什么 | 看哪里 |
|---|---|
| 日常使用 | [docs/usage.md](docs/usage.md) |
| 自己编译、跑测试 | [docs/development.md](docs/development.md) |
| 提交改动 | [CONTRIBUTING.md](CONTRIBUTING.md) |
| 报告安全问题 | [SECURITY.md](SECURITY.md) |

## 许可证

GPL-3.0-or-later。界面用到了 Zed 项目里以 GPL-3.0 授权的几个 crate（`markdown`、`ui`、`theme`），所以整个应用按同样的条款分发；GPUI 本身是 Apache-2.0。所有 Zed 相关 crate 都钉在 [`Cargo.toml`](Cargo.toml) 里的同一个版本上。
