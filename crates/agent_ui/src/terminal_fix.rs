//! Turning a failed command into a diff.
//!
//! The editor already knows everything needed to diagnose a build failure: what
//! ran, where it ran, what it printed, and which files it named. Today the user
//! reproduces all of that by hand -- select the error, copy it, switch windows,
//! describe the situation, paste, read a reply, and type the fix in themselves.
//! Every step of that is the editor withholding something it already has.
//!
//! This takes the recorded block (see `terminal::blocks`) and produces edits in
//! the buffers, unsaved. Unsaved is the point: modified buffers already have a
//! hunk-review UI in this editor -- gutter marks, per-hunk revert, diff view --
//! so the fix arrives in the surface people already use to review a change, and
//! nothing reaches disk until they save it.
//!
//! # Why a diff and not a chat message
//!
//! A chat reply about a compile error is a thing you read and then retype. A
//! diff is a thing you accept. The model is constrained to unified diff output
//! for that reason, and anything it says outside a diff hunk is discarded rather
//! than shown.

use std::{collections::BTreeSet, path::PathBuf, sync::LazyLock};

use futures::StreamExt as _;
use gpui::{App, AsyncApp, Entity};
use language::Buffer;
use language_model::{
    ConfiguredModel, LanguageModelRegistry, LanguageModelRequest,
    LanguageModelRequestMessage, Role,
};
use project::{Project, ProjectPath};
use regex::Regex;
use terminal::blocks::CommandBlock;
use terminal_view::terminal_panel::TerminalPanel;
use ui::prelude::*;
use util::ResultExt as _;
use workspace::Workspace;

use gpui::actions;

actions!(
    agent,
    [
        /// Turns the terminal's most recent failed command into edits you can
        /// review.
        FixTerminalFailure
    ]
);

/// How many lines of the tail to send.
///
/// The end of a log is where the error summary is; the beginning is the
/// toolchain banner. A hundred and twenty lines covers a Rust error with its
/// notes and the cargo summary underneath.
const OUTPUT_TAIL_LINES: usize = 120;

/// How many files named in the output to include.
const MAX_CONTEXT_FILES: usize = 6;

/// How much of any one file to include.
const MAX_FILE_BYTES: usize = 32 * 1024;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(
            |workspace, _: &FixTerminalFailure, window, cx: &mut Context<Workspace>| {
                fix_terminal_failure(workspace, window, cx);
            },
        );
    })
    .detach();
}

