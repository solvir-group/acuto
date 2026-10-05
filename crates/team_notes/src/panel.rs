//! The panel: notes on lines, tickets for work, both carried by the repository.

use std::{path::PathBuf, rc::Rc, sync::Arc};

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
    Anchor, Kind, Message, NOTES_PATH, NoteThread, Status, author_name,
    chat_completions::ChatCompletionProvider, notes_file, resolve_anchor, team_roster,
};

/// The widest the chat composer grows. A message box as wide as a maximised
/// window sends the caret on a long trip for every line.
const CHAT_COMPOSER_MAX_WIDTH: f32 = 440.;

/// What the chat box says before you type.
const CHAT_PLACEHOLDER: &str = "Message the team \u{2014} @ to mention, # for a ticket";

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
            // Opens as a centre-pane tab: this panel is registered with the
            // workspace but not docked, so there is no dock focus to toggle.
            workspace.open_panel_as_tab::<TeamNotesPanel>(window, cx);
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
    /// The latest queued write. Each write waits for the one before it, so
    /// two quick changes reach the file in the order they were made.
    _write: Option<Task<()>>,
    /// The repository the records belong to: the one holding the file you are
    /// working in. See [`Self::preferred_root`].
    root: Option<PathBuf>,
    _subscriptions: Vec<Subscription>,
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
        // The repository of the file in front, as `preferred_root` would pick
        // later; read from `workspace` directly because this runs while the
        // workspace is being updated and cannot be read through its handle.
        let active_worktree = workspace
            .active_item(cx)
            .and_then(|item| item.project_path(cx))
            .and_then(|path| project.read(cx).worktree_for_id(path.worktree_id, cx))
            .filter(|worktree| worktree.read(cx).is_visible());
        let root = active_worktree
            .or_else(|| project.read(cx).visible_worktrees(cx).next())
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf());

        // Follows the repository you are in. A panel that read the first folder
        // once, when the window opened, showed an empty chat for a folder opened
        // later and the wrong repository's chat in a window with two.
        let mut subscriptions = vec![cx.subscribe(&project, |this, _, event, cx| {
            match event {
                project::Event::WorktreeAdded(_)
                | project::Event::WorktreeRemoved(_)
                | project::Event::WorktreeOrderChanged => this.refresh_root(cx),
                // A teammate's messages arrive with a pull; the worktree reports
                // the notes file changing, which is the cue to read it again.
                project::Event::WorktreeUpdatedEntries(_, changes) => {
                    if changes
                        .iter()
                        .any(|(path, _, _)| path.as_unix_str() == NOTES_PATH)
                    {
                        this.reload(cx);
                    }
                }
                _ => {}
            }
        })];
        if let Some(workspace_entity) = workspace.weak_handle().upgrade() {
            subscriptions.push(cx.subscribe(
                &workspace_entity,
                |this, _, event: &workspace::Event, cx| {
                    if matches!(event, workspace::Event::ActiveItemChanged) {
                        this.refresh_root(cx);
                    }
                },
            ));
        }

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
            _write: None,
            root,
            _subscriptions: subscriptions,
        };

        this.load_team(cx);
        this.reload(cx);
        this
    }

    pub(crate) fn records(&self) -> &[NoteThread] {
        &self.records
    }

    pub(crate) fn team(&self) -> &[Arc<str>] {
        &self.team
    }

    /// The repository the records should come from.
    ///
    /// The worktree holding the active file; failing that the one already in
    /// use, as long as it is still open; failing that the first. Only a file
    /// moves it, so clicking into this panel -- which has no file -- does not
    /// flip a two-repository window back to the first repository.
    fn preferred_root(&self, cx: &App) -> Option<PathBuf> {
        let project = self.project.read(cx);
        let active_worktree = self
            .workspace
            .upgrade()
            .and_then(|workspace| workspace.read(cx).active_item(cx))
            .and_then(|item| item.project_path(cx))
            .and_then(|path| project.worktree_for_id(path.worktree_id, cx))
            .filter(|worktree| worktree.read(cx).is_visible());
        if let Some(worktree) = active_worktree {
            return Some(worktree.read(cx).abs_path().to_path_buf());
        }
        let still_open = self.root.as_ref().is_some_and(|root| {
            project
                .visible_worktrees(cx)
                .any(|worktree| worktree.read(cx).abs_path().as_ref() == root.as_path())
        });
        if still_open {
            return self.root.clone();
        }
        project
            .visible_worktrees(cx)
            .next()
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
    }

    fn refresh_root(&mut self, cx: &mut Context<Self>) {
        let root = self.preferred_root(cx);
        if root == self.root {
            return;
        }
        // Held while something is being written: it belongs to the repository
        // it was started in, and switching now would send it to another one.
        // The switch happens once it has been sent or dropped.
        let still_open = self.root.as_ref().is_some_and(|root| {
            self.project
                .read(cx)
                .visible_worktrees(cx)
                .any(|worktree| worktree.read(cx).abs_path().as_ref() == root.as_path())
        });
        let writing = !self.draft_is_empty(cx)
            || self
                .replying_to
                .as_ref()
                .is_some_and(|(_, composer)| !composer.read(cx).text(cx).trim().is_empty());
        if writing && still_open {
            return;
        }
        log::info!(
            "team notes: now showing {}",
            root.as_deref()
                .map(|root| root.display().to_string())
                .unwrap_or_else(|| "no repository".into())
        );
        self.root = root;
        self.records.clear();
        self.team.clear();
        self.load_team(cx);
        self.reload(cx);
        cx.notify();
    }

    /// Who you are and who else works on this repository.
    fn load_team(&mut self, cx: &mut Context<Self>) {
        let root = self.root.clone();
        cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let author = author_name(&executor).await;
            let mut team = match root.clone() {
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
                // Only for the repository still showing: a slow roster for the
                // previous one would otherwise land on this one.
                if this.root == root {
                    this.team = team;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The worktree the file belongs to.
    ///
    /// The first visible worktree. A window with several of them keeps its
    /// records with the first, because they have to live in exactly one
    /// repository and splitting them would make "where is this" unanswerable.
    fn worktree_root(&self, _cx: &App) -> Option<PathBuf> {
        self.root.clone()
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.worktree_root(cx) else {
            return;
        };
        let fs = self.fs.clone();

        self._reload = Some(cx.spawn(async move |this, cx| {
            let path = notes_file(&root);
            // Missing is empty: a repository with no notes yet. A file that is
            // there but cannot be read -- mid-checkout, locked by another
            // program -- keeps what is showing. Treating that as empty would
            // write the empty list back on the next change and delete every
            // note in the repository.
            let contents = match fs.load(&path).await {
                Ok(contents) => contents,
                Err(error) => {
                    if fs.is_file(&path).await {
                        log::warn!("team notes: could not read {}: {error:#}", path.display());
                        return;
                    }
                    String::new()
                }
            };
            let records = crate::parse(&contents);
            this.update(cx, |this, cx| {
                // Dropped if the repository changed while this was reading.
                if this.root.as_deref() == Some(root.as_path()) {
                    this.records = records;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// Shows `records` at once and writes the records that changed to disk.
    ///
    /// Only the changed records are written, merged into the file as it is
    /// on disk at the time of writing. Writing the whole in-memory list
    /// replaced the file with this window's view of it, deleting whatever a
    /// pull or another window had added in the meantime.
    fn commit(&mut self, records: Vec<NoteThread>, cx: &mut Context<Self>) {
        let changed = records
            .iter()
            .filter(|record| !self.records.contains(record))
            .cloned()
            .collect::<Vec<_>>();
        self.records = records;
        cx.notify();

        if changed.is_empty() {
            return;
        }
        let Some(root) = self.worktree_root(cx) else {
            return;
        };
        let fs = self.fs.clone();
        let previous = self._write.take();

        self._write = Some(cx.spawn(async move |this, cx| {
            if let Some(previous) = previous {
                previous.await;
            }
            let path = notes_file(&root);
            let current = match fs.load(&path).await {
                Ok(current) => current,
                Err(error) => {
                    // Unreadable is not empty: writing now would replace every
                    // record in the file with this window's changes alone.
                    if fs.is_file(&path).await {
                        log::error!(
                            "team notes: could not read {} to save a change: {error:#}",
                            path.display()
                        );
                        return;
                    }
                    String::new()
                }
            };
            let mut merged = crate::parse(&current);
            for record in changed {
                match merged.iter_mut().find(|existing| existing.id == record.id) {
                    Some(existing) => *existing = record,
                    None => merged.push(record),
                }
            }
            let Some(contents) = crate::render(&merged).log_err() else {
                return;
            };
            if let Some(parent) = path.parent() {
                fs.create_dir(parent).await.log_err();
            }
            if fs.atomic_write(path, contents).await.log_err().is_none() {
                return;
            }
            this.update(cx, |this, cx| {
                if this.root.as_deref() == Some(root.as_path()) {
                    merged.sort_by(|left, right| left.id.cmp(&right.id));
                    this.records = merged;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn open_composer(
        &mut self,
        draft: Draft,
        placeholder: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let placeholder = placeholder.to_string();
        let panel = cx.weak_entity();
        let composer = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 8, window, cx);
            editor.set_placeholder_text(placeholder.as_str(), window, cx);
            enable_suggestions(&mut editor, panel);
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
        log::info!("team notes: add note requested");
        // The editor in front, or failing that the one used last. Pressing the
        // note button in the team tab makes the tab the active item, so asking
        // only for the active item found no editor and the button did nothing.
        let Some(editor) = workspace
            .active_item(cx)
            .and_then(|item| item.act_as::<Editor>(cx))
            .or_else(|| last_used_editor(workspace, cx))
        else {
            log::info!("team notes: no editor to attach a note to");
            return;
        };
        let Some(panel) = workspace.panel::<TeamNotesPanel>(cx) else {
            log::warn!("team notes: the team panel is not registered in this workspace");
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
            log::warn!("team notes: the last editor has no file on disk to anchor a note to");
            return;
        };

        log::info!("team notes: opening a note on {file}:{}", anchor.line + 1);
        // A tab, not a dock: `open_panel` only knows docks, so it silently
        // did nothing and the composer opened somewhere nobody could see.
        workspace.open_panel_as_tab::<TeamNotesPanel>(window, cx);
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
        // A tab, not a dock: `open_panel` only knows docks, so it silently
        // did nothing and the composer opened somewhere nobody could see.
        workspace.open_panel_as_tab::<TeamNotesPanel>(window, cx);
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
        self.hand_to_agents(&record, &said_in(&record), window, cx);
        records.push(record);
        self.commit(records, cx);
        self.focus_handle.focus(window, cx);
    }

    /// Sends the record to every agent `said` mentions.
    ///
    /// Dispatched by name because the agent panel's crate depends on this one
    /// for ticket mentions, so this one cannot depend on it back.
    fn hand_to_agents(
        &self,
        record: &NoteThread,
        said: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Tickets named with `#ref` go with the message, so "@claude do #e8e265"
        // hands over the ticket and not just the six characters naming it.
        let referenced: Vec<NoteThread> = crate::referenced_records(said, &self.records)
            .into_iter()
            .filter(|ticket| ticket.id != record.id)
            .cloned()
            .collect();
        for agent in crate::mentioned_agents(said) {
            let mut prompt = crate::prompt_for_agent(record, said);
            for ticket in &referenced {
                prompt.push_str("\n\n");
                prompt.push_str(&crate::brief_for_agent(ticket));
            }
            match cx.build_action(
                "agent::AskAgent",
                Some(serde_json::json!({ "agent": agent, "prompt": prompt })),
            ) {
                Ok(action) => window.dispatch_action(action, cx),
                Err(error) => log::warn!("team notes: could not hand {agent} the record: {error}"),
            }
        }
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
                body: body.clone(),
            });
            // Replying `@claude can you take this` on a ticket hands it over,
            // with the reply as the ask and the ticket as the brief.
            let record = record.clone();
            self.hand_to_agents(&record, &body, window, cx);
        }
        self.commit(records, cx);
        self.focus_handle.focus(window, cx);
    }

    fn start_reply(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let panel = cx.weak_entity();
        let composer = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 8, window, cx);
            editor.set_placeholder_text("Reply…", window, cx);
            enable_suggestions(&mut editor, panel);
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
                Kind::Ticket => record.status = if finished { Status::Open } else { Status::Done },
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
        // In this repository. Looking the relative path up across the whole
        // project found the first worktree with a file of that name, which in
        // a window with two repositories was often the other one.
        let project = self.project.read(cx);
        let in_this_repository = self.root.as_ref().and_then(|root| {
            let worktree = project
                .visible_worktrees(cx)
                .find(|worktree| worktree.read(cx).abs_path().as_ref() == root.as_path())?;
            let path = util::rel_path::RelPath::from_unix_str(&file)
                .ok()?
                .into_arc();
            Some(project::ProjectPath {
                worktree_id: worktree.read(cx).id(),
                path,
            })
        });
        let Some(project_path) = in_this_repository
            .or_else(|| project.find_project_path(std::path::Path::new(&file), cx))
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

    fn render_record(
        &self,
        record: &NoteThread,
        now: chrono::DateTime<chrono::Utc>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let id = record.id.clone();
        let replying = self
            .replying_to
            .as_ref()
            .is_some_and(|(open, _)| *open == record.id);
        let closed = record.is_closed();
        let is_ticket = record.kind == Kind::Ticket;
        let (status_label, status_color) = status_presentation(record);
        let colors = cx.theme().colors();

        let location = record.file.as_ref().map(|file| {
            let line = record.anchor.as_ref().map_or(0, |anchor| anchor.line);
            format!("{file}:{}", line + 1)
        });
        let mine = record
            .assignee
            .as_deref()
            .is_some_and(|name| name.eq_ignore_ascii_case(&self.author));
        let opener = record.messages.first();
        let group = SharedString::from(format!("record-group-{}", record.id));

        let (kind_icon, kind_color) = match record.kind {
            Kind::Ticket => (IconName::ListTodo, Color::Accent),
            Kind::Note => (IconName::Pin, Color::Warning),
            Kind::Message => (IconName::Chat, Color::Muted),
        };

        let separator = || {
            Label::new("·")
                .size(LabelSize::XSmall)
                .color(Color::Disabled)
        };

        // The status is the control, not a label beside one: there is one
        // thing you do to a record and this is it.
        let status_pill = h_flex()
            .id(SharedString::from(format!("status-{}", record.id)))
            .flex_none()
            .gap_1()
            .px_1p5()
            .py_px()
            .rounded_full()
            .border_1()
            .border_color(colors.border_variant)
            .cursor_pointer()
            .hover(|style| style.bg(colors.element_hover))
            .child(div().size_1p5().rounded_full().bg(status_color.color(cx)))
            .child(
                Label::new(status_label)
                    .size(LabelSize::XSmall)
                    .color(status_color),
            )
            .tooltip(Tooltip::text(if is_ticket {
                "Move to the next status"
            } else {
                "Resolve or reopen"
            }))
            .on_click(cx.listener({
                let id = id.clone();
                move |this, _, _window, cx| this.advance(&id, cx)
            }));

        let meta = h_flex()
            .w_full()
            .min_w_0()
            .gap_1()
            .child(
                Label::new(format!("#{}", crate::short_ref(&record.id)))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .buffer_font(cx),
            )
            .when_some(opener, |this, opener| {
                this.child(separator())
                    .child(
                        Label::new(opener.author.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(separator())
                    .child(
                        Label::new(age(&opener.at, now))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
            })
            .when_some(location, |this, location| {
                this.child(separator()).child(
                    Button::new(SharedString::from(format!("open-{}", record.id)), location)
                        .label_size(LabelSize::XSmall)
                        .color(Color::Accent)
                        .truncate(true)
                        .tooltip(Tooltip::text("Go to this line"))
                        .on_click(cx.listener({
                            let id = id.clone();
                            move |this, _, window, cx| this.jump_to(id.clone(), window, cx)
                        })),
                )
            });

        let replies = record.messages.iter().skip(1).map(|message| {
            h_flex()
                .w_full()
                .items_start()
                .gap_2()
                .child(initials_avatar(&message.author, px(18.), None, cx))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(
                            h_flex()
                                .gap_1()
                                .child(Label::new(message.author.clone()).size(LabelSize::XSmall))
                                .child(
                                    Label::new(age(&message.at, now))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                ),
                        )
                        .child(Label::new(message.body.clone()).size(LabelSize::Small)),
                )
        });
        let reply_count = record.messages.len().saturating_sub(1);

        v_flex()
            .id(SharedString::from(record.id.clone()))
            .group(group.clone())
            .w_full()
            .p_2()
            .gap_1p5()
            .rounded_lg()
            .border_1()
            .border_color(if mine && !closed {
                colors.border_focused
            } else {
                colors.border_variant
            })
            .bg(colors.elevated_surface_background)
            .hover(|style| style.border_color(colors.border))
            .when(closed, |this| this.opacity(0.6))
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .items_start()
                    .gap_2()
                    .child(
                        h_flex()
                            .flex_none()
                            .size_6()
                            .justify_center()
                            .rounded_md()
                            .bg(colors.element_background)
                            .child(Icon::new(kind_icon).size(IconSize::Small).color(kind_color)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                div().w_full().min_w_0().child(
                                    Label::new(record.headline().to_string())
                                        .size(LabelSize::Small)
                                        .weight(gpui::FontWeight::MEDIUM)
                                        .when(closed, |label| label.strikethrough()),
                                ),
                            )
                            .child(meta),
                    )
                    .child(status_pill),
            )
            .when(reply_count > 0, |this| {
                this.child(
                    v_flex()
                        .w_full()
                        .gap_2()
                        .ml_8()
                        .pl_2()
                        .border_l_1()
                        .border_color(colors.border_variant)
                        .children(replies),
                )
            })
            .when(replying, |this| {
                this.child(
                    v_flex()
                        .ml_8()
                        .gap_1p5()
                        .p_1p5()
                        .rounded_md()
                        .border_1()
                        .border_color(colors.border_focused)
                        .bg(colors.editor_background)
                        .children(
                            self.replying_to
                                .as_ref()
                                .map(|(_, composer)| composer.clone()),
                        )
                        .child(
                            h_flex()
                                .justify_end()
                                .gap_1()
                                .child(
                                    Button::new(
                                        SharedString::from(format!("cancel-reply-{}", record.id)),
                                        "Cancel",
                                    )
                                    .label_size(LabelSize::Small)
                                    .color(Color::Muted)
                                    .on_click(cx.listener(
                                        |this, _, _window, cx| {
                                            this.replying_to = None;
                                            cx.notify();
                                        },
                                    )),
                                )
                                .child(
                                    Button::new(
                                        SharedString::from(format!("send-{}", record.id)),
                                        "Reply",
                                    )
                                    .style(ButtonStyle::Filled)
                                    .label_size(LabelSize::Small)
                                    .on_click(cx.listener(
                                        |this, _, window, cx| this.submit_reply(window, cx),
                                    )),
                                ),
                        ),
                )
            })
            .child(
                h_flex()
                    .w_full()
                    .ml_8()
                    .gap_1()
                    .when_some(record.assignee.clone(), |this, assignee| {
                        this.child(
                            h_flex()
                                .gap_1()
                                .child(initials_avatar(&assignee, px(16.), None, cx))
                                .child(
                                    Label::new(if mine {
                                        "You".to_string()
                                    } else {
                                        assignee.clone()
                                    })
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                                ),
                        )
                    })
                    .when(reply_count > 0 && !replying, |this| {
                        this.child(
                            h_flex()
                                .gap_0p5()
                                .child(
                                    Icon::new(IconName::Chat)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Label::new(reply_count.to_string())
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                ),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        h_flex()
                            .gap_0p5()
                            .when(!replying, |this| this.visible_on_hover(group.clone()))
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
                                        move |this, _, window, cx| {
                                            this.start_reply(id.clone(), window, cx)
                                        }
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
                            // Done in one click, wherever the record is. The
                            // status pill cycles, which takes two clicks to
                            // finish a ticket that has not been started.
                            .child(
                                Button::new(
                                    SharedString::from(format!("complete-{}", record.id)),
                                    if closed { "Reopen" } else { "Done" },
                                )
                                .start_icon(
                                    Icon::new(if closed {
                                        IconName::RotateCcw
                                    } else {
                                        IconName::Check
                                    })
                                    .size(IconSize::XSmall),
                                )
                                .label_size(LabelSize::XSmall)
                                .color(if closed { Color::Muted } else { Color::Success })
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
                    ),
            )
            .into_any_element()
    }
}

/// The row `offset` falls on, and that row's text.
/// `#ticket` and `@name` suggestions while typing.
///
/// Forced on for these editors: Acuto turns suggestions-while-typing off for
/// code, and a chat where `#` offers nothing is one where tickets cannot be
/// named at all.
fn enable_suggestions(editor: &mut Editor, panel: WeakEntity<TeamNotesPanel>) {
    editor.set_completion_provider(Some(Rc::new(ChatCompletionProvider::new(panel))));
    editor.set_show_completions_on_input(Some(true));
}

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
                    .children(
                        self.team
                            .iter()
                            .take(STACK)
                            .enumerate()
                            .map(|(index, member)| {
                                div()
                                    // Overlapped rather than spaced: a stack reads
                                    // as one group, a row reads as a list.
                                    .when(index > 0, |this| this.ml(px(-8.)))
                                    .child(initials_avatar(member, px(24.), Some(ring), cx))
                            }),
                    ),
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
        let mut previous_at: Option<String> = None;
        for record in &ordered {
            for message in &record.messages {
                let mine = message.author.eq_ignore_ascii_case(&me);
                let starts_run = previous_author
                    .as_deref()
                    .is_none_or(|author| !author.eq_ignore_ascii_case(&message.author));

                // A time only after a real silence. Stamping every message that
                // fell in a different minute puts a row of text between two
                // things typed twenty seconds apart, which is most of the space
                // between messages and none of the information.
                let at = local_time(&message.at);
                let stamp = match previous_at.as_deref() {
                    None => Some(separator_stamp(&at, None)),
                    Some(previous) => match minutes_apart(previous, &at) {
                        Some(gap) if gap < STAMP_AFTER_MINUTES => None,
                        _ => Some(separator_stamp(&at, Some(previous))),
                    },
                };

                previous_author = Some(message.author.clone());
                previous_at = Some(at);
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
        let on_sent = readable_on(sent_background);
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
                            px(10.)
                        } else if bubble.starts_run {
                            px(8.)
                        } else {
                            // Enough to see the seam between two bubbles and no
                            // more. At zero the backgrounds merge into one
                            // block and you cannot tell where a message ended.
                            px(2.)
                        };

                        v_flex()
                            .w_full()
                            .mt(space_above)
                            .gap_0p5()
                            .when_some(bubble.stamp, |this, stamp| {
                                this.child(
                                    Label::new(stamp)
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
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
                                                    .child(self.render_message_body(
                                                        &bubble.message.body,
                                                        bubble.mine.then_some(on_sent),
                                                        index,
                                                        cx,
                                                    )),
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
                                        Label::new(format!(
                                            "Sent · {}",
                                            short_time(&local_time(&at))
                                        ))
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

    /// A message's text, with each `#ref` naming a ticket drawn as a chip.
    ///
    /// Clicking a chip opens the tickets, so a reference can be followed rather
    /// than copied into a search box.
    fn render_message_body(
        &self,
        body: &str,
        text_color: Option<gpui::Hsla>,
        index: usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let color = |label: Label| match text_color {
            Some(color) => label.color(Color::Custom(color)),
            None => label,
        };
        if crate::referenced_records(body, &self.records).is_empty() {
            return color(Label::new(body.to_string()).size(LabelSize::Small)).into_any_element();
        }

        // Line by line, and within a line only the references are replaced,
        // so a pasted snippet or a list keeps its line breaks and spacing.
        let mut lines = Vec::new();
        let mut chip_index = 0;
        for line in body.split('\n') {
            let mut pieces = Vec::new();
            let mut cursor = 0;
            for span in crate::ref_spans(line) {
                let ticket = crate::referenced_records(&line[span.clone()], &self.records)
                    .into_iter()
                    .next();
                let Some(ticket) = ticket else {
                    continue;
                };
                if span.start > cursor {
                    pieces.push(
                        color(
                            Label::new(line[cursor..span.start].to_string()).size(LabelSize::Small),
                        )
                        .into_any_element(),
                    );
                }
                cursor = span.end;
                chip_index += 1;
                pieces.push(
                    h_flex()
                        .id(("ticket-ref", index * 1000 + chip_index))
                        .px_1()
                        .rounded_sm()
                        .bg(cx.theme().colors().element_background)
                        .border_1()
                        .border_color(cx.theme().colors().border_variant)
                        .cursor_pointer()
                        .hover(|style| style.bg(cx.theme().colors().element_hover))
                        .child(
                            Label::new(format!(
                                "#{} {}",
                                crate::short_ref(&ticket.id),
                                ticket.headline()
                            ))
                            .size(LabelSize::Small)
                            .color(Color::Default),
                        )
                        .tooltip(Tooltip::text(format!("Ticket · {}", ticket.status.label())))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.filter = Filter::Tickets;
                            cx.notify();
                        }))
                        .into_any_element(),
                );
            }
            if cursor < line.len() || pieces.is_empty() {
                // An empty line keeps its height with a single space.
                let rest = if cursor < line.len() {
                    &line[cursor..]
                } else {
                    " "
                };
                pieces.push(
                    color(Label::new(rest.to_string()).size(LabelSize::Small)).into_any_element(),
                );
            }
            lines.push(h_flex().flex_wrap().items_center().children(pieces));
        }

        v_flex().children(lines).into_any_element()
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
        let on_accent = readable_on(accent);

        h_flex()
            .flex_none()
            .w_full()
            .justify_center()
            .px_2()
            .pt_1()
            .pb_2()
            .child(
                h_flex()
                    .key_context("TeamNotesComposer")
                    .w_full()
                    .max_w(px(CHAT_COMPOSER_MAX_WIDTH))
                    .items_end()
                    .gap_1()
                    .p_1()
                    .rounded_2xl()
                    .bg(cx.theme().colors().editor_background)
                    .border_1()
                    // Neutral in both states. A focus ring in the accent colour
                    // competes with the sent bubbles and the send button, which
                    // are the two things in this panel that have earned the
                    // colour; the border only has to say where the box is.
                    .border_color(if composing {
                        cx.theme().colors().border
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
                                    Label::new(CHAT_PLACEHOLDER)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .truncate(),
                                ),
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                if !matches!(this.drafting, Some((Draft::Message, _))) {
                                    this.open_composer(
                                        Draft::Message,
                                        CHAT_PLACEHOLDER,
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
                            .child(Icon::new(IconName::Send).size(IconSize::XSmall).color(
                                Color::Custom(if ready {
                                    on_accent
                                } else {
                                    gpui::Hsla {
                                        a: 0.5,
                                        ..on_accent
                                    }
                                }),
                            ))
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
        self.refresh_root(cx);
        if self.drafting.is_none() {
            self.open_composer(Draft::Message, CHAT_PLACEHOLDER, window, cx);
        }
    }

    /// Drops the path of the file you are looking at into the message.
    ///
    /// The one thing a chat inside an editor can do that a chat beside it
    /// cannot: say which file you mean without alt-tabbing to find out.
    fn attach_current_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.drafting, Some((Draft::Message, _))) {
            self.open_composer(Draft::Message, CHAT_PLACEHOLDER, window, cx);
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
    let background = avatar_color(name, cx);

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
                .color(Color::Custom(readable_on(background))),
        )
        .into_any_element()
}

/// A person's colour: a hue from their name, at a strength suited to the theme.
///
/// Not the theme's player colours. Acuto's themes define a single player, so
/// every teammate came out the same colour, and in a dark theme that colour is
/// white. A hue per name tells people apart; the lightness is set per
/// appearance so the initials on it always read.
fn avatar_color(name: &str, cx: &App) -> gpui::Hsla {
    let hue = (name_hash(name) % 360) as f32 / 360.;
    if cx.theme().appearance().is_light() {
        gpui::hsla(hue, 0.42, 0.44, 1.)
    } else {
        gpui::hsla(hue, 0.45, 0.70, 1.)
    }
}

/// How long ago `at` was, in the few characters a list row can spare: `now`,
/// `5m`, `3h`, `2d`, and the date beyond a week. A stamp that does not parse
/// is shown as its clock time rather than guessed at.
fn age(at: &str, now: chrono::DateTime<chrono::Utc>) -> String {
    let Ok(stamp) = chrono::DateTime::parse_from_rfc3339(at) else {
        return short_time(at);
    };
    let minutes = now
        .signed_duration_since(stamp.with_timezone(&chrono::Utc))
        .num_minutes();
    const HOUR: i64 = 60;
    const DAY: i64 = 24 * HOUR;
    match minutes {
        minutes if minutes < 1 => "now".to_string(),
        minutes if minutes < HOUR => format!("{minutes}m"),
        minutes if minutes < DAY => format!("{}h", minutes / HOUR),
        minutes if minutes < 7 * DAY => format!("{}d", minutes / DAY),
        _ => stamp
            .with_timezone(&chrono::Local)
            .format("%b %-d")
            .to_string(),
    }
}

/// What a record's status pill says, and in what colour.
fn status_presentation(record: &NoteThread) -> (&'static str, Color) {
    match record.kind {
        Kind::Ticket => match record.status {
            Status::Open => ("Open", Color::Muted),
            Status::InProgress => ("In Progress", Color::Accent),
            Status::Done => ("Done", Color::Success),
        },
        Kind::Note | Kind::Message if record.resolved => ("Resolved", Color::Success),
        Kind::Note | Kind::Message => ("Open", Color::Muted),
    }
}

/// Whichever of white and near-black reads against `background`.
///
/// The local player's cursor colour is light in several dark themes, and fixed
/// white text on it was white on white.
fn readable_on(background: gpui::Hsla) -> gpui::Hsla {
    if background.l > 0.6 {
        gpui::hsla(0., 0., 0.08, 1.)
    } else {
        gpui::hsla(0., 0., 1., 1.)
    }
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

/// How long a silence has to be before it is worth putting a time on it.
///
/// Below this, two messages are one thought and a timestamp between them is
/// noise occupying a whole row.
const STAMP_AFTER_MINUTES: i64 = 10;

/// Minutes from `earlier` to `later`, when both fall on the same day.
///
/// `None` when the day differs, or when either stamp is not the shape this
/// writes -- both of which the caller treats as far enough apart to label.
/// Deliberately no date library: these are slices of the RFC 3339 string that
/// was written to the file, so this cannot disagree with what is stored.
fn minutes_apart(earlier: &str, later: &str) -> Option<i64> {
    if earlier.get(..10)? != later.get(..10)? {
        return None;
    }
    Some(clock_minutes(later)? - clock_minutes(earlier)?)
}

/// `2026-08-29T08:01:42Z` as minutes since midnight.
fn clock_minutes(at: &str) -> Option<i64> {
    let hours: i64 = at.get(11..13)?.parse().ok()?;
    let minutes: i64 = at.get(14..16)?.parse().ok()?;
    Some(hours * 60 + minutes)
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

/// A stored RFC 3339 stamp in this machine's time zone, in the same shape.
///
/// Stamps are written in UTC so teammates in different zones agree on order;
/// shown as written, everyone outside UTC read the wrong time of day. Anything
/// that does not parse is passed through to be shown as is.
fn local_time(at: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(at)
        .map(|stamp| {
            stamp
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|_| at.to_string())
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

impl TeamNotesPanel {
    /// The panel's title row: what this is and which repository it is for.
    fn render_title(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let repository = self
            .root
            .as_ref()
            .and_then(|root| root.file_name())
            .map(|name| name.to_string_lossy().into_owned());

        h_flex()
            .w_full()
            .gap_1p5()
            .child(
                Icon::new(IconName::UserGroup)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .child(Label::new("Team").weight(gpui::FontWeight::SEMIBOLD))
            .when_some(repository, |this, repository| {
                this.child(
                    Label::new(repository)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                )
            })
            .child(div().flex_1())
            .child(
                IconButton::new("reload", IconName::ArrowCircle)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::text("Re-read from the repository"))
                    .on_click(cx.listener(|this, _, _window, cx| this.reload(cx))),
            )
            .into_any_element()
    }

    /// The tabs, each with the number of open records it holds.
    fn render_tabs(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let filter = self.filter;
        let me = self.author.to_string();
        let open_count = |option: Filter| {
            self.records
                .iter()
                .filter(|record| !record.is_closed() && option.admits(record, &me))
                .count()
        };

        h_flex()
            .w_full()
            .p_0p5()
            .gap_0p5()
            .rounded_md()
            .bg(cx.theme().colors().element_background)
            .children(Filter::ALL.into_iter().map(|option| {
                let is_selected = option == filter;
                // Counts only where a number answers a question: what is
                // waiting on me, and what is still to do. A count of every
                // message ever sent answers nothing.
                let count = match option {
                    Filter::Inbox | Filter::Tickets | Filter::Notes => open_count(option),
                    Filter::Messages | Filter::All => 0,
                };
                h_flex()
                    .id(SharedString::from(option.label()))
                    .flex_1()
                    .justify_center()
                    .gap_1()
                    .py_0p5()
                    .rounded_sm()
                    .cursor_pointer()
                    .when(is_selected, |this| {
                        this.bg(cx.theme().colors().elevated_surface_background)
                            .shadow_xs()
                    })
                    .when(!is_selected, |this| {
                        this.hover(|style| style.bg(cx.theme().colors().element_hover))
                    })
                    .child(Label::new(option.label()).size(LabelSize::XSmall).color(
                        if is_selected {
                            Color::Default
                        } else {
                            Color::Muted
                        },
                    ))
                    .when(count > 0, |this| {
                        this.child(
                            h_flex()
                                .px_1()
                                .rounded_full()
                                .bg(if option == Filter::Inbox {
                                    cx.theme().status().info_background
                                } else {
                                    cx.theme().colors().element_selected
                                })
                                .child(
                                    Label::new(count.to_string()).size(LabelSize::XSmall).color(
                                        if option == Filter::Inbox {
                                            Color::Info
                                        } else {
                                            Color::Muted
                                        },
                                    ),
                                ),
                        )
                    })
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.filter = option;
                        // An untouched composer does not follow you to another
                        // tab, where it would be invisible but still open. One
                        // with text in it does, because discarding what
                        // someone typed to tidy up the UI is worse than the
                        // stray composer.
                        if this.draft_is_empty(cx) {
                            this.drafting = None;
                            this.replying_to = None;
                        }
                        cx.notify();
                    }))
            }))
            .into_any_element()
    }

    /// The ways to add something, the same on every list tab.
    fn render_toolbar(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let show_closed = self.show_closed;
        h_flex()
            .w_full()
            .gap_1()
            .child(
                Button::new("new-ticket", "Ticket")
                    .start_icon(Icon::new(IconName::Plus).size(IconSize::XSmall))
                    .style(ButtonStyle::Filled)
                    .label_size(LabelSize::Small)
                    .tooltip(Tooltip::text("New ticket"))
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
                Button::new("new-note", "Note")
                    .start_icon(Icon::new(IconName::Pin).size(IconSize::XSmall))
                    .label_size(LabelSize::Small)
                    .tooltip(Tooltip::text("Add a note on the line your cursor is on"))
                    // Called directly rather than by dispatching `AddNote`: an
                    // action only reaches the workspace when focus is inside
                    // it, and a click on this button does not put it there, so
                    // the note silently never started. Deferred, because
                    // starting a note updates this panel, which is mid-render.
                    .on_click(cx.listener(|this, _, window, cx| {
                        let workspace = this.workspace.clone();
                        window.defer(cx, move |window, cx| {
                            workspace
                                .update(cx, |workspace, cx| {
                                    TeamNotesPanel::start_note_at_cursor(workspace, window, cx);
                                })
                                .log_err();
                        });
                    })),
            )
            .child(
                Button::new("new-message", "Message")
                    .start_icon(Icon::new(IconName::Chat).size(IconSize::XSmall))
                    .label_size(LabelSize::Small)
                    .tooltip(Tooltip::text("Message the team"))
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
            .child(div().flex_1())
            .child(
                Button::new("show-closed", "Finished")
                    .start_icon(Icon::new(IconName::Check).size(IconSize::XSmall))
                    .label_size(LabelSize::Small)
                    .toggle_state(show_closed)
                    .color(if show_closed {
                        Color::Default
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
            .into_any_element()
    }

    /// The note, ticket or message being written, framed as the card it will
    /// become.
    fn render_draft(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let (draft, composer) = self.drafting.as_ref()?;
        let (icon, title) = match draft {
            Draft::Note { file, anchor } => {
                (IconName::Pin, format!("Note on {file}:{}", anchor.line + 1))
            }
            Draft::Ticket => (IconName::ListTodo, "New ticket".to_string()),
            Draft::Message => (IconName::Chat, "New message".to_string()),
        };
        let colors = cx.theme().colors();

        Some(
            v_flex()
                .key_context("TeamNotesDraft")
                .w_full()
                .p_2()
                .gap_2()
                .rounded_lg()
                .border_1()
                .border_color(colors.border_focused)
                .bg(colors.elevated_surface_background)
                .on_action(
                    cx.listener(|this, _: &CancelDraft, window, cx| this.cancel_draft(window, cx)),
                )
                .child(
                    h_flex()
                        .gap_1p5()
                        .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
                        .child(
                            Label::new(title)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                )
                .child(
                    div()
                        .p_1p5()
                        .rounded_md()
                        .bg(colors.editor_background)
                        .border_1()
                        .border_color(colors.border_variant)
                        .child(composer.clone()),
                )
                .child(
                    h_flex()
                        .justify_end()
                        .gap_1()
                        .child(
                            Button::new("cancel-draft", "Cancel")
                                .label_size(LabelSize::Small)
                                .color(Color::Muted)
                                .on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.cancel_draft(window, cx)
                                    }),
                                ),
                        )
                        .child(
                            Button::new("submit-draft", "Add")
                                .style(ButtonStyle::Filled)
                                .label_size(LabelSize::Small)
                                .on_click(
                                    cx.listener(|this, _, window, cx| {
                                        this.submit_draft(window, cx)
                                    }),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// What an empty tab says, and the way to put something in it.
    fn render_empty(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (icon, title, detail) = match self.filter {
            Filter::Inbox => (
                IconName::Check,
                "You're all caught up",
                "Tickets assigned to you and anything that @mentions you show up here.",
            ),
            Filter::Tickets => (
                IconName::ListTodo,
                "No open tickets",
                "Track work alongside the code. @mention someone to assign it, or \
                 @claude to hand it to an agent.",
            ),
            Filter::Notes => (
                IconName::Pin,
                "No notes yet",
                "Put the cursor on a line and add a note to explain why it is the \
                 way it is. Teammates see it when they open the file.",
            ),
            Filter::Messages | Filter::All => (
                IconName::UserGroup,
                "Nothing here yet",
                "Messages, notes and tickets live in .acuto/notes.jsonl and travel \
                 with the repository: your team sees them on their next pull.",
            ),
        };

        v_flex()
            .w_full()
            .py_8()
            .px_4()
            .gap_2()
            .items_center()
            .child(
                h_flex()
                    .size_10()
                    .justify_center()
                    .rounded_full()
                    .bg(cx.theme().colors().element_background)
                    .child(Icon::new(icon).size(IconSize::Medium).color(Color::Muted)),
            )
            .child(Label::new(title).weight(gpui::FontWeight::MEDIUM))
            .child(
                div().max_w(px(300.)).text_center().child(
                    Label::new(detail)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            )
            .into_any_element()
    }

    /// One heading of the list, with how many records sit under it.
    fn render_section(label: &'static str, count: usize) -> gpui::AnyElement {
        h_flex()
            .w_full()
            .pt_1()
            .gap_1()
            .child(
                Label::new(label.to_uppercase())
                    .size(LabelSize::XSmall)
                    .color(Color::Muted)
                    .weight(gpui::FontWeight::MEDIUM),
            )
            .child(
                Label::new(count.to_string())
                    .size(LabelSize::XSmall)
                    .color(Color::Disabled),
            )
            .into_any_element()
    }
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

        // Newest first within each section: what was raised last is the most
        // likely to still be on someone's mind. Ids are time-ordered.
        records.sort_by(|a, b| b.id.cmp(&a.id));

        let header = v_flex()
            .p_2()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(self.render_title(cx))
            .child(self.render_tabs(cx))
            .child(if filter == Filter::Messages {
                self.render_chat_header(cx)
            } else {
                self.render_toolbar(cx)
            });

        let base = v_flex()
            .key_context("TeamNotesPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(header);

        // A conversation is read in order and answered at the end; a list of
        // tickets is scanned. Same records, two shapes, because one shape
        // cannot do both jobs well.
        if filter == Filter::Messages {
            return base.child(self.render_chat(&records, cx));
        }

        // Tickets by where they have got to; everything else by whether it
        // still needs anything.
        let sections: Vec<(&'static str, Vec<&NoteThread>)> = if filter == Filter::Tickets {
            [Status::InProgress, Status::Open, Status::Done]
                .into_iter()
                .map(|status| {
                    (
                        status.label(),
                        records
                            .iter()
                            .filter(|record| record.status == status)
                            .collect(),
                    )
                })
                .collect()
        } else {
            vec![
                (
                    "Open",
                    records
                        .iter()
                        .filter(|record| !record.is_closed())
                        .collect(),
                ),
                (
                    "Finished",
                    records.iter().filter(|record| record.is_closed()).collect(),
                ),
            ]
        };
        let now = chrono::Utc::now();
        let draft = self.render_draft(cx);
        let empty = records.is_empty() && draft.is_none();

        let mut list = v_flex()
            .id("team-list")
            .p_2()
            .gap_2()
            .size_full()
            .overflow_y_scroll()
            .children(draft);
        if empty {
            list = list.child(self.render_empty(cx));
        }
        for (label, section) in sections {
            if section.is_empty() {
                continue;
            }
            list = list.child(Self::render_section(label, section.len()));
            for record in section {
                list = list.child(self.render_record(record, now, cx));
            }
        }

        base.child(list)
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
    fn a_short_pause_earns_no_timestamp() {
        // The case that made every message carry its own row: two messages six
        // seconds apart that happened to straddle a minute boundary.
        assert_eq!(
            minutes_apart("2026-08-29T07:15:54Z", "2026-08-29T07:16:00Z"),
            Some(1)
        );
        assert_eq!(
            minutes_apart("2026-08-29T07:16:00Z", "2026-08-29T07:29:48Z"),
            Some(13)
        );
        // A different day is not measured in minutes; the caller stamps it.
        assert_eq!(
            minutes_apart("2026-08-29T23:59:00Z", "2026-08-30T00:01:00Z"),
            None
        );
        assert_eq!(minutes_apart("nonsense", "2026-08-30T00:01:00Z"), None);
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
    fn ages_are_short_and_never_negative() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-08-29T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(age("2026-08-29T12:00:30Z", now), "now");
        assert_eq!(age("2026-08-29T11:55:00Z", now), "5m");
        assert_eq!(age("2026-08-29T09:00:00Z", now), "3h");
        assert_eq!(age("2026-08-27T12:00:00Z", now), "2d");
        assert_eq!(age("not a time", now), "not a time");
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

/// What the author wrote in a new record, for recognising who it mentions.
///
/// A ticket's words are its title; everything else's are its first message.
fn said_in(record: &NoteThread) -> String {
    match record.kind {
        Kind::Ticket => record.title.clone().unwrap_or_default(),
        _ => record
            .messages
            .first()
            .map(|message| message.body.clone())
            .unwrap_or_default(),
    }
}

/// The file editor used most recently in any pane, for attaching a note to.
///
/// Ranked by each pane's activation history rather than by pane order, so it
/// is the file you were last looking at, not the first one that happens to be
/// open.
fn last_used_editor(workspace: &Workspace, cx: &App) -> Option<Entity<Editor>> {
    let mut best: Option<(usize, Entity<Editor>)> = None;
    for pane in workspace.panes() {
        let pane = pane.read(cx);
        for entry in pane.activation_history() {
            let Some(editor) = pane
                .items()
                .find(|item| item.item_id() == entry.entity_id)
                .and_then(|item| item.act_as::<Editor>(cx))
            else {
                continue;
            };
            let has_file = editor
                .read(cx)
                .buffer()
                .read(cx)
                .as_singleton()
                .is_some_and(|buffer| buffer.read(cx).file().is_some());
            if has_file
                && best
                    .as_ref()
                    .is_none_or(|(time, _)| entry.timestamp > *time)
            {
                best = Some((entry.timestamp, editor));
            }
        }
    }
    best.map(|(_, editor)| editor)
}
