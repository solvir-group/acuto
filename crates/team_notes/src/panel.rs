//! The panel: notes on lines, tickets for work, both carried by the repository.

use std::{path::PathBuf, sync::Arc};

use editor::{Editor, MultiBufferOffset};
use fs::Fs;
use gpui::{
    Action, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Task,
    WeakEntity, Window, actions,
};
use language::ToOffset as _;
use project::Project;
use ui::{Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::{Anchor, Kind, Message, NoteThread, Status, author_name, notes_file, resolve_anchor};

actions!(
    team_notes,
    [
        /// Opens or closes the team panel.
        ToggleFocus,
        /// Starts a note on the line the cursor is on.
        AddNote,
        /// Starts a ticket.
        NewTicket,
    ]
);

/// Which records the list is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    /// Assigned to you or mentioning you. The first thing to look at, so it is
    /// the first tab and the default.
    Inbox,
    Messages,
    Tickets,
    Notes,
    All,
}

impl Filter {
    const ALL: [Filter; 5] = [
        Filter::Inbox,
        Filter::Messages,
        Filter::Tickets,
        Filter::Notes,
        Filter::All,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Inbox => "Inbox",
            Self::Messages => "Messages",
            Self::Tickets => "Tickets",
            Self::Notes => "Notes",
            Self::All => "All",
        }
    }

    fn admits(self, record: &NoteThread, me: &str) -> bool {
        match self {
            Self::Inbox => record.concerns(me),
            Self::Messages => record.kind == Kind::Message,
            Self::Notes => record.kind == Kind::Note,
            Self::Tickets => record.kind == Kind::Ticket,
            Self::All => true,
        }
    }
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<TeamNotesPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &AddNote, window, cx| {
            TeamNotesPanel::start_note_at_cursor(workspace, window, cx);
        });
        workspace.register_action(|workspace, _: &NewTicket, window, cx| {
            TeamNotesPanel::start_ticket(workspace, window, cx);
        });
    })
    .detach();
}

/// What is being written, and where it will attach.
enum Draft {
    /// A note on a line of a file.
    Note { file: String, anchor: Anchor },
    /// A ticket, which may or may not be about a place in the code.
    Ticket,
    /// A message to the team.
    Message,
}