fn fix_terminal_failure(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(block) = latest_failure(workspace, cx) else {
        workspace.show_error(
            anyhow::anyhow!(
                "No failed command to fix. Shell integration has to be active for the \
                 editor to know a command failed."
            ),
            cx,
        );
        return;
    };

    // A block whose output carried something credential-shaped is never sent
    // anywhere. The redaction in `terminal::blocks` already replaced it, but a
    // pattern that matched once may have missed a second form of the same
    // secret, and this is the one place where being wrong means it leaves the
    // machine.
    if block.redacted {
        workspace.show_error(
            anyhow::anyhow!(
                "That command's output looked like it contained a credential, so it \
                 was not sent to a model."
            ),
            cx,
        );
        return;
    }

    let Some(ConfiguredModel { model, .. }) =
        LanguageModelRegistry::read_global(cx).inline_assistant_model()
    else {
        workspace.show_error(anyhow::anyhow!("No model configured."), cx);
        return;
    };

    let project = workspace.project().clone();
    let workspace_handle = cx.weak_entity();

    // `spawn_in` rather than `spawn`: opening the changed file at the end needs
    // a window, and an async context without one can only raise a toast saying
    // that something changed somewhere.
    cx.spawn_in(window, async move |_workspace, cx| {
        let context = gather_context(&project, &block, cx).await;
        let request = build_request(&block, &context);

        let response = model.stream_completion_text(request, cx).await;
        let mut reply = String::new();
        match response {
            Ok(response) => {
                let mut chunks = response.stream;
                while let Some(chunk) = chunks.next().await {
                    match chunk {
                        Ok(chunk) => reply.push_str(&chunk),
                        Err(error) => return Err(anyhow::anyhow!(error)),
                    }
                }
            }
            Err(error) => return Err::<(), _>(anyhow::anyhow!(error)),
        }

        let files = split_unified_diff(&reply);
        if files.is_empty() {
            workspace_handle
                .update(cx, |workspace, cx| {
                    workspace.show_error(
                        anyhow::anyhow!("The model did not produce a patch for that failure."),
                        cx,
                    );
                })
                .log_err();
            return Ok(());
        }

        let mut applied = 0usize;
        let mut first_changed: Option<ProjectPath> = None;
        for file in files {
            let Some(project_path) = resolve_path(&project, &file.path, cx).await else {
                continue;
            };
            let Some(buffer) = open_buffer(&project, &project_path, cx).await else {
                continue;
            };
            if apply_to_buffer(&buffer, &file.diff, cx) {
                applied += 1;
                first_changed.get_or_insert(project_path);
            }
        }

        workspace_handle
            .update(cx, |workspace, cx| {
                if applied == 0 {
                    workspace.show_error(
                        anyhow::anyhow!(
                            "The patch did not apply -- the files have changed since \
                             the command ran."
                        ),
                        cx,
                    );
                    return;
                }
                let files = if applied == 1 { "file" } else { "files" };
                workspace.show_toast(
                    workspace::Toast::new(
                        workspace::notifications::NotificationId::unique::<FixTerminalFailure>(),
                        format!(
                            "Proposed a fix in {applied} {files}. Review the changes and \
                             save to keep them, or undo to discard."
                        ),
                    ),
                    cx,
                );
            })
            .log_err();

        // Land on the change. The edits are unsaved, so the file opens with the
        // usual gutter marks and per-hunk revert -- the review surface already
        // in this editor, rather than a second one built for this feature.
        if let Some(path) = first_changed {
            let opened = workspace_handle.update_in(cx, |workspace, window, cx| {
                workspace.open_path(path, None, true, window, cx)
            });
            if let Ok(opened) = opened {
                opened.await.log_err();
            }
        }

        Ok(())
    })
    .detach_and_log_err(cx);
}

/// The most recent failure in any terminal.
///
/// Searched across every terminal rather than only the focused one: by the time
/// you notice a build broke you have usually clicked into the editor, and an
/// action that silently did nothing because focus moved would read as broken.
/// Newest wins, so two terminals running builds resolve to the one that failed
/// last.
fn latest_failure(workspace: &Workspace, cx: &App) -> Option<CommandBlock> {
    let mut panes: Vec<Entity<workspace::Pane>> = workspace.panes().iter().map(|pane| (*pane).clone()).collect();
    if let Some(panel) = workspace.panel::<TerminalPanel>(cx) {
        panes.extend(panel.read(cx).panes().into_iter().cloned());
    }

    let mut newest: Option<CommandBlock> = None;
    for pane in panes {
        for item in pane.read(cx).items() {
            let Some(view) = item.downcast::<terminal_view::TerminalView>() else {
                continue;
            };
            let Some(block) = view
                .read(cx)
                .terminal()
                .read(cx)
                .blocks()
                .last_failure()
                .cloned()
            else {
                continue;
            };
            if newest
                .as_ref()
                .is_none_or(|current| block.started_at >= current.started_at)
            {
                newest = Some(block);
            }
        }
    }
    newest
}

/// A file's contents, to be shown to the model alongside the failure.
struct FileContext {
    path: String,
    text: String,
}

/// Reads the files the output named.
///
/// Only files the project actually contains. A compiler prints paths from its
/// own dependencies too, and shipping the source of a crates.io package to a
/// model is both useless and expensive.
async fn gather_context(
    project: &Entity<Project>,
    block: &CommandBlock,
    cx: &mut AsyncApp,
) -> Vec<FileContext> {
    let mut files = Vec::new();
    for path in paths_in_output(&block.output) {
        if files.len() >= MAX_CONTEXT_FILES {
            break;
        }
        let Some(project_path) = resolve_path(project, &path, cx).await else {
            continue;
        };
        let Some(buffer) = open_buffer(project, &project_path, cx).await else {
            continue;
        };
        let text = buffer.read_with(cx, |buffer, _| buffer.text());
        if text.len() > MAX_FILE_BYTES {
            continue;
        }
        files.push(FileContext { path, text });
    }
    files
}

