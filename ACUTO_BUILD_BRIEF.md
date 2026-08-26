# Acuto — Build Brief: Top 5 Switch Problems

**For:** Claude Code, working in the Acuto repo (Zed fork, Rust/GPUI).
**Read this first, then verify every file path in this doc against the actual tree before writing code. Paths below are starting guesses, not facts.**

---

## Ground rules

1. **Verify before assuming.** This brief names crates and modules from upstream Zed. Confirm they exist and do what's claimed (`rg`, read the code) before planning changes. If a path is wrong, correct it and note the correction.
2. **No feature is done until it demos in one clip with zero setup.** If the user has to configure something first, it isn't a switch feature.
3. **Licence:** the fork is GPL-3.0. Anything that must stay proprietary lives behind a network boundary, not in the binary.
4. **Solo maintainer.** Prefer narrow, deep, correct over broad and half-working. A half-working switch feature is worse than none, because the demo *is* the product.
5. **Dev machine:** Windows, Ryzen 5 3400GE, ~14GB RAM, Vega iGPU. Low-end integrated GPU and modest RAM is a *feature* for this project — it's exactly the config where Zed's Windows build falls over. Test on it, don't work around it.
6. Strip residual user-facing "Zed" strings as you touch files. Don't do a repo-wide rename pass as a task; do it opportunistically.

---

## Why these five

Sourced from 2025–2026 developer complaint volume across Reddit, HN, X, LinkedIn, GitHub issues, and vendor forums. Selection filter was: **frequent × emotionally charged × not already solved by anyone**. Complaints that are loud but already solved (raw editor speed, AI autocomplete, vim motions) are excluded — Zed, Neovim, Sublime, and Copilot already own those and we inherit the speed for free.

Evidence anchors worth knowing:
- Sonar 2026 survey (1,149 devs): 96% don't fully trust AI code correctness; 38% say reviewing AI code is *more* work than reviewing a human's.
- LinearB 2026 (8.1M+ PRs): agentic AI PRs wait 5.3x longer for review (1,055 vs 201 min) and are accepted 32.7% vs 84.4%.
- Cursor's June 2025 pricing change → CEO public apology + refunds. Opacity is an existential trust risk.
- Zed shipped native Windows Oct 2025; startup crashes, GPU allocation crashes, and WSL/SSH path bugs persist.

---

# P1 — Agent diff review at scale

**The problem.** The single loudest unsolved complaint of the period. Agents produce 40-file changesets; every editor reviews them in a diff UI designed for hand-written git commits in 2005. Reviewers can't tell intent, can't tell which changes are load-bearing, and can't safely accept part of a change. Result: rubber-stamping or abandonment.

**The fix — three parts, in order.**

### 1a. Per-hunk accept/reject that actually applies
Cursor's worktree mode has documented bugs where Apply/Undo doesn't apply all file changes. Correctness here is the entire pitch. Non-negotiable.

- **Verified.** The diff surface is `crates/agent_ui/src/agent_diff.rs`; diff computation is `crates/buffer_diff/`; the multibuffer is `crates/multi_buffer/`. All confirmed present.
- **Correction — this is not greenfield.** Per-hunk accept/reject already exists upstream. `agent_diff.rs` renders per-hunk Keep/Reject buttons and registers `Keep`/`Reject`/`KeepAll`/`RejectAll` actions; the apply logic lives in **`crates/action_log/src/action_log.rs`** (`keep_edits_in_range`, `reject_edits_in_ranges`), which this brief did not name. P1a is therefore an audit-and-harden of existing code, not a new feature. Read `action_log.rs` end to end before changing anything in `agent_ui`.
- Accept/reject at hunk granularity, not file granularity.
- Every accept/reject is a single undoable transaction in the buffer's history. Reject must restore *exactly* the prior bytes — write a property test that round-trips accept-then-reject to byte equality across a generated corpus.
- Never lose a rejected hunk silently. Rejections stay visible in the review list, greyed, re-acceptable.

