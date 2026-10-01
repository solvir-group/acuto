use std::{ops::RangeInclusive, path::PathBuf};

use acp_thread::EDITOR_CONTEXT_OPEN_TAG;
use collections::HashSet;
use editor::{DisplayPoint, Editor, display_map::DisplayRow};
use gpui::{App, AppContext as _, Entity, Task};
use language::{Buffer, BufferSnapshot, DiagnosticEntryRef, DiagnosticSeverity, Point};
use project::{Project, ProjectPath};
use text::Bias;
use workspace::Workspace;

// Bounds on the snapshot. The block rides along with every message, so its
// cost is paid once per turn for the whole conversation: a few thousand
// tokens at most, however large the selection or noisy the language server.
const MAX_SELECTION_CHARS: usize = 6_000;
const MAX_DIAGNOSTICS: usize = 25;
const MAX_DIAGNOSTIC_MESSAGE_CHARS: usize = 300;
const MAX_OPEN_FILES: usize = 25;

const EDITOR_CONTEXT_CLOSE_TAG: &str = "</editor_context>";

// Bounds for the on-demand diagnostics tool. Larger than the per-message
// snapshot, because the agent asked for exactly this, but still bounded: a
// misconfigured linter can report tens of thousands of problems.
const MAX_REPORTED_FILES: usize = 40;
const MAX_REPORTED_DIAGNOSTICS: usize = 200;

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct EditorContext {
    pub active_file: Option<ActiveFile>,
    pub diagnostics: Vec<DiagnosticLine>,
    pub total_diagnostics: usize,
    pub open_files: Vec<OpenFile>,
    pub total_open_files: usize,
}