/// Paths mentioned in command output, in the order they appear.
///
/// Compilers and test runners all report locations the same way -- a path
/// followed by a line, sometimes a column -- so one pattern covers rustc, tsc,
/// eslint, pytest, go, and clang. Deduplicated because the same file is named
/// once per error and the first mention is the one that matters.
fn paths_in_output(output: &str) -> Vec<String> {
    static LOCATION: LazyLock<Option<Regex>> = LazyLock::new(|| {
        // A path is anything up to a `:line`, restricted to characters that
        // actually appear in source paths so that prose does not match.
        Regex::new(r"(?m)(?:^|[\s(\[\x22'>])(--> )?([A-Za-z]:[\\/])?([\w.\-/\\]+\.[A-Za-z]\w{0,9}):(\d+)")
            .ok()
    });
    let Some(pattern) = LOCATION.as_ref() else {
        return Vec::new();
    };

    let mut seen = BTreeSet::new();
    let mut ordered = Vec::new();
    for capture in pattern.captures_iter(output) {
        let Some(path) = capture.get(3) else {
            continue;
        };
        let drive = capture.get(2).map(|drive| drive.as_str()).unwrap_or("");
        let path = format!("{drive}{}", path.as_str()).replace('\\', "/");
        if seen.insert(path.clone()) {
            ordered.push(path);
        }
    }
    ordered
}

async fn resolve_path(
    project: &Entity<Project>,
    path: &str,
    cx: &mut AsyncApp,
) -> Option<ProjectPath> {
    let path = PathBuf::from(path);
    project.update(cx, |project, cx| project.find_project_path(&path, cx))
}

async fn open_buffer(
    project: &Entity<Project>,
    path: &ProjectPath,
    cx: &mut AsyncApp,
) -> Option<Entity<Buffer>> {
    let task = project.update(cx, |project, cx| project.open_buffer(path.clone(), cx));
    task.await.log_err()
}

/// The system prompt. Deliberately narrow.
const SYSTEM_PROMPT: &str = concat!(
    "You are fixing a command that failed inside a code editor.\n\n",
    "You are given the command, its exit code, the tail of its output, and the \
     current contents of the files that output named.\n\n",
    "Reply with a unified diff and nothing else.\n\n",
    "Rules:\n",
    "1. Output only unified diff hunks. No prose, no explanation, no markdown \
     fences, no summary before or after.\n",
    "2. Use the exact path shown in the file headers you were given, in `--- \
     a/<path>` and `+++ b/<path>` lines.\n",
    "3. Context lines must match the file contents you were given, character for \
     character, including indentation.\n",
    "4. Change as little as possible. Fix the reported error; do not reformat, \
     rename, or improve anything else.\n",
    "5. Only edit files you were given the contents of. If the fix needs a file \
     you cannot see, reply with nothing.\n",
    "6. If you cannot tell what the fix is, reply with nothing. An empty reply \
     is a correct answer; a guessed patch costs the user a review and a revert.",
);

