

# Acuto

Acuto is a code editor for working with AI coding agents. Claude Code, Codex, GitHub Copilot, Gemini and Google Antigravity run side by side in one window, and you review every change they make before it counts.

Acuto is built on [Zed](https://github.com/zed-industries/zed) and keeps Zed's speed: it is written in Rust and draws its UI on the GPU.

## Download

**[Download Acuto for Windows](https://github.com/solvir-group/acuto/releases/latest/download/Acuto-x86_64.exe)** (64-bit, Windows 10 or later)

Run the installer. Windows may show "Windows protected your PC", because Acuto is not code signed yet. Click **More info**, then **Run anyway**.

All versions are on the [releases page](https://github.com/solvir-group/acuto/releases). macOS and Linux builds are not available yet.

## What it does

- **Your agents, in one place.** Claude Code, Codex, GitHub Copilot, Gemini and Google Antigravity connect over the [Agent Client Protocol](https://agentclientprotocol.com). Each signs in with its own account, so you use the plans you already have.
- **Review before it lands.** Every edit an agent makes shows up as a diff. Keep or reject it by file, by hunk or by line.
- **Inline suggestions.** Ghost-text completions as you type; press Tab to accept. Bring your own OpenAI-compatible endpoint and key.
- **Team notes and tickets.** Leave notes on a line of code, track tickets, and hand a ticket to an agent by mentioning it, for example `@claude`.
- **Themes.** Acuto Frosted (a frosted-glass frame over your desktop wallpaper, on Windows), Acuto Paper and Acuto Noir.

## Building from source

Acuto builds like Zed; see [Building Zed for Windows](./docs/src/development/windows.md) for the toolchain. Then:

```sh
cargo run -p acuto
```

To build the Windows installer locally, install [Inno Setup 6](https://jrsoftware.org/isinfo.php) and run:

```powershell
./script/bundle-windows.ps1 -Architecture x86_64
```

The installer is written to `target/Acuto-x86_64.exe`.

## Releasing

Pushing a version tag builds the installer on GitHub Actions and publishes it as a release:

```sh
git tag v0.1.1
git push origin v0.1.1
```

See [.github/workflows/acuto_release_windows.yml](./.github/workflows/acuto_release_windows.yml).

## Licence

Acuto is free software under the [GNU General Public License v3.0 or later](./LICENSE-GPL), with some components under [Apache-2.0](./LICENSE-APACHE) where marked. It is a modified version of Zed, © Zed Industries, Inc.

Acuto is an independent project. It is not affiliated with or endorsed by Zed Industries, Anthropic, OpenAI, GitHub or Google; their product names belong to them.

Third-party licences are collected with [`cargo-about`](https://github.com/EmbarkStudios/cargo-about). If a new crate trips it, add `publish = false` to the crate's `Cargo.toml`, or add a checked licence to `accepted` in [`script/licenses/zed-licenses.toml`](./script/licenses/zed-licenses.toml).
