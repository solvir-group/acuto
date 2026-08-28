//! The panel that shows team notes and lets you write them.

use std::{path::PathBuf, sync::Arc};

use editor::{Editor, MultiBufferOffset};
use language::ToOffset as _;
use fs::Fs;
use gpui::{
    Action, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Task,
    WeakEntity, Window, actions,
};
use project::Project;
use ui::{Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::{Message, NoteThread, author_name, notes_file, resolve_anchor};

actions!(
    team_notes,
    [
        /// Opens or closes the team notes panel.
        ToggleFocus,
        /// Starts a note on the line the cursor is on.
        AddNote,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<TeamNotesPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &AddNote, window, cx| {
            TeamNotesPanel::start_note_at_cursor(workspace, window, cx);
        });
    })
    .detach();
}

pub struct TeamNotesPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    threads: Vec<NoteThread>,
    /// The thread whose reply box is open, and the box itself.
    composing: Option<(String, Entity<Editor>)>,
    /// A note being written for a place that has no thread yet.
    drafting: Option<(DraftAnchor, Entity<Editor>)>,
    show_resolved: bool,
    author: Arc<str>,
    _reload: Option<Task<()>>,
}

/// Where a not-yet-created note will attach.
struct DraftAnchor {
    file: String,
    line: u32,
    text: String,
}