**Acceptance:** 100-hunk changeset across 40 files. Accept 40, reject 60, in arbitrary order, interleaved with manual edits to the same buffers. Final on-disk state matches expected byte-for-byte. Full undo restores pre-review state.

### 1b. Group hunks by intent, not by file
The reviewer's question is "what did it do," not "what files changed."

- Group hunks into semantic units: a rename touching 12 files is *one* review item, not 12.
- Cheap first pass: cluster by symbol identity using the existing tree-sitter/LSP layer (`crates/language/`, verified present), not by heuristics on the diff text.
- Fallback: ask the agent to emit a grouping alongside its edits. Cheaper than inferring, and the agent already knows its own plan. Prefer this for v1; upgrade to inferred grouping later.
- Each group gets a one-line summary and a hunk count. Collapsed by default. Expand to see hunks.

**Acceptance:** a refactor that renames a symbol across 12 files renders as one collapsible group with 12 hunks under it.

### 1c. Scope enforcement
Recurring complaint: agents edit files outside the declared task. Do this at the *tool* layer, not by prompting.

- Before a run, the agent declares (or the user sets) a path scope.
- Edits outside scope are blocked at the file-write tool and surfaced as an explicit "requested out-of-scope edit" prompt the user approves or denies.
- Denied edits are logged and visible, not swallowed.

- **Correction:** `crates/assistant_tools/` does not exist in this tree. Tools live in **`crates/agent/src/tools/`** and are registered via `add_tool` in **`crates/agent/src/thread.rs`**. The write path to intercept is `crates/agent/src/tools/edit_file_tool.rs` and `crates/agent/src/tools/write_file_tool.rs`.
- There is an existing authorization mechanism to build on rather than invent: `ToolCallEventStream::authorize` with a `ToolPermissionContext`, as used by `terminal_tool.rs`. Out-of-scope edits should reuse it so the prompt matches every other tool permission in the app.

**Acceptance:** agent instructed to modify a file outside scope cannot write it without an explicit user approval step.

---

## P1a decision record

**Written before the corpus produced a verdict.** The point of pre-registering is
that a result cannot be rationalised after the fact, so the ordering here is not
incidental: this section was committed while the test run was still going.

### What each outcome triggers

| Outcome | Action |
| --- | --- |
| **Any failure** | The apply layer is broken. Fixing it is the sprint. Visible rejections and P1b wait. |
| **Clean pass**, with generator coverage proven by `test_corpus_generator_produces_hard_shapes` | The apply layer is sound at single-buffer granularity. Move immediately to visible rejections, then P1b grouping. |
| **No verdict within one working day** | Stop. Take the answer in hand and move on. The corpus exists to produce a verdict on the apply layer, not to become a test suite. |

**Explicitly disallowed:** strengthening the corpus until it finds something. A
clean pass is a verdict, not an invitation to keep fishing. The only further
corpus work that is justified is the multi-buffer phase below, and that is
justified because it tests a *different claim*, not because the first phase came
back green.

### In-hunk edit semantics — settled on paper

**The problem.** A user edit inside a pending hunk is written on top of the
agent's bytes. Rejecting therefore means unwinding the agent's change while
preserving an edit that was derived from it. That is a rebase, and it is not
always possible.

**The decision: do not attempt it.** No merge engine, no inference, no
heuristic reconstruction.

- A hunk the user has typed into is marked **diverged**.
- Rejecting a diverged hunk asks, with exactly two options:
  1. **Restore the original** — the agent's change is unwound and the user's edit
     inside it is discarded.
  2. **Keep mine** — the user's bytes stand and the hunk leaves review unapplied.
- Neither option destroys anything silently, and there is no third "smart" path.

**Why.** Both outcomes are defensible and neither is inferable; only silence is
indefensible. Restoring original bytes silently destroys the user's work, and
keeping their bytes while reporting "rejected" is a lie about what happened. A
user hits this in week one, so it needs an answer before it arrives as a bug
report rather than after.

This is why the in-hunk case is excluded from both P1a tests: a test that
discovered it would settle a specification question by adopting whatever the
implementation happens to do.