pub struct TeamNotesPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    fs: Arc<dyn Fs>,
    focus_handle: FocusHandle,
    records: Vec<NoteThread>,
    filter: Filter,
    show_closed: bool,
    /// The record whose reply box is open.
    replying_to: Option<(String, Entity<Editor>)>,
    /// A record being written that does not exist yet.
    drafting: Option<(Draft, Entity<Editor>)>,
    author: Arc<str>,
    _reload: Option<Task<()>>,
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
            records: Vec::new(),
            filter: Filter::Inbox,
            show_closed: false,
            replying_to: None,
            drafting: None,
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

    /// The worktree the file belongs to.
    ///
    /// The first visible worktree. A window with several of them keeps its
    /// records with the first, because they have to live in exactly one
    /// repository and splitting them would make "where is this" unanswerable.
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
            let contents = fs.load(&notes_file(&root)).await.unwrap_or_default();
            let records = crate::parse(&contents);
            this.update(cx, |this, cx| {
                this.records = records;
                cx.notify();
            })
            .ok();
        }));
    }

    fn commit(&mut self, records: Vec<NoteThread>, cx: &mut Context<Self>) {
        self.records = records.clone();
        cx.notify();

        let Some(root) = self.worktree_root(cx) else {
            return;
        };
        let fs = self.fs.clone();

        cx.background_spawn(async move {
            let Some(contents) = crate::render(&records).log_err() else {
                return;
            };
            let path = notes_file(&root);
            if let Some(parent) = path.parent() {
                fs.create_dir(parent).await.log_err();
            }
            fs.write(&path, contents.as_bytes()).await.log_err();
        })
        .detach();
    }

    fn open_composer(
        &mut self,
        draft: Draft,
        placeholder: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let placeholder = placeholder.to_string();
        let composer = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 8, window, cx);
            editor.set_placeholder_text(placeholder.as_str(), window, cx);
            editor
        });
        composer.focus_handle(cx).focus(window, cx);
        self.replying_to = None;
        self.drafting = Some((draft, composer));
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

        let Some((file, anchor)) = editor.update(cx, |editor, cx| {
            let buffer = editor.buffer().read(cx).as_singleton()?;
            let file = buffer.read(cx).file()?.path().as_std_path().to_path_buf();

            // Converted through the multibuffer, then read against the buffer's
            // own text. A note is only ever attached to a single-file buffer,
            // so the two coordinate spaces agree.
            let head = editor.selections.newest_anchor().head();
            let multi_snapshot = editor.buffer().read(cx).snapshot(cx);
            let (text_anchor, _) = multi_snapshot.anchor_to_buffer_anchor(head)?;

            let snapshot = buffer.read(cx).snapshot();
            let offset = text_anchor.to_offset(&snapshot);
            let text = snapshot.text();
            let (row, line_text) = row_and_line_at(&text, offset);

            Some((
                crate::normalize_path(&file),
                Anchor {
                    line: row,
                    text: line_text,
                },
            ))
        }) else {
            return;
        };

        workspace.open_panel::<TeamNotesPanel>(window, cx);
        panel.update(cx, |panel, cx| {
            panel.filter = Filter::Notes;
            panel.open_composer(
                Draft::Note { file, anchor },
                "Ask about this line…",
                window,
                cx,
            );
        });
    }

    pub fn start_ticket(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(panel) = workspace.panel::<TeamNotesPanel>(cx) else {
            return;
        };
        workspace.open_panel::<TeamNotesPanel>(window, cx);
        panel.update(cx, |panel, cx| {
            panel.filter = Filter::Tickets;
            panel.open_composer(
                Draft::Ticket,
                "What needs doing? @mention someone to assign it.",
                window,
                cx,
            );
        });
    }

    fn submit_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((draft, composer)) = self.drafting.take() else {
            return;
        };
        let body = composer.read(cx).text(cx).trim().to_string();
        if body.is_empty() {
            cx.notify();
            return;
        }

        // The first person mentioned takes the ticket. Typing "@sam ship the
        // installer" is how anyone would say it out loud, and making them fill
        // in a separate assignee field afterwards is a form, not a sentence.
        let assignee = crate::extract_mentions(&body).into_iter().next();

        let record = match draft {
            Draft::Note { file, anchor } => NoteThread {
                id: crate::new_thread_id(),
                kind: Kind::Note,
                file: Some(file),
                anchor: Some(anchor),
                title: None,
                status: Status::Open,
                assignee,
                resolved: false,
                messages: vec![Message {
                    author: self.author.to_string(),
                    at: crate::now_timestamp(),
                    body,
                }],
            },
            Draft::Ticket => NoteThread {
                id: crate::new_thread_id(),
                kind: Kind::Ticket,
                file: None,
                anchor: None,
                title: Some(body),
                status: Status::Open,
                assignee,
                resolved: false,
                messages: Vec::new(),
            },
            Draft::Message => NoteThread {
                id: crate::new_thread_id(),
                kind: Kind::Message,
                file: None,
                anchor: None,
                title: None,
                status: Status::Open,
                assignee,
                resolved: false,
                messages: vec![Message {
                    author: self.author.to_string(),
                    at: crate::now_timestamp(),
                    body,
                }],
            },
        };

        let mut records = self.records.clone();
        records.push(record);
        self.commit(records, cx);
        self.focus_handle.focus(window, cx);
    }

    fn submit_reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((id, composer)) = self.replying_to.take() else {
            return;
        };
        let body = composer.read(cx).text(cx).trim().to_string();
        if body.is_empty() {
            cx.notify();
            return;
        }

        let mut records = self.records.clone();
        if let Some(record) = records.iter_mut().find(|record| record.id == id) {
            record.messages.push(Message {
                author: self.author.to_string(),
                at: crate::now_timestamp(),
                body,
            });
        }
        self.commit(records, cx);
        self.focus_handle.focus(window, cx);
    }

    fn start_reply(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let composer = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 8, window, cx);
            editor.set_placeholder_text("Reply…", window, cx);
            editor
        });
        composer.focus_handle(cx).focus(window, cx);
        self.drafting = None;
        self.replying_to = Some((id, composer));
        cx.notify();
    }

    /// Moves a record forward: a ticket to its next status, anything else
    /// between open and resolved.
    fn advance(&mut self, id: &str, cx: &mut Context<Self>) {
        let mut records = self.records.clone();
        if let Some(record) = records.iter_mut().find(|record| record.id == id) {
            match record.kind {
                Kind::Ticket => record.status = record.status.next(),
                Kind::Note | Kind::Message => record.resolved = !record.resolved,
            }
        }
        self.commit(records, cx);
    }

    /// Marks a record finished in one click, wherever it currently is.
    ///
    /// Separate from `advance`, which cycles: getting a ticket to Done from
    /// Open takes two clicks through In Progress, and "this is finished" is the
    /// thing you most often want to say.
    fn complete(&mut self, id: &str, cx: &mut Context<Self>) {
        let mut records = self.records.clone();
        if let Some(record) = records.iter_mut().find(|record| record.id == id) {
            let finished = record.is_closed();
            match record.kind {
                Kind::Ticket => {
                    record.status = if finished { Status::Open } else { Status::Done }
                }
                Kind::Note | Kind::Message => record.resolved = !finished,
            }
        }
        self.commit(records, cx);
    }

    /// Turns a message into a ticket, keeping what was already said.
    ///
    /// The common path: something is raised in conversation and turns out to be
    /// work. Retyping it into a tracker is where the detail gets lost.
    fn promote_to_ticket(&mut self, id: &str, cx: &mut Context<Self>) {
        let mut records = self.records.clone();
        if let Some(record) = records.iter_mut().find(|record| record.id == id) {
            if record.kind == Kind::Message {
                record.kind = Kind::Ticket;
                record.title = Some(record.headline().to_string());
                record.status = Status::Open;
                record.resolved = false;
            }
        }
        self.commit(records, cx);
    }

    fn take_ownership(&mut self, id: &str, cx: &mut Context<Self>) {
        let me = self.author.to_string();
        let mut records = self.records.clone();
        if let Some(record) = records.iter_mut().find(|record| record.id == id) {
            record.assignee = match record.assignee.as_deref() {
                Some(existing) if existing.eq_ignore_ascii_case(&me) => None,
                _ => Some(me),
            };
        }
        self.commit(records, cx);
    }

    /// Opens the file a record is attached to, at wherever its anchor resolves
    /// to now.
    ///
    /// Resolved against the buffer after it opens rather than before: the file
    /// may have changed since the record was written, and carrying the anchor
    /// text is what lets it find its way back.
    fn jump_to(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(record) = self.records.iter().find(|record| record.id == id).cloned() else {
            return;
        };
        let (Some(file), Some(anchor)) = (record.file.clone(), record.anchor.clone()) else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let Some(project_path) = self
            .project
            .read(cx)
            .find_project_path(std::path::Path::new(&file), cx)
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
                let row = resolve_anchor(&anchor, &lines).line();

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

    /// How many records are waiting for this person.
    fn inbox_count(&self) -> usize {
        self.records
            .iter()
            .filter(|record| !record.is_closed() && record.concerns(&self.author))
            .count()
    }

    fn render_record(&self, record: &NoteThread, cx: &mut Context<Self>) -> gpui::AnyElement {
        let id = record.id.clone();
        let replying = self
            .replying_to
            .as_ref()
            .is_some_and(|(open, _)| *open == record.id);
        let closed = record.is_closed();
        let is_ticket = record.kind == Kind::Ticket;

        let status_label = if is_ticket {
            record.status.label().to_string()
        } else if record.resolved {
            "Resolved".to_string()
        } else {
            "Open".to_string()
        };
        let status_color = if closed {
            Color::Success
        } else if is_ticket && record.status == Status::InProgress {
            Color::Accent
        } else {
            Color::Muted
        };

        let location = record.file.as_ref().map(|file| {
            let line = record.anchor.as_ref().map_or(0, |anchor| anchor.line);
            format!("{file}:{}", line + 1)
        });

        let mine = record
            .assignee
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case(&self.author));

        v_flex()
            .id(SharedString::from(record.id.clone()))
            .w_full()
            .p_2()
            .gap_1p5()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .bg(cx.theme().colors().elevated_surface_background.opacity(0.5))
            .when(closed, |this| this.opacity(0.55))
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_1()
                    .child(
                        Icon::new(match record.kind {
                            Kind::Ticket => IconName::ListTodo,
                            Kind::Message => IconName::Chat,
                            Kind::Note => IconName::Pin,
                        })
                        .size(IconSize::XSmall)
                        .color(status_color),
                    )
                    // The status is the control, not a label beside one: there
                    // is one thing you do to a record and this is it.
                    .child(
                        Button::new(
                            SharedString::from(format!("status-{}", record.id)),
                            status_label,
                        )
                        .label_size(LabelSize::XSmall)
                        .color(status_color)
                        .tooltip(Tooltip::text(if is_ticket {
                            "Move to the next status"
                        } else {
                            "Resolve or reopen"
                        }))
                        .on_click(cx.listener({
                            let id = id.clone();
                            move |this, _, _window, cx| this.advance(&id, cx)
                        })),
                    )
                    .when_some(record.assignee.clone(), |this, assignee| {
                        this.child(
                            Label::new(format!("@{assignee}"))
                                .size(LabelSize::XSmall)
                                .color(Color::Accent),
                        )
                    })
                    .when_some(location, |this, location| {
                        this.child(
                            Label::new(location)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                        )
                    }),
            )
            .child(Label::new(record.headline().to_string()).size(LabelSize::Small))
            .children(record.messages.iter().skip(1).map(|message| {
                v_flex()
                    .child(
                        Label::new(message.author.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(Label::new(message.body.clone()).size(LabelSize::Small))
            }))
            .when(replying, |this| {
                this.children(
                    self.replying_to
                        .as_ref()
                        .map(|(_, composer)| composer.clone()),
                )
                .child(
                    Button::new(SharedString::from(format!("send-{}", record.id)), "Send")
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, window, cx| this.submit_reply(window, cx))),
                )
            })
            .child(
                h_flex()
                    .gap_1()
                    .when(record.file.is_some(), |this| {
                        this.child(
                            Button::new(SharedString::from(format!("open-{}", record.id)), "Open")
                                .label_size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .on_click(cx.listener({
                                    let id = id.clone();
                                    move |this, _, window, cx| this.jump_to(id.clone(), window, cx)
                                })),
                        )
                    })
                    .when(!replying, |this| {
                        this.child(
                            Button::new(
                                SharedString::from(format!("reply-{}", record.id)),
                                "Reply",
                            )
                            .label_size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .on_click(cx.listener({
                                let id = id.clone();
                                move |this, _, window, cx| this.start_reply(id.clone(), window, cx)
                            })),
                        )
                    })
                    .child(
                        Button::new(
                            SharedString::from(format!("assign-{}", record.id)),
                            if mine { "Unassign" } else { "Take" },
                        )
                        .label_size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .on_click(cx.listener({
                            let id = id.clone();
                            move |this, _, _window, cx| this.take_ownership(&id, cx)
                        })),
                    )
                    .when(record.kind == Kind::Message, |this| {
                        this.child(
                            Button::new(
                                SharedString::from(format!("promote-{}", record.id)),
                                "Make Ticket",
                            )
                            .label_size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .tooltip(Tooltip::text(
                                "Turn this into a ticket, keeping the conversation",
                            ))
                            .on_click(cx.listener({
                                let id = id.clone();
                                move |this, _, _window, cx| this.promote_to_ticket(&id, cx)
                            })),
                        )
                    })
                    // Done in one click, wherever the record currently is. The
                    // status button cycles, which takes two clicks to finish a
                    // ticket that has not been started.
                    .child(
                        IconButton::new(
                            SharedString::from(format!("complete-{}", record.id)),
                            IconName::Check,
                        )
                        .icon_size(IconSize::XSmall)
                        .icon_color(if closed { Color::Success } else { Color::Muted })
                        .tooltip(Tooltip::text(if closed {
                            "Reopen"
                        } else if is_ticket {
                            "Mark this ticket done"
                        } else {
                            "Mark this resolved"
                        }))
                        .on_click(cx.listener({
                            let id = id.clone();
                            move |this, _, _window, cx| this.complete(&id, cx)
                        })),
                    ),
            )
            .into_any_element()
    }
}

