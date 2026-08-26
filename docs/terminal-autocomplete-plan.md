# Terminal autocomplete — Phase 0 recon

Findings from reading the codebase, not from assumption. Every path and type
name below was verified in this tree. Where the brief and the codebase disagree,
the codebase is recorded as the fact and the disagreement is called out.

---

## Headline: three things in the brief are wrong about this codebase

### 1. There is no shell integration to extend. Phase 1 is greenfield.

The brief says "If Zed already ships partial integration, extend it rather than
replacing it."

Searching for `shell_integration`, `zshrc`, `bashrc`, `Microsoft.PowerShell_profile`
and `133` across `crates/` and `assets/` returns nothing relevant — the only hits
are `dev_container`, the bash *grammar* config, the docker remote transport, the
icon theme, and the Windows installer script. `find assets -iname "*integration*"
-o -iname "*.zsh" -o -iname "*profile*"` returns empty.

**There is no OSC 133 emission, no shell hook injection, and no semantic prompt
concept anywhere in the tree.** All four shells must be written from scratch,
including the injection mechanism (deciding where the scripts live, how they are
sourced without clobbering a user's rc file, and how PowerShell's profile is
handled on Windows where execution policy may block it).

This is the single largest under-estimate in the brief.

### 2. OSC 133 will be silently swallowed. There is no hook to add.

The brief assumes there is a place "where custom OSC handling would hook in".
There isn't, and the reason is structural.

- `alacritty_terminal` is **not vendored**. Root `Cargo.toml:521` pins it to a
  Zed fork: `git = "https://github.com/zed-industries/alacritty", rev = "4c12966…"`.
- PTY bytes never pass through Zed code. `alacritty_terminal::event_loop::EventLoop`
  owns the PTY, parses on its own thread, and mutates the `Term` directly. Zed
  only learns that *something* happened via `AlacTermEvent::Wakeup`.
- The listener seam is `impl EventListener for ZedListener` at
  `crates/terminal/src/alacritty.rs:326`, whose `send_event` forwards
  `AlacTermEvent` into Zed's channel.
- **`alacritty_terminal::event::Event` has no OSC passthrough variant.** It
  carries `MouseCursorDirty`, `Title`, `ResetTitle`, `ClipboardStore`,
  `ClipboardLoad`, `ColorRequest`, `PtyWrite`, `TextAreaSizeRequest`, `Bell`,
  `Wakeup`, `Exit`, `ChildExit`. An unrecognised OSC is parsed and dropped.
- A wrapper newtype implementing `Handler` cannot be interposed, because
  `EventLoop::new(terminal: Arc<FairMutex<Term<U>>>, …)` takes `Term<U>`
  concretely rather than `impl Handler`.

`Terminal::write_output` (`crates/terminal/src/terminal.rs:1897`) *does* drive a
Zed-owned `Processor` at line 1904, but that path is display-only and explicitly
"bypasses the PTY/event loop". It is not the path real shell output takes.

**Consequence:** Phase 1 cannot be done without one of the two options below.
This decision gates everything and should be made before any Phase 1 code.

| Option | Cost | Risk |
| --- | --- | --- |
| **A. Fork the Zed alacritty fork.** Add an `Event::Osc(Vec<Vec<u8>>)` variant (or narrow `Event::PromptMarker`) emitted from `osc_dispatch` for unhandled codes. Repoint `Cargo.toml:521`. | ~30 lines in alacritty; one more repo; a rev bump whenever Zed bumps theirs | Low technically. Ongoing maintenance. Plausibly upstreamable — alacritty has long-standing interest in OSC 133. |
| **B. Replace `EventLoop` with a Zed-owned read loop.** Read the PTY in-tree, tap the byte stream for OSC 133/7, forward everything to `Processor::advance`. | ~200–400 lines: PTY polling, resize, drain-on-exit, child-exit handling | Higher. Reimplements load-bearing upstream code that currently works. |

**Recommendation: A.** It is an order of magnitude less code, the dependency is
already a fork so the maintenance pattern exists, and B means owning terminal
I/O correctness forever in exchange for avoiding one small patch.

### 3. A competing implementation already exists in this fork and contradicts the thesis.

`crates/terminal_view/src/command_suggest.rs` (commit `a77dac0fb2`) implements
fish-style suggestions by **reconstructing the typed line from keystrokes** —
precisely the approach the brief's core thesis rejects. Its own module doc admits
the flaw: tracking is abandoned whenever an unmodellable key arrives, and it
never resumes until the next Enter.

It should be **deleted in Phase 1**, not extended. Its history store and ranking
ideas are worth reading; its input model is the thing the brief exists to replace.

---

## The six areas

### 1. `crates/terminal/`

