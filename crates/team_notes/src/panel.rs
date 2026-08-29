//! The panel: notes on lines, tickets for work, both carried by the repository.

use std::{path::PathBuf, sync::Arc};

use editor::{Editor, EditorEvent, MultiBufferOffset};
use fs::Fs;
use gpui::{
    Action, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Subscription,
    Task, WeakEntity, Window, actions,
};
use language::ToOffset as _;
use project::Project;
use ui::{Icon, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::{
    Anchor, Kind, Message, NoteThread, Status, author_name, notes_file, resolve_anchor,
    team_roster,
};

actions!(
    team_notes,
    [
        /// Opens or closes the team panel.
        ToggleFocus,
        /// Starts a note on the line the cursor is on.
        AddNote,
        /// Starts a ticket.
        NewTicket,
        /// Sends whatever is in the chat composer.
        SendMessage,
        /// Abandons the note, ticket or message being written.
        CancelDraft,
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
    /// Everyone on the repository, most recently active first.
    team: Vec<Arc<str>>,
    /// Redraws the panel as the composer is typed in.
    ///
    /// Without it the send button cannot know whether there is anything to
    /// send: nothing else makes the panel re-render between keystrokes, so it
    /// would sit dimmed until some unrelated event happened to repaint.
    _composer_edits: Option<Subscription>,
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
        // Taken before `project` moves into the struct, so the roster can be
        // fetched by the same task that resolves the author name.
        let root = project
            .read(cx)
            .visible_worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf());

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
            team: Vec::new(),
            _composer_edits: None,
            _reload: None,
        };

        cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let author = author_name(&executor).await;
            let mut team = match root {
                Some(root) => team_roster(root, &executor).await,
                None => Vec::new(),
            };
            // You belong in the roster before your first commit lands, which is
            // exactly when a new person is most likely to open this panel.
            if !team
                .iter()
                .any(|member| member.as_ref().eq_ignore_ascii_case(author.as_ref()))
            {
                team.insert(0, author.clone());
            }
            this.update(cx, |this, cx| {
                this.author = author;
                this.team = team;
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
        self._composer_edits = Some(cx.subscribe(&composer, |_this, _composer, event, cx| {
            if matches!(event, EditorEvent::BufferEdited) {
                cx.notify();
            }
        }));
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

    /// Throws away whatever was being written.
    ///
    /// A composer with no way out is a trap: until this existed the only way to
    /// stop writing a ticket was to submit one.
    fn cancel_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.drafting = None;
        self.replying_to = None;
        self._composer_edits = None;
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    /// Whether the draft has anything in it worth keeping.
    fn draft_is_empty(&self, cx: &App) -> bool {
        match self.drafting.as_ref() {
            Some((_, composer)) => composer.read(cx).text(cx).trim().is_empty(),
            None => true,
        }
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

impl TeamNotesPanel {
    /// Who the conversation is with.
    ///
    /// Sits where the panel's own title row does on the other tabs, so the chat
    /// gets a conversation header without the panel growing a second one.
    fn render_chat_header(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        /// Enough faces to say "this is a group" in a 340px dock.
        const STACK: usize = 3;

        let ring = cx.theme().colors().panel_background;

        h_flex()
            .w_full()
            .gap_2()
            .child(
                h_flex()
                    .flex_none()
                    .children(self.team.iter().take(STACK).enumerate().map(
                        |(index, member)| {
                            div()
                                // Overlapped rather than spaced: a stack reads
                                // as one group, a row reads as a list.
                                .when(index > 0, |this| this.ml(px(-8.)))
                                .child(initials_avatar(member, px(24.), Some(ring), cx))
                        },
                    )),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(Label::new("Everyone").size(LabelSize::Small))
                    .child(
                        Label::new(self.roster_summary())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(
                IconButton::new("chat-reload", IconName::ArrowCircle)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text("Re-read from the repository"))
                    .on_click(cx.listener(|this, _, _window, cx| this.reload(cx))),
            )
            .into_any_element()
    }

    /// The subtitle under "Everyone".
    ///
    /// Deliberately not "Active now". Presence needs a server telling you who
    /// has the editor open, and there is none -- the whole point of this panel
    /// is that it works with nothing but the repository. A count of who is on
    /// the repository is the true version of the same reassurance.
    fn roster_summary(&self) -> String {
        match self.team.len() {
            0 => "Reading the repository…".to_string(),
            1 => "Just you, so far".to_string(),
            count => format!("{count} people on this repo"),
        }
    }

    /// The Messages view: a transcript, and a composer pinned to the bottom.
    ///
    /// Deliberately not the card list the other tabs use. A message is read in
    /// order and answered at the end, so the shape everyone already knows for
    /// that -- oldest at the top, newest above the box you type in -- is the
    /// shape that needs no explanation. Cards are right for tickets, where you
    /// scan for the one you want.
    fn render_chat(&self, records: &[NoteThread], cx: &mut Context<Self>) -> gpui::AnyElement {
        let me = self.author.to_string();

        // Oldest first: a transcript reads down the page. Ids are time-ordered,
        // so this is chronological without storing a second field to sort on.
        let mut ordered: Vec<&NoteThread> = records.iter().collect();
        ordered.sort_by(|a, b| a.id.cmp(&b.id));

        // Flattened into one stream, each entry carrying what the renderer needs
        // to know about its neighbours. Working that out here rather than in the
        // element tree keeps the layout code to one shape per entry.
        struct Bubble {
            message: Message,
            mine: bool,
            /// First of a run by the same person: the one that gets a face and
            /// a name.
            starts_run: bool,
            /// A time to draw above it, when the clock has moved on.
            stamp: Option<String>,
        }

        let mut bubbles: Vec<Bubble> = Vec::new();
        let mut previous_author: Option<String> = None;
        let mut previous_minute: Option<String> = None;
        for record in &ordered {
            for message in &record.messages {
                let mine = message.author.eq_ignore_ascii_case(&me);
                let starts_run = previous_author
                    .as_deref()
                    .is_none_or(|author| !author.eq_ignore_ascii_case(&message.author));
                let minute = message.at.get(..16).unwrap_or(&message.at).to_string();
                let stamp = (previous_minute.as_deref() != Some(minute.as_str()))
                    .then(|| separator_stamp(&message.at, previous_minute.as_deref()));

                previous_author = Some(message.author.clone());
                previous_minute = Some(minute);
                bubbles.push(Bubble {
                    message: message.clone(),
                    mine,
                    starts_run,
                    stamp,
                });
            }
        }

        // The status line goes under the last thing *you* said, which is where
        // you look to check it went.
        let last_own = bubbles.iter().rposition(|bubble| bubble.mine);

        let sent_background = cx.theme().players().local().cursor;
        let received_background = cx.theme().colors().elevated_surface_background;
        let on_sent = gpui::hsla(0., 0., 1., 1.);
        let empty = bubbles.is_empty();

        v_flex()
            .size_full()
            .justify_between()
            .child(
                v_flex()
                    .id("team-chat-transcript")
                    .flex_1()
                    .px_2()
                    .py_1()
                    .overflow_y_scroll()
                    .when(empty, |this| {
                        this.child(
                            Label::new(
                                "Nothing said yet. Messages are written to \
                                 .acuto/notes.jsonl and travel with the repository, so \
                                 your team sees them on the next pull.",
                            )
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        )
                    })
                    .children(bubbles.into_iter().enumerate().map(|(index, bubble)| {
                        let at = bubble.message.at.clone();
                        let is_last_own = last_own == Some(index);

                        // Messages fired off in one go sit almost on top of each
                        // other; a change of speaker, or a jump in the clock,
                        // gets air. The spacing is what tells you where one
                        // thought ends, without drawing anything to say so.
                        let space_above = if bubble.stamp.is_some() {
                            px(7.)
                        } else if bubble.starts_run {
                            px(4.)
                        } else {
                            px(1.)
                        };

                        v_flex()
                            .w_full()
                            .mt(space_above)
                            .gap_0p5()
                            .when_some(bubble.stamp, |this, stamp| {
                                this.child(
                                    Label::new(stamp).size(LabelSize::XSmall).color(Color::Muted),
                                )
                            })
                            .child(
                                h_flex()
                                    .w_full()
                                    .items_end()
                                    .gap_1()
                                    .when(bubble.mine, |this| this.justify_end())
                                    .when(!bubble.mine, |this| {
                                        // The face sits against the first bubble
                                        // of a run; the rest of the run is
                                        // indented past the same gap so the
                                        // column of text stays straight.
                                        this.child(if bubble.starts_run {
                                            initials_avatar(
                                                &bubble.message.author,
                                                px(22.),
                                                None,
                                                cx,
                                            )
                                        } else {
                                            div().flex_none().w(px(22.)).into_any_element()
                                        })
                                    })
                                    .child(
                                        v_flex()
                                            .max_w(relative(0.78))
                                            .min_w_0()
                                            .gap_0p5()
                                            // A name only when the speaker
                                            // changes, and never on your own:
                                            // sitting on the right already says
                                            // who wrote it.
                                            .when(!bubble.mine && bubble.starts_run, |this| {
                                                this.child(
                                                    Label::new(bubble.message.author.clone())
                                                        .size(LabelSize::XSmall)
                                                        .color(Color::Muted),
                                                )
                                            })
                                            .child(
                                                div()
                                                    .px_2p5()
                                                    .py_1p5()
                                                    .rounded_2xl()
                                                    .bg(if bubble.mine {
                                                        sent_background
                                                    } else {
                                                        received_background
                                                    })
                                                    .child({
                                                        let label =
                                                            Label::new(bubble.message.body.clone())
                                                                .size(LabelSize::Small);
                                                        if bubble.mine {
                                                            label.color(Color::Custom(on_sent))
                                                        } else {
                                                            label
                                                        }
                                                    }),
                                            ),
                                    ),
                            )
                            .when(is_last_own, |this| {
                                // "Sent", not "Seen". Nothing here can know
                                // whether anyone read it -- that needs a server,
                                // and claiming it without one would be a lie on
                                // every message.
                                this.child(
                                    h_flex().w_full().justify_end().child(
                                        Label::new(format!("Sent · {}", short_time(&at)))
                                            .size(LabelSize::XSmall)
                                            .color(Color::Muted),
                                    ),
                                )
                            })
                    })),
            )
            .child(self.render_chat_composer(cx))
            .into_any_element()
    }

    /// One pill: attach on the left, what you are typing in the middle, send on
    /// the right.
    ///
    /// Everything lives inside a single rounded container rather than floating
    /// beside it as three separate controls, which is what made the old row look
    /// unaligned no matter what the spacing was -- three shapes with three
    /// different heights cannot be lined up, only approximately stacked. One
    /// container with one padding and one control height has nothing left to
    /// misalign.
    fn render_chat_composer(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        /// Both round buttons and the collapsed text area are this tall.
        const CONTROL: f32 = 26.;

        let composing = matches!(self.drafting, Some((Draft::Message, _)));
        // Not the same as "the composer is open". A send button that looks
        // active with an empty box is a button that does nothing when pressed.
        let ready = composing && !self.draft_is_empty(cx);
        let accent = cx.theme().players().local().cursor;
        let on_accent = gpui::hsla(0., 0., 1., 1.);

        div()
            .flex_none()
            .w_full()
            .px_2()
            .pt_1()
            .pb_2()
            .child(
                h_flex()
                    .key_context("TeamNotesComposer")
                    .w_full()
                    .items_end()
                    .gap_1()
                    .p_1()
                    .rounded_2xl()
                    .bg(cx.theme().colors().editor_background)
                    .border_1()
                    .border_color(if composing {
                        accent
                    } else {
                        cx.theme().colors().border_variant
                    })
                    .on_action(cx.listener(|this, _: &SendMessage, window, cx| {
                        this.send_chat_message(window, cx)
                    }))
                    .on_action(cx.listener(|this, _: &CancelDraft, window, cx| {
                        this.cancel_draft(window, cx)
                    }))
                    .child(
                        h_flex()
                            .id("chat-attach")
                            .flex_none()
                            .size(px(CONTROL))
                            .justify_center()
                            .rounded_full()
                            .cursor_pointer()
                            .hover(|style| style.bg(cx.theme().colors().element_hover))
                            .child(
                                Icon::new(IconName::Plus)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .tooltip(Tooltip::text("Mention the file you are looking at"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.attach_current_file(window, cx)
                            })),
                    )
                    .child(
                        v_flex()
                            .id("chat-input")
                            .flex_1()
                            .min_w_0()
                            .min_h(px(CONTROL))
                            .justify_center()
                            .cursor_text()
                            .map(|this| match self.drafting.as_ref() {
                                Some((Draft::Message, composer)) => this.child(composer.clone()),
                                _ => this.child(
                                    Label::new("Type your message")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                if !matches!(this.drafting, Some((Draft::Message, _))) {
                                    this.open_composer(
                                        Draft::Message,
                                        "Type your message",
                                        window,
                                        cx,
                                    );
                                }
                            })),
                    )
                    .child(
                        // Send, not a smiley. The reference shows an emoji
                        // button because its field is empty; an emoji button
                        // here would be a control that does nothing, which is
                        // worse than one that does the thing you came to do.
                        h_flex()
                            .id("chat-send")
                            .flex_none()
                            .size(px(CONTROL))
                            .justify_center()
                            .rounded_full()
                            .when(ready, |this| {
                                this.bg(accent)
                                    .cursor_pointer()
                                    .hover(|style| style.opacity(0.85))
                            })
                            .when(!ready, |this| this.bg(gpui::Hsla { a: 0.25, ..accent }))
                            .child(
                                Icon::new(IconName::Send)
                                    .size(IconSize::XSmall)
                                    .color(Color::Custom(if ready {
                                        on_accent
                                    } else {
                                        gpui::Hsla {
                                            a: 0.5,
                                            ..on_accent
                                        }
                                    })),
                            )
                            // The keybinding lives in the tooltip rather than
                            // in a hint line under the box: a line that appears
                            // when you focus the composer shifts the whole
                            // transcript up by its own height every time.
                            .tooltip(Tooltip::text("Send · Enter"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.send_chat_message(window, cx)
                            })),
                    ),
            )
            .into_any_element()
    }

    /// Sends, then reopens the box.
    ///
    /// A chat that closes its own composer after every line makes you click
    /// before each message.
    fn send_chat_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.submit_draft(window, cx);
        if self.drafting.is_none() {
            self.open_composer(Draft::Message, "Type your message", window, cx);
        }
    }

    /// Drops the path of the file you are looking at into the message.
    ///
    /// The one thing a chat inside an editor can do that a chat beside it
    /// cannot: say which file you mean without alt-tabbing to find out.
    fn attach_current_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.drafting, Some((Draft::Message, _))) {
            self.open_composer(Draft::Message, "Type your message", window, cx);
        }
        let Some(path) = self.active_file_path(cx) else {
            return;
        };
        let Some((Draft::Message, composer)) = self.drafting.as_ref() else {
            return;
        };
        let composer = composer.clone();
        composer.update(cx, |editor, cx| {
            editor.insert(&format!("{path} "), window, cx);
        });
        composer.focus_handle(cx).focus(window, cx);
    }

    /// The active editor's path, worktree-relative with forward slashes.
    fn active_file_path(&self, cx: &App) -> Option<String> {
        let workspace = self.workspace.upgrade()?;
        let item = workspace.read(cx).active_item(cx)?;
        let editor = item.act_as::<Editor>(cx)?;
        let buffer = editor.read(cx).buffer().read(cx).as_singleton()?;
        let path = buffer.read(cx).file()?.path().as_std_path().to_path_buf();
        Some(
            path.to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/"),
        )
    }
}

/// A circle with someone's initials, coloured from their name.
///
/// Not `ui::Avatar`, which needs an image URL: there is no account system here
/// and no server to fetch a picture from, so the name is all there is. The
/// colour is hashed from the name, so one person is the same colour on every
/// teammate's screen without anything having to agree on it.
fn initials_avatar(
    name: &str,
    size: gpui::Pixels,
    ring: Option<gpui::Hsla>,
    cx: &App,
) -> gpui::AnyElement {
    let background = cx
        .theme()
        .players()
        .color_for_participant(name_hash(name))
        .cursor;

    h_flex()
        .flex_none()
        .size(size)
        .justify_center()
        .rounded_full()
        .bg(background)
        .when_some(ring, |this, ring| this.border_2().border_color(ring))
        .child(
            Label::new(initials(name))
                .size(LabelSize::XSmall)
                .color(Color::Custom(gpui::hsla(0., 0., 1., 1.))),
        )
        .into_any_element()
}

/// `Drew Wycherley` as `DW`, `procoder30001` as `P`.
fn initials(name: &str) -> String {
    let mut words = name.split_whitespace();
    let first = words.next().and_then(|word| word.chars().next());
    let last = words.last().and_then(|word| word.chars().next());
    match (first, last) {
        (Some(first), Some(last)) => format!("{}{}", upper(first), upper(last)),
        (Some(first), None) => upper(first),
        _ => "?".to_string(),
    }
}

fn upper(character: char) -> String {
    character.to_uppercase().collect()
}

/// FNV-1a over the lowercased name.
///
/// Any stable hash would do; what matters is that it is computed from the name
/// and not from a position in a list, so someone's colour does not change when
/// a different teammate commits.
fn name_hash(name: &str) -> u32 {
    let mut hash: u32 = 2166136261;
    for byte in name.as_bytes() {
        hash ^= u32::from(byte.to_ascii_lowercase());
        hash = hash.wrapping_mul(16777619);
    }
    hash
}

/// The time to draw above a message, given the one before it.
///
/// Just the clock time within a day, and the date as well when the day changes.
/// Both are slices of the stored RFC 3339 string, so this needs no date library
/// and cannot disagree with what was written down.
fn separator_stamp(at: &str, previous_minute: Option<&str>) -> String {
    let day = at.get(..10).unwrap_or_default();
    let time = short_time(at);
    let same_day = previous_minute.and_then(|previous| previous.get(..10)) == Some(day);
    if same_day || day.is_empty() {
        time
    } else {
        format!("{day} · {time}")
    }
}

/// `2026-08-29T15:04:05Z` as `15:04`.
///
/// Anything that does not parse is shown whole rather than guessed at.
fn short_time(timestamp: &str) -> String {
    timestamp
        .split('T')
        .nth(1)
        .and_then(|time| time.get(..5))
        .unwrap_or(timestamp)
        .to_string()
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
                    .when(filter == Filter::Messages, |this| {
                        this.child(self.render_chat_header(cx))
                    })
                    .when(filter != Filter::Messages, |this| this.child(
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
                    ))
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
                                        // An untouched composer does not follow
                                        // you to another tab, where it would be
                                        // invisible but still open. One with
                                        // text in it does, because discarding
                                        // what someone typed to tidy up the UI
                                        // is worse than the stray composer.
                                        if this.draft_is_empty(cx) {
                                            this.drafting = None;
                                            this.replying_to = None;
                                        }
                                        cx.notify();
                                    }))
                            })),
                    ),
            )
            .map(|this| {
                // A conversation is read in order and answered at the end; a
                // list of tickets is scanned. Same records, two shapes, because
                // one shape cannot do both jobs well.
                if filter == Filter::Messages {
                    return this.child(self.render_chat(&records, cx));
                }
                this.child(
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
                                .key_context("TeamNotesDraft")
                                .p_2()
                                .gap_1p5()
                                .rounded_md()
                                .border_1()
                                .border_color(cx.theme().colors().border_focused)
                                .on_action(cx.listener(|this, _: &CancelDraft, window, cx| {
                                    this.cancel_draft(window, cx)
                                }))
                                .child(
                                    Label::new(where_to)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .child(composer.clone())
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .child(
                                            Button::new("submit-draft", "Add")
                                                .label_size(LabelSize::Small)
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.submit_draft(window, cx)
                                                })),
                                        )
                                        .child(
                                            Button::new("cancel-draft", "Cancel")
                                                .label_size(LabelSize::Small)
                                                .color(Color::Muted)
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.cancel_draft(window, cx)
                                                })),
                                        ),
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
            })
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
    fn a_name_becomes_one_or_two_initials() {
        assert_eq!(initials("Drew Wycherley"), "DW");
        assert_eq!(initials("procoder30001"), "P");
        assert_eq!(initials("ada lovelace king"), "AK");
        assert_eq!(initials("  "), "?");
        assert_eq!(initials(""), "?");
        // Not every contributor's name is ASCII.
        assert_eq!(initials("Ólafur Jónsson"), "ÓJ");
    }

    #[test]
    fn a_persons_colour_does_not_depend_on_who_else_committed() {
        // The hash is over the name alone, so adding a teammate cannot shuffle
        // everyone else's avatar colour.
        assert_eq!(name_hash("Drew"), name_hash("Drew"));
        assert_eq!(name_hash("Drew"), name_hash("drew"));
        assert_ne!(name_hash("Drew"), name_hash("Ada"));
    }

    #[test]
    fn the_time_separator_adds_the_date_only_when_the_day_changes() {
        assert_eq!(
            separator_stamp("2026-08-29T15:04:05Z", Some("2026-08-29T14:31")),
            "15:04"
        );
        assert_eq!(
            separator_stamp("2026-08-30T09:12:00Z", Some("2026-08-29T23:58")),
            "2026-08-30 · 09:12"
        );
        // The first message of a conversation has nothing before it.
        assert_eq!(
            separator_stamp("2026-08-29T15:04:05Z", None),
            "2026-08-29 · 15:04"
        );
    }

    #[test]
    fn a_timestamp_that_makes_no_sense_is_shown_rather_than_guessed_at() {
        assert_eq!(short_time("not a timestamp"), "not a timestamp");
        assert_eq!(separator_stamp("", None), "");
    }

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