fn build_request(block: &CommandBlock, files: &[FileContext]) -> LanguageModelRequest {
    let mut prompt = String::new();
    prompt.push_str(&format!("Command: {}\n", block.command));
    if let Some(code) = block.exit_code {
        prompt.push_str(&format!("Exit code: {code}\n"));
    }
    if let Some(cwd) = &block.cwd {
        prompt.push_str(&format!("Working directory: {}\n", cwd.display()));
    }
    prompt.push_str("\nOutput (tail):\n");
    prompt.push_str(&block.output_tail(OUTPUT_TAIL_LINES));
    prompt.push('\n');

    if files.is_empty() {
        prompt.push_str(
            "\nNo files from this project were named in the output, so no contents \
             are available. If you cannot fix it without them, reply with nothing.\n",
        );
    } else {
        for file in files {
            prompt.push_str(&format!("\n--- file: {} ---\n", file.path));
            prompt.push_str(&file.text);
            if !file.text.ends_with('\n') {
                prompt.push('\n');
            }
        }
    }

    LanguageModelRequest {
        thread_id: None,
        prompt_id: None,
        intent: None,
        messages: vec![
            LanguageModelRequestMessage {
                role: Role::System,
                content: vec![SYSTEM_PROMPT.into()],
                cache: false,
                reasoning_details: None,
            },
            LanguageModelRequestMessage {
                role: Role::User,
                content: vec![prompt.into()],
                cache: false,
                reasoning_details: None,
            },
        ],
        tools: Vec::new(),
        tool_choice: None,
        stop: Vec::new(),
        temperature: Some(0.0),
        thinking_allowed: false,
        thinking_effort: None,
        speed: None,
        compact_at_tokens: None,
    }
}

/// One file's worth of a unified diff.
#[derive(Debug, PartialEq, Eq)]
struct DiffForFile {
    path: String,
    diff: String,
}

/// Splits a multi-file unified diff into one diff per file.
///
/// Written here rather than reusing a diff library because the input is model
/// output, not git output: it may carry a stray fence, a preamble the model was
/// told not to write, or a trailing sentence. Everything outside a recognised
/// file section is dropped rather than parsed.
fn split_unified_diff(reply: &str) -> Vec<DiffForFile> {
    let mut files = Vec::new();
    let mut current: Option<(String, Vec<&str>)> = None;

    let mut lines = reply.lines().peekable();
    while let Some(line) = lines.next() {
        if let Some(target) = line.strip_prefix("--- ") {
            // A file section starts at `---` only when `+++` follows it;
            // otherwise this is a `---` inside prose or a horizontal rule.
            let Some(next) = lines.peek() else { break };
            let Some(new_target) = next.strip_prefix("+++ ") else {
                continue;
            };

            if let Some((path, body)) = current.take() {
                files.push(DiffForFile {
                    path,
                    diff: body.join("\n"),
                });
            }

            // Prefer the `+++` side: for a rename or a new file the `---` side
            // is `/dev/null`.
            let path = strip_diff_prefix(new_target)
                .or_else(|| strip_diff_prefix(target))
                .unwrap_or_default();
            let header = vec![line, lines.next().unwrap_or_default()];
            current = Some((path, header));
            continue;
        }

        let Some((_, body)) = current.as_mut() else {
            continue;
        };

        // Hunk headers and hunk lines. Anything else ends the section, which is
        // how a trailing sentence after the patch gets dropped.
        if line.starts_with("@@")
            || line.starts_with('+')
            || line.starts_with('-')
            || line.starts_with(' ')
            || line.is_empty()
            || line.starts_with('\\')
        {
            body.push(line);
        } else if let Some((path, body)) = current.take() {
            files.push(DiffForFile {
                path,
                diff: body.join("\n"),
            });
        }
    }

    if let Some((path, body)) = current.take() {
        files.push(DiffForFile {
            path,
            diff: body.join("\n"),
        });
    }

    files
        .into_iter()
        .filter(|file| !file.path.is_empty() && file.diff.contains("@@"))
        .collect()
}

/// `a/src/main.rs` and `b/src/main.rs` both name `src/main.rs`.
///
/// Also drops the tab-separated timestamp that `diff -u` appends.
fn strip_diff_prefix(target: &str) -> Option<String> {
    let target = target.split('\t').next().unwrap_or(target).trim();
    if target.is_empty() || target == "/dev/null" {
        return None;
    }
    let target = target
        .strip_prefix("a/")
        .or_else(|| target.strip_prefix("b/"))
        .unwrap_or(target);
    Some(target.replace('\\', "/"))
}

