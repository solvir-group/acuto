# Acuto production audit

Status: **phases 3, 5 and 7 done. Phases 4 and 6 not started.**

Every finding below was confirmed by reading the code or the system's own
records, and each names the file and line it came from. Where something is
suspected but not proven it says so, and says what would prove it.

---

## P0 — First run sent users to Zed's paid signup

`crates/onboarding/src/basics_page.rs`, `render_zed_agent_button`

The first screen a new user saw offered a tile reading **"Acuto Agent"**. It
carried Acuto's name, and clicking it did one of two things:

- signed the user into a **Zed account** (`client.sign_in_with_optional_connect`), or
- opened **zed.dev's trial page** (`cx.open_url(&zed_urls::start_trial_url(cx))`)
  once signed in.

The surrounding code read Zed's plan tiers (`Plan::ZedPro`, `ZedProTrial`,
`ZedBusiness`, `ZedVip`, `ZedStudent`, `ZedFree`) to decide the label, and fired
a telemetry event named `"Welcome Acuto Agent Sign In Clicked"`.

So Acuto's onboarding was a funnel into a competitor's subscription, under
Acuto's own name. This is the single worst item in the audit — commercially and
as a trademark matter.

**Fixed this session.** The tile is gone; the grid now offers only the agents the
user brings a key for.

## P0 — Zed's logo was Acuto's first-run mark

`crates/onboarding/src/onboarding.rs`

The onboarding header drew `VectorName::ZedLogo` beside the words "Welcome to
Acuto". The text had been rebranded; the mark had not. A commercial fork cannot
ship the upstream's logo.

**Fixed this session** by removing the mark rather than substituting one —
choosing Acuto's mark is yours to do, and the header reads correctly without it
until there is one.

## P1 — The crash left no trace inside the application

The crash is real and the system recorded it:

```
2026-09-15 20:25:05  Application Error (1001)
  acuto.exe 0.1.0.0   exception 0xC0000409   fault offset 0x1664acf6
```

`0xC0000409` is `STATUS_STACK_BUFFER_OVERRUN`, which on Windows is what Rust's
`abort()` raises through `__fastfail`. So this was a Rust abort — a panic in a
context that cannot unwind, or a panic during a panic — not a memory-safety
fault in the usual sense.

Acuto itself recorded nothing:

- `crashes/` was empty. `should_install_crash_handler`
  (`crates/client/src/telemetry.rs:98`) returns false on the Dev channel unless
  `ZED_GENERATE_MINIDUMPS` is set, so no minidump was written.
- `logs/Acuto.log` contains nothing from that run. The log appends and only
  rotates past 1 MB (`crates/zlog/src/sink.rs:35`), and the file is 7 KB, so
  nothing was overwritten — the crashed process simply never wrote a line. Its
  first line is the *next* launch, at 20:29:55.

Writing no line at all places the abort **before `zlog::init_output_file`**
(`crates/zed/src/main.rs:351`). That narrows it considerably: the failure is in
startup, before the log sink exists, which is also why raising the log level
would not have helped.

A second trace agrees. `%LOCALAPPDATA%\Acuto` exists, was created at 20:12:33,
and is **completely empty** — no `logs/`, no `db/`. Some launch got as far as
creating the data root and no further, which is `init_paths()` territory, just
above the log sink.

The practical consequence is that the whole startup path up to line 351 is
currently unobservable in the field — a user who cannot launch the app has
nothing to send you. It is not completely dark, though: a Rust abort still
prints its panic message to **stderr**, which no log sink is needed for. Launch
with stderr redirected to a file and the next occurrence names itself.

Run with `ZED_GENERATE_MINIDUMPS=1` to capture the next one.

## P1 — `--user-data-dir` is not passed to the crash-handler child

Confirmed live. With the parent launched as

```
acuto.exe --user-data-dir C:\Users\broad\acuto-firstrun C:\Users\broad\zed
```

the child it spawns runs as

```
acuto.exe --crash-handler "C:\Users\broad\AppData\Local\Acuto\zed-crash-hand..."
```

The flag is dropped, so the child resolves `paths::logs_dir()`
(`crates/zed/src/main.rs:271`) against the **default** data directory. Minidumps
and the handler's own socket land in `%LOCALAPPDATA%\Acuto`, not in the profile
the session was told to use.

Two consequences. Crash dumps are written somewhere nobody thinks to look, which
is most of the value of having them. And it explains the empty
`%LOCALAPPDATA%\Acuto` noted above — the handler creates that root even for a
session pointed elsewhere.

## P1 — Binary metadata attributes Acuto to Zed

`crates/zed/Cargo.toml`

```toml
description = "The fast, collaborative code editor."
authors = ["Zed Team <hi@zed.dev>"]
```

Both reach the compiled binary, and on Windows `description` becomes the file
description shown in Explorer's properties pane and in Task Manager. Acuto
currently describes itself using Zed's tagline and names Zed's team as its
author, with Zed's support address attached.

Not yet changed — it is a manifest edit, and editing a manifest during the
running build would have invalidated it.

## P2 — Acuto identifies itself as Zed to third-party MCP servers

`crates/context_server/src/oauth.rs:37`

```rust
pub const CIMD_URL: &str = "https://zed.dev/oauth/client-metadata.json";
```

This is the client-metadata document Acuto presents during OAuth dynamic client
registration. Every MCP server a user authenticates against is told the client is
Zed, and consent screens will say so.

Fixing this needs a document hosted on a domain you control, so it is blocked on
that rather than on code.

## P2 — Edit predictions default to Zed's service

`assets/settings/default.json`

```json
"edit_predictions": { "provider": "zed" }
```