| Item | Location |
| --- | --- |
| Main type | `Terminal` in `crates/terminal/src/terminal.rs` (~4700 lines) |
| Alacritty glue | `crates/terminal/src/alacritty.rs` — `ZedListener`, event conversion |
| Hyperlink detection | `crates/terminal/src/alacritty/hyperlinks.rs` |
| Settings | `crates/terminal/src/terminal_settings.rs` — `TerminalSettings` |
| PTY metadata | `crates/terminal/src/pty_info.rs` |
| Parser imports | `use vte::ansi::{Attr, Handler, Processor, StdSyncHandler}` (`terminal.rs:53`) |
| Grid snapshot | `Terminal::last_content: Content` (`terminal.rs:1460`) |
| Write to PTY | `Terminal::input(&mut self, …)` (`terminal.rs:2062`), private `write_to_pty` (`2046`) |
| Working directory | `Terminal::working_directory()` (`terminal.rs:2801`) — already exists, currently derived from the child process, not OSC 7 |

`Content` in `last_content` is the grid snapshot the UI paints from. `edit_buffer()`
should be derived from it plus a `command_start` anchor, not from a parallel
reconstruction.

### 2. `crates/terminal_view/`

| Item | Location |
| --- | --- |
| View | `TerminalView` in `crates/terminal_view/src/terminal_view.rs` |
| Element | `crates/terminal_view/src/terminal_element.rs` |
| Key entry | `TerminalView::key_down` → `process_keystroke` → `term.try_keystroke(...)` |
| Render | `impl Render for TerminalView` — root `div().id("terminal-view")`, `.on_key_down(cx.listener(Self::key_down))` |

`key_down` is the correct place to intercept Tab-while-popup-open. It already
returns early on handled keys via `cx.stop_propagation()`, so the pattern exists.
The fork's own `command_suggest` hook sits here and shows the shape.

### 3. Shell integration

**Does not exist.** See headline 1.

### 4. `crates/db/` and `crates/sqlez/`

Confirmed present and usable; **no `rusqlite` needed**.

- `crates/db/src/db.rs`, `kvp.rs`, `query.rs`
- `crates/sqlez/src/`: `connection.rs`, `migrations.rs`, `bindable.rs`,
  `savepoint.rs`, `domain.rs`, `statement.rs`

`domain.rs` is the registration point — a new crate declares a `Domain` with its
migrations. The history table in Phase 2 should be a new `Domain`, not an
addition to an existing one, so the migration is independently versioned.

### 5. `crates/settings/`

`TerminalSettings` (`terminal_settings.rs:22`) is the struct to extend. Note this
fork already carries settings additions (`ProjectPanelRowSettings`), so the
pattern of adding a nested settings struct is established — but it lives in
`crates/settings_content`, which sits near the root of the dependency graph.
**Adding fields there triggers a near-full rebuild (~93 crates).** On this
machine that is a real cost; batch all `terminal.completion` settings into one
change rather than adding them per phase.

### 6. Existing completion UI worth reusing

`CompletionsMenu` in `crates/editor/src/code_context_menus.rs:246`.

Worth reading for the popup's list rendering, fuzzy filtering and keyboard
handling. It is coupled to `Editor` state, so expect to borrow the *shape* rather
than the type. `crates/ui`'s `ContextMenu` and `right_click_menu` are the
lighter-weight primitives already used elsewhere in this fork.

---

## Reference repos

Cloned to `reference/`, excluded via `.git/info/exclude` (not `.gitignore`), so
the tracked diff against upstream Zed stays clean.

| Repo | Size | Licence |
| --- | --- | --- |
| `q-autocomplete` | 248M | MIT/Apache-2.0 |
| `carapace-bin` | 145M | MIT |
| `fig-specs` | 112M | MIT |
| `atuin` | 14M | MIT |
| `inshellisense` | 2.2M | MIT |

**Correction to the brief:** the atuin URL is `atuinsh/atuin`, not
`atuinsh/atuin`. The latter 404s.

fish-shell was **not** cloned and must not be, per the GPL-2.0-only
incompatibility noted in the brief.

---

## Revised phase ordering

Phase 1 as written is blocked on the alacritty decision, so it splits:

- **Phase 1a — the OSC seam.** Fork alacritty, add the passthrough variant,
  repoint the dependency, prove an arbitrary OSC reaches Zed code. Nothing
  user-visible. This is the whole risk of Phase 1 concentrated in one small
  change; do it first and alone.
- **Phase 1b — `ShellState` and shell scripts.** Only meaningful once 1a lands.
  Four shells, from scratch, plus injection and a dismissible install prompt.
- **Phase 1c — delete `command_suggest.rs`** and its wiring in `terminal_view.rs`.

Everything from Phase 2 onward is unaffected by this restructuring.

---

## Open questions for the brief's author

1. **Fork or replace the event loop?** Recommendation above is fork. It is a
   one-way-ish door (another repo to keep) and worth an explicit answer.
2. **Where do shell integration scripts live and how are they injected?** Zed has
   no precedent here. Sourcing into a user's rc file is invasive; the alternative
   is injecting via the shell's own startup flags, which differs per shell and is
   awkward for PowerShell under a restrictive execution policy.
3. **Does the remote-session guard need to work before Phase 2 ships?** The guard
   checklist lists it, and this fork's terminal is frequently used over SSH.
4. **Settings batching** — confirm all `terminal.completion` keys land in one
   `settings_content` change rather than accreting per phase.
