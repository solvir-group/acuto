# Fork config

Version-controlled source for everything that lives in the fork's data dir.
The repo is the source of truth; the data dir is a build artifact.

```powershell
.\config\apply.ps1
```

Copies `themes/*.json` (and, from Phase 3, `settings.json` / `keymap.json`) into
`<data-dir>\config\`. Zed's themes watcher polls the destination, so the copy triggers
a hot reload — **no rebuild, no restart**.

Run Zed against the isolated dir so none of this touches the installed Zed:

```powershell
cargo run -- --user-data-dir C:\Users\broad\zed-fork-data
```

## themes/monochrome.json

One family, three themes, all `appearance: "dark"`:

| Theme | Chrome | Editor | Character |
|---|---|---|---|
| **Monochrome Dark** | `#050506` | `#0a0a0b` | the spec palette as written |
| **Monochrome Warm** | `#070605` | `#0c0b0a` | greys shifted warm, syntax hues warmed to match |
| **Monochrome Contrast** | `#000000` | `#060607` | pure-black chrome, white text, saturated syntax |

Each carries 130 style keys and all 46 syntax tokens. Validated against the key list
generated from `crates/settings_content/src/theme.rs` — a misspelled key does **not**
error, it silently falls back to the Zed default, so re-run validation after editing.

### Structure

Chrome is greyscale because it is structure, not content. Two planes:
`--chrome` (`#050506`) is recessed — tab strip, status bar, title bar, panels.
`--bg` (`#0a0a0b`) is raised — the editor surface. The active tab is filled with
`--bg` so it merges into the editor below.

Syntax uses five hues on top of a weight axis. Only `keyword` / `preproc` carry
weight 500; everything else is normal weight, with `title` and `emphasis.strong`
at 600.

## Judgement calls

Three places where the spec didn't determine the answer. Each is a one-line change.

**`operator` is punctuation grey, not keyword violet.** The spec puts "word-form
operators" in the violet keyword row, but Zed has no `keyword.operator` key — there
is only `operator`, and across tree-sitter grammars that capture is overwhelmingly
symbolic (`+`, `==`, `=>`). Word-form operators like `and` / `in` / `is` are usually
captured as `keyword` anyway, so they already come out violet. Flip `operator` to
`#a78bfaff` with `"font_weight": 500` if you disagree once you see it on screen.

**`variable.special` inherits from `variable`**, per the spec's "fill the remaining
tokens by inheritance from the nearest parent" rule. That means `self` / `this` render
as plain variables. Making them violet reads better to some people — one line if you
want it.

**`text.accent` is violet, not white.** Chrome is greyscale by spec, but `text.accent`
is what marks *matched characters* in search results and file finder. Against `text`
(`#f2f0ed`) a white accent is invisible, so the match signal would be lost — that is
colour carrying information, which the spec permits. Set it to `#f2f0edff` for strict
greyscale, accepting that fuzzy-match highlighting disappears.

## `accents` — reserved for Phase 5

The `accents` array is ordered deliberately. Phase 5's coloured file-tree icons will
index into it, so the hues stay tunable in JSON without a rebuild:

| Index | Hue | Use |
|---|---|---|
| 0 | `#f59e0b` amber | folder, folder-open |
| 1 | `#3b82f6` blue | code — `.ts` `.tsx` `.rs` |
| 2 | `#a855f7` purple | styles — `.css` |
| 3 | `#eab308` yellow | config — `.json` `.toml` |
| 4 | muted grey | text and everything else |

**Do not reorder without updating the Phase 5 mapping.** These are the hexes from the
design spec's icon table, unchanged.

## Not set here

`ui_font_family`, `buffer_font_family`, line height, panel docks and density are
**settings**, not theme — they land in `settings.json` in Phase 3. Tab geometry,
scrollbar width and the UI text ramp are hardcoded in Rust and are Phase 5.