impl TeamNotesPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: gpui::AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, _window, cx| {
            let handle = workspace as &Workspace;
            cx.new(|cx| TeamNotesPanel::new(handle, cx))
        })
    }

    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let fs = project.read(cx).fs().clone();

        let mut this = Self {
            workspace: workspace.weak_handle(),
            project,
            fs,
            focus_handle: cx.focus_handle(),
            threads: Vec::new(),
            composing: None,
            drafting: None,
            show_resolved: false,
            author: "unknown".into(),
            _reload: None,
        };

        cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let author = author_name(&executor).await;
            this.update(cx, |this, cx| {
                this.author = author;
                cx.notify();
            })
            .ok();
        })
        .detach();

        this.reload(cx);
        this
    }

    /// The worktree the notes file belongs to.
    ///
    /// The first visible worktree. A window with several of them keeps its
    /// notes with the first, because a note has to live in exactly one
    /// repository and splitting them across worktrees would make "where is
    /// this note" unanswerable.
    fn worktree_root(&self, cx: &App) -> Option<PathBuf> {
        let worktree = self.project.read(cx).visible_worktrees(cx).next()?;
        Some(worktree.read(cx).abs_path().to_path_buf())
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.worktree_root(cx) else {
            return;
        };
        let fs = self.fs.clone();

        self._reload = Some(cx.spawn(async move |this, cx| {
            let path = notes_file(&root);
            let contents = fs.load(&path).await.unwrap_or_default();
            let threads = crate::parse(&contents);
            this.update(cx, |this, cx| {
                this.threads = threads;
                cx.notify();
            })
            .ok();
        }));
    }

    fn save(&self, threads: Vec<NoteThread>, cx: &mut Context<Self>) -> Task<()> {
        let Some(root) = self.worktree_root(cx) else {
            return Task::ready(());
        };
        let fs = self.fs.clone();

        cx.background_spawn(async move {
            let Some(contents) = crate::render(&threads).log_err() else {
                return;
            };
            let path = notes_file(&root);
            if let Some(parent) = path.parent() {
                fs.create_dir(parent).await.log_err();
            }
            fs.write(&path, contents.as_bytes()).await.log_err();
        })
    }

    fn commit(&mut self, threads: Vec<NoteThread>, cx: &mut Context<Self>) {
        self.threads = threads.clone();
        let write = self.save(threads, cx);
        cx.spawn(async move |_, _| write.await).detach();
        cx.notify();
    }

    /// Opens a draft note for whatever the active editor's cursor is on.
    pub fn start_note_at_cursor(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(editor) = workspace
            .active_item(cx)
            .and_then(|item| item.act_as::<Editor>(cx))
        else {
            return;
        };
        let Some(panel) = workspace.panel::<TeamNotesPanel>(cx) else {
            return;
        };

        let Some((file, line, text)) = editor.update(cx, |editor, cx| {
            let buffer = editor.buffer().read(cx).as_singleton()?;
            let buffer = buffer.read(cx);
            let file = buffer.file()?.path().as_std_path().to_path_buf();

            // Converted through the multibuffer, then read against the buffer's
            // own text. A note is only ever attached to a single-file buffer,
            // so the two coordinate spaces agree.
            let head = editor.selections.newest_anchor().head();
            let multi_snapshot = editor.buffer().read(cx).snapshot(cx);
            let (text_anchor, _) = multi_snapshot.anchor_to_buffer_anchor(head)?;

            let snapshot = buffer.snapshot();
            let offset = text_anchor.to_offset(&snapshot);
            let text = snapshot.text();
            let mut row = 0u32;
            let mut consumed = 0usize;
            for line in text.lines() {
                let next = consumed + line.len() + 1;
                if offset < next {
                    break;
                }
                consumed = next;
                row += 1;
            }
            let line_text = text.lines().nth(row as usize).unwrap_or_default().to_string();

            Some((crate::normalize_path(&file), row, line_text))
        }) else {
            return;
        };

        workspace.open_panel::<TeamNotesPanel>(window, cx);
        panel.update(cx, |panel, cx| {
            let composer = cx.new(|cx| {
                let mut editor = Editor::auto_height(1, 6, window, cx);
                editor.set_placeholder_text("Ask about this line…", window, cx);
                editor
            });
            composer.focus_handle(cx).focus(window, cx);
            panel.composing = None;
            panel.drafting = Some((DraftAnchor { file, line, text }, composer));
            cx.notify();
        });
    }

    fn submit_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((anchor, composer)) = self.drafting.take() else {
            return;
        };
        let body = composer.read(cx).text(cx).trim().to_string();
        if body.is_empty() {
            cx.notify();
            return;
        }

        let mut threads = self.threads.clone();
        threads.push(NoteThread {
            id: crate::new_thread_id(),
            file: anchor.file,
            anchor: crate::Anchor {
                line: anchor.line,
                text: anchor.text,
            },
            resolved: false,
            messages: vec![Message {
                author: self.author.to_string(),
                at: crate::now_timestamp(),
                body,
            }],
        });
        self.commit(threads, cx);
        self.focus_handle.focus(window, cx);
    }

    fn submit_reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((thread_id, composer)) = self.composing.take() else {
            return;
        };
        let body = composer.read(cx).text(cx).trim().to_string();
        if body.is_empty() {
            cx.notify();
            return;
        }

        let mut threads = self.threads.clone();
        if let Some(thread) = threads.iter_mut().find(|thread| thread.id == thread_id) {
            thread.messages.push(Message {
                author: self.author.to_string(),
                at: crate::now_timestamp(),
                body,
            });
        }
        self.commit(threads, cx);
        self.focus_handle.focus(window, cx);
    }

    fn toggle_resolved(&mut self, thread_id: &str, cx: &mut Context<Self>) {
        let mut threads = self.threads.clone();
        if let Some(thread) = threads.iter_mut().find(|thread| thread.id == thread_id) {
            thread.resolved = !thread.resolved;
        }
        self.commit(threads, cx);
    }

    fn start_reply(&mut self, thread_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let composer = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 6, window, cx);
            editor.set_placeholder_text("Reply…", window, cx);
            editor
        });
        composer.focus_handle(cx).focus(window, cx);
        self.drafting = None;
        self.composing = Some((thread_id, composer));
        cx.notify();
    }

    /// Opens the file a thread is attached to, at wherever its anchor resolves
    /// to now.
    ///
    /// Resolved against the buffer after it opens rather than before: the file
    /// may have changed since the note was written, and the whole point of
    /// carrying the anchor text is that the note can find its way back.
    fn jump_to(&mut self, thread_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(thread) = self
            .threads
            .iter()
            .find(|thread| thread.id == thread_id)
            .cloned()
        else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(project_path) = self
            .project
            .read(cx)
            .find_project_path(std::path::Path::new(&thread.file), cx)
        else {
            return;
        };

        cx.spawn_in(window, async move |_, cx| {
            let item = workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.open_path(project_path, None, true, window, cx)
                })?
                .await?;

            let Some(editor) = item.downcast::<Editor>() else {
                return anyhow::Ok(());
            };

            editor.update_in(cx, |editor, window, cx| {
                let text = editor.buffer().read(cx).read(cx).text();
                let lines: Vec<&str> = text.lines().collect();
                let row = resolve_anchor(&thread.anchor, &lines).line();

                // Byte offset of the row's first character, counted from the
                // text itself so no editor-private coordinate type is needed.
                let offset: usize = lines
                    .iter()
                    .take(row as usize)
                    .map(|line| line.len() + 1)
                    .sum();

                let offset = MultiBufferOffset(offset);
                editor.change_selections(Default::default(), window, cx, |selections| {
                    selections.select_ranges([offset..offset]);
                });
            })?;

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }


    fn render_thread(
        &self,
        thread: &NoteThread,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let thread_id = thread.id.clone();
        let replying = self
            .composing
            .as_ref()
            .is_some_and(|(id, _)| *id == thread.id);

        let header = h_flex()
            .w_full()
            .gap_1()
            .child(
                Icon::new(if thread.resolved {
                    IconName::Check
                } else {
                    IconName::Chat
                })
                .size(IconSize::XSmall)
                .color(if thread.resolved {
                    Color::Success
                } else {
                    Color::Muted
                }),
            )
            .child(
                Label::new(format!("{}:{}", thread.file, thread.anchor.line + 1))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .truncate(),
            );

        let messages = v_flex().gap_1().children(thread.messages.iter().map(|message| {
            v_flex()
                .child(
                    Label::new(message.author.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Accent),
                )
                .child(Label::new(message.body.clone()).size(LabelSize::Small))
        }));

        v_flex()
            .id(SharedString::from(thread.id.clone()))
            .w_full()
            .p_2()
            .gap_1p5()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .bg(cx.theme().colors().elevated_surface_background.opacity(0.5))
            .when(thread.resolved, |this| this.opacity(0.6))
            .child(header)
            .child(messages)
            .when(replying, |this| {
                this.child(
                    self.composing
                        .as_ref()
                        .map(|(_, composer)| composer.clone().into_any_element())
                        .unwrap_or_else(|| div().into_any_element()),
                )
                .child(
                    Button::new(SharedString::from(format!("send-reply-{}", thread.id)), "Send")
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, window, cx| this.submit_reply(window, cx))),
                )
            })
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new(SharedString::from(format!("open-{}", thread.id)), "Open")
                            .label_size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .on_click(cx.listener({
                                let thread_id = thread_id.clone();
                                move |this, _, window, cx| {
                                    this.jump_to(thread_id.clone(), window, cx)
                                }
                            })),
                    )
                    .when(!replying, |this| {
                        this.child(
                            Button::new(SharedString::from(format!("reply-{}", thread.id)), "Reply")
                                .label_size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .on_click(cx.listener({
                                    let thread_id = thread_id.clone();
                                    move |this, _, window, cx| {
                                        this.start_reply(thread_id.clone(), window, cx)
                                    }
                                })),
                        )
                    })
                    .child(
                        Button::new(
                            SharedString::from(format!("resolve-{}", thread.id)),
                            if thread.resolved {
                                "Reopen"
                            } else {
                                "Resolve"
                            },
                        )
                        .label_size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .on_click(cx.listener({
                            let thread_id = thread_id.clone();
                            move |this, _, _window, cx| this.toggle_resolved(&thread_id, cx)
                        })),
                    ),
            )
            .into_any_element()
    }
}