/// Applies one file's diff to its buffer, leaving it unsaved.
///
/// Returns whether anything changed. A diff whose context does not match is
/// dropped whole rather than partially applied: half a fix in a file the user
/// then has to reconstruct is worse than no fix.
fn apply_to_buffer(buffer: &Entity<Buffer>, diff: &str, cx: &mut AsyncApp) -> bool {
    let text = buffer.read_with(cx, |buffer, _| buffer.text());
    let Some(edits) = edit_prediction::udiff::edits_for_diff(&text, diff).log_err() else {
        return false;
    };
    if edits.is_empty() {
        return false;
    }

    buffer.update(cx, |buffer, cx| {
        buffer.edit(edits, None, cx);
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiler_locations_are_recognised_across_toolchains() {
        let output = "\
error[E0599]: no method named `ok`
  --> crates/agent/src/tools/edit_session.rs:373:10
src/app.tsx:12:5 - error TS2345
  File \"tests/test_api.py\", line 40
tests/test_api.py:40: AssertionError
";
        let paths = paths_in_output(output);
        assert!(paths.contains(&"crates/agent/src/tools/edit_session.rs".to_string()));
        assert!(paths.contains(&"src/app.tsx".to_string()));
        assert!(paths.contains(&"tests/test_api.py".to_string()));
        // Named once per error, listed once.
        assert_eq!(
            paths.iter().filter(|p| p.ends_with("test_api.py")).count(),
            1
        );
    }

    #[test]
    fn windows_paths_survive_with_their_drive() {
        let paths = paths_in_output("C:\\Users\\broad\\zed\\crates\\ui\\src\\ui.rs:42:1");
        assert_eq!(paths, vec!["C:/Users/broad/zed/crates/ui/src/ui.rs"]);
    }

    #[test]
    fn prose_is_not_mistaken_for_a_path() {
        // No extension-then-colon-then-digit anywhere here.
        assert!(paths_in_output("error: could not compile `agent` due to 1 error").is_empty());
        assert!(paths_in_output("warning: 3 warnings emitted").is_empty());
    }

    #[test]
    fn a_single_file_patch_is_split_out() {
        let reply = "\
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,3 @@
 fn main() {
-    println!(\"hello\")
+    println!(\"hello\");
 }
";
        let files = split_unified_diff(reply);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "src/main.rs");
        assert!(files[0].diff.contains("@@ -1,3 +1,3 @@"));
    }

    #[test]
    fn several_files_are_kept_apart() {
        let reply = "\
--- a/one.rs
+++ b/one.rs
@@ -1 +1 @@
-a
+b
--- a/two.rs
+++ b/two.rs
@@ -1 +1 @@
-c
+d
";
        let files = split_unified_diff(reply);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "one.rs");
        assert_eq!(files[1].path, "two.rs");
        assert!(files[0].diff.contains("+b"));
        assert!(!files[0].diff.contains("+d"), "sections bled into each other");
    }

    #[test]
    fn prose_around_the_patch_is_discarded() {
        // The model was told not to do this. It will anyway.
        let reply = "\
Here is the fix:

--- a/src/main.rs
+++ b/src/main.rs
@@ -1 +1 @@
-old
+new

Let me know if you would like me to explain further.
";
        let files = split_unified_diff(reply);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "src/main.rs");
        assert!(
            !files[0].diff.contains("explain further"),
            "trailing prose kept: {}",
            files[0].diff
        );
    }

    #[test]
    fn a_reply_with_no_patch_yields_nothing() {
        assert!(split_unified_diff("").is_empty());
        assert!(split_unified_diff("I cannot tell what the fix is.").is_empty());
        // Headers with no hunk are not a patch.
        assert!(split_unified_diff("--- a/x.rs\n+++ b/x.rs\n").is_empty());
        // A horizontal rule is not a file header.
        assert!(split_unified_diff("---\nsome notes\n---\n").is_empty());
    }

    #[test]
    fn a_new_file_takes_its_name_from_the_plus_side() {
        let reply = "\
--- /dev/null
+++ b/src/new.rs
@@ -0,0 +1 @@
+fn added() {}
";
        let files = split_unified_diff(reply);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "src/new.rs");
    }
}