/// Rows are 1-based, as people and compilers number them.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ActiveFile {
    pub path: String,
    pub visible_rows: Option<RangeInclusive<u32>>,
    pub cursor_row: u32,
    pub selection: Option<SelectionContext>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SelectionContext {
    pub rows: RangeInclusive<u32>,
    pub text: String,
    pub truncated: bool,
    pub language: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Severity {
    Error,
    Warning,
    Information,
}

impl Severity {
    fn from_lsp(severity: DiagnosticSeverity) -> Option<Self> {
        match severity {
            DiagnosticSeverity::ERROR => Some(Self::Error),
            DiagnosticSeverity::WARNING => Some(Self::Warning),
            DiagnosticSeverity::INFORMATION => Some(Self::Information),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Information => "info",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DiagnosticLine {
    pub path: String,
    pub row: u32,
    pub column: u32,
    pub severity: Severity,
    pub message: String,
    pub source: Option<String>,
}

impl DiagnosticLine {
    fn new(path: &str, entry: &DiagnosticEntryRef<'_, Point>, severity: Severity) -> Self {
        let first_line = entry.diagnostic.message.lines().next().unwrap_or_default();
        let (message, truncated) = truncate_to_chars(first_line, MAX_DIAGNOSTIC_MESSAGE_CHARS);
        Self {
            path: path.to_string(),
            row: entry.range.start.row + 1,
            column: entry.range.start.column + 1,
            severity,
            message: if truncated {
                format!("{message}…")
            } else {
                message
            },
            source: entry.diagnostic.source.clone(),
        }
    }

    fn render(&self) -> String {
        let source = self
            .source
            .as_ref()
            .map(|source| format!(" [{source}]"))
            .unwrap_or_default();
        format!(
            "{}:{}:{} {}{source}: {}",
            self.path,
            self.row,
            self.column,
            self.severity.label(),
            self.message
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OpenFile {
    pub path: String,
    pub is_active: bool,
    pub is_dirty: bool,
}

impl EditorContext {
    pub fn is_empty(&self) -> bool {
        self.active_file.is_none() && self.diagnostics.is_empty() && self.open_files.is_empty()
    }

    pub fn render(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }

        let mut output = String::new();
        output.push_str(EDITOR_CONTEXT_OPEN_TAG);
        output.push_str(
            "\nThe user's editor state when they sent this message, attached automatically \
             by the editor. It may or may not be relevant to the request.\n",
        );

        if let Some(active_file) = &self.active_file {
            output.push_str(&format!("\nActive file: {}", active_file.path));
            let mut details = Vec::new();
            if let Some(visible_rows) = &active_file.visible_rows {
                details.push(format!(
                    "lines {}-{} visible",
                    visible_rows.start(),
                    visible_rows.end()
                ));
            }
            details.push(format!("cursor on line {}", active_file.cursor_row));
            output.push_str(&format!(" ({})\n", details.join(", ")));

            if let Some(selection) = &active_file.selection {
                let fence = code_fence_for(&selection.text);
                output.push_str(&format!(
                    "\nSelected lines {}-{} of {}:\n{fence}{}\n{}",
                    selection.rows.start(),
                    selection.rows.end(),
                    active_file.path,
                    selection.language.as_deref().unwrap_or_default(),
                    selection.text,
                ));
                if !selection.text.ends_with('\n') {
                    output.push('\n');
                }
                output.push_str(&fence);
                output.push('\n');
                if selection.truncated {
                    output.push_str(&format!(
                        "(Selection truncated to its first {MAX_SELECTION_CHARS} characters; \
                         read the file for the rest.)\n"
                    ));
                }
            }
        }

        if !self.diagnostics.is_empty() {
            output.push_str(&format!(
                "\nDiagnostics in open files ({} of {}, errors first):\n",
                self.diagnostics.len(),
                self.total_diagnostics
            ));
            for diagnostic in &self.diagnostics {
                output.push_str(&format!("- {}\n", diagnostic.render()));
            }
        }

        if !self.open_files.is_empty() {
            output.push_str(&format!(
                "\nOpen files ({} of {}):\n",
                self.open_files.len(),
                self.total_open_files
            ));
            for file in &self.open_files {
                let mut markers = Vec::new();
                if file.is_active {
                    markers.push("active");
                }
                if file.is_dirty {
                    markers.push("unsaved changes");
                }
                if markers.is_empty() {
                    output.push_str(&format!("- {}\n", file.path));
                } else {
                    output.push_str(&format!("- {} ({})\n", file.path, markers.join(", ")));
                }
            }
        }

        output.push_str(EDITOR_CONTEXT_CLOSE_TAG);
        Some(output)
    }
}

/// A fence longer than any backtick run in `text`, so a selection that itself
/// contains a fenced block cannot close ours early.
fn code_fence_for(text: &str) -> String {
    let mut longest_run = 0;
    let mut current_run = 0;
    for character in text.chars() {
        if character == '`' {
            current_run += 1;
            longest_run = longest_run.max(current_run);
        } else {
            current_run = 0;
        }
    }
    "`".repeat((longest_run + 1).max(3))
}

fn truncate_to_chars(text: &str, max_chars: usize) -> (String, bool) {
    match text.char_indices().nth(max_chars) {
        Some((byte_index, _)) => (text[..byte_index].to_string(), true),
        None => (text.to_string(), false),
    }
}

fn buffer_path(buffer: &Buffer, cx: &App) -> Option<String> {
    let file = buffer.file()?;
    // Absolute where possible: it is what the agent's own tools take, and it
    // stays unambiguous when the project has more than one root.
    Some(match file.as_local() {
        Some(local_file) => local_file.abs_path(cx).display().to_string(),
        None => file.full_path(cx).display().to_string(),
    })
}

fn singleton_buffer(editor: &Entity<Editor>, cx: &App) -> Option<Entity<Buffer>> {
    editor.read(cx).buffer().read(cx).as_singleton()
}

/// Snapshots the editor state of `workspace` for an agent.
pub(crate) fn gather(workspace: &Entity<Workspace>, cx: &mut App) -> EditorContext {
    let (active_editor, editors) = {
        let workspace = workspace.read(cx);
        // When the thread itself is a tab in the active pane, the file the
        // user was looking at is the active item of another pane.
        let active_editor = workspace
            .active_item_as::<Editor>(cx)
            .filter(|editor| singleton_buffer(editor, cx).is_some())
            .or_else(|| {
                workspace.panes().iter().find_map(|pane| {
                    pane.read(cx)
                        .active_item()?
                        .act_as::<Editor>(cx)
                        .filter(|editor| singleton_buffer(editor, cx).is_some())
                })
            });
        let editors = workspace.items_of_type::<Editor>(cx).collect::<Vec<_>>();
        (active_editor, editors)
    };

    let active_buffer = active_editor
        .as_ref()
        .and_then(|editor| singleton_buffer(editor, cx));

    let mut buffers: Vec<Entity<Buffer>> = Vec::new();
    for buffer in active_buffer
        .iter()
        .cloned()
        .chain(editors.iter().filter_map(|editor| singleton_buffer(editor, cx)))
    {
        if !buffers.iter().any(|existing| existing == &buffer) {
            buffers.push(buffer);
        }
    }

    let active_file = active_editor.and_then(|editor| active_file_context(&editor, cx));

    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut total_diagnostics = 0;
    let mut open_files = Vec::new();
    let mut total_open_files = 0;
    for buffer_entity in &buffers {
        let buffer = buffer_entity.read(cx);
        let Some(path) = buffer_path(buffer, cx) else {
            continue;
        };

        total_open_files += 1;
        if open_files.len() < MAX_OPEN_FILES {
            open_files.push(OpenFile {
                path: path.clone(),
                is_active: active_buffer.as_ref() == Some(buffer_entity),
                is_dirty: buffer.is_dirty(),
            });
        }

        let snapshot = buffer.snapshot();
        for entry in snapshot.diagnostics_in_range::<_, Point>(0..snapshot.len(), false) {
            if !entry.diagnostic.is_primary {
                continue;
            }
            let (severity, bucket) = match Severity::from_lsp(entry.diagnostic.severity) {
                Some(Severity::Error) => (Severity::Error, &mut errors),
                Some(Severity::Warning) => (Severity::Warning, &mut warnings),
                _ => continue,
            };
            total_diagnostics += 1;
            if bucket.len() < MAX_DIAGNOSTICS {
                bucket.push(DiagnosticLine::new(&path, &entry, severity));
            }
        }
    }

    let mut diagnostics = errors;
    diagnostics.extend(warnings);
    diagnostics.truncate(MAX_DIAGNOSTICS);

    EditorContext {
        active_file,
        diagnostics,
        total_diagnostics,
        open_files,
        total_open_files,
    }
}

/// The `get_diagnostics` tool the editor's MCP server offers external agents:
/// LSP diagnostics for one file, or for every file the language servers have
/// reported on when `path` is `None`.
pub(crate) fn diagnostics_report(path: Option<PathBuf>, cx: &mut App) -> Task<Result<String, String>> {
    let mut projects: Vec<Entity<Project>> = Vec::new();
    for workspace in crate::thread_worktree_archive::all_open_workspaces(cx) {
        let project = workspace.read(cx).project().clone();
        if !projects.contains(&project) {
            projects.push(project);
        }
    }

    let mut project_paths: Vec<(Entity<Project>, ProjectPath)> = Vec::new();
    let mut total_files = 0;
    match &path {
        Some(path) => {
            // Only files inside a project that is already open: the tool must
            // not make the editor start indexing arbitrary directories.
            let Some((project, project_path)) = projects.iter().find_map(|project| {
                let (worktree, relative_path) = project.read(cx).find_worktree(path, cx)?;
                let worktree_id = worktree.read(cx).id();
                Some((
                    project.clone(),
                    ProjectPath {
                        worktree_id,
                        path: relative_path,
                    },
                ))
            }) else {
                return Task::ready(Err(format!(
                    "{} is not inside a project open in the editor.",
                    path.display()
                )));
            };
            total_files = 1;
            project_paths.push((project, project_path));
        }
        None => {
            let mut seen = HashSet::default();
            for project in &projects {
                for (project_path, _, summary) in project.read(cx).diagnostic_summaries(false, cx) {
                    if summary.error_count + summary.warning_count == 0
                        || !seen.insert(project_path.clone())
                    {
                        continue;
                    }
                    total_files += 1;
                    if project_paths.len() < MAX_REPORTED_FILES {
                        project_paths.push((project.clone(), project_path));
                    }
                }
            }
        }
    }

    let buffer_tasks = project_paths
        .into_iter()
        .map(|(project, project_path)| {
            project.update(cx, |project, cx| project.open_buffer(project_path, cx))
        })
        .collect::<Vec<_>>();

    cx.spawn(async move |cx| {
        let mut lines = Vec::new();
        let mut total_diagnostics = 0;
        for buffer_task in buffer_tasks {
            let buffer = buffer_task.await.map_err(|error| format!("{error:#}"))?;
            let (file_path, snapshot) = buffer.read_with(cx, |buffer, cx| {
                (buffer_path(buffer, cx), buffer.snapshot())
            });
            let Some(file_path) = file_path else {
                continue;
            };
            let mut file_lines = diagnostic_lines(&file_path, &snapshot);
            file_lines.sort_by_key(|line| line.severity);
            total_diagnostics += file_lines.len();
            lines.extend(file_lines);
        }
        lines.sort_by_key(|line| line.severity);
        let shown = lines.len().min(MAX_REPORTED_DIAGNOSTICS);

        Ok(render_report(
            path.as_ref(),
            &lines[..shown],
            total_diagnostics,
            total_files,
        ))
    })
}

fn diagnostic_lines(path: &str, snapshot: &BufferSnapshot) -> Vec<DiagnosticLine> {
    snapshot
        .diagnostics_in_range::<_, Point>(0..snapshot.len(), false)
        .filter(|entry| entry.diagnostic.is_primary)
        .filter_map(|entry| {
            let severity = Severity::from_lsp(entry.diagnostic.severity)?;
            Some(DiagnosticLine::new(path, &entry, severity))
        })
        .collect()
}

fn render_report(
    path: Option<&PathBuf>,
    lines: &[DiagnosticLine],
    total_diagnostics: usize,
    total_files: usize,
) -> String {
    let scope = match path {
        Some(path) => path.display().to_string(),
        None => format!("{total_files} file(s) with errors or warnings"),
    };
    if lines.is_empty() {
        return format!("No diagnostics for {scope}.");
    }
    let mut output = format!(
        "{} of {total_diagnostics} diagnostic(s) for {scope}, errors first:\n",
        lines.len()
    );
    for line in lines {
        output.push_str(&line.render());
        output.push('\n');
    }
    if path.is_none() && total_files > MAX_REPORTED_FILES {
        output.push_str(&format!(
            "Only the first {MAX_REPORTED_FILES} files were read; pass `path` for any other.\n"
        ));
    }
    output
}

fn active_file_context(editor: &Entity<Editor>, cx: &mut App) -> Option<ActiveFile> {
    let buffer = singleton_buffer(editor, cx)?;
    let path = buffer_path(buffer.read(cx), cx)?;
    let language = buffer
        .read(cx)
        .language()
        .map(|language| language.code_fence_block_name().to_string());

    editor.update(cx, |editor, cx| {
        let display_snapshot = editor.display_snapshot(cx);
        let selection = editor.selections.newest::<Point>(&display_snapshot);

        // Only known once the editor has been laid out; a file opened in a
        // background tab has no visible range yet.
        let visible_rows = editor.visible_line_count().map(|line_count| {
            let max_row = display_snapshot.max_point().row().0;
            let top = (editor.scroll_position(cx).y.max(0.) as u32).min(max_row);
            let bottom = (top + line_count.ceil() as u32).min(max_row);
            let buffer_row = |row: u32| {
                display_snapshot
                    .clip_point(DisplayPoint::new(DisplayRow(row), 0), Bias::Left)
                    .to_point(&display_snapshot)
                    .row
                    + 1
            };
            buffer_row(top)..=buffer_row(bottom)
        });

        let selection_context = (selection.start != selection.end).then(|| {
            let buffer_snapshot = editor.buffer().read(cx).snapshot(cx);
            // Collected chunk by chunk and cut off at the bound, so selecting
            // a whole generated file does not copy all of it first.
            let mut text = String::new();
            let mut truncated = false;
            for chunk in buffer_snapshot.text_for_range(selection.start..selection.end) {
                text.push_str(chunk);
                if text.chars().count() > MAX_SELECTION_CHARS {
                    truncated = true;
                    break;
                }
            }
            if truncated {
                text = truncate_to_chars(&text, MAX_SELECTION_CHARS).0;
            }

            // A selection that ends at the start of a line does not include
            // that line in any sense a person would mean.
            let end_row = if selection.end.column == 0 && selection.end.row > selection.start.row
            {
                selection.end.row
            } else {
                selection.end.row + 1
            };
            SelectionContext {
                rows: selection.start.row + 1..=end_row,
                text,
                truncated,
                language: language.clone(),
            }
        });

        Some(ActiveFile {
            path,
            visible_rows,
            cursor_row: selection.head().row + 1,
            selection: selection_context,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_context() -> EditorContext {
        EditorContext {
            active_file: Some(ActiveFile {
                path: "/project/src/main.rs".into(),
                visible_rows: Some(10..=52),
                cursor_row: 14,
                selection: Some(SelectionContext {
                    rows: 12..=14,
                    text: "fn main() {\n    run();\n}".into(),
                    truncated: false,
                    language: Some("rust".into()),
                }),
            }),
            diagnostics: vec![DiagnosticLine {
                path: "/project/src/main.rs".into(),
                row: 13,
                column: 5,
                severity: Severity::Error,
                message: "cannot find function `run` in this scope".into(),
                source: Some("rustc".into()),
            }],
            total_diagnostics: 1,
            open_files: vec![
                OpenFile {
                    path: "/project/src/main.rs".into(),
                    is_active: true,
                    is_dirty: false,
                },
                OpenFile {
                    path: "/project/src/lib.rs".into(),
                    is_active: false,
                    is_dirty: true,
                },
            ],
            total_open_files: 2,
        }
    }

    #[test]
    fn renders_every_part_of_the_snapshot() {
        let rendered = sample_context().render().expect("non-empty context renders");

        assert_eq!(
            rendered,
            "<editor_context>\n\
             The user's editor state when they sent this message, attached automatically by \
             the editor. It may or may not be relevant to the request.\n\
             \n\
             Active file: /project/src/main.rs (lines 10-52 visible, cursor on line 14)\n\
             \n\
             Selected lines 12-14 of /project/src/main.rs:\n\
             ```rust\n\
             fn main() {\n    run();\n}\n\
             ```\n\
             \n\
             Diagnostics in open files (1 of 1, errors first):\n\
             - /project/src/main.rs:13:5 error [rustc]: cannot find function `run` in this scope\n\
             \n\
             Open files (2 of 2):\n\
             - /project/src/main.rs (active)\n\
             - /project/src/lib.rs (unsaved changes)\n\
             </editor_context>"
        );
        assert!(rendered.starts_with(EDITOR_CONTEXT_OPEN_TAG));
    }

    #[test]
    fn diagnostics_report_lists_lines_and_reports_truncation() {
        let line = DiagnosticLine {
            path: "/project/src/lib.rs".into(),
            row: 3,
            column: 1,
            severity: Severity::Warning,
            message: "unused variable: `x`".into(),
            source: None,
        };
        let report = render_report(None, &[line], 120, MAX_REPORTED_FILES + 1);
        assert_eq!(
            report,
            format!(
                "1 of 120 diagnostic(s) for {} file(s) with errors or warnings, errors first:\n\
                 /project/src/lib.rs:3:1 warning: unused variable: `x`\n\
                 Only the first {MAX_REPORTED_FILES} files were read; pass `path` for any other.\n",
                MAX_REPORTED_FILES + 1
            )
        );

        let path = PathBuf::from("/project/src/main.rs");
        assert_eq!(
            render_report(Some(&path), &[], 0, 1),
            "No diagnostics for /project/src/main.rs."
        );
    }

    #[test]
    fn empty_context_is_not_sent() {
        assert_eq!(EditorContext::default().render(), None);
    }

    #[test]
    fn selection_containing_a_fence_cannot_close_the_block_early() {
        assert_eq!(code_fence_for("plain"), "```");
        assert_eq!(code_fence_for("```rust\nx\n```"), "````");
        assert_eq!(code_fence_for("a ````` b"), "``````");
    }

    #[test]
    fn truncation_respects_character_boundaries() {
        assert_eq!(truncate_to_chars("héllo", 2), ("hé".to_string(), true));
        assert_eq!(truncate_to_chars("héllo", 5), ("héllo".to_string(), false));
        assert_eq!(truncate_to_chars("", 3), (String::new(), false));
    }

    #[test]
    fn rendered_size_stays_bounded_for_worst_case_input() {
        let long_path = format!("/project/{}.rs", "nested/".repeat(20));
        let context = EditorContext {
            active_file: Some(ActiveFile {
                path: long_path.clone(),
                visible_rows: Some(1..=80),
                cursor_row: 1,
                selection: Some(SelectionContext {
                    rows: 1..=10_000,
                    text: "x".repeat(MAX_SELECTION_CHARS),
                    truncated: true,
                    language: None,
                }),
            }),
            diagnostics: (0..MAX_DIAGNOSTICS)
                .map(|index| DiagnosticLine {
                    path: long_path.clone(),
                    row: index as u32,
                    column: 1,
                    severity: Severity::Warning,
                    message: "m".repeat(MAX_DIAGNOSTIC_MESSAGE_CHARS),
                    source: Some("clippy".into()),
                })
                .collect(),
            total_diagnostics: 10_000,
            open_files: (0..MAX_OPEN_FILES)
                .map(|_| OpenFile {
                    path: long_path.clone(),
                    is_active: false,
                    is_dirty: true,
                })
                .collect(),
            total_open_files: 500,
        };

        let rendered = context.render().expect("non-empty context renders");
        // Roughly 6k tokens at ~4 characters per token: well inside any
        // context window, and paid only once per user turn.
        assert!(
            rendered.chars().count() < 24_000,
            "editor context grew to {} characters",
            rendered.chars().count()
        );
        assert!(rendered.contains("Selection truncated"));
        assert!(rendered.contains(&format!("({MAX_DIAGNOSTICS} of 10000, errors first)")));
        assert!(rendered.contains(&format!("Open files ({MAX_OPEN_FILES} of 500)")));
    }
}