impl Render for TeamNotesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let show_resolved = self.show_resolved;
        let mut threads: Vec<NoteThread> = self
            .threads
            .iter()
            .filter(|thread| show_resolved || !thread.resolved)
            .cloned()
            .collect();
        // Unresolved first, then by file, so the thing needing an answer is at
        // the top rather than wherever its id happens to sort.
        threads.sort_by(|a, b| {
            a.resolved
                .cmp(&b.resolved)
                .then_with(|| a.file.cmp(&b.file))
                .then_with(|| a.anchor.line.cmp(&b.anchor.line))
        });

        let empty = threads.is_empty() && self.drafting.is_none();

        v_flex()
            .key_context("TeamNotesPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(
                h_flex()
                    .p_2()
                    .gap_1()
                    .justify_between()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(Label::new("Team Notes").size(LabelSize::Small))
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                IconButton::new("toggle-resolved", IconName::Check)
                                    .icon_size(IconSize::Small)
                                    .icon_color(if show_resolved {
                                        Color::Accent
                                    } else {
                                        Color::Muted
                                    })
                                    .tooltip(Tooltip::text(if show_resolved {
                                        "Hide resolved notes"
                                    } else {
                                        "Show resolved notes"
                                    }))
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.show_resolved = !this.show_resolved;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                IconButton::new("reload-notes", IconName::ArrowCircle)
                                    .icon_size(IconSize::Small)
                                    .icon_color(Color::Muted)
                                    .tooltip(Tooltip::text("Re-read notes from the repository"))
                                    .on_click(cx.listener(|this, _, _window, cx| this.reload(cx))),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .id("team-notes-list")
                    .p_2()
                    .gap_2()
                    .size_full()
                    .overflow_y_scroll()
                    .when_some(self.drafting.as_ref(), |this, (anchor, composer)| {
                        this.child(
                            v_flex()
                                .p_2()
                                .gap_1p5()
                                .rounded_md()
                                .border_1()
                                .border_color(cx.theme().colors().border_focused)
                                .child(
                                    Label::new(format!("{}:{}", anchor.file, anchor.line + 1))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .child(composer.clone())
                                .child(
                                    Button::new("send-note", "Add Note")
                                        .label_size(LabelSize::Small)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.submit_draft(window, cx)
                                        })),
                                ),
                        )
                    })
                    .when(empty, |this| {
                        this.child(
                            v_flex()
                                .p_4()
                                .gap_1()
                                .child(Label::new("No notes yet").color(Color::Muted))
                                .child(
                                    Label::new(
                                        "Put the cursor on a line and run \
                                         `team notes: add note`. Notes are written to \
                                         .acuto/notes.jsonl and travel with the repository.",
                                    )
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                                ),
                        )
                    })
                    .children(
                        threads
                            .iter()
                            .map(|thread| self.render_thread(thread, cx)),
                    ),
            )
    }
}

impl Focusable for TeamNotesPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for TeamNotesPanel {}
impl EventEmitter<DismissEvent> for TeamNotesPanel {}

impl Panel for TeamNotesPanel {
    fn persistent_name() -> &'static str {
        "TeamNotesPanel"
    }

    fn panel_key() -> &'static str {
        "team_notes_panel"
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, _position: DockPosition, _window: &mut Window, _cx: &mut Context<Self>) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(320.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Chat)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Team Notes")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    /// After the panels that are part of writing code and before the ones that
    /// are occasional: notes are read alongside the file, not instead of it.
    fn activation_priority(&self) -> u32 {
        6
    }
}