### Multi-buffer and partial application — next in the queue

Single-buffer correctness is **necessary and invisible**. The claim P1 would
actually be advertising is the 40-file one: partial application, one file failing
mid-batch, review state desynchronised from disk. That is Cursor's documented
failure mode and the thing a switch pitch would be promising to have fixed.

Queue position: **immediately after the P1a verdict, whichever way it goes.** Not
"someday". It needs `FakeFs` write-failure injection, which is the piece of work
that does not exist yet.

### Upstream finding — worth filing

`test_random_diffs` in `crates/action_log/src/action_log.rs` computes its
expected value from `tracked_buffer.diff_base` and
`tracked_buffer.unreviewed_edits`, then asserts it equals the buffer. Both sides
are state under test, so the assertion proves self-consistency rather than
correctness: an implementation that is wrong in the same way twice satisfies it.
It never compares against the original file bytes, and never checks that
accepting keeps the agent's bytes or that rejecting restores the original ones.

The consequence is broader than one test. Upstream's confidence in the
edit-tracking layer rests on a suite whose central randomized test cannot fail
for the reason it exists, and every fork inherits that assumption.

Worth filing upstream on its merits. The moat is the review UX, not private
knowledge of a defect, and credibility in that repository is cheap at this price.

---

**Demo clip:** agent makes a 40-file change → review panel shows 6 semantic groups → accept 4, reject 2 → tests re-run on only the touched files → done in 30 seconds.

---

# P2 — Context as an inspectable object

**The problem.** "Context rot." Agents forget architectural decisions made 200 messages ago. Users work around it with hand-rolled markdown memory banks and MCP servers — a user-built band-aid for a missing product feature. Second-order: users have no idea what the agent can currently see, so failures are unattributable.

**The fix.**

- **Show the context.** A panel listing exactly what goes into the next request: files, symbols, prior turns, system rules. Nothing hidden.
- **Make it editable.** Add/remove/pin items directly. Drag a symbol or file in. Pinned items survive compaction.
- **Live token meter.** Per-item token cost and a running total against the model's window. Users should see the window filling before it overflows, not after.
- **Named saved contexts.** Save a curated set ("auth subsystem") and reload it. This is what the memory-bank hack is approximating.
- **Explicit compaction.** When the window fills, show what got summarised away and let the user re-pin it. Never silently drop.

Start by finding where the request payload is assembled before the provider call (likely `crates/agent/`). The panel renders that structure directly — don't build a parallel model of it, or they'll drift.

**Acceptance:** user can predict, before sending, exactly what the model will see, and the token count shown matches the provider's reported input tokens within 2%.

---

# P3 — Windows parity as the wedge

**The problem.** Zed's own Windows build is young and crashes: startup `0xc0000409` (STATUS_STACK_BUFFER_OVERRUN), GPU memory allocation crashes, auto-update failures, WSL/SSH path-convention bugs. Every other Zed fork is macOS-first. Cursor and VS Code work fine on Windows, so this isn't a switch *feature* — but it's a hard blocker that disqualifies us for the largest developer platform. Fix it and we're the only fast native editor Windows devs can actually run.

**The fix.**