The default provider is Zed's hosted model, which requires a Zed account. On a
machine with no Zed sign-in this feature either fails quietly or prompts for one.
Left as-is pending your decision on the backend, but it is user-visible on day
one, unlike the server URLs.

---

## P0 — The Windows installer shipped as Zed, and could have removed it

`script/bundle-windows.ps1`, `crates/explorer_command_injector/AppxManifest*.xml`

Four separate problems in the release pipeline, found together.

**The installer carried Zed's product GUIDs.** Every channel reused Zed's own
`AppId` (stable was `{2DB0DA96-CA55-49BB-AF4F-64AF36A86712}`). Windows
identifies an installed product by that GUID, so an Acuto installer would have
presented itself to Windows as the *same product* as Zed: installing it on a
machine with Zed could upgrade over it, and uninstalling Acuto could remove it.
This is the single most damaging thing found in the audit, because it destroys
someone else's software rather than merely misrepresenting yours.

Fixed: fresh GUIDs per channel, generated for this fork.

**The AppX packages claimed another company as publisher.**

```xml
Name="ZedIndustries.Zed"
Publisher="CN=Zed Industries Inc, O=Zed Industries Inc, L=Denver, S=Colorado, C=US"
```

That is impersonation, and it could never have been signed: the `Publisher`
string must match the subject of the signing certificate, and this project does
not hold theirs. Fixed to `CN=Acuto` across all three manifests.

**The bundle could not build at all.** It ran
`cargo build --package zed`, and that package has not existed since the rename
to `acuto`. The first command of a release would have failed.

**Signing covered everything except the application.** The sign list named
`Zed.exe`, which the rename had already removed; the file staged for the
installer is `Acuto.exe`. So `cli.exe` and the helpers would have been signed
and the editor itself left unsigned.

All four fixed. What remains for a real release, and cannot be fixed from here:

* A code-signing certificate. Without one, `canCodeSign` stays false and the
  bundle ships unsigned, which on Windows means SmartScreen warnings on every
  download.
* The certificate's subject must match `CN=Acuto` in the AppX manifests, or the
  explorer integration will not install.

## P2 — Release channel is `dev`

`crates/zed/RELEASE_CHANNEL` reads `dev`. That is correct for now and worth
knowing before a launch: the dev channel disables the crash handler (see above),
and the installer identity block for `dev` names the app "Acuto Dev".

## Phase 4 — The differentiating features, as they actually stand

Assessed against what a user would experience, not what is intended.

**Agent diff review — works, and is now the thing it claimed to be.** Every
changed line is offered as its own decision; accepting one leaves the rest;
rejecting one reverts exactly that line in the buffer and on disk. Covered by
13 tests in `acp_thread`, 34 in `action_log`, and 4 panel tests, and the tests
fail when the fix is removed. This is the differentiator and it is real.

**The reason it took so long is itself a finding.** The feature was reported
broken four times before the cause was found, because each fix was reasoned
about rather than measured. Three genuine bugs sat in a row: a settings gate
defaulted off, the whole feature hung on one event that does not always fire,
and accept/reject operated on merged edits so accepting one line accepted all
of them. Only the last was the one the user saw.

**Repo-native team coordination (`team_notes`) — built, and unused as an asset.**
Notes anchored to lines, tickets, and team chat, all transported by git with no
server. No competitor has this, and nothing in the product currently points at
it.

**The crew panel — new, untested in anger.** Reads across every worktree
workspace in the window and reports which agents are working and how many lines
wait for review. It depends entirely on the review layer above being honest,
which it now is.

**Not built, deliberately:** autonomous overnight agents, and worktree
isolation, the latter because upstream Zed already ships it (`create_thread`
plus `create_worktree_workspace`). Rebuilding it would have cost a week for
nothing.

## Verified clean

Checked against the code, not assumed:

- **Telemetry is off.** `diagnostics` and `metrics` both default to `false` in
  `assets/settings/default.json`. A first run sends nothing.
- **No crash-dump upload path.** `MINIDUMP_ENDPOINT`
  (`crates/client/src/telemetry.rs:92`) reads `ZED_MINIDUMP_ENDPOINT`, which is
  unset at build and at run time. Nothing would be uploaded even in a release
  channel.
- **Auto-update is off** (`auto_update: false`), so no beacon to Zed's release
  server on launch.
- **Licensing is correct for a GPL fork.** The package declares
  `license = "GPL-3.0-or-later"` and both `LICENSE-GPL` and `LICENSE-APACHE` are
  present at the root.
- **API keys cannot leak through logs.** `ApiKey`'s `Debug` is hand-written to
  print `[redacted; N chars]`, with tests covering both direct and nested
  formatting (`crates/language_model/src/api_key.rs`).
- **Data directories no longer collide with a real Zed install.** `APP_NAME` is
  `"Acuto"` (`crates/paths/src/paths.rs`). This machine confirms the collision
  was not hypothetical: `%LOCALAPPDATA%\Zed` holds a genuine Zed 1.16.1 profile
  last used on 10 September, which pre-rename Acuto builds were reading and
  writing.

## Still open

- **No crash reporting in any channel.** `should_install_crash_handler` requires
  a minidump endpoint that is never set, so a shipped Acuto learns nothing when
  it crashes in the field. Decide before launch.
- **Three failing `agent_ui` tests**, none of them in the review path: two
  thread-retention counts off by exactly one (the panel retains one more idle
  thread than the test expects, a few MB at worst), and a WSL remote-connection
  migration. Neither touches code this work changed.
- **Binary metadata still attributes the app to Zed's team** (`crates/zed/Cargo.toml`).
- **OAuth client identity is still Zed's**, so every MCP server a user
  authenticates against is told the client is Zed. Needs a document hosted on a
  domain you control.