/// The row `offset` falls on, and that row's text.
fn row_and_line_at(text: &str, offset: usize) -> (u32, String) {
    let mut row = 0u32;
    let mut consumed = 0usize;
    for line in text.lines() {
        let next = consumed + line.len() + 1;
        if offset < next {
            return (row, line.to_string());
        }
        consumed = next;
        row += 1;
    }
    (
        row.saturating_sub(1),
        text.lines().last().unwrap_or_default().to_string(),
    )
}

impl Render for TeamNotesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let filter = self.filter;
        let show_closed = self.show_closed;
        let me = self.author.to_string();

        let mut records: Vec<NoteThread> = self
            .records
            .iter()
            .filter(|record| show_closed || !record.is_closed())
            .filter(|record| filter.admits(record, &me))
            .cloned()
            .collect();

        // Open first, then by file, then by line. What needs an answer is at
        // the top rather than wherever its id happens to sort.
        records.sort_by(|a, b| {
            a.is_closed()
                .cmp(&b.is_closed())
                .then_with(|| a.file.cmp(&b.file))
                .then_with(|| {
                    a.anchor
                        .as_ref()
                        .map(|anchor| anchor.line)
                        .cmp(&b.anchor.as_ref().map(|anchor| anchor.line))
                })
        });

        let inbox_count = self.inbox_count();
        let empty = records.is_empty() && self.drafting.is_none();

        v_flex()
            .key_context("TeamNotesPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(
                v_flex()
                    .p_2()
                    .gap_1p5()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        h_flex()
                            .justify_between()
                            .child(Label::new("Team").size(LabelSize::Small))
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(
                                        IconButton::new("new-message", IconName::Chat)
                                            .icon_size(IconSize::Small)
                                            .icon_color(Color::Muted)
                                            .tooltip(Tooltip::text("New Message"))
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.filter = Filter::Messages;
                                                this.open_composer(
                                                    Draft::Message,
                                                    "Say something to the team. @mention to reach someone.",
                                                    window,
                                                    cx,
                                                );
                                            })),
                                    )
                                    .child(
                                        IconButton::new("new-ticket", IconName::Plus)
                                            .icon_size(IconSize::Small)
                                            .icon_color(Color::Muted)
                                            .tooltip(Tooltip::text("New Ticket"))
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.filter = Filter::Tickets;
                                                this.open_composer(
                                                    Draft::Ticket,
                                                    "What needs doing? @mention someone to assign it.",
                                                    window,
                                                    cx,
                                                );
                                            })),
                                    )
                                    .child(
                                        IconButton::new("show-closed", IconName::Check)
                                            .icon_size(IconSize::Small)
                                            .icon_color(if show_closed {
                                                Color::Accent
                                            } else {
                                                Color::Muted
                                            })
                                            .tooltip(Tooltip::text(if show_closed {
                                                "Hide finished"
                                            } else {
                                                "Show finished"
                                            }))
                                            .on_click(cx.listener(|this, _, _window, cx| {
                                                this.show_closed = !this.show_closed;
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        IconButton::new("reload", IconName::ArrowCircle)
                                            .icon_size(IconSize::Small)
                                            .icon_color(Color::Muted)
                                            .tooltip(Tooltip::text("Re-read from the repository"))
                                            .on_click(cx.listener(|this, _, _window, cx| {
                                                this.reload(cx)
                                            })),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_0p5()
                            .children(Filter::ALL.into_iter().map(|option| {
                                let label = if option == Filter::Inbox && inbox_count > 0 {
                                    format!("Inbox {inbox_count}")
                                } else {
                                    option.label().to_string()
                                };
                                Button::new(SharedString::from(option.label()), label)
                                    .label_size(LabelSize::XSmall)
                                    .color(if option == filter {
                                        Color::Accent
                                    } else {
                                        Color::Muted
                                    })
                                    .on_click(cx.listener(move |this, _, _window, cx| {
                                        this.filter = option;
                                        cx.notify();
                                    }))
                            })),
                    ),
            )
            .child(
                v_flex()
                    .id("team-list")
                    .p_2()
                    .gap_2()
                    .size_full()
                    .overflow_y_scroll()
                    .when_some(self.drafting.as_ref(), |this, (draft, composer)| {
                        let where_to = match draft {
                            Draft::Note { file, anchor } => format!("{file}:{}", anchor.line + 1),
                            Draft::Ticket => "New ticket".to_string(),
                            Draft::Message => "New message".to_string(),
                        };
                        this.child(
                            v_flex()
                                .p_2()
                                .gap_1p5()
                                .rounded_md()
                                .border_1()
                                .border_color(cx.theme().colors().border_focused)
                                .child(
                                    Label::new(where_to)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .child(composer.clone())
                                .child(
                                    Button::new("submit-draft", "Add")
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
                                .p_3()
                                .gap_1()
                                .child(
                                    Label::new(match filter {
                                        Filter::Inbox => "Nothing waiting on you",
                                        Filter::Messages => "No messages",
                                        Filter::Notes => "No notes",
                                        Filter::Tickets => "No tickets",
                                        Filter::All => "Nothing here yet",
                                    })
                                    .color(Color::Muted),
                                )
                                .child(
                                    Label::new(
                                        "Messages and tickets are the two buttons above; a note \
                                         attaches to a line, so put the cursor on one and run \
                                         `team notes: add note`. Everything is written to \
                                         .acuto/notes.jsonl and travels with the repository, so \
                                         your team sees it on the next pull -- no account, no \
                                         server, and it reviews as part of the diff.",
                                    )
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                                ),
                        )
                    })
                    .children(records.iter().map(|record| self.render_record(record, cx))),
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

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(340.)
    }

    /// Not `Chat`: the agent's toggle in the title bar already uses that, and
    /// two chat icons that open different things is a coin toss every time.
    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::ListTodo)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Team")
    }

    /// The count of things waiting on you, on the dock button, so the panel is
    /// worth glancing at without opening it.
    fn icon_label(&self, _window: &Window, _cx: &App) -> Option<String> {
        let waiting = self.inbox_count();
        (waiting > 0).then(|| waiting.to_string())
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    /// Last of the docked panels. The numbers have to be unique -- `Dock`
    /// panics on a collision rather than picking an order -- and 0 through 7
    /// are taken by the panels that ship above this one.
    fn activation_priority(&self) -> u32 {
        8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_row_and_line_are_found_from_a_byte_offset() {
        let text = "one\ntwo\nthree\n";
        assert_eq!(row_and_line_at(text, 0), (0, "one".to_string()));
        assert_eq!(row_and_line_at(text, 4), (1, "two".to_string()));
        assert_eq!(row_and_line_at(text, 9), (2, "three".to_string()));
        // Past the end clamps to the last line rather than panicking.
        assert_eq!(row_and_line_at(text, 999), (2, "three".to_string()));
    }

    #[test]
    fn an_empty_file_has_no_rows_to_find() {
        assert_eq!(row_and_line_at("", 0), (0, String::new()));
    }
}