- **Crash budget: zero P1 crashes at GA.** Reproduce the known Windows crash classes on the Vega iGPU dev machine first; it's the hostile config where they surface.
- Audit `crates/gpui/src/platform/windows/` and the graphics backend for: GPU memory allocation failure paths (must degrade, not abort), swapchain recreation on device loss / resize / display change, and stack usage on the startup path.
- **Path handling:** one canonical path type at the boundary. Audit every place a path crosses the WSL/SSH/native line. UNC paths, drive letters, `\` vs `/`, case-insensitivity. Write tests for the conversion table.
- **Auto-update must work**, including when the binary is locked by the running process. This is a trust killer if broken.
- Crash reporting on by default with a visible opt-out, so real-world failures come back to you.

**Acceptance:** 100 consecutive cold starts on the Vega iGPU machine, zero crashes. Open a WSL project and an SSH remote project; file open, save, and search all work with correct paths.

---

# P4 — Large repo / monorepo indexing

**The problem.** JetBrains' most-complained-about weakness, with real defection language attached: multi-second waits to open a single file, branch switches that freeze the UI to "10 characters per minute," users on 32GB machines threatening to leave. VS Code's C/C++ IntelliSense re-indexes on restart. Nobody has solved this well.

**The fix.**

- **Never block the UI on indexing.** Ever. Editing, search, and navigation stay responsive at all times; results improve as the index fills. This is an architectural invariant, not a setting.
- **Incremental and persistent.** Index survives restart. Branch switch diffs the tree and re-indexes only what changed — not a full rescan.
- **Lazy and prioritised.** Index open files and their imports first, then the rest in the background at low priority.
- **Honest progress.** Show what's indexed, what's pending, and that results are partial. Silent partial results are worse than visible ones.

Start with `crates/worktree/` (file scanning) and whatever project-symbol/search index exists. Measure before optimising: get a real timing harness on a 100k-file repo first.

**Acceptance:** on a 100k-file repo, open-file-to-first-keystroke under 200ms cold, and a branch switch never blocks input.

---

# P5 — A real three-way merge editor

**The problem.** VS Code's merge editor is slow ("50% CPU for 10s of seconds to load each file") and isn't genuinely three-way — you can't see base, ours, theirs, and result at once, which is exactly what kdiff3 users want. Conflict highlighting also breaks depending on whether the file was opened from Explorer or Source Control. Lower frequency than P1–P4, but high intensity and clearly solvable.

**The fix.**

- Four panes, actually simultaneous: base, ours, theirs, result. Result is editable.
- Auto-resolve the structurally unambiguous conflicts using tree-sitter — separate functions modified on both sides are not a conflict, they're two edits. Surface what was auto-resolved and let the user inspect it.
- Consistent behaviour regardless of entry point. One code path.
- Fast: no multi-second per-file load. Budget 100ms.

Look at `crates/git_ui/` and the existing diff infrastructure. Reuse `buffer_diff` rather than writing a second diff engine.

**Acceptance:** a 20-conflict merge where the majority auto-resolve, all four panes visible, per-conflict manual resolution, under 100ms per file load.

---

# Sequencing

**Stage 1 — Don't lose them (table-stakes debt).**
P3 Windows stability. Plus two known switch-back reasons that need a decision, not necessarily code, this quarter:
- **Debugger.** Upstream's is unreliable for several languages. Either fix it for the top 3 languages we target or scope out of them explicitly. Don't ship a debugger that half-works.
- **Remote / devcontainers.** Documented "I have to go back to VS Code" reason. Decide now whether it's in scope for v1; if not, say so publicly.
- **Extensions.** We inherit a small ecosystem. Curate first-party coverage for the top ~20 language/framework needs, and be upfront about the gap rather than letting people discover it.

**Stage 2 — Win them.**
P1 agent diff review, then P2 context object. These are the reason someone switches and the reason they stay after the clip. This is the core bet.

**Stage 3 — Differentiate.**
P4 indexing, P5 merge editor. Also worth queuing: transparent per-request token cost display and no surprise overages by default. The Cursor episode proved pricing opacity is an existential trust risk, and being visibly the opposite is cheap positioning.

---

# What would change this plan

- If upstream Zed ships a reliable debugger, remote devcontainers, and Windows stability before we launch, Stage 1 differentiation collapses — go all-in on P1/P2.
- If a major agent provider ships good native diff review UX, P1's window narrows — fall back to performance + Windows + privacy as a "premium minimal, no AI lock-in" position.

---

# How to work this

For each item, before writing code:
1. Confirm the real file paths and correct this doc.
2. Read the existing upstream implementation end to end. Most of these are *upgrades* to something that partly exists, not greenfield.
3. Write the acceptance test first. Every item above has one.
4. Propose the plan, then implement.

Start with **P1a**. It is the smallest correct thing that is also the highest-value thing, and everything else in P1 depends on it being right.