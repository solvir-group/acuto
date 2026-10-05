use anyhow::Result;
use buffer_diff::BufferDiff;
use clock;
use collections::{BTreeMap, HashMap, HashSet};
use fs::MTime;
use futures::{StreamExt, channel::mpsc};
use gpui::{
    App, AppContext, AsyncApp, Context, Entity, EntityId, SharedString, Subscription, Task,
    TaskExt as _, WeakEntity,
};
use language::{Anchor, Buffer, BufferEvent, Point};
use project::{Project, ProjectItem, lsp_store::OpenLspBufferHandle};
use std::{
    cmp,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};
use text::{Edit, Patch, Rope};
use util::{RangeExt, ResultExt as _};

/// Stores undo information for a single buffer's rejected edits
#[derive(Clone)]
pub struct PerBufferUndo {
    pub buffer: WeakEntity<Buffer>,
    pub edits_to_restore: Vec<(Range<Anchor>, String)>,
    pub status: UndoBufferStatus,
}

/// Tracks the buffer status for undo purposes
#[derive(Clone, Debug)]
pub enum UndoBufferStatus {
    Modified,
    /// Buffer was created by the agent.
    /// - `had_existing_content: true` - Agent overwrote an existing file. On reject, the
    ///   original content was restored. Undo is supported: we restore the agent's content.
    /// - `had_existing_content: false` - Agent created a new file that didn't exist before.
    ///   On reject, the file was deleted. Undo is NOT currently supported (would require
    ///   recreating the file). Future TODO.
    Created {
        had_existing_content: bool,
    },
}

/// Stores undo information for the most recent reject operation
#[derive(Clone)]
pub struct LastRejectUndo {
    /// Per-buffer undo information
    pub buffers: Vec<PerBufferUndo>,
}

/// Tracks actions performed by tools in a thread
/// A rejection that is kept rather than discarded.
///
/// The brief's rule is that a rejected hunk must never disappear silently: it
/// stays in the review list, greyed, and can be taken back. Everything needed
/// for that is already computed while rejecting — the range the restored
/// original now occupies, and the agent text that was displaced — it was simply
/// handed back to the caller and dropped. This retains it.
#[derive(Clone, Debug)]
pub struct RejectedHunk {
    /// Stable across list mutations, so the UI can identify a row without
    /// depending on its position.
    pub id: RejectedHunkId,
    pub buffer: WeakEntity<Buffer>,
    /// Where the restored original text sits now. An anchor range, so it
    /// survives later edits elsewhere in the buffer.
    pub range: Range<Anchor>,
    /// What the agent had proposed, so rejection can be taken back.
    pub agent_text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RejectedHunkId(pub usize);

pub struct ActionLog {
    /// Whether an agent turn is in progress. While it is, a change reaching a
    /// tracked buffer from disk is taken as the agent's, whether or not the
    /// agent announced that file: agents also edit through shells, and their
    /// announcements are not guaranteed to arrive before their writes.
    agent_turn_active: bool,
    /// Every buffer that was open when the current agent turn began, as it
    /// stood then. A file first claimed partway through a turn takes its base
    /// from here, so an agent write that lands before the file is claimed is
    /// still measured against the text it replaced.
    turn_start_snapshots: HashMap<Entity<Buffer>, text::BufferSnapshot>,
    /// Watches the turn-start buffers that are not tracked yet, so typing in
    /// them moves their snapshot forward and a reload claims them on the spot.
    _turn_start_subscriptions: Vec<Subscription>,
    /// Whether the agent has changed any file since the current turn began.
    agent_edited_this_turn: bool,
    /// Buffers the user has kept or rejected changes in since the current turn
    /// began. A base worked out from the agent's report of its edits is never
    /// applied to these: the report still describes changes the user has
    /// already answered, and applying it would put them back under review.
    answered_this_turn: HashSet<EntityId>,
    /// Buffers that we want to notify the model about when they change.
    tracked_buffers: BTreeMap<Entity<Buffer>, TrackedBuffer>,
    /// The project this action log is associated with
    project: Entity<Project>,
    /// An action log to forward all public methods to
    /// Useful in cases like subagents, where we want to track individual diffs for this subagent,
    /// but also want to associate the reads/writes with a parent review experience
    linked_action_log: Option<Entity<ActionLog>>,
    /// Stores undo information for the most recent reject operation
    last_reject_undo: Option<LastRejectUndo>,
    /// Rejections kept so they stay visible and can be taken back, rather than
    /// vanishing from the review list the moment they are made.
    rejected_hunks: Vec<RejectedHunk>,
    next_rejected_hunk_id: usize,
    /// Tracks the last time files were read by the agent, to detect external modifications
    file_read_times: HashMap<PathBuf, MTime>,
}

impl ActionLog {
    /// Creates a new, empty action log associated with the given project.
    pub fn new(project: Entity<Project>) -> Self {
        Self {
            agent_turn_active: false,
            turn_start_snapshots: HashMap::default(),
            _turn_start_subscriptions: Vec::new(),
            agent_edited_this_turn: false,
            answered_this_turn: HashSet::default(),
            tracked_buffers: BTreeMap::default(),
            project,
            linked_action_log: None,
            last_reject_undo: None,
            rejected_hunks: Vec::new(),
            next_rejected_hunk_id: 0,
            file_read_times: HashMap::default(),
        }
    }

    pub fn with_linked_action_log(mut self, linked_action_log: Entity<ActionLog>) -> Self {
        self.linked_action_log = Some(linked_action_log);
        self
    }

    pub fn project(&self) -> &Entity<Project> {
        &self.project
    }

    pub fn file_read_time(&self, path: &Path) -> Option<MTime> {
        self.file_read_times.get(path).copied()
    }

    fn update_file_read_time(&mut self, buffer: &Entity<Buffer>, cx: &App) {
        let buffer = buffer.read(cx);
        if let Some(file) = buffer.file() {
            if let Some(local_file) = file.as_local() {
                if let Some(mtime) = file.disk_state().mtime() {
                    let abs_path = local_file.abs_path(cx);
                    self.file_read_times.insert(abs_path, mtime);
                }
            }
        }
    }

    fn remove_file_read_time(&mut self, buffer: &Entity<Buffer>, cx: &App) {
        let buffer = buffer.read(cx);
        if let Some(file) = buffer.file() {
            if let Some(local_file) = file.as_local() {
                let abs_path = local_file.abs_path(cx);
                self.file_read_times.remove(&abs_path);
            }
        }
    }

    fn track_buffer_internal(
        &mut self,
        buffer: Entity<Buffer>,
        is_created: bool,
        cx: &mut Context<Self>,
    ) -> &mut TrackedBuffer {
        let status = if is_created {
            if let Some(tracked) = self.tracked_buffers.remove(&buffer) {
                match tracked.status {
                    TrackedBufferStatus::Created {
                        existing_file_content,
                    } => TrackedBufferStatus::Created {
                        existing_file_content,
                    },
                    TrackedBufferStatus::Modified | TrackedBufferStatus::Deleted => {
                        TrackedBufferStatus::Created {
                            existing_file_content: Some(tracked.diff_base),
                        }
                    }
                }
            } else if buffer
                .read(cx)
                .file()
                .is_some_and(|file| file.disk_state().exists())
            {
                TrackedBufferStatus::Created {
                    existing_file_content: Some(buffer.read(cx).as_rope().clone()),
                }
            } else {
                TrackedBufferStatus::Created {
                    existing_file_content: None,
                }
            }
        } else {
            TrackedBufferStatus::Modified
        };

        let tracked_buffer = self
            .tracked_buffers
            .entry(buffer.clone())
            .or_insert_with(|| {
                let open_lsp_handle = self.project.update(cx, |project, cx| {
                    project.register_buffer_with_language_servers(&buffer, cx)
                });

                let text_snapshot = buffer.read(cx).text_snapshot();
                let language = buffer.read(cx).language().cloned();
                let language_registry = buffer.read(cx).language_registry();
                let diff = cx.new(|cx| {
                    let mut diff = BufferDiff::new(
                        &text_snapshot,
                        language,
                        language_registry,
                        buffer_diff::DiffBaseKind::Custom,
                        cx,
                    );
                    // One line, one decision. A turn that rewrites a file
                    // arrives as a single run of changed lines, and offered
                    // whole it can only be taken whole -- accepting any part
                    // of it accepts the rest unseen.
                    diff.set_line_granularity(true);
                    diff
                });
                let (diff_update_tx, diff_update_rx) = mpsc::unbounded();
                let diff_base = if is_created {
                    Rope::default()
                } else {
                    buffer.read(cx).as_rope().clone()
                };
                let mut tracked_buffer = TrackedBuffer {
                    buffer: buffer.clone(),
                    agent_writes_to_disk: false,
                    base_from_report: false,
                    diff_base,
                    unreviewed_edits: Patch::default(),
                    changes: Vec::new(),
                    accepted: Vec::new(),
                    user_edited: false,
                    snapshot: text_snapshot,
                    status,
                    version: buffer.read(cx).version(),
                    diff,
                    diff_update: diff_update_tx,
                    _open_lsp_handle: open_lsp_handle,
                    _maintain_diff: cx.spawn({
                        let buffer = buffer.clone();
                        async move |this, cx| {
                            Self::maintain_diff(this, buffer, diff_update_rx, cx)
                                .await
                                .ok();
                        }
                    }),
                    _subscription: cx.subscribe(&buffer, Self::handle_buffer_event),
                };
                tracked_buffer.refresh();
                tracked_buffer
            });
        tracked_buffer.version = buffer.read(cx).version();
        tracked_buffer
    }

    fn handle_buffer_event(
        &mut self,
        buffer: Entity<Buffer>,
        event: &BufferEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            BufferEvent::Edited { .. } => {
                let Some(tracked_buffer) = self.tracked_buffers.get_mut(&buffer) else {
                    return;
                };
                let buffer_version = buffer.read(cx).version();
                if !buffer_version.changed_since(&tracked_buffer.version) {
                    return;
                }
                self.handle_buffer_edited(buffer, cx);
            }
            BufferEvent::FileHandleChanged => {
                self.handle_buffer_file_changed(buffer, cx);
            }
            _ => {}
        };
    }

    fn handle_buffer_edited(&mut self, buffer: Entity<Buffer>, cx: &mut Context<Self>) {
        let Some(tracked_buffer) = self.tracked_buffers.get_mut(&buffer) else {
            return;
        };
        // A reload applies the disk's contents and marks them saved in the same
        // step. Typing always leaves the buffer dirty; undoing back to the saved
        // text leaves it clean but at a version the save never saw. So a change
        // that left the buffer clean *at its saved version* came from disk, and
        // to a file the agent writes, or to any tracked file while the agent is
        // working, it is the agent's.
        let reloaded = was_reloaded(buffer.read(cx));
        if reloaded && (tracked_buffer.agent_writes_to_disk || self.agent_turn_active) {
            log::info!(
                "agent review: {} changed on disk, recorded as the agent's edit",
                buffer_display_path(&buffer, cx)
            );
            self.buffer_edited(buffer, cx);
            return;
        }
        tracked_buffer.observe(ChangeAuthor::User, cx);
        cx.notify();
    }

    /// Takes in any change to the buffer the log has not seen yet, the way the
    /// buffer's own edit event would.
    ///
    /// Answering a change is always measured against the text the log last
    /// saw. A change still waiting on its event would otherwise sit under the
    /// answer: accepted or rejected as part of something it was not.
    fn catch_up(&mut self, buffer: &Entity<Buffer>, cx: &mut Context<Self>) {
        let Some(tracked_buffer) = self.tracked_buffers.get(buffer) else {
            return;
        };
        if tracked_buffer.snapshot.version() != &buffer.read(cx).version() {
            self.handle_buffer_edited(buffer.clone(), cx);
        }
    }

    fn handle_buffer_file_changed(&mut self, buffer: Entity<Buffer>, cx: &mut Context<Self>) {
        let Some(tracked_buffer) = self.tracked_buffers.get_mut(&buffer) else {
            return;
        };

        match tracked_buffer.status {
            TrackedBufferStatus::Created { .. } | TrackedBufferStatus::Modified => {
                if buffer
                    .read(cx)
                    .file()
                    .is_some_and(|file| file.disk_state().is_deleted())
                {
                    // If the buffer had been edited by a tool, but it got
                    // deleted externally, we want to stop tracking it.
                    self.tracked_buffers.remove(&buffer);
                }
                cx.notify();
            }
            TrackedBufferStatus::Deleted => {
                if buffer
                    .read(cx)
                    .file()
                    .is_some_and(|file| !file.disk_state().is_deleted())
                {
                    // If the buffer had been deleted by a tool, but it got
                    // resurrected externally, we want to clear the edits we
                    // were tracking and reset the buffer's state.
                    self.tracked_buffers.remove(&buffer);
                    self.track_buffer_internal(buffer, false, cx);
                }
                cx.notify();
            }
        }
    }

    /// Keeps the buffer's diff drawn against the log's base.
    ///
    /// Only the drawing happens here. The base, the snapshot and the list of
    /// unanswered changes are all updated on the spot, in the order things
    /// happen: when this task used to write them back after an await, an
    /// answer given while it was running was overwritten by the older base it
    /// had started from, and the accept or reject simply did not happen.
    async fn maintain_diff(
        this: WeakEntity<Self>,
        buffer: Entity<Buffer>,
        mut updates: mpsc::UnboundedReceiver<DiffUpdate>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        while let Some(mut update) = updates.next().await {
            // Only the newest matters; anything queued behind it is already
            // out of date.
            while let Ok(newer) = updates.try_recv() {
                update = newer;
            }
            let (base_text, buffer_snapshot, line_changes) = update;
            let diff = this.read_with(cx, |this, _cx| {
                this.tracked_buffers
                    .get(&buffer)
                    .map(|tracked_buffer| tracked_buffer.diff.clone())
            })?;
            let Some(diff) = diff else {
                break;
            };
            diff.update(cx, |diff, cx| {
                diff.set_base_text_with_line_changes(base_text, buffer_snapshot, line_changes, cx)
            })
            .await;
            this.update(cx, |_this, cx| cx.notify())?;
        }
        Ok(())
    }

    /// Track a buffer as read by agent, so we can notify the model about user edits.
    pub fn buffer_read(&mut self, buffer: Entity<Buffer>, cx: &mut Context<Self>) {
        self.buffer_read_impl(buffer, true, cx);
    }

    fn buffer_read_impl(
        &mut self,
        buffer: Entity<Buffer>,
        record_file_read_time: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(linked_action_log) = &self.linked_action_log {
            // We don't want to share read times since the other agent hasn't read it necessarily
            linked_action_log.update(cx, |log, cx| {
                log.buffer_read_impl(buffer.clone(), false, cx);
            });
        }
        if record_file_read_time {
            self.update_file_read_time(&buffer, cx);
        }
        self.track_buffer_internal(buffer, false, cx);
    }

    /// Mark a buffer as created by agent, so we can refresh it in the context
    pub fn buffer_created(&mut self, buffer: Entity<Buffer>, cx: &mut Context<Self>) {
        self.buffer_created_impl(buffer, true, cx);
    }

    fn buffer_created_impl(
        &mut self,
        buffer: Entity<Buffer>,
        record_file_read_time: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(linked_action_log) = &self.linked_action_log {
            // We don't want to share read times since the other agent hasn't read it necessarily
            linked_action_log.update(cx, |log, cx| {
                log.buffer_created_impl(buffer.clone(), false, cx);
            });
        }
        if record_file_read_time {
            self.update_file_read_time(&buffer, cx);
        }
        self.track_buffer_internal(buffer, true, cx);
    }

    /// Marks the start of an agent turn, snapshotting every buffer open now.
    pub fn begin_agent_turn(
        &mut self,
        open_buffers: impl IntoIterator<Item = Entity<Buffer>>,
        cx: &mut Context<Self>,
    ) {
        self.agent_turn_active = true;
        self.agent_edited_this_turn = false;
        self.answered_this_turn.clear();
        for tracked_buffer in self.tracked_buffers.values_mut() {
            tracked_buffer.base_from_report = false;
        }
        // Lines accepted in a file stay on screen for reference while any of
        // that file is still under review, and are let go once the agent starts
        // on something new after it was all answered.
        for tracked_buffer in self.tracked_buffers.values_mut() {
            if !tracked_buffer.accepted.is_empty() && tracked_buffer.unreviewed_edits.is_empty() {
                tracked_buffer.accepted.clear();
                cx.notify();
            }
        }
        // Rejections are let go on the same rule: kept while their file is
        // still under review, dropped once it was all answered and the agent
        // moves on. Without this the list only ever grew.
        let tracked_buffers = &self.tracked_buffers;
        let rejected_before = self.rejected_hunks.len();
        self.rejected_hunks.retain(|hunk| {
            hunk.buffer.upgrade().is_some_and(|buffer| {
                tracked_buffers
                    .get(&buffer)
                    .is_some_and(|tracked| !tracked.unreviewed_edits.is_empty())
            })
        });
        if self.rejected_hunks.len() != rejected_before {
            cx.notify();
        }
        self.turn_start_snapshots = open_buffers
            .into_iter()
            .map(|buffer| {
                let snapshot = buffer.read(cx).text_snapshot();
                (buffer, snapshot)
            })
            .collect();
        self._turn_start_subscriptions = self
            .turn_start_snapshots
            .keys()
            .map(|buffer| cx.subscribe(buffer, Self::handle_turn_start_buffer_event))
            .collect();
    }

    /// Keeps a turn-start snapshot honest for a buffer not yet tracked.
    ///
    /// Without this, everything typed in an open file during a turn sat between
    /// the snapshot and the file, and the moment the agent claimed the file it
    /// was offered for review as the agent's -- Reject threw the typing away.
    fn handle_turn_start_buffer_event(
        &mut self,
        buffer: Entity<Buffer>,
        event: &BufferEvent,
        cx: &mut Context<Self>,
    ) {
        if !matches!(event, BufferEvent::Edited { .. })
            || !self.agent_turn_active
            || self.tracked_buffers.contains_key(&buffer)
            || !self.turn_start_snapshots.contains_key(&buffer)
        {
            return;
        }
        if was_reloaded(buffer.read(cx)) {
            log::info!(
                "agent review: {} changed on disk during the turn, recorded as the agent's edit",
                buffer_display_path(&buffer, cx)
            );
            self.agent_will_write(buffer, cx);
        } else {
            let snapshot = buffer.read(cx).text_snapshot();
            self.turn_start_snapshots.insert(buffer, snapshot);
        }
    }

    /// Whether a prompt is in flight.
    ///
    /// Tool calls arrive both from a live turn and from a session's replayed
    /// history when a thread is reopened. Only the first are edits happening
    /// now; the second already happened and were reviewed long ago.
    pub fn agent_turn_active(&self) -> bool {
        self.agent_turn_active
    }

    /// Marks the end of an agent turn.
    ///
    /// Called once the turn's files have been brought up to date with the disk,
    /// so the agent's last writes have landed. From here a file changing on disk
    /// is a git checkout, a formatter or another editor, not the agent; leaving
    /// the file claimed would offer that change for Reject, which would undo it.
    pub fn end_agent_turn(&mut self) {
        self.agent_turn_active = false;
        self.turn_start_snapshots.clear();
        self._turn_start_subscriptions.clear();
        for tracked_buffer in self.tracked_buffers.values_mut() {
            tracked_buffer.agent_writes_to_disk = false;
        }
    }

    /// Whether the agent has changed any file since the current or most recent
    /// turn began.
    pub fn agent_edited_this_turn(&self) -> bool {
        self.agent_edited_this_turn
    }

    /// Every buffer the log is tracking.
    pub fn tracked_buffers(&self) -> Vec<Entity<Buffer>> {
        self.tracked_buffers.keys().cloned().collect()
    }

    /// Starts tracking a buffer the agent has touched, taking as its base the
    /// text it held when the turn began if it was open then, or its current
    /// text otherwise. Returns whether it was newly tracked.
    fn track_agent_buffer(&mut self, buffer: &Entity<Buffer>, cx: &mut Context<Self>) -> bool {
        if self.tracked_buffers.contains_key(buffer) {
            return false;
        }
        let exists = buffer
            .read(cx)
            .file()
            .is_some_and(|file| file.disk_state().exists());
        let turn_start = self.turn_start_snapshots.get(buffer).cloned();
        if !exists && turn_start.is_none() {
            self.buffer_created(buffer.clone(), cx);
            log::info!(
                "agent review: tracking {} as a new file",
                buffer_display_path(buffer, cx)
            );
            return true;
        }

        self.update_file_read_time(buffer, cx);
        let tracked_buffer = self.track_buffer_internal(buffer.clone(), false, cx);
        let base_from = if let Some(turn_start) = turn_start {
            // The agent may already have written the file by the time it is
            // claimed. Measuring from the turn's start covers that.
            tracked_buffer.diff_base = turn_start.as_rope().clone();
            tracked_buffer.snapshot = turn_start;
            "the start of the turn"
        } else {
            "its current text"
        };
        tracked_buffer.observe(ChangeAuthor::Agent, cx);
        log::info!(
            "agent review: tracking {} against {base_from}",
            buffer_display_path(buffer, cx)
        );
        true
    }

    /// Records that the agent read a file, so later edits to it -- including
    /// ones it makes through a shell -- are measured against what it read.
    pub fn agent_read(&mut self, buffer: Entity<Buffer>, cx: &mut Context<Self>) {
        self.track_agent_buffer(&buffer, cx);
    }

    /// Claims a file an agent is about to write to disk with its own tools.
    ///
    /// What agents report about an edit is a snippet of the change, never the
    /// file as it stood, so the file itself is the record: tracked here if it
    /// is not already, and from then on a change to it arriving from disk is
    /// the agent's. A file already tracked keeps its base, so several edits
    /// across tool calls and turns review as one change against the last thing
    /// the user accepted.
    pub fn agent_will_write(&mut self, buffer: Entity<Buffer>, cx: &mut Context<Self>) {
        self.track_agent_buffer(&buffer, cx);
        if let Some(tracked_buffer) = self.tracked_buffers.get_mut(&buffer) {
            tracked_buffer.agent_writes_to_disk = true;
        }
    }

    /// Settles a file an agent wrote to disk with its own tools, given the text
    /// it held before this turn's writes, worked out from the agent's own
    /// report of them. `None` means the turn created the file.
    ///
    /// A file the log already measured from before the write needs nothing:
    /// the reload was recorded as the agent's edit. But claiming a file opens
    /// it first, and an agent allowed to edit without asking has often written
    /// it by then. Its base was then the new text, nothing was left to review,
    /// and the file was missing from the diff. This puts the true base in.
    pub fn agent_wrote(
        &mut self,
        buffer: Entity<Buffer>,
        text_before: Option<Rope>,
        cx: &mut Context<Self>,
    ) {
        // Deliberately not gated on the turn still running: the report of a
        // turn's last write often lands after the turn has ended. Replayed
        // history never gets here; the caller turns it away when it arrives.
        if self.answered_this_turn.contains(&buffer.entity_id()) {
            return;
        }
        if let Some(tracked_buffer) = self.tracked_buffers.get(&buffer)
            && !tracked_buffer.base_from_report
            && !tracked_buffer.changes.is_empty()
        {
            return;
        }
        let current = buffer.read(cx).as_rope().clone();
        if text_before.as_ref().is_some_and(|before| {
            before.len() == current.len() && before.chars().eq(current.chars())
        }) {
            return;
        }

        let created = text_before.is_none();
        if !self.tracked_buffers.contains_key(&buffer) {
            self.update_file_read_time(&buffer, cx);
            self.track_buffer_internal(buffer.clone(), created, cx);
        }
        let Some(tracked_buffer) = self.tracked_buffers.get_mut(&buffer) else {
            return;
        };
        if created {
            tracked_buffer.status = TrackedBufferStatus::Created {
                existing_file_content: None,
            };
        } else if let TrackedBufferStatus::Created { .. } = tracked_buffer.status {
            tracked_buffer.status = TrackedBufferStatus::Modified;
        }
        tracked_buffer.diff_base = text_before.unwrap_or_default();
        tracked_buffer.base_from_report = true;
        tracked_buffer.agent_writes_to_disk = true;
        tracked_buffer.snapshot = buffer.read(cx).text_snapshot();
        tracked_buffer.version = buffer.read(cx).version();
        tracked_buffer.refresh();
        self.agent_edited_this_turn = true;
        log::info!(
            "agent review: {} measured against its text before the agent's report of this turn",
            buffer_display_path(&buffer, cx)
        );
        cx.notify();
    }

    /// Mark a buffer as edited by agent, so we can refresh it in the context
    pub fn buffer_edited(&mut self, buffer: Entity<Buffer>, cx: &mut Context<Self>) {
        self.buffer_edited_impl(buffer, true, cx);
    }

    fn buffer_edited_impl(
        &mut self,
        buffer: Entity<Buffer>,
        record_file_read_time: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(linked_action_log) = &self.linked_action_log {
            // We don't want to share read times since the other agent hasn't read it necessarily
            linked_action_log.update(cx, |log, cx| {
                log.buffer_edited_impl(buffer.clone(), false, cx);
            });
        }
        if record_file_read_time {
            self.update_file_read_time(&buffer, cx);
        }
        let new_version = buffer.read(cx).version();
        self.agent_edited_this_turn = true;
        let tracked_buffer = self.track_buffer_internal(buffer, false, cx);
        if let TrackedBufferStatus::Deleted = tracked_buffer.status {
            tracked_buffer.status = TrackedBufferStatus::Modified;
        }

        tracked_buffer.version = new_version;
        tracked_buffer.observe(ChangeAuthor::Agent, cx);
        cx.notify();
    }

    pub fn will_delete_buffer(&mut self, buffer: Entity<Buffer>, cx: &mut Context<Self>) {
        // Ok to propagate file read time removal to linked action log
        self.remove_file_read_time(&buffer, cx);
        let has_linked_action_log = self.linked_action_log.is_some();
        let tracked_buffer = self.track_buffer_internal(buffer.clone(), false, cx);
        match tracked_buffer.status {
            TrackedBufferStatus::Created { .. } => {
                self.tracked_buffers.remove(&buffer);
                cx.notify();
            }
            TrackedBufferStatus::Modified => {
                tracked_buffer.status = TrackedBufferStatus::Deleted;
                if !has_linked_action_log {
                    buffer.update(cx, |buffer, cx| buffer.set_text("", cx));
                    tracked_buffer.observe(ChangeAuthor::Agent, cx);
                }
            }

            TrackedBufferStatus::Deleted => {}
        }

        if let Some(linked_action_log) = &mut self.linked_action_log {
            linked_action_log.update(cx, |log, cx| log.will_delete_buffer(buffer.clone(), cx));
        }

        if has_linked_action_log && let Some(tracked_buffer) = self.tracked_buffers.get_mut(&buffer)
        {
            tracked_buffer.observe(ChangeAuthor::Agent, cx);
        }

        cx.notify();
    }

    /// The agent's changes to this buffer that the given ranges answer, a
    /// line at a time.
    ///
    /// Computed with the same diff that draws the hunks, against the log's own
    /// base and the text it last saw, so the line a click lands on is the line
    /// that is answered.
    ///
    /// Matched by byte offset, the way the diff places its hunks. Rows cannot
    /// tell apart a file's unterminated last line and the deletion just after
    /// it, which share a row, and answering one answered the other.
    ///
    /// - An empty range is a click or a cursor. It answers the deletion it sits
    ///   on, or else the line it is in.
    /// - Any other range answers the lines it overlaps and the deletions inside
    ///   it. A hunk's range ends where the next line starts, so a change there
    ///   is not part of it: reading it as included made every Keep and Reject
    ///   take two lines.
    fn reviewed_changes(
        tracked_buffer: &TrackedBuffer,
        selection: &Selection,
    ) -> Vec<ReviewedChange> {
        let ranges = match selection {
            Selection::Everything => return Self::all_changes(tracked_buffer),
            Selection::Ranges(ranges) => ranges,
        };
        let snapshot = &tracked_buffer.snapshot;
        let len = snapshot.len();
        let unterminated = snapshot
            .reversed_chars_at(len)
            .next()
            .is_some_and(|c| c != '\n');
        let changes = Self::all_changes(tracked_buffer);
        let bytes = changes
            .iter()
            .map(|change| change.buffer_bytes(snapshot))
            .collect::<Vec<_>>();
        let mut answered = vec![false; changes.len()];
        for range in ranges {
            if range.is_empty() {
                let offset = range.start;
                let deletions_here = bytes
                    .iter()
                    .map(|change| change.is_empty() && change.start == offset)
                    .collect::<Vec<_>>();
                if deletions_here.contains(&true) {
                    for (answered, here) in answered.iter_mut().zip(deletions_here) {
                        *answered |= here;
                    }
                } else {
                    for (answered, change) in answered.iter_mut().zip(&bytes) {
                        let at_unterminated_end = unterminated && change.end == len;
                        *answered |= !change.is_empty()
                            && change.start <= offset
                            && (offset < change.end
                                || (offset == change.end && at_unterminated_end));
                    }
                }
            } else {
                for (answered, change) in answered.iter_mut().zip(&bytes) {
                    *answered |= if change.is_empty() {
                        range.start <= change.start && change.start < range.end
                    } else {
                        change.start < range.end && range.start < change.end
                    };
                }
            }
        }
        changes
            .into_iter()
            .zip(answered)
            .filter_map(|(change, answered)| answered.then_some(change))
            .collect()
    }

    /// The buffer's changes the log has not had answered, one per line.
    fn all_changes(tracked_buffer: &TrackedBuffer) -> Vec<ReviewedChange> {
        tracked_buffer
            .changes
            .iter()
            .map(|(base_rows, buffer_rows)| ReviewedChange {
                base_rows: base_rows.clone(),
                buffer_rows: buffer_rows.clone(),
            })
            .collect()
    }

    pub fn keep_edits_in_range(
        &mut self,
        buffer: Entity<Buffer>,
        buffer_range: Range<impl language::ToPoint>,
        telemetry: Option<ActionLogTelemetry>,
        cx: &mut Context<Self>,
    ) {
        self.keep_edits_in_ranges(buffer, vec![buffer_range], telemetry, cx)
    }

    /// Accepts every agent change the given ranges touch, in one pass.
    ///
    /// One pass matters: accepting folds the change into the base, so answering
    /// a selection hunk by hunk measures the second hunk against a base the
    /// first one already moved, and everything after the first is found to have
    /// no change left in it. Selecting forty lines then accepted one.
    pub fn keep_edits_in_ranges(
        &mut self,
        buffer: Entity<Buffer>,
        buffer_ranges: Vec<Range<impl language::ToPoint>>,
        telemetry: Option<ActionLogTelemetry>,
        cx: &mut Context<Self>,
    ) {
        self.catch_up(&buffer, cx);
        let Some(tracked_buffer) = self.tracked_buffers.get(&buffer) else {
            return;
        };
        let selection = Selection::Ranges(offset_ranges(&tracked_buffer.snapshot, buffer_ranges));
        self.keep_selected_edits(buffer, selection, telemetry, cx);
    }

    /// Accepts every agent change in the buffer.
    pub fn keep_all_edits_in_buffer(
        &mut self,
        buffer: Entity<Buffer>,
        telemetry: Option<ActionLogTelemetry>,
        cx: &mut Context<Self>,
    ) {
        self.keep_selected_edits(buffer, Selection::Everything, telemetry, cx);
    }

    fn keep_selected_edits(
        &mut self,
        buffer: Entity<Buffer>,
        selection: Selection,
        telemetry: Option<ActionLogTelemetry>,
        cx: &mut Context<Self>,
    ) {
        self.answered_this_turn.insert(buffer.entity_id());
        self.catch_up(&buffer, cx);
        let Some(tracked_buffer) = self.tracked_buffers.get_mut(&buffer) else {
            return;
        };

        let mut metrics = ActionLogMetrics::for_buffer(buffer.read(cx));
        match tracked_buffer.status {
            TrackedBufferStatus::Deleted => {
                metrics.add_edits(tracked_buffer.unreviewed_edits.edits());
                self.tracked_buffers.remove(&buffer);
                cx.notify();
            }
            _ => {
                let accepted = Self::reviewed_changes(tracked_buffer, &selection);
                tracked_buffer.remember_accepted(&accepted);
                // Latest first, so each replacement leaves the rows of the ones
                // before it where they were.
                for change in accepted.iter().rev() {
                    let (range, replacement) = splice_rows(
                        &tracked_buffer.diff_base,
                        change.base_rows.clone(),
                        change.buffer_text(&tracked_buffer.snapshot),
                    );
                    tracked_buffer.diff_base.replace(range, &replacement);
                }
                let reviewed = accepted
                    .iter()
                    .map(|change| change.as_edit())
                    .collect::<Vec<_>>();
                metrics.add_edits(&reviewed);
                log::info!(
                    "agent review: accepted {} line change(s) in {}",
                    accepted.len(),
                    buffer_display_path(&buffer, cx)
                );
                if tracked_buffer.diff_base.len() == tracked_buffer.snapshot.len()
                    && tracked_buffer
                        .diff_base
                        .chars_at(0)
                        .eq(tracked_buffer.snapshot.as_rope().chars_at(0))
                    && let TrackedBufferStatus::Created { .. } = &mut tracked_buffer.status
                {
                    tracked_buffer.status = TrackedBufferStatus::Modified;
                }
                tracked_buffer.settle_answers(&accepted, Answer::Kept);
                cx.notify();
            }
        }
        if let Some(telemetry) = telemetry {
            telemetry_report_accepted_edits(&telemetry, metrics);
        }
    }

    pub fn reject_edits_in_ranges(
        &mut self,
        buffer: Entity<Buffer>,
        buffer_ranges: Vec<Range<impl language::ToPoint>>,
        telemetry: Option<ActionLogTelemetry>,
        cx: &mut Context<Self>,
    ) -> (Task<Result<()>>, Option<PerBufferUndo>) {
        self.catch_up(&buffer, cx);
        let Some(tracked_buffer) = self.tracked_buffers.get(&buffer) else {
            return (Task::ready(Ok(())), None);
        };
        let selection = Selection::Ranges(offset_ranges(&tracked_buffer.snapshot, buffer_ranges));
        self.reject_selected_edits(buffer, selection, telemetry, cx)
    }

    /// Rejects every agent change in the buffer.
    pub fn reject_all_edits_in_buffer(
        &mut self,
        buffer: Entity<Buffer>,
        telemetry: Option<ActionLogTelemetry>,
        cx: &mut Context<Self>,
    ) -> (Task<Result<()>>, Option<PerBufferUndo>) {
        self.reject_selected_edits(buffer, Selection::Everything, telemetry, cx)
    }

    fn reject_selected_edits(
        &mut self,
        buffer: Entity<Buffer>,
        selection: Selection,
        telemetry: Option<ActionLogTelemetry>,
        cx: &mut Context<Self>,
    ) -> (Task<Result<()>>, Option<PerBufferUndo>) {
        self.answered_this_turn.insert(buffer.entity_id());
        self.catch_up(&buffer, cx);
        let Some(tracked_buffer) = self.tracked_buffers.get_mut(&buffer) else {
            return (Task::ready(Ok(())), None);
        };

        let mut metrics = ActionLogMetrics::for_buffer(buffer.read(cx));
        let mut undo_info: Option<PerBufferUndo> = None;

        // Whether this rejection answers every change in the file, and the
        // ranges as rows. Deleting a file the agent created is only right when
        // the whole thing is being turned down; rejecting one line of it has to
        // remove that line, like rejecting one line of any other file.
        //
        // And only while none of it has been accepted: a file whose base holds
        // lines the user kept is no longer the agent's alone, and turning down
        // the rest of it must not take the kept lines with it. Deleting there
        // is what made a file vanish after part of it had been accepted.
        let (rejects_whole_file, change_count) = {
            let selected = Self::reviewed_changes(tracked_buffer, &selection);
            let all = Self::all_changes(tracked_buffer);
            let count = selected.len();
            let nothing_accepted = tracked_buffer.diff_base.len() == 0;
            // Deleting a new file is only right while it holds the agent's
            // text alone. Once the user has typed in it, the lines on screen
            // are what is being turned down, and they are reverted like any
            // other; the file stays. Dropping the file from review without
            // touching it, as was done before, turned the rejection into
            // nothing at all.
            let untouched_since_agent = match &tracked_buffer.status {
                TrackedBufferStatus::Created {
                    existing_file_content: None,
                } => !tracked_buffer.user_edited,
                _ => true,
            };
            (
                nothing_accepted && untouched_since_agent && !all.is_empty() && count == all.len(),
                count,
            )
        };

        log::info!(
            "agent review: reject in {} ({} line change(s) of the file, whole file: {}, dirty: {})",
            buffer_display_path(&buffer, cx),
            change_count,
            rejects_whole_file,
            buffer.read(cx).is_dirty()
        );

        let task = match (&tracked_buffer.status, rejects_whole_file) {
            (
                TrackedBufferStatus::Created {
                    existing_file_content,
                },
                true,
            ) => {
                let task = if let Some(existing_file_content) = existing_file_content {
                    // Capture the agent's content before restoring existing file content
                    let agent_content = buffer.read(cx).text();
                    let buffer_id = buffer.read(cx).remote_id();

                    buffer.update(cx, |buffer, cx| {
                        buffer.start_transaction();
                        buffer.set_text("", cx);
                        for chunk in existing_file_content.chunks() {
                            buffer.append(chunk, cx);
                        }
                        buffer.end_transaction(cx);
                    });

                    undo_info = Some(PerBufferUndo {
                        buffer: buffer.downgrade(),
                        edits_to_restore: vec![(
                            Anchor::min_for_buffer(buffer_id)..Anchor::max_for_buffer(buffer_id),
                            agent_content,
                        )],
                        status: UndoBufferStatus::Created {
                            had_existing_content: true,
                        },
                    });

                    self.project
                        .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
                } else {
                    let task = buffer
                        .read(cx)
                        .entry_id(cx)
                        .and_then(|entry_id| {
                            self.project
                                .update(cx, |project, cx| project.delete_entry(entry_id, cx))
                        })
                        .unwrap_or_else(|| Task::ready(Ok(())));

                    cx.background_spawn(async move {
                        task.await?;
                        Ok(())
                    })
                };

                metrics.add_edits(tracked_buffer.unreviewed_edits.edits());
                self.tracked_buffers.remove(&buffer);
                cx.notify();
                task
            }
            (TrackedBufferStatus::Deleted, _) => {
                buffer.update(cx, |buffer, cx| {
                    buffer.set_text(tracked_buffer.diff_base.to_string(), cx)
                });
                let save = self
                    .project
                    .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx));

                // Clear all tracked edits for this buffer and start over as if we just read it.
                metrics.add_edits(tracked_buffer.unreviewed_edits.edits());
                self.tracked_buffers.remove(&buffer);
                self.buffer_read(buffer.clone(), cx);
                cx.notify();
                save
            }
            _ => {
                // The same hunks the panel offers, reverted one by one. Going
                // through `unreviewed_edits` would revert whole runs of
                // touching lines at once, because a `Patch` merges them.
                let rejected = Self::reviewed_changes(tracked_buffer, &selection);
                log::info!("agent review: reverting {} line change(s)", rejected.len());
                let base = tracked_buffer.diff_base.clone();
                let mut newline_only = Vec::new();
                let edits_to_restore = buffer.update(cx, |buffer, cx| {
                    let mut edits_for_undo = Vec::new();
                    buffer.start_transaction();
                    // Latest first and one at a time, so each line is put back
                    // into the text that will actually surround it: whether it
                    // needs a newline depends on whether anything still
                    // follows it once the lines after it are reverted.
                    for change in rejected.iter().rev() {
                        let restored = base
                            .chunks_in_range(change.base_bytes(&base))
                            .collect::<String>();
                        let (bytes, restored) =
                            splice_rows(buffer.as_rope(), change.buffer_rows.clone(), restored);
                        let agent_text = buffer.text_for_range(bytes.clone()).collect::<String>();
                        if agent_text == restored {
                            newline_only.push(change);
                            continue;
                        }
                        let range =
                            buffer.anchor_before(bytes.start)..buffer.anchor_after(bytes.end);
                        edits_for_undo.push((range.clone(), agent_text));
                        buffer.edit([(range, restored)], None, cx);
                    }
                    buffer.end_transaction(cx);
                    edits_for_undo
                });
                // The original's last line, without a newline, now has lines
                // after it. It differs from the agent's only by the newline
                // those lines need, so putting it back changes nothing and
                // would leave it in review forever. The text already reads as
                // the original there; the base is told so.
                for change in newline_only {
                    let (range, replacement) = splice_rows(
                        &tracked_buffer.diff_base,
                        change.base_rows.clone(),
                        change.buffer_text(&tracked_buffer.snapshot),
                    );
                    tracked_buffer.diff_base.replace(range, &replacement);
                }
                // Taken in now rather than when the edit event arrives, so an
                // answer given straight after this one sees the reverted text.
                tracked_buffer.snapshot = buffer.read(cx).text_snapshot();
                tracked_buffer.settle_answers(&rejected, Answer::Rejected);

                let reviewed = rejected
                    .iter()
                    .map(|change| change.as_edit())
                    .collect::<Vec<_>>();
                metrics.add_edits(&reviewed);

                if !edits_to_restore.is_empty() {
                    undo_info = Some(PerBufferUndo {
                        buffer: buffer.downgrade(),
                        edits_to_restore,
                        status: UndoBufferStatus::Modified,
                    });
                }

                self.project
                    .update(cx, |project, cx| project.save_buffer(buffer, cx))
            }
        };
        if let Some(telemetry) = telemetry {
            telemetry_report_rejected_edits(&telemetry, metrics);
        }

        // Retained before the task is returned, so a rejection is recorded even
        // if the caller discards the undo handle — which every caller currently
        // does.
        if let Some(undo) = &undo_info {
            self.retain_rejection(undo, cx);
        }

        (task, undo_info)
    }

    /// Records a rejection so it stays visible and re-acceptable.
    fn retain_rejection(&mut self, undo: &PerBufferUndo, cx: &mut Context<Self>) {
        for (range, agent_text) in &undo.edits_to_restore {
            let id = RejectedHunkId(self.next_rejected_hunk_id);
            self.next_rejected_hunk_id += 1;
            self.rejected_hunks.push(RejectedHunk {
                id,
                buffer: undo.buffer.clone(),
                range: range.clone(),
                agent_text: agent_text.clone(),
            });
        }
        cx.notify();
    }

    /// Rejections made so far, oldest first.
    pub fn rejected_hunks(&self) -> &[RejectedHunk] {
        &self.rejected_hunks
    }

    /// Rejections recorded against one buffer.
    pub fn rejected_hunks_for_buffer(
        &self,
        buffer: &Entity<Buffer>,
    ) -> impl Iterator<Item = &RejectedHunk> {
        self.rejected_hunks
            .iter()
            .filter(move |hunk| hunk.buffer.entity_id() == buffer.entity_id())
    }

    /// Takes a rejection back, putting the agent's text where the original was
    /// restored and returning the hunk to review.
    ///
    /// Returns whether the rejection was found and applied.
    pub fn restore_rejected_hunk(&mut self, id: RejectedHunkId, cx: &mut Context<Self>) -> bool {
        let Some(index) = self.rejected_hunks.iter().position(|hunk| hunk.id == id) else {
            return false;
        };
        let hunk = self.rejected_hunks.remove(index);
        let Some(buffer) = hunk.buffer.upgrade() else {
            // The buffer is gone; dropping the record is the only option left,
            // and it is better than keeping a row that can never be acted on.
            cx.notify();
            return false;
        };

        // Anything typed since the log last looked is the user's, and has to
        // be taken in as theirs before the restored text is marked the agent's.
        if self.tracked_buffers.contains_key(&buffer) {
            self.catch_up(&buffer, cx);
        } else {
            self.buffer_read(buffer.clone(), cx);
        }
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(hunk.range.clone(), hunk.agent_text.clone())], None, cx);
        });
        // Re-tracked as an agent edit, so the restored hunk reappears in review
        // rather than being mistaken for something the user typed.
        self.buffer_edited(buffer.clone(), cx);
        // The undo of the rejection this restores would put the same text back
        // a second time.
        if let Some(undo) = self.last_reject_undo.as_mut() {
            for per_buffer in &mut undo.buffers {
                if per_buffer.buffer.entity_id() == buffer.entity_id() {
                    per_buffer.edits_to_restore.retain(|(range, agent_text)| {
                        *range != hunk.range || *agent_text != hunk.agent_text
                    });
                }
            }
            undo.buffers
                .retain(|per_buffer| !per_buffer.edits_to_restore.is_empty());
            if undo.buffers.is_empty() {
                self.last_reject_undo = None;
            }
        }
        // Rejecting wrote the original to disk; taking it back has to as well,
        // or an agent reading the file sees the rejection still in force.
        self.project
            .update(cx, |project, cx| project.save_buffer(buffer, cx))
            .detach_and_log_err(cx);
        cx.notify();
        true
    }

    /// Forgets retained rejections. Called when a review session ends, so a new
    /// one does not inherit the previous one's list.
    pub fn clear_rejected_hunks(&mut self, cx: &mut Context<Self>) {
        if !self.rejected_hunks.is_empty() {
            self.rejected_hunks.clear();
            cx.notify();
        }
    }

    pub fn keep_all_edits(
        &mut self,
        telemetry: Option<ActionLogTelemetry>,
        cx: &mut Context<Self>,
    ) {
        self.answered_this_turn
            .extend(self.tracked_buffers.keys().map(|buffer| buffer.entity_id()));
        self.tracked_buffers.retain(|buffer, tracked_buffer| {
            let mut metrics = ActionLogMetrics::for_buffer(buffer.read(cx));
            metrics.add_edits(tracked_buffer.unreviewed_edits.edits());
            if let Some(telemetry) = telemetry.as_ref() {
                telemetry_report_accepted_edits(telemetry, metrics);
            }
            match tracked_buffer.status {
                TrackedBufferStatus::Deleted => false,
                _ => {
                    if let TrackedBufferStatus::Created { .. } = &mut tracked_buffer.status {
                        tracked_buffer.status = TrackedBufferStatus::Modified;
                    }
                    if tracked_buffer.snapshot.version() != &buffer.read(cx).version() {
                        tracked_buffer.observe(ChangeAuthor::User, cx);
                    }
                    let accepted = Self::all_changes(tracked_buffer);
                    tracked_buffer.remember_accepted(&accepted);
                    tracked_buffer.diff_base = tracked_buffer.snapshot.as_rope().clone();
                    tracked_buffer.refresh();
                    true
                }
            }
        });

        cx.notify();
    }

    pub fn reject_all_edits(
        &mut self,
        telemetry: Option<ActionLogTelemetry>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        // Clear any previous undo state before starting a new reject operation
        self.last_reject_undo = None;

        let mut undo_buffers = Vec::new();
        let mut futures = Vec::new();

        for buffer in self
            .changed_buffers(cx)
            .map(|(buffer, _)| buffer)
            .collect::<Vec<_>>()
        {
            let (reject_task, undo_info) =
                self.reject_all_edits_in_buffer(buffer, telemetry.clone(), cx);

            if let Some(undo) = undo_info {
                undo_buffers.push(undo);
            }

            futures.push(async move {
                reject_task.await.log_err();
            });
        }

        // Store the undo information if we have any
        if !undo_buffers.is_empty() {
            self.last_reject_undo = Some(LastRejectUndo {
                buffers: undo_buffers,
            });
        }

        let task = futures::future::join_all(futures);
        cx.background_spawn(async move {
            task.await;
        })
    }

    pub fn has_pending_undo(&self) -> bool {
        self.last_reject_undo.is_some()
    }

    pub fn set_last_reject_undo(&mut self, undo: LastRejectUndo) {
        self.last_reject_undo = Some(undo);
    }

    /// Undoes the most recent reject operation, restoring the rejected agent changes.
    /// This is a best-effort operation: if buffers have been closed or modified externally,
    /// those buffers will be skipped.
    pub fn undo_last_reject(&mut self, cx: &mut Context<Self>) -> Task<()> {
        let Some(undo) = self.last_reject_undo.take() else {
            return Task::ready(());
        };

        let mut save_tasks = Vec::with_capacity(undo.buffers.len());

        for per_buffer_undo in undo.buffers {
            // Skip if the buffer entity has been deallocated
            let Some(buffer) = per_buffer_undo.buffer.upgrade() else {
                continue;
            };

            let buffer_id = buffer.read(cx).remote_id();
            let valid_edits = per_buffer_undo
                .edits_to_restore
                .into_iter()
                .filter(|(anchor_range, _)| {
                    anchor_range.start.buffer_id == buffer_id
                        && anchor_range.end.buffer_id == buffer_id
                })
                .collect::<Vec<_>>();
            if valid_edits.is_empty() {
                continue;
            }

            // The text as it is now is the base the restored lines are
            // measured against. Applied as a plain edit and left to the edit
            // event, the restored lines were taken for the user's typing and
            // folded into the base: undo silently accepted them.
            if self.tracked_buffers.contains_key(&buffer) {
                self.catch_up(&buffer, cx);
            } else {
                self.buffer_read(buffer.clone(), cx);
            }

            let entity_id = buffer.entity_id();
            self.rejected_hunks.retain(|hunk| {
                hunk.buffer.entity_id() != entity_id
                    || !valid_edits.iter().any(|(range, agent_text)| {
                        *range == hunk.range && *agent_text == hunk.agent_text
                    })
            });

            buffer.update(cx, |buffer, cx| {
                buffer.edit(valid_edits, None, cx);
            });
            self.buffer_edited(buffer.clone(), cx);

            let save = self
                .project
                .update(cx, |project, cx| project.save_buffer(buffer, cx));
            save_tasks.push(save);
        }

        cx.notify();

        cx.background_spawn(async move {
            futures::future::join_all(save_tasks).await;
        })
    }

    /// Returns the set of buffers that contain edits that haven't been reviewed by the user.
    pub fn changed_buffers(
        &self,
        cx: &App,
    ) -> impl Iterator<Item = (Entity<Buffer>, Entity<BufferDiff>)> {
        self.tracked_buffers
            .iter()
            .filter(|(_, tracked)| tracked.has_edits(cx))
            .map(|(buffer, tracked)| (buffer.clone(), tracked.diff.clone()))
    }

    /// Every buffer the review shows: those with changes still to answer, and
    /// those whose changes were accepted in this review, which stay on screen
    /// for reference. Each comes with the lines accepted in it.
    pub fn review_buffers(
        &self,
        cx: &App,
    ) -> Vec<(Entity<Buffer>, Entity<BufferDiff>, Vec<Range<Anchor>>)> {
        self.tracked_buffers
            .iter()
            .filter(|(_, tracked)| tracked.has_edits(cx) || !tracked.accepted.is_empty())
            .map(|(buffer, tracked)| {
                (
                    buffer.clone(),
                    tracked.diff.clone(),
                    tracked.accepted.clone(),
                )
            })
            .collect()
    }

    /// Returns the total number of lines added and removed across all unreviewed buffers.
    pub fn diff_stats(&self, cx: &App) -> DiffStats {
        DiffStats::all_files(self.changed_buffers(cx), cx)
    }

    /// Iterate over buffers changed since last read or edited by the model
    pub fn stale_buffers<'a>(&'a self, cx: &'a App) -> impl Iterator<Item = &'a Entity<Buffer>> {
        self.tracked_buffers
            .iter()
            .filter(|(buffer, tracked)| {
                let buffer = buffer.read(cx);

                tracked.version != buffer.version
                    && buffer
                        .file()
                        .is_some_and(|file| !file.disk_state().is_deleted())
            })
            .map(|(buffer, _)| buffer)
    }
}

#[derive(Default, Debug, Clone, Copy)]
pub struct DiffStats {
    pub lines_added: u32,
    pub lines_removed: u32,
}

impl DiffStats {
    pub fn single_file(diff: &BufferDiff) -> Self {
        let (lines_added, lines_removed) = diff.changed_row_counts();
        DiffStats {
            lines_added,
            lines_removed,
        }
    }

    pub fn all_files(
        changed_buffers: impl IntoIterator<Item = (Entity<Buffer>, Entity<BufferDiff>)>,
        cx: &App,
    ) -> Self {
        let mut total = DiffStats::default();
        for (_, diff) in changed_buffers {
            let stats = DiffStats::single_file(diff.read(cx));
            total.lines_added += stats.lines_added;
            total.lines_removed += stats.lines_removed;
        }
        total
    }
}

#[derive(Clone)]
pub struct ActionLogTelemetry {
    pub agent_telemetry_id: SharedString,
    pub session_id: Arc<str>,
}

struct ActionLogMetrics {
    lines_removed: u32,
    lines_added: u32,
    language: Option<SharedString>,
}

impl ActionLogMetrics {
    fn for_buffer(buffer: &Buffer) -> Self {
        Self {
            language: buffer.language().map(|l| l.name().0),
            lines_removed: 0,
            lines_added: 0,
        }
    }

    fn add_edits(&mut self, edits: &[Edit<u32>]) {
        for edit in edits {
            self.add_edit(edit);
        }
    }

    fn add_edit(&mut self, edit: &Edit<u32>) {
        self.lines_added += edit.new_len();
        self.lines_removed += edit.old_len();
    }
}

fn telemetry_report_accepted_edits(telemetry: &ActionLogTelemetry, metrics: ActionLogMetrics) {
    telemetry::event!(
        "Agent Edits Accepted",
        agent = telemetry.agent_telemetry_id,
        session = telemetry.session_id,
        language = metrics.language,
        lines_added = metrics.lines_added,
        lines_removed = metrics.lines_removed
    );
}

fn telemetry_report_rejected_edits(telemetry: &ActionLogTelemetry, metrics: ActionLogMetrics) {
    telemetry::event!(
        "Agent Edits Rejected",
        agent = telemetry.agent_telemetry_id,
        session = telemetry.session_id,
        language = metrics.language,
        lines_added = metrics.lines_added,
        lines_removed = metrics.lines_removed
    );
}

fn apply_non_conflicting_edits(
    patch: &Patch<u32>,
    edits: Vec<Edit<u32>>,
    old_text: &mut Rope,
    new_text: &Rope,
) -> bool {
    let mut old_edits = patch.edits().iter().cloned().peekable();
    let mut new_edits = edits.into_iter().peekable();
    let mut applied_delta = 0i32;
    let mut rebased_delta = 0i32;
    let mut has_made_changes = false;

    while let Some(mut new_edit) = new_edits.next() {
        let mut conflict = false;

        // Push all the old edits that are before this new edit or that intersect with it.
        while let Some(old_edit) = old_edits.peek() {
            if new_edit.old.end < old_edit.new.start
                || (!old_edit.new.is_empty() && new_edit.old.end == old_edit.new.start)
            {
                break;
            } else if new_edit.old.start > old_edit.new.end
                || (!old_edit.new.is_empty() && new_edit.old.start == old_edit.new.end)
            {
                let old_edit = old_edits.next().unwrap();
                rebased_delta += old_edit.new_len() as i32 - old_edit.old_len() as i32;
            } else {
                conflict = true;
                if new_edits
                    .peek()
                    .is_some_and(|next_edit| next_edit.old.overlaps(&old_edit.new))
                {
                    new_edit = new_edits.next().unwrap();
                } else {
                    let old_edit = old_edits.next().unwrap();
                    rebased_delta += old_edit.new_len() as i32 - old_edit.old_len() as i32;
                }
            }
        }

        if !conflict {
            // This edit doesn't intersect with any old edit, so we can apply it to the old text.
            new_edit.old.start = (new_edit.old.start as i32 + applied_delta - rebased_delta) as u32;
            new_edit.old.end = (new_edit.old.end as i32 + applied_delta - rebased_delta) as u32;
            let old_bytes = old_text.point_to_offset(Point::new(new_edit.old.start, 0))
                ..old_text.point_to_offset(cmp::min(
                    Point::new(new_edit.old.end, 0),
                    old_text.max_point(),
                ));
            let new_bytes = new_text.point_to_offset(Point::new(new_edit.new.start, 0))
                ..new_text.point_to_offset(cmp::min(
                    Point::new(new_edit.new.end, 0),
                    new_text.max_point(),
                ));

            old_text.replace(
                old_bytes,
                &new_text.chunks_in_range(new_bytes).collect::<String>(),
            );
            applied_delta += new_edit.new_len() as i32 - new_edit.old_len() as i32;
            has_made_changes = true;
        }
    }
    has_made_changes
}

fn diff_snapshots(
    old_snapshot: &text::BufferSnapshot,
    new_snapshot: &text::BufferSnapshot,
) -> Vec<Edit<u32>> {
    let mut edits = new_snapshot
        .edits_since::<Point>(&old_snapshot.version)
        .map(|edit| point_to_row_edit(edit, old_snapshot.as_rope(), new_snapshot.as_rope()))
        .peekable();
    let mut row_edits = Vec::new();
    while let Some(mut edit) = edits.next() {
        while let Some(next_edit) = edits.peek() {
            if edit.old.end >= next_edit.old.start {
                edit.old.end = next_edit.old.end;
                edit.new.end = next_edit.new.end;
                edits.next();
            } else {
                break;
            }
        }
        row_edits.push(edit);
    }
    row_edits
}

fn point_to_row_edit(edit: Edit<Point>, old_text: &Rope, new_text: &Rope) -> Edit<u32> {
    if edit.old.start.column == old_text.line_len(edit.old.start.row)
        && new_text
            .chars_at(new_text.point_to_offset(edit.new.start))
            .next()
            == Some('\n')
        && edit.old.start != old_text.max_point()
    {
        Edit {
            old: edit.old.start.row + 1..edit.old.end.row + 1,
            new: edit.new.start.row + 1..edit.new.end.row + 1,
        }
    } else if edit.old.start.column == 0 && edit.old.end.column == 0 && edit.new.end.column == 0 {
        Edit {
            old: edit.old.start.row..edit.old.end.row,
            new: edit.new.start.row..edit.new.end.row,
        }
    } else {
        Edit {
            old: edit.old.start.row..edit.old.end.row + 1,
            new: edit.new.start.row..edit.new.end.row + 1,
        }
    }
}

/// A base text, the buffer it is compared with, and the changes between them.
type DiffUpdate = (
    Arc<str>,
    text::BufferSnapshot,
    Arc<[(Range<u32>, Range<u32>)]>,
);

#[derive(Copy, Clone, Debug)]
enum ChangeAuthor {
    User,
    Agent,
}

#[derive(Debug)]
enum TrackedBufferStatus {
    Created { existing_file_content: Option<Rope> },
    Modified,
    Deleted,
}

pub struct TrackedBuffer {
    buffer: Entity<Buffer>,
    /// Set while an agent that writes to disk with its own tools has said it is
    /// working on this file. Its writes reach the buffer as reloads, and a
    /// reload on its own looks exactly like the user editing -- which folds the
    /// change into the base and leaves nothing to review. While this is set, a
    /// reload is recorded as the agent's edit instead.
    agent_writes_to_disk: bool,
    /// Whether `diff_base` was worked out this turn from the agent's report of
    /// its edits rather than taken from the file before they landed. Such a
    /// base is replaced when a later report covers more of the turn.
    base_from_report: bool,
    diff_base: Rope,
    /// The changes from `diff_base` to `snapshot` still to be answered, as row
    /// ranges. Always recomputed from those two, never kept separately.
    unreviewed_edits: Patch<u32>,
    /// Lines accepted in this review. They are no longer changes, so they are
    /// no longer highlighted, but they stay in the review pane for reference.
    accepted: Vec<Range<Anchor>>,
    /// Whether the user has typed in the buffer since it was tracked. The
    /// log's own reverts are not counted.
    user_edited: bool,
    status: TrackedBufferStatus,
    version: clock::Global,
    diff: Entity<BufferDiff>,
    snapshot: text::BufferSnapshot,
    /// What is left to answer, one change per line, as rows of `diff_base`
    /// and of `snapshot`. The diff is drawn from these, and answering a line
    /// takes that line out and leaves the rest paired as they were.
    changes: Vec<(Range<u32>, Range<u32>)>,
    diff_update: mpsc::UnboundedSender<DiffUpdate>,
    _open_lsp_handle: OpenLspBufferHandle,
    _maintain_diff: Task<()>,
    _subscription: Subscription,
}

impl TrackedBuffer {
    #[cfg(any(test, feature = "test-support"))]
    pub fn diff(&self) -> &Entity<BufferDiff> {
        &self.diff
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn diff_base_len(&self) -> usize {
        self.diff_base.len()
    }

    fn has_edits(&self, _cx: &App) -> bool {
        !self.unreviewed_edits.is_empty()
    }

    /// Takes in the buffer as it is now.
    ///
    /// A change the user made is folded into the base wherever it does not
    /// touch a change still under review, so it is never mistaken for the
    /// agent's. A change the agent made stays out of the base: it is what is
    /// being reviewed.
    fn observe(&mut self, author: ChangeAuthor, cx: &App) {
        let new_snapshot = self.buffer.read(cx).text_snapshot();
        // Already taken in, as a rejection's own edit is. Working the changes
        // out again would throw away how the remaining lines are paired.
        if new_snapshot.version() == self.snapshot.version() {
            return;
        }
        if let ChangeAuthor::User = author {
            self.user_edited = true;
        }
        // A new file is the agent's change as a whole until some of it is
        // accepted, so nothing typed into it is set apart as the user's: an
        // agent's write that arrives as a plain edit before the agent says so
        // would otherwise be taken as the user's and never offered for review.
        let whole_file_under_review =
            matches!(self.status, TrackedBufferStatus::Created { .. }) && self.diff_base.len() == 0;
        if let ChangeAuthor::User = author
            && !whole_file_under_review
        {
            let edits = diff_snapshots(&self.snapshot, &new_snapshot);
            apply_non_conflicting_edits(
                &self.unreviewed_edits,
                edits,
                &mut self.diff_base,
                new_snapshot.as_rope(),
            );
        }
        self.snapshot = new_snapshot;
        self.refresh();
    }

    /// Works out afresh what is left to answer and redraws the diff.
    fn refresh(&mut self) {
        let base_text = self.diff_base.to_string();
        let buffer_text = self.snapshot.text();
        self.changes = buffer_diff::line_changes(&base_text, &buffer_text, true);
        self.publish(base_text);
    }

    /// Takes the answered lines out of what is left to answer, after the base
    /// (kept) or the buffer (rejected) was edited to agree with the other side
    /// on them.
    ///
    /// Every other line stays paired as it was, with its rows moved by the
    /// lines added or removed before it. Working the changes out afresh
    /// instead can pair identical lines differently each time, so a line
    /// nobody answered would turn from added into deleted-and-added. The
    /// result is checked against the texts, and worked out afresh only if it
    /// does not describe them.
    fn settle_answers(&mut self, answered: &[ReviewedChange], answer: Answer) {
        let mut shift = 0i64;
        let mut remaining = Vec::with_capacity(self.changes.len());
        let mut out_of_range = false;
        for (base_rows, buffer_rows) in &self.changes {
            if answered
                .iter()
                .any(|change| change.base_rows == *base_rows && change.buffer_rows == *buffer_rows)
            {
                let base_len = i64::from(base_rows.end - base_rows.start);
                let buffer_len = i64::from(buffer_rows.end - buffer_rows.start);
                shift += match answer {
                    Answer::Kept => buffer_len - base_len,
                    Answer::Rejected => base_len - buffer_len,
                };
                continue;
            }
            let shifted = |rows: &Range<u32>| -> Option<Range<u32>> {
                Some(
                    u32::try_from(i64::from(rows.start) + shift).ok()?
                        ..u32::try_from(i64::from(rows.end) + shift).ok()?,
                )
            };
            let moved = match answer {
                Answer::Kept => shifted(base_rows).map(|rows| (rows, buffer_rows.clone())),
                Answer::Rejected => shifted(buffer_rows).map(|rows| (base_rows.clone(), rows)),
            };
            match moved {
                Some(change) => remaining.push(change),
                None => out_of_range = true,
            }
        }
        let base_text = self.diff_base.to_string();
        let buffer_text = self.snapshot.text();
        match (!out_of_range)
            .then(|| describe_exactly(&base_text, &buffer_text, remaining))
            .flatten()
        {
            Some(changes) => {
                self.changes = changes;
                self.publish(base_text);
            }
            None => {
                log::warn!("agent review: remaining changes no longer line up, recomputing them");
                self.refresh();
            }
        }
    }

    fn publish(&mut self, base_text: String) {
        let mut edits: Vec<Edit<u32>> = Vec::with_capacity(self.changes.len());
        // One run of touching lines is one edit: a `Patch` cannot hold edits
        // that touch.
        for (old, new) in &self.changes {
            match edits.last_mut() {
                Some(last) if old.start <= last.old.end || new.start <= last.new.end => {
                    last.old.end = last.old.end.max(old.end);
                    last.new.end = last.new.end.max(new.end);
                }
                _ => edits.push(Edit {
                    old: old.clone(),
                    new: new.clone(),
                }),
            }
        }
        self.unreviewed_edits = Patch::new(edits);
        self.diff_update
            .unbounded_send((
                Arc::from(base_text),
                self.snapshot.clone(),
                Arc::from(self.changes.clone()),
            ))
            .ok();
    }

    /// Records accepted lines so the review pane keeps showing them.
    fn remember_accepted(&mut self, changes: &[ReviewedChange]) {
        let snapshot = &self.snapshot;
        for change in changes {
            let bytes = change.buffer_bytes(snapshot);
            self.accepted
                .push(snapshot.anchor_before(bytes.start)..snapshot.anchor_after(bytes.end));
        }
    }
}

/// Which of a buffer's changes an answer is for.
///
/// The whole buffer is said outright rather than as a range over it: a range
/// spanning a one-line file is also exactly that line's hunk, and taking it as
/// the whole file answered the deletion after the line along with it.
enum Selection {
    Ranges(Vec<Range<usize>>),
    Everything,
}

#[derive(Clone, Copy)]
enum Answer {
    Kept,
    Rejected,
}

/// `changes` if they describe `base` against `buffer` exactly -- in order,
/// within both texts, with identical lines between them -- leaving out any
/// whose two sides have come to read the same. `None` if they do not.
fn describe_exactly(
    base: &str,
    buffer: &str,
    changes: Vec<(Range<u32>, Range<u32>)>,
) -> Option<Vec<(Range<u32>, Range<u32>)>> {
    let base_lines = base.split_inclusive('\n').collect::<Vec<_>>();
    let buffer_lines = buffer.split_inclusive('\n').collect::<Vec<_>>();
    let mut base_row = 0usize;
    let mut buffer_row = 0usize;
    let mut described = Vec::with_capacity(changes.len());
    for (base_rows, buffer_rows) in changes {
        let base_range = base_rows.start as usize..base_rows.end as usize;
        let buffer_range = buffer_rows.start as usize..buffer_rows.end as usize;
        let unchanged_before = base_lines.get(base_row..base_range.start)?;
        if unchanged_before != buffer_lines.get(buffer_row..buffer_range.start)? {
            return None;
        }
        let base_side = base_lines.get(base_range.clone())?;
        let buffer_side = buffer_lines.get(buffer_range.clone())?;
        if base_side != buffer_side {
            described.push((base_rows, buffer_rows));
        }
        base_row = base_range.end;
        buffer_row = buffer_range.end;
    }
    (base_lines.get(base_row..)? == buffer_lines.get(buffer_row..)?).then_some(described)
}

/// Where a run of whole lines goes in `target`, and the text to put there.
///
/// Only a file's last line may lack a newline. A line that was last on one
/// side but is not on the other has to gain one, or it runs into the line
/// after it ("b" put back before "c\n" made "bc\n"); and a line added after an
/// unterminated last line has to start a new line rather than extend it.
fn splice_rows(target: &Rope, rows: Range<u32>, mut text: String) -> (Range<usize>, String) {
    let len = target.len();
    let max = target.max_point();
    let range = target.point_to_offset(cmp::min(Point::new(rows.start, 0), max))
        ..target.point_to_offset(cmp::min(Point::new(rows.end, 0), max));
    if !text.is_empty() {
        if range.end < len && !text.ends_with('\n') {
            text.push('\n');
        }
        let unterminated = target
            .reversed_chars_at(len)
            .next()
            .is_some_and(|c| c != '\n');
        if range.start == len && unterminated {
            text.insert(0, '\n');
        }
    }
    (range, text)
}

fn offset_ranges(
    snapshot: &text::BufferSnapshot,
    ranges: Vec<Range<impl language::ToPoint>>,
) -> Vec<Range<usize>> {
    ranges
        .into_iter()
        .map(|range| {
            snapshot.point_to_offset(range.start.to_point(snapshot))
                ..snapshot.point_to_offset(range.end.to_point(snapshot))
        })
        .collect()
}

/// Whether the buffer's latest change was a reload from disk: clean, and at the
/// version the reload marked as saved.
fn was_reloaded(buffer: &Buffer) -> bool {
    !buffer.is_dirty() && *buffer.saved_version() == buffer.version()
}

fn buffer_display_path(buffer: &Entity<Buffer>, cx: &App) -> String {
    buffer
        .read(cx)
        .file()
        .map(|file| file.full_path(cx).to_string_lossy().into_owned())
        .unwrap_or_else(|| "an unsaved buffer".to_string())
}

/// One line of the agent's change, as the rows it occupies on each side.
///
/// Rows rather than byte offsets, because the text on either side is edited as
/// changes are answered and offsets taken beforehand would be stale by the
/// second one.
struct ReviewedChange {
    base_rows: Range<u32>,
    buffer_rows: Range<u32>,
}

impl ReviewedChange {
    fn base_bytes(&self, base: &Rope) -> Range<usize> {
        base.point_to_offset(cmp::min(
            Point::new(self.base_rows.start, 0),
            base.max_point(),
        ))
            ..base.point_to_offset(cmp::min(
                Point::new(self.base_rows.end, 0),
                base.max_point(),
            ))
    }

    fn buffer_bytes(&self, buffer: &text::BufferSnapshot) -> Range<usize> {
        let max = buffer.max_point();
        buffer.point_to_offset(cmp::min(Point::new(self.buffer_rows.start, 0), max))
            ..buffer.point_to_offset(cmp::min(Point::new(self.buffer_rows.end, 0), max))
    }

    fn buffer_text(&self, buffer: &text::BufferSnapshot) -> String {
        buffer.text_for_range(self.buffer_bytes(buffer)).collect()
    }

    /// For the telemetry counts, which are in rows.
    fn as_edit(&self) -> Edit<u32> {
        Edit {
            old: self.base_rows.clone(),
            new: self.buffer_rows.clone(),
        }
    }
}

pub struct ChangedBuffer {
    pub diff: Entity<BufferDiff>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use buffer_diff::DiffHunkStatusKind;
    use gpui::TestAppContext;
    use language::{OffsetRangeExt, Point};
    use project::{FakeFs, Fs, Project, RemoveOptions};
    use rand::prelude::*;
    use serde_json::json;
    use settings::SettingsStore;
    use std::env;
    use util::{RandomCharIter, path};

    #[ctor::ctor(unsafe)]
    fn init_logger() {
        zlog::init_test();
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    /// A deliberately dumb model of what hunk review must produce.
    ///
    /// Plain `String`, flat byte ranges, offset arithmetic done by hand. It
    /// shares nothing with the code under test — no rope, no anchors, no diff
    /// engine, no `TrackedBuffer`. That is the entire point: an oracle built
    /// from the machinery under test only proves self-consistency, and a
    /// consistently-wrong implementation satisfies it.
    ///
    /// This is not hypothetical. `test_random_diffs` in this file asserts that
    /// `diff_base` with `unreviewed_edits` applied equals the buffer — both
    /// sides are state under test, so it cannot distinguish "correct" from
    /// "wrong in the same way twice".
    ///
    /// Slow and ugly is fine. It never ships.
    #[derive(Debug, Clone)]
    struct ReferenceCorpus {
        original: String,
        /// Sorted by start, non-overlapping, byte ranges into `original`.
        hunks: Vec<ReferenceHunk>,
    }

    #[derive(Debug, Clone)]
    struct ReferenceHunk {
        range: Range<usize>,
        replacement: String,
    }

    /// Shapes the generator must actually produce. Checked after generation,
    /// because a generator that only emits tidy well-separated hunks yields a
    /// confident green that means nothing.
    #[derive(Debug, Default, Clone, Copy)]
    struct CorpusCoverage {
        /// Consecutive hunks with exactly one untouched line between them —
        /// the tightest packing that still yields two distinct diff hunks.
        minimally_separated: usize,
        touches_first_line: usize,
        touches_last_line: usize,
        pure_deletion: usize,
        pure_insertion: usize,
        partial_line: usize,
    }

    impl ReferenceCorpus {
        /// Splices `original`, taking each hunk's replacement in order.
        fn splice(&self, replacement_for: impl Fn(usize, &ReferenceHunk) -> String) -> String {
            let mut out = String::new();
            let mut cursor = 0usize;
            for (index, hunk) in self.hunks.iter().enumerate() {
                out.push_str(&self.original[cursor..hunk.range.start]);
                out.push_str(&replacement_for(index, hunk));
                cursor = hunk.range.end;
            }
            out.push_str(&self.original[cursor..]);
            out
        }

        /// The text as the agent leaves it: every hunk applied.
        fn agent_text(&self) -> String {
            self.splice(|_, hunk| hunk.replacement.clone())
        }

        /// The text review must produce: accepted hunks keep the agent's bytes,
        /// rejected hunks are back to the original bytes exactly.
        fn expected(&self, accepted: &[bool]) -> String {
            self.splice(|index, hunk| {
                if accepted.get(index).copied().unwrap_or(false) {
                    hunk.replacement.clone()
                } else {
                    self.original[hunk.range.clone()].to_string()
                }
            })
        }

        /// Where each hunk sits in `agent_text`, by hand-carried delta.
        fn agent_ranges(&self) -> Vec<Range<usize>> {
            let mut ranges = Vec::with_capacity(self.hunks.len());
            let mut delta: isize = 0;
            for hunk in &self.hunks {
                let start = (hunk.range.start as isize + delta) as usize;
                let end = start + hunk.replacement.len();
                delta += hunk.replacement.len() as isize - hunk.range.len() as isize;
                ranges.push(start..end);
            }
            ranges
        }

        fn coverage(&self) -> CorpusCoverage {
            let mut coverage = CorpusCoverage::default();
            let last_line_start = self.original.rfind('\n').map(|index| index + 1);
            for (index, hunk) in self.hunks.iter().enumerate() {
                if hunk.range.start < 7 {
                    coverage.touches_first_line += 1;
                }
                if last_line_start.is_some_and(|start| hunk.range.start >= start.saturating_sub(7))
                {
                    coverage.touches_last_line += 1;
                }
                if hunk.replacement.is_empty() && !hunk.range.is_empty() {
                    coverage.pure_deletion += 1;
                }
                if hunk.range.is_empty() && !hunk.replacement.is_empty() {
                    coverage.pure_insertion += 1;
                }
                if !hunk.range.is_empty() && hunk.range.len() < 6 {
                    coverage.partial_line += 1;
                }
                if index > 0
                    && let Some(previous) = self.hunks.get(index - 1)
                {
                    let between = &self.original[previous.range.end..hunk.range.start];
                    if between.matches('\n').count() == 2 {
                        // two newlines = exactly one untouched line between hunks
                        coverage.minimally_separated += 1;
                    }
                }
            }
            coverage
        }
    }

    /// Builds a corpus biased towards the shapes that break naive
    /// implementations.
    ///
    /// **Hunks are separated by at least one untouched line, deliberately.**
    /// The review unit is a coalesced, line-aligned diff hunk, not a byte
    /// range: two edits with no untouched line between them merge into a single
    /// hunk, and "accept the first, reject the second" is then not an operation
    /// the product has. Generating those cases tested something that cannot be
    /// expressed rather than something that is broken.
    ///
    /// This is a correction to the *unit*, not a relaxation of the property.
    /// Sub-line ranges, deletions, insertions and first/last-line placement all
    /// remain, and separation is still allowed to be the tightest legal value.
    fn generate_corpus(rng: &mut StdRng) -> ReferenceCorpus {
        let line_count = rng.random_range(6..16);
        let line_width = 7; // "lineNN\n"
        let mut original = String::new();
        for line in 0..line_count {
            original.push_str(&format!("line{line:02}\n"));
        }

        let mut hunks: Vec<ReferenceHunk> = Vec::new();
        let mut line = rng.random_range(0..2);

        while line < line_count && hunks.len() < 5 {
            let line_start = line * line_width;
            // Column range within the line's text, excluding its newline.
            let (range, replacement) = match rng.random_range(0..10) {
                0..2 => {
                    let column = rng.random_range(0..6);
                    (
                        line_start + column..line_start + column,
                        format!("INS{}", rng.random_range(0..100)),
                    )
                }
                2..4 => {
                    let start_column = rng.random_range(0..5);
                    let end_column = rng.random_range(start_column + 1..=6);
                    (
                        line_start + start_column..line_start + end_column,
                        String::new(),
                    )
                }
                4..7 => {
                    let start_column = rng.random_range(0..5);
                    let end_column = rng.random_range(start_column + 1..=6);
                    (
                        line_start + start_column..line_start + end_column,
                        format!("R{}", rng.random_range(0..1000)),
                    )
                }
                _ => (
                    line_start..line_start + 6,
                    format!("WHOLE{}", rng.random_range(0..100)),
                ),
            };

            assert!(original.is_char_boundary(range.start));
            assert!(original.is_char_boundary(range.end));
            hunks.push(ReferenceHunk { range, replacement });

            // At least one untouched line before the next hunk.
            line += if rng.random_bool(0.5) {
                2
            } else {
                rng.random_range(2..4)
            };
        }

        ReferenceCorpus { original, hunks }
    }

    /// P1a — a rejected hunk is kept, not lost, and can be taken back.
    ///
    /// The brief's rule is that a rejection never disappears silently: it stays
    /// in the review list and remains re-acceptable. Upstream computed
    /// everything needed for that while rejecting and handed it to the caller,
    /// which discarded it, so a rejection was unrecoverable in practice.
    /// A file first seen after the agent wrote it is measured against the
    /// text the agent's report says it replaced; once the user has answered
    /// it, a later report for the same write changes nothing.
    #[gpui::test]
    async fn test_reported_base_for_a_file_claimed_after_the_write(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), json!({"file": "one\nTWO\nthree\n"}))
            .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let file_path = project
            .read_with(cx, |project, cx| project.find_project_path("dir/file", cx))
            .unwrap();
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(file_path, cx))
            .await
            .unwrap();
        let changed = |cx: &mut TestAppContext| {
            action_log.read_with(cx, |log, cx| log.changed_buffers(cx).count())
        };

        action_log.update(cx, |log, cx| {
            log.begin_agent_turn(Vec::new(), cx);
            // Claimed only now, after the write: its base is the new text.
            log.agent_will_write(buffer.clone(), cx);
        });
        cx.run_until_parked();
        assert_eq!(changed(cx), 0);

        let before = Rope::from("one\ntwo\nthree\n");
        action_log.update(cx, |log, cx| {
            log.agent_wrote(buffer.clone(), Some(before.clone()), cx)
        });
        cx.run_until_parked();
        assert_eq!(changed(cx), 1);

        action_log.update(cx, |log, cx| log.keep_all_edits(None, cx));
        cx.run_until_parked();
        assert_eq!(changed(cx), 0);

        action_log.update(cx, |log, cx| {
            log.agent_wrote(buffer.clone(), Some(before), cx)
        });
        cx.run_until_parked();
        assert_eq!(changed(cx), 0, "an answered change came back for review");
    }

    #[gpui::test]
    async fn test_rejected_hunks_are_retained_and_restorable(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/dir"),
            json!({"file": "line0\nline1\nline2\nline3\nline4\n"}),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let file_path = project
            .read_with(cx, |project, cx| project.find_project_path("dir/file", cx))
            .unwrap();
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(file_path, cx))
            .await
            .unwrap();

        // Single `cx.update`: effects must not flush between the read, the edit
        // and `buffer_edited`, or the edit is attributed to the user.
        cx.update(|cx| {
            action_log.update(cx, |log, cx| log.buffer_read(buffer.clone(), cx));
            buffer.update(cx, |buffer, cx| {
                buffer
                    .edit(
                        [(Point::new(1, 0)..Point::new(1, 5), "AGENT_ONE")],
                        None,
                        cx,
                    )
                    .unwrap()
            });
            action_log.update(cx, |log, cx| log.buffer_edited(buffer.clone(), cx));
        });
        cx.run_until_parked();

        let task = action_log.update(cx, |log, cx| {
            let (task, _undo) = log.reject_edits_in_ranges(
                buffer.clone(),
                vec![Point::new(1, 0)..Point::new(1, 9)],
                None,
                cx,
            );
            task
        });
        task.await.unwrap();
        cx.run_until_parked();

        // The original is back in the buffer...
        buffer.read_with(cx, |buffer, _| {
            assert_eq!(buffer.text(), "line0\nline1\nline2\nline3\nline4\n");
        });

        // ...and the rejection is still on the books, carrying what the agent
        // had proposed.
        let rejected_id = action_log.read_with(cx, |log, _| {
            let rejected = log.rejected_hunks();
            assert_eq!(
                rejected.len(),
                1,
                "rejection was dropped instead of retained"
            );
            // Line-granular: the hunk spans the whole line, newline included.
            assert_eq!(
                rejected[0].agent_text,
                "AGENT_ONE
"
            );
            rejected[0].id
        });

        let restored = action_log.update(cx, |log, cx| log.restore_rejected_hunk(rejected_id, cx));
        assert!(restored, "restoring a retained rejection should succeed");
        cx.run_until_parked();

        buffer.read_with(cx, |buffer, _| {
            assert_eq!(
                buffer.text(),
                "line0\nAGENT_ONE\nline2\nline3\nline4\n",
                "taking a rejection back should restore the agent's text exactly"
            );
        });

        action_log.read_with(cx, |log, _| {
            assert!(
                log.rejected_hunks().is_empty(),
                "a restored rejection should leave the rejected list"
            );
        });
    }

    /// P1a — a failed write during a multi-file rejection must surface.
    ///
    /// This is the failure the pitch is actually about. Cursor's documented bug
    /// is not "one buffer computed the wrong bytes" — it is forty files where
    /// some apply and some do not, and a review UI that then disagrees with
    /// disk. Single-buffer correctness cannot reach it: it needs more than one
    /// file and a write that fails.
    ///
    /// **The assertion here is deliberately narrow**: a rejection whose save
    /// fails must report an error. That is unambiguous — reporting success for
    /// a write that did not happen is indefensible under any specification.
    ///
    /// What should happen to the *review state* afterwards is a different
    /// question — whether the hunk stays in review, or is marked rejected with
    /// a warning — and it is a spec decision, not something this test may settle
    /// by adopting whatever the implementation currently does. The observed
    /// behaviour is printed rather than asserted, so it informs that decision
    /// without pre-empting it.
    #[gpui::test]
    async fn test_failed_write_during_multi_file_reject_surfaces(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/dir"),
            json!({
                "a.txt": "alpha0\nalpha1\nalpha2\n",
                // Same line width as a.txt, so one column range is valid in both.
                "b.txt": "beta00\nbeta01\nbeta02\n",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let action_log = cx.new(|_| ActionLog::new(project.clone()));

        let mut buffers = Vec::new();
        for name in ["dir/a.txt", "dir/b.txt"] {
            let file_path = project
                .read_with(cx, |project, cx| project.find_project_path(name, cx))
                .unwrap();
            buffers.push(
                project
                    .update(cx, |project, cx| project.open_buffer(file_path, cx))
                    .await
                    .unwrap(),
            );
        }

        // The agent edits both files.
        cx.update(|cx| {
            for (index, buffer) in buffers.iter().enumerate() {
                action_log.update(cx, |log, cx| log.buffer_read(buffer.clone(), cx));
                buffer.update(cx, |buffer, cx| {
                    buffer
                        .edit(
                            [(Point::new(1, 0)..Point::new(1, 6), format!("AGENT{index}"))],
                            None,
                            cx,
                        )
                        .unwrap()
                });
                action_log.update(cx, |log, cx| log.buffer_edited(buffer.clone(), cx));
            }
        });
        cx.run_until_parked();

        // Persist the agent's edits first. Without this, disk still holds the
        // original bytes and the blocked write would only have rewritten what
        // was already there — the assertions below would pass while testing
        // nothing. The dangerous case is disk holding the agent's version at
        // the moment the restore fails.
        for buffer in &buffers {
            project
                .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
                .await
                .unwrap();
        }
        cx.run_until_parked();

        // The second file cannot be written.
        fs.fail_writes_to(path!("/dir/b.txt"), 1);

        let mut outcomes = Vec::new();
        for buffer in &buffers {
            let task = action_log.update(cx, |log, cx| {
                let (task, _undo) = log.reject_edits_in_ranges(
                    buffer.clone(),
                    vec![Point::new(1, 0)..Point::new(1, 6)],
                    None,
                    cx,
                );
                task
            });
            outcomes.push(task.await);
            cx.run_until_parked();
        }

        let on_disk_a = fs.load(path!("/dir/a.txt").as_ref()).await.unwrap();
        let on_disk_b = fs.load(path!("/dir/b.txt").as_ref()).await.unwrap();

        eprintln!(
            "\n--- multi-file reject with an injected write failure ---\n\
             reject a.txt -> {:?}\n\
             reject b.txt -> {:?}\n\
             a.txt on disk = {on_disk_a:?}\n\
             b.txt on disk = {on_disk_b:?}\n\
             rejections retained = {}\n",
            outcomes[0].as_ref().map(|_| "Ok"),
            outcomes[1].as_ref().map(|_| "Ok"),
            action_log.read_with(cx, |log, _| log.rejected_hunks().len()),
        );

        // The file that could be written is back to its original bytes.
        assert_eq!(
            on_disk_a, "alpha0\nalpha1\nalpha2\n",
            "the writable file should have been restored on disk"
        );

        // And the file that could not be written still holds the agent's
        // version. This divergence between review state and disk is the thing
        // the feature exists to make impossible to miss.
        assert_eq!(
            on_disk_b, "beta00\nAGENT1\nbeta02\n",
            "the unwritable file should still hold the agent's text on disk"
        );

        // The one assertion that no specification can excuse.
        assert!(
            outcomes[1].is_err(),
            "rejecting a file whose write failed reported success; \
             a review UI trusting this would show the change reverted while \
             disk still holds the agent's version"
        );
    }

    /// P1a property test — accept/reject round-trips to byte equality.
    ///
    /// Phase one of the corpus, deliberately: **pure accept/reject permutations,
    /// no interleaved user edits.** An edit landing inside a pending hunk has no
    /// unambiguous correct outcome, so including it here would let the test
    /// settle a specification question by adopting whatever the implementation
    /// already does. That phase waits until the semantics are written down.
    ///
    /// This phase is still worth running: it covers decision *order* over
    /// adjacent, zero-context, boundary and empty-range hunks, which is where
    /// offset arithmetic goes wrong.
    ///
    /// Expected bytes come from [`ReferenceCorpus`], which shares nothing with
    /// the code under test.
    #[gpui::test(iterations = 50)]
    async fn test_accept_reject_round_trips_to_byte_equality(
        mut rng: StdRng,
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let corpus = generate_corpus(&mut rng);
        if corpus.hunks.is_empty() {
            return;
        }

        let coverage = corpus.coverage();
        let agent_text = corpus.agent_text();
        let agent_ranges = corpus.agent_ranges();

        let accepted: Vec<bool> = (0..corpus.hunks.len())
            .map(|_| rng.random_bool(0.5))
            .collect();
        let mut order: Vec<usize> = (0..corpus.hunks.len()).collect();
        for index in (1..order.len()).rev() {
            order.swap(index, rng.random_range(0..=index));
        }

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), json!({"file": corpus.original.clone()}))
            .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let file_path = project
            .read_with(cx, |project, cx| project.find_project_path("dir/file", cx))
            .unwrap();
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(file_path, cx))
            .await
            .unwrap();

        // Read, edit and edited must happen inside ONE `cx.update`. Split across
        // separate context calls, GPUI flushes effects between them, the
        // buffer's change event reaches the action log before `buffer_edited`
        // does, and the edits are attributed to the user — leaving nothing
        // recorded as an agent hunk and every rejection a silent no-op.
        cx.update(|cx| {
            action_log.update(cx, |log, cx| log.buffer_read(buffer.clone(), cx));
            // One batched edit, so every range stays in original coordinates.
            buffer.update(cx, |buffer, cx| {
                buffer
                    .edit(
                        corpus
                            .hunks
                            .iter()
                            .map(|hunk| (hunk.range.clone(), hunk.replacement.clone())),
                        None,
                        cx,
                    )
                    .unwrap()
            });
            action_log.update(cx, |log, cx| log.buffer_edited(buffer.clone(), cx));
        });
        cx.run_until_parked();

        buffer.read_with(cx, |buffer, _| {
            pretty_assertions::assert_eq!(
                buffer.text(),
                agent_text,
                "agent edits did not produce the reference agent text"
            );
        });

        // Positions carried by hand as decisions land, the same way the
        // reference model does everything else.
        let mut pending: Vec<Option<Range<usize>>> =
            agent_ranges.iter().cloned().map(Some).collect();

        for &index in &order {
            let Some(range) = pending.get(index).cloned().flatten() else {
                continue;
            };

            if accepted[index] {
                action_log.update(cx, |log, cx| {
                    log.keep_edits_in_range(buffer.clone(), range.clone(), None, cx);
                });
                cx.run_until_parked();
            } else {
                let task = action_log.update(cx, |log, cx| {
                    let (task, _undo) =
                        log.reject_edits_in_ranges(buffer.clone(), vec![range.clone()], None, cx);
                    task
                });
                task.await.unwrap();
                cx.run_until_parked();

                // Rejection puts the original slice back, so everything after it
                // shifts by the length difference.
                let original_len = corpus.hunks[index].range.len();
                let delta = original_len as isize - range.len() as isize;
                for (other, slot) in pending.iter_mut().enumerate() {
                    if other == index {
                        continue;
                    }
                    if let Some(other_range) = slot
                        && other_range.start >= range.end
                    {
                        *slot = Some(
                            (other_range.start as isize + delta) as usize
                                ..(other_range.end as isize + delta) as usize,
                        );
                    }
                }
            }

            if let Some(slot) = pending.get_mut(index) {
                *slot = None;
            }
        }

        let expected = corpus.expected(&accepted);
        buffer.read_with(cx, |buffer, _| {
            pretty_assertions::assert_eq!(
                buffer.text(),
                expected,
                "review result did not match the reference model (corpus {:?}, accepted {:?}, order {:?}, coverage {:?})",
                corpus,
                accepted,
                order,
                coverage
            );
        });
    }

    /// Review the way the panel does it: a hunk per line, each answered on its
    /// own through the range its Keep or Reject button passes, in any order.
    ///
    /// Answering one line must answer that line and nothing else. Keep leaves
    /// the file exactly as it is; Reject puts back that line's original text
    /// and touches nothing around it; every other hunk is still offered,
    /// unchanged; and the review is over after exactly one answer per line.
    #[gpui::test(iterations = 200)]
    async fn test_each_line_is_answered_on_its_own(mut rng: StdRng, cx: &mut TestAppContext) {
        init_test(cx);

        // Few distinct words, so lines repeat and the diff has real choices
        // to make about what lines up with what.
        const WORDS: &[&str] = &["alpha", "beta", "gamma", "delta", "}", ""];
        let random_line = |rng: &mut StdRng| -> String {
            WORDS
                .choose(rng)
                .map(|word| word.to_string())
                .unwrap_or_default()
        };

        let base_lines = (0..rng.random_range(0..10))
            .map(|_| random_line(&mut rng))
            .collect::<Vec<_>>();
        let mut agent_lines = Vec::new();
        for line in &base_lines {
            match rng.random_range(0..10) {
                0 => {}
                1 => agent_lines.push(format!("{line} changed")),
                2 => {
                    agent_lines.push(line.clone());
                    agent_lines.push(random_line(&mut rng));
                }
                3 => {
                    agent_lines.push(random_line(&mut rng));
                    agent_lines.push(line.clone());
                }
                _ => agent_lines.push(line.clone()),
            }
        }
        if rng.random_bool(0.2) {
            agent_lines.push("appended".to_string());
        }
        let join = |lines: &[String], trailing_newline: bool| {
            let mut text = lines.join("\n");
            if trailing_newline && !lines.is_empty() {
                text.push('\n');
            }
            text
        };
        let base_text = join(&base_lines, rng.random_bool(0.8));
        let agent_text = join(&agent_lines, rng.random_bool(0.8));
        if base_text == agent_text {
            return;
        }
        log::info!("base: {base_text:?}");
        log::info!("agent: {agent_text:?}");

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), json!({"file": base_text.clone()}))
            .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let file_path = project
            .read_with(cx, |project, cx| project.find_project_path("dir/file", cx))
            .unwrap();
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(file_path, cx))
            .await
            .unwrap();
        cx.update(|cx| {
            action_log.update(cx, |log, cx| log.buffer_read(buffer.clone(), cx));
            buffer.update(cx, |buffer, cx| buffer.set_text(agent_text.clone(), cx));
            action_log.update(cx, |log, cx| log.buffer_edited(buffer.clone(), cx));
        });
        cx.run_until_parked();

        /// What the panel shows of a hunk: where it is, what it replaced and
        /// what replaced it.
        #[derive(Clone, Debug, PartialEq)]
        struct ShownHunk {
            buffer_range: Range<Anchor>,
            bytes: Range<usize>,
            old_text: String,
            new_text: String,
        }
        let shown_hunks = |cx: &mut TestAppContext| -> Vec<ShownHunk> {
            cx.read(|cx| {
                let Some(tracked) = action_log.read(cx).tracked_buffers.get(&buffer) else {
                    return Vec::new();
                };
                let snapshot = buffer.read(cx).snapshot();
                let diff = tracked.diff.read(cx);
                let base = diff.base_text(cx);
                diff.snapshot(cx)
                    .hunks(&snapshot)
                    .map(|hunk| {
                        let bytes = hunk.buffer_range.to_offset(&snapshot);
                        ShownHunk {
                            buffer_range: hunk.buffer_range.clone(),
                            old_text: base.text_for_range(hunk.diff_base_byte_range).collect(),
                            new_text: snapshot.text_for_range(bytes.clone()).collect(),
                            bytes,
                        }
                    })
                    .collect()
            })
        };
        // A line that differs only by the newline after it is not a change
        // to the code. It shows up when the original's last line had no
        // newline and the agent added lines after it (or the reverse), and it
        // goes away with whichever line next to it is answered, because a line
        // that is not last cannot do without one.
        let newline_only = |hunk: &ShownHunk| {
            hunk.old_text.trim_end_matches('\n') == hunk.new_text.trim_end_matches('\n')
        };
        let texts = |hunks: &[ShownHunk]| {
            hunks
                .iter()
                .filter(|hunk| !newline_only(hunk))
                // Compared without the newline at the end: the unterminated
                // last line gains one when a line is put back after it.
                .map(|hunk| {
                    (
                        hunk.old_text.trim_end_matches('\n').to_string(),
                        hunk.new_text.trim_end_matches('\n').to_string(),
                    )
                })
                .collect::<Vec<_>>()
        };

        let initial_count = shown_hunks(cx).len();
        let initial_lines = texts(&shown_hunks(cx)).len();
        assert!(initial_count > 0, "the agent's change must be offered");
        let mut answers = 0;
        let mut attempts = 0;
        loop {
            let before = shown_hunks(cx);
            let Some(chosen_index) = (0..before.len()).choose(&mut rng) else {
                break;
            };
            let chosen = before[chosen_index].clone();
            let text_before = buffer.read_with(cx, |buffer, _| buffer.text());
            let mut expected_remaining = before.clone();
            expected_remaining.remove(chosen_index);

            attempts += 1;
            assert!(
                attempts <= initial_count * 4,
                "review of {initial_count} line(s) is not finishing; still offered: {before:?}"
            );
            if !newline_only(&chosen) {
                answers += 1;
            }

            if rng.random_bool(0.5) {
                log::info!("keep {chosen:?}");
                action_log.update(cx, |log, cx| {
                    log.keep_edits_in_ranges(
                        buffer.clone(),
                        vec![chosen.buffer_range.clone()],
                        None,
                        cx,
                    )
                });
                cx.run_until_parked();
                assert_eq!(
                    buffer.read_with(cx, |buffer, _| buffer.text()),
                    text_before,
                    "keeping a line must not change the file"
                );
            } else {
                log::info!("reject {chosen:?}");
                let task = action_log.update(cx, |log, cx| {
                    log.reject_edits_in_ranges(
                        buffer.clone(),
                        vec![chosen.buffer_range.clone()],
                        None,
                        cx,
                    )
                    .0
                });
                task.await.unwrap();
                cx.run_until_parked();
                let text_after = buffer.read_with(cx, |buffer, _| buffer.text());
                // Exactly the original line back in place of the agent's, give
                // or take the newline that separates it from its neighbours
                // when one of them was the file's unterminated last line.
                let expected = [
                    chosen.old_text.clone(),
                    format!("{}\n", chosen.old_text),
                    format!("\n{}", chosen.old_text),
                ]
                .map(|restored| {
                    let mut text = text_before.clone();
                    text.replace_range(chosen.bytes.clone(), &restored);
                    text
                });
                assert!(
                    expected.contains(&text_after),
                    "rejecting a line must put back only that line: \
                     expected one of {expected:?}, got {text_after:?}"
                );
            }

            pretty_assertions::assert_eq!(
                texts(&shown_hunks(cx)),
                texts(&expected_remaining),
                "answering one line must leave every other line offered as it was"
            );
        }
        assert_eq!(answers, initial_lines, "one answer per line, no more");
        cx.read(|cx| {
            assert!(
                action_log.read(cx).changed_buffers(cx).next().is_none(),
                "nothing must be left to answer"
            )
        });
    }

    /// Guards the corpus generator itself.
    ///
    /// A generator that only ever emits tidy, well-separated hunks makes
    /// [`test_accept_reject_round_trips_to_byte_equality`] pass for reasons that
    /// have nothing to do with the implementation being correct. This asserts
    /// the nasty shapes actually occur.
    #[gpui::test]
    async fn test_corpus_generator_produces_hard_shapes(_cx: &mut TestAppContext) {
        let mut rng = StdRng::seed_from_u64(0);
        let mut total = CorpusCoverage::default();
        let mut hunks_seen = 0usize;

        for _ in 0..400 {
            let corpus = generate_corpus(&mut rng);
            hunks_seen += corpus.hunks.len();
            let coverage = corpus.coverage();
            total.minimally_separated += coverage.minimally_separated;
            total.touches_first_line += coverage.touches_first_line;
            total.touches_last_line += coverage.touches_last_line;
            total.pure_deletion += coverage.pure_deletion;
            total.pure_insertion += coverage.pure_insertion;
            total.partial_line += coverage.partial_line;
        }

        assert!(
            hunks_seen > 400,
            "generator produced too few hunks: {hunks_seen}"
        );
        assert!(
            total.minimally_separated > 40,
            "generator rarely emits minimally separated hunks: {total:?}"
        );
        assert!(
            total.touches_first_line > 5,
            "generator rarely touches the first line: {total:?}"
        );
        assert!(
            total.touches_last_line > 5,
            "generator rarely touches the last line: {total:?}"
        );
        assert!(
            total.pure_deletion > 20,
            "generator rarely emits pure deletions: {total:?}"
        );
        assert!(
            total.pure_insertion > 20,
            "generator rarely emits pure insertions: {total:?}"
        );
        assert!(
            total.partial_line > 20,
            "generator rarely emits partial-line edits: {total:?}"
        );
    }

    /// P1a — accept/reject stays byte-exact when a user edit lands *between*
    /// pending hunks.
    ///
    /// Scope is deliberately limited to a user edit that falls **outside every
    /// pending hunk's range**, which has one obvious right answer: the decision
    /// must not disturb it. An edit *inside* a pending hunk has no obvious
    /// answer — restoring original bytes silently destroys the user's work,
    /// keeping their bytes means "reject" did not reject — and that is a
    /// specification decision, not something a test may settle by discovering
    /// whatever the implementation happens to do. Those cases are excluded here
    /// and from the corpus below until the semantics are written down.
    ///
    /// What this still catches: the user edit shifts the anchors of the second
    /// hunk before it is reviewed, so an implementation tracking offsets rather
    /// than anchors passes the trivial case and fails this one.
    #[gpui::test(iterations = 25)]
    async fn test_accept_reject_survives_user_edit_between_hunks(cx: &mut TestAppContext) {
        init_test(cx);

        // Distinct per line so a misplaced hunk boundary shows up as wrong
        // content rather than coincidentally-equal text.
        const ORIGINAL: &str = "line0
line1
line2
line3
line4
line5
line6
line7
";

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), json!({"file": ORIGINAL}))
            .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let action_log = cx.new(|_| ActionLog::new(project.clone()));

        let file_path = project
            .read_with(cx, |project, cx| project.find_project_path("dir/file", cx))
            .unwrap();
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(file_path, cx))
            .await
            .unwrap();

        // The agent rewrites two widely separated lines, producing two hunks
        // with untouched context between them.
        // Single `cx.update`: see the note in the property test below.
        cx.update(|cx| {
            action_log.update(cx, |log, cx| log.buffer_read(buffer.clone(), cx));
            buffer.update(cx, |buffer, cx| {
                buffer
                    .edit(
                        [
                            (Point::new(1, 0)..Point::new(1, 5), "AGENT_ONE"),
                            (Point::new(5, 0)..Point::new(5, 5), "AGENT_TWO"),
                        ],
                        None,
                        cx,
                    )
                    .unwrap()
            });
            action_log.update(cx, |log, cx| log.buffer_edited(buffer.clone(), cx));
        });
        cx.run_until_parked();

        // A manual edit between the two hunks, before either is reviewed. This
        // is what shifts the anchors of the second hunk.
        buffer.update(cx, |buffer, cx| {
            buffer.edit(
                [(Point::new(3, 0)..Point::new(3, 5), "USER_EDIT")],
                None,
                cx,
            );
        });
        cx.run_until_parked();

        // Accept the first hunk and reject the second. Order is deliberate:
        // the rejection happens after an accept has already mutated review
        // state, which is the interleaving the brief asks about.
        action_log.update(cx, |log, cx| {
            log.keep_edits_in_range(buffer.clone(), Point::new(1, 0)..Point::new(1, 9), None, cx);
        });
        cx.run_until_parked();

        let (reject, _undo) = action_log.update(cx, |log, cx| {
            log.reject_edits_in_ranges(
                buffer.clone(),
                vec![Point::new(5, 0)..Point::new(5, 9)],
                None,
                cx,
            )
        });
        reject.await.unwrap();
        cx.run_until_parked();

        // Expected: the accepted hunk keeps the agent's bytes, the rejected one
        // is back to the original bytes, and the user's own edit is untouched
        // by either decision.
        let expected = "line0
AGENT_ONE
line2
USER_EDIT
line4
line5
line6
line7
";
        buffer.read_with(cx, |buffer, _| {
            assert_eq!(
                buffer.text(),
                expected,
                "accept/reject did not produce byte-exact content under interleaving"
            );
        });
    }

    #[gpui::test(iterations = 10)]
    async fn test_keep_edits(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), json!({"file": "abc\ndef\nghi\njkl\nmno"}))
            .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let file_path = project
            .read_with(cx, |project, cx| project.find_project_path("dir/file", cx))
            .unwrap();
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(file_path, cx))
            .await
            .unwrap();

        cx.update(|cx| {
            action_log.update(cx, |log, cx| log.buffer_read(buffer.clone(), cx));
            buffer.update(cx, |buffer, cx| {
                buffer
                    .edit([(Point::new(1, 1)..Point::new(1, 2), "E")], None, cx)
                    .unwrap()
            });
            buffer.update(cx, |buffer, cx| {
                buffer
                    .edit([(Point::new(4, 2)..Point::new(4, 3), "O")], None, cx)
                    .unwrap()
            });
            action_log.update(cx, |log, cx| log.buffer_edited(buffer.clone(), cx));
        });
        cx.run_until_parked();
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "abc\ndEf\nghi\njkl\nmnO"
        );
        assert_eq!(
            unreviewed_hunks(&action_log, cx),
            vec![(
                buffer.clone(),
                vec![
                    HunkStatus {
                        range: Point::new(1, 0)..Point::new(2, 0),
                        diff_status: DiffHunkStatusKind::Modified,
                        old_text: "def\n".into(),
                    },
                    HunkStatus {
                        range: Point::new(4, 0)..Point::new(4, 3),
                        diff_status: DiffHunkStatusKind::Modified,
                        old_text: "mno".into(),
                    }
                ],
            )]
        );

        action_log.update(cx, |log, cx| {
            log.keep_edits_in_range(buffer.clone(), Point::new(3, 0)..Point::new(4, 3), None, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            unreviewed_hunks(&action_log, cx),
            vec![(
                buffer.clone(),
                vec![HunkStatus {
                    range: Point::new(1, 0)..Point::new(2, 0),
                    diff_status: DiffHunkStatusKind::Modified,
                    old_text: "def\n".into(),
                }],
            )]
        );

        action_log.update(cx, |log, cx| {
            log.keep_edits_in_range(buffer.clone(), Point::new(0, 0)..Point::new(4, 3), None, cx)
        });
        cx.run_until_parked();
        assert_eq!(unreviewed_hunks(&action_log, cx), vec![]);
    }

    #[gpui::test(iterations = 10)]
    async fn test_deletions(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/dir"),
            json!({"file": "abc\ndef\nghi\njkl\nmno\npqr"}),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let file_path = project
            .read_with(cx, |project, cx| project.find_project_path("dir/file", cx))
            .unwrap();
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(file_path, cx))
            .await
            .unwrap();

        cx.update(|cx| {
            action_log.update(cx, |log, cx| log.buffer_read(buffer.clone(), cx));
            buffer.update(cx, |buffer, cx| {
                buffer
                    .edit([(Point::new(1, 0)..Point::new(2, 0), "")], None, cx)
                    .unwrap();
                buffer.finalize_last_transaction();
            });
            buffer.update(cx, |buffer, cx| {
                buffer
                    .edit([(Point::new(3, 0)..Point::new(4, 0), "")], None, cx)
                    .unwrap();
                buffer.finalize_last_transaction();
            });
            action_log.update(cx, |log, cx| log.buffer_edited(buffer.clone(), cx));
        });
        cx.run_until_parked();
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "abc\nghi\njkl\npqr"
        );
        assert_eq!(
            unreviewed_hunks(&action_log, cx),
            vec![(
                buffer.clone(),
                vec![
                    HunkStatus {
                        range: Point::new(1, 0)..Point::new(1, 0),
                        diff_status: DiffHunkStatusKind::Deleted,
                        old_text: "def\n".into(),
                    },
                    HunkStatus {
                        range: Point::new(3, 0)..Point::new(3, 0),
                        diff_status: DiffHunkStatusKind::Deleted,
                        old_text: "mno\n".into(),
                    }
                ],
            )]
        );

        buffer.update(cx, |buffer, cx| buffer.undo(cx));
        cx.run_until_parked();
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "abc\nghi\njkl\nmno\npqr"
        );
        assert_eq!(
            unreviewed_hunks(&action_log, cx),
            vec![(
                buffer.clone(),
                vec![HunkStatus {
                    range: Point::new(1, 0)..Point::new(1, 0),
                    diff_status: DiffHunkStatusKind::Deleted,
                    old_text: "def\n".into(),
                }],
            )]
        );

        action_log.update(cx, |log, cx| {
            log.keep_edits_in_range(buffer.clone(), Point::new(1, 0)..Point::new(1, 0), None, cx)
        });
        cx.run_until_parked();
        assert_eq!(unreviewed_hunks(&action_log, cx), vec![]);
    }

    #[gpui::test(iterations = 10)]
    async fn test_overlapping_user_edits(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), json!({"file": "abc\ndef\nghi\njkl\nmno"}))
            .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let file_path = project
            .read_with(cx, |project, cx| project.find_project_path("dir/file", cx))
            .unwrap();
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(file_path, cx))
            .await
            .unwrap();

        cx.update(|cx| {
            action_log.update(cx, |log, cx| log.buffer_read(buffer.clone(), cx));
            buffer.update(cx, |buffer, cx| {
                buffer
                    .edit([(Point::new(1, 2)..Point::new(2, 3), "F\nGHI")], None, cx)
                    .unwrap()
            });
            action_log.update(cx, |log, cx| log.buffer_edited(buffer.clone(), cx));
        });
        cx.run_until_parked();
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "abc\ndeF\nGHI\njkl\nmno"
        );
        assert_eq!(
            unreviewed_hunks(&action_log, cx),
            vec![(
                buffer.clone(),
                vec![
                    HunkStatus {
                        range: Point::new(1, 0)..Point::new(2, 0),
                        diff_status: DiffHunkStatusKind::Modified,
                        old_text: "def\n".into(),
                    },
                    HunkStatus {
                        range: Point::new(2, 0)..Point::new(3, 0),
                        diff_status: DiffHunkStatusKind::Modified,
                        old_text: "ghi\n".into(),
                    },
                ],
            )]
        );

        buffer.update(cx, |buffer, cx| {
            buffer.edit(
                [
                    (Point::new(0, 2)..Point::new(0, 2), "X"),
                    (Point::new(3, 0)..Point::new(3, 0), "Y"),
                ],
                None,
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "abXc\ndeF\nGHI\nYjkl\nmno"
        );
        assert_eq!(
            unreviewed_hunks(&action_log, cx),
            vec![(
                buffer.clone(),
                vec![
                    HunkStatus {
                        range: Point::new(1, 0)..Point::new(2, 0),
                        diff_status: DiffHunkStatusKind::Modified,
                        old_text: "def\n".into(),
                    },
                    HunkStatus {
                        range: Point::new(2, 0)..Point::new(3, 0),
                        diff_status: DiffHunkStatusKind::Modified,
                        old_text: "ghi\n".into(),
                    },
                ],
            )]
        );

        buffer.update(cx, |buffer, cx| {
            buffer.edit([(Point::new(1, 1)..Point::new(1, 1), "Z")], None, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "abXc\ndZeF\nGHI\nYjkl\nmno"
        );
        assert_eq!(
            unreviewed_hunks(&action_log, cx),
            vec![(
                buffer.clone(),
                vec![
                    HunkStatus {
                        range: Point::new(1, 0)..Point::new(2, 0),
                        diff_status: DiffHunkStatusKind::Modified,
                        old_text: "def\n".into(),
                    },
                    HunkStatus {
                        range: Point::new(2, 0)..Point::new(3, 0),
                        diff_status: DiffHunkStatusKind::Modified,
                        old_text: "ghi\n".into(),
                    },
                ],
            )]
        );

        action_log.update(cx, |log, cx| {
            // Both rewritten lines, since each is now answered separately.
            log.keep_edits_in_range(buffer.clone(), Point::new(0, 0)..Point::new(3, 0), None, cx)
        });
        cx.run_until_parked();
        assert_eq!(unreviewed_hunks(&action_log, cx), vec![]);
    }

    #[gpui::test(iterations = 10)]
    async fn test_creating_files(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), json!({})).await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let file_path = project
            .read_with(cx, |project, cx| project.find_project_path("dir/file1", cx))
            .unwrap();

        let buffer = project
            .update(cx, |project, cx| project.open_buffer(file_path, cx))
            .await
            .unwrap();
        cx.update(|cx| {
            action_log.update(cx, |log, cx| log.buffer_created(buffer.clone(), cx));
            buffer.update(cx, |buffer, cx| buffer.set_text("lorem", cx));
            action_log.update(cx, |log, cx| log.buffer_edited(buffer.clone(), cx));
        });
        project
            .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
            .await
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            unreviewed_hunks(&action_log, cx),
            vec![(
                buffer.clone(),
                vec![HunkStatus {
                    range: Point::new(0, 0)..Point::new(0, 5),
                    diff_status: DiffHunkStatusKind::Added,
                    old_text: "".into(),
                }],
            )]
        );

        buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "X")], None, cx));
        cx.run_until_parked();
        assert_eq!(
            unreviewed_hunks(&action_log, cx),
            vec![(
                buffer.clone(),
                vec![HunkStatus {
                    range: Point::new(0, 0)..Point::new(0, 6),
                    diff_status: DiffHunkStatusKind::Added,
                    old_text: "".into(),
                }],
            )]
        );

        action_log.update(cx, |log, cx| {
            log.keep_edits_in_range(buffer.clone(), 0..5, None, cx)
        });
        cx.run_until_parked();
        assert_eq!(unreviewed_hunks(&action_log, cx), vec![]);
    }

    #[gpui::test(iterations = 10)]
    async fn test_overwriting_files(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/dir"),
            json!({
                "file1": "Lorem ipsum dolor"
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let file_path = project
            .read_with(cx, |project, cx| project.find_project_path("dir/file1", cx))
            .unwrap();

        let buffer = project
            .update(cx, |project, cx| project.open_buffer(file_path, cx))
            .await
            .unwrap();
        cx.update(|cx| {
            action_log.update(cx, |log, cx| log.buffer_created(buffer.clone(), cx));
            buffer.update(cx, |buffer, cx| buffer.set_text("sit amet consecteur", cx));
            action_log.update(cx, |log, cx| log.buffer_edited(buffer.clone(), cx));
        });
        project
            .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
            .await
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            unreviewed_hunks(&action_log, cx),
            vec![(
                buffer.clone(),
                vec![HunkStatus {
                    range: Point::new(0, 0)..Point::new(0, 19),
                    diff_status: DiffHunkStatusKind::Added,
                    old_text: "".into(),
                }],
            )]
        );

        action_log
            .update(cx, |log, cx| {
                let (task, _) = log.reject_edits_in_ranges(buffer.clone(), vec![2..5], None, cx);
                task
            })
            .await
            .unwrap();
        cx.run_until_parked();
        assert_eq!(unreviewed_hunks(&action_log, cx), vec![]);
        assert_eq!(
            buffer.read_with(cx, |buffer, _cx| buffer.text()),
            "Lorem ipsum dolor"
        );
    }

    #[gpui::test(iterations = 10)]
    async fn test_overwriting_previously_edited_files(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/dir"),
            json!({
                "file1": "Lorem ipsum dolor"
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let file_path = project
            .read_with(cx, |project, cx| project.find_project_path("dir/file1", cx))
            .unwrap();

        let buffer = project
            .update(cx, |project, cx| project.open_buffer(file_path, cx))
            .await
            .unwrap();
        cx.update(|cx| {
            action_log.update(cx, |log, cx| log.buffer_read(buffer.clone(), cx));
            buffer.update(cx, |buffer, cx| buffer.append(" sit amet consecteur", cx));
            action_log.update(cx, |log, cx| log.buffer_edited(buffer.clone(), cx));
        });
        project
            .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
            .await
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            unreviewed_hunks(&action_log, cx),
            vec![(
                buffer.clone(),
                vec![HunkStatus {
                    range: Point::new(0, 0)..Point::new(0, 37),
                    diff_status: DiffHunkStatusKind::Modified,
                    old_text: "Lorem ipsum dolor".into(),
                }],
            )]
        );

        cx.update(|cx| {
            action_log.update(cx, |log, cx| log.buffer_created(buffer.clone(), cx));
            buffer.update(cx, |buffer, cx| buffer.set_text("rewritten", cx));
            action_log.update(cx, |log, cx| log.buffer_edited(buffer.clone(), cx));
        });
        project
            .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
            .await
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            unreviewed_hunks(&action_log, cx),
            vec![(
                buffer.clone(),
                vec![HunkStatus {
                    range: Point::new(0, 0)..Point::new(0, 9),
                    diff_status: DiffHunkStatusKind::Added,
                    old_text: "".into(),
                }],
            )]
        );

        action_log
            .update(cx, |log, cx| {
                let (task, _) = log.reject_edits_in_ranges(buffer.clone(), vec![2..5], None, cx);
                task
            })
            .await
            .unwrap();
        cx.run_until_parked();
        assert_eq!(unreviewed_hunks(&action_log, cx), vec![]);
        assert_eq!(
            buffer.read_with(cx, |buffer, _cx| buffer.text()),
            "Lorem ipsum dolor"
        );
    }

    #[gpui::test(iterations = 10)]
    async fn test_deleting_files(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/dir"),
            json!({"file1": "lorem\n", "file2": "ipsum\n"}),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let file1_path = project
            .read_with(cx, |project, cx| project.find_project_path("dir/file1", cx))
            .unwrap();
        let file2_path = project
            .read_with(cx, |project, cx| project.find_project_path("dir/file2", cx))
            .unwrap();

        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let buffer1 = project
            .update(cx, |project, cx| {
                project.open_buffer(file1_path.clone(), cx)
            })
            .await
            .unwrap();
        let buffer2 = project
            .update(cx, |project, cx| {
                project.open_buffer(file2_path.clone(), cx)
            })
            .await
            .unwrap();

        action_log.update(cx, |log, cx| log.will_delete_buffer(buffer1.clone(), cx));
        action_log.update(cx, |log, cx| log.will_delete_buffer(buffer2.clone(), cx));
        project
            .update(cx, |project, cx| {
                project.delete_file(file1_path.clone(), cx)
            })
            .unwrap()
            .await
            .unwrap();
        project
            .update(cx, |project, cx| {
                project.delete_file(file2_path.clone(), cx)
            })
            .unwrap()
            .await
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            unreviewed_hunks(&action_log, cx),
            vec![
                (
                    buffer1.clone(),
                    vec![HunkStatus {
                        range: Point::new(0, 0)..Point::new(0, 0),
                        diff_status: DiffHunkStatusKind::Deleted,
                        old_text: "lorem\n".into(),
                    }]
                ),
                (
                    buffer2.clone(),
                    vec![HunkStatus {
                        range: Point::new(0, 0)..Point::new(0, 0),
                        diff_status: DiffHunkStatusKind::Deleted,
                        old_text: "ipsum\n".into(),
                    }],
                )
            ]
        );

        // Simulate file1 being recreated externally.
        fs.insert_file(path!("/dir/file1"), "LOREM".as_bytes().to_vec())
            .await;

        // Simulate file2 being recreated by a tool.
        let buffer2 = project
            .update(cx, |project, cx| project.open_buffer(file2_path, cx))
            .await
            .unwrap();
        action_log.update(cx, |log, cx| log.buffer_created(buffer2.clone(), cx));
        buffer2.update(cx, |buffer, cx| buffer.set_text("IPSUM", cx));
        action_log.update(cx, |log, cx| log.buffer_edited(buffer2.clone(), cx));
        project
            .update(cx, |project, cx| project.save_buffer(buffer2.clone(), cx))
            .await
            .unwrap();

        cx.run_until_parked();
        assert_eq!(
            unreviewed_hunks(&action_log, cx),
            vec![(
                buffer2.clone(),
                vec![HunkStatus {
                    range: Point::new(0, 0)..Point::new(0, 5),
                    diff_status: DiffHunkStatusKind::Added,
                    old_text: "".into(),
                }],
            )]
        );

        // Simulate file2 being deleted externally.
        fs.remove_file(path!("/dir/file2").as_ref(), RemoveOptions::default())
            .await
            .unwrap();
        cx.run_until_parked();
        assert_eq!(unreviewed_hunks(&action_log, cx), vec![]);
    }

    #[gpui::test(iterations = 10)]
    async fn test_reject_edits(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), json!({"file": "abc\ndef\nghi\njkl\nmno"}))
            .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let file_path = project
            .read_with(cx, |project, cx| project.find_project_path("dir/file", cx))
            .unwrap();
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(file_path, cx))
            .await
            .unwrap();

        cx.update(|cx| {
            action_log.update(cx, |log, cx| log.buffer_read(buffer.clone(), cx));
            buffer.update(cx, |buffer, cx| {
                buffer
                    .edit([(Point::new(1, 1)..Point::new(1, 2), "E\nXYZ")], None, cx)
                    .unwrap()
            });
            buffer.update(cx, |buffer, cx| {
                buffer
                    .edit([(Point::new(5, 2)..Point::new(5, 3), "O")], None, cx)
                    .unwrap()
            });
            action_log.update(cx, |log, cx| log.buffer_edited(buffer.clone(), cx));
        });
        cx.run_until_parked();
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "abc\ndE\nXYZf\nghi\njkl\nmnO"
        );
        assert_eq!(
            unreviewed_hunks(&action_log, cx),
            vec![(
                buffer.clone(),
                vec![
                    HunkStatus {
                        range: Point::new(1, 0)..Point::new(2, 0),
                        diff_status: DiffHunkStatusKind::Modified,
                        old_text: "def\n".into(),
                    },
                    HunkStatus {
                        range: Point::new(2, 0)..Point::new(3, 0),
                        diff_status: DiffHunkStatusKind::Added,
                        old_text: "".into(),
                    },
                    HunkStatus {
                        range: Point::new(5, 0)..Point::new(5, 3),
                        diff_status: DiffHunkStatusKind::Modified,
                        old_text: "mno".into(),
                    }
                ],
            )]
        );

        // If the rejected range doesn't overlap with any hunk, we ignore it.
        action_log
            .update(cx, |log, cx| {
                let (task, _) = log.reject_edits_in_ranges(
                    buffer.clone(),
                    vec![Point::new(4, 0)..Point::new(4, 0)],
                    None,
                    cx,
                );
                task
            })
            .await
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "abc\ndE\nXYZf\nghi\njkl\nmnO"
        );
        assert_eq!(
            unreviewed_hunks(&action_log, cx),
            vec![(
                buffer.clone(),
                vec![
                    HunkStatus {
                        range: Point::new(1, 0)..Point::new(2, 0),
                        diff_status: DiffHunkStatusKind::Modified,
                        old_text: "def\n".into(),
                    },
                    HunkStatus {
                        range: Point::new(2, 0)..Point::new(3, 0),
                        diff_status: DiffHunkStatusKind::Added,
                        old_text: "".into(),
                    },
                    HunkStatus {
                        range: Point::new(5, 0)..Point::new(5, 3),
                        diff_status: DiffHunkStatusKind::Modified,
                        old_text: "mno".into(),
                    }
                ],
            )]
        );

        action_log
            .update(cx, |log, cx| {
                let (task, _) = log.reject_edits_in_ranges(
                    buffer.clone(),
                    // The rewritten line and the line added below it: two
                    // changes now, each answered on its own.
                    vec![Point::new(0, 0)..Point::new(3, 0)],
                    None,
                    cx,
                );
                task
            })
            .await
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "abc\ndef\nghi\njkl\nmnO"
        );
        assert_eq!(
            unreviewed_hunks(&action_log, cx),
            vec![(
                buffer.clone(),
                vec![HunkStatus {
                    range: Point::new(4, 0)..Point::new(4, 3),
                    diff_status: DiffHunkStatusKind::Modified,
                    old_text: "mno".into(),
                }],
            )]
        );

        action_log
            .update(cx, |log, cx| {
                let (task, _) = log.reject_edits_in_ranges(
                    buffer.clone(),
                    vec![Point::new(4, 0)..Point::new(4, 0)],
                    None,
                    cx,
                );
                task
            })
            .await
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "abc\ndef\nghi\njkl\nmno"
        );
        assert_eq!(unreviewed_hunks(&action_log, cx), vec![]);
    }

    #[gpui::test(iterations = 10)]
    async fn test_reject_multiple_edits(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), json!({"file": "abc\ndef\nghi\njkl\nmno"}))
            .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let file_path = project
            .read_with(cx, |project, cx| project.find_project_path("dir/file", cx))
            .unwrap();
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(file_path, cx))
            .await
            .unwrap();

        cx.update(|cx| {
            action_log.update(cx, |log, cx| log.buffer_read(buffer.clone(), cx));
            buffer.update(cx, |buffer, cx| {
                buffer
                    .edit([(Point::new(1, 1)..Point::new(1, 2), "E\nXYZ")], None, cx)
                    .unwrap()
            });
            buffer.update(cx, |buffer, cx| {
                buffer
                    .edit([(Point::new(5, 2)..Point::new(5, 3), "O")], None, cx)
                    .unwrap()
            });
            action_log.update(cx, |log, cx| log.buffer_edited(buffer.clone(), cx));
        });
        cx.run_until_parked();
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "abc\ndE\nXYZf\nghi\njkl\nmnO"
        );
        assert_eq!(
            unreviewed_hunks(&action_log, cx),
            vec![(
                buffer.clone(),
                vec![
                    HunkStatus {
                        range: Point::new(1, 0)..Point::new(2, 0),
                        diff_status: DiffHunkStatusKind::Modified,
                        old_text: "def\n".into(),
                    },
                    HunkStatus {
                        range: Point::new(2, 0)..Point::new(3, 0),
                        diff_status: DiffHunkStatusKind::Added,
                        old_text: "".into(),
                    },
                    HunkStatus {
                        range: Point::new(5, 0)..Point::new(5, 3),
                        diff_status: DiffHunkStatusKind::Modified,
                        old_text: "mno".into(),
                    }
                ],
            )]
        );

        action_log.update(cx, |log, cx| {
            let range_1 = buffer.read(cx).anchor_before(Point::new(0, 0))
                ..buffer.read(cx).anchor_before(Point::new(3, 0));
            let range_2 = buffer.read(cx).anchor_before(Point::new(5, 0))
                ..buffer.read(cx).anchor_before(Point::new(5, 3));

            let (task, _) =
                log.reject_edits_in_ranges(buffer.clone(), vec![range_1, range_2], None, cx);
            task.detach();
            assert_eq!(
                buffer.read_with(cx, |buffer, _| buffer.text()),
                "abc\ndef\nghi\njkl\nmno"
            );
        });
        cx.run_until_parked();
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "abc\ndef\nghi\njkl\nmno"
        );
        assert_eq!(unreviewed_hunks(&action_log, cx), vec![]);
    }

    #[gpui::test(iterations = 10)]
    async fn test_reject_deleted_file(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), json!({"file": "content"}))
            .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let file_path = project
            .read_with(cx, |project, cx| project.find_project_path("dir/file", cx))
            .unwrap();
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(file_path.clone(), cx))
            .await
            .unwrap();

        cx.update(|cx| {
            action_log.update(cx, |log, cx| log.will_delete_buffer(buffer.clone(), cx));
        });
        project
            .update(cx, |project, cx| project.delete_file(file_path.clone(), cx))
            .unwrap()
            .await
            .unwrap();
        cx.run_until_parked();
        assert!(!fs.is_file(path!("/dir/file").as_ref()).await);
        assert_eq!(
            unreviewed_hunks(&action_log, cx),
            vec![(
                buffer.clone(),
                vec![HunkStatus {
                    range: Point::new(0, 0)..Point::new(0, 0),
                    diff_status: DiffHunkStatusKind::Deleted,
                    old_text: "content".into(),
                }]
            )]
        );

        action_log
            .update(cx, |log, cx| {
                let (task, _) = log.reject_edits_in_ranges(
                    buffer.clone(),
                    vec![Point::new(0, 0)..Point::new(0, 0)],
                    None,
                    cx,
                );
                task
            })
            .await
            .unwrap();
        cx.run_until_parked();
        assert_eq!(buffer.read_with(cx, |buffer, _| buffer.text()), "content");
        assert!(fs.is_file(path!("/dir/file").as_ref()).await);
        assert_eq!(unreviewed_hunks(&action_log, cx), vec![]);
    }

    #[gpui::test(iterations = 10)]
    async fn test_reject_created_file(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let file_path = project
            .read_with(cx, |project, cx| {
                project.find_project_path("dir/new_file", cx)
            })
            .unwrap();
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(file_path, cx))
            .await
            .unwrap();
        cx.update(|cx| {
            action_log.update(cx, |log, cx| log.buffer_created(buffer.clone(), cx));
            buffer.update(cx, |buffer, cx| buffer.set_text("content", cx));
            action_log.update(cx, |log, cx| log.buffer_edited(buffer.clone(), cx));
        });
        project
            .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
            .await
            .unwrap();
        assert!(fs.is_file(path!("/dir/new_file").as_ref()).await);
        cx.run_until_parked();
        assert_eq!(
            unreviewed_hunks(&action_log, cx),
            vec![(
                buffer.clone(),
                vec![HunkStatus {
                    range: Point::new(0, 0)..Point::new(0, 7),
                    diff_status: DiffHunkStatusKind::Added,
                    old_text: "".into(),
                }],
            )]
        );

        action_log
            .update(cx, |log, cx| {
                let (task, _) = log.reject_edits_in_ranges(
                    buffer.clone(),
                    vec![Point::new(0, 0)..Point::new(0, 11)],
                    None,
                    cx,
                );
                task
            })
            .await
            .unwrap();
        cx.run_until_parked();
        assert!(!fs.is_file(path!("/dir/new_file").as_ref()).await);
        assert_eq!(unreviewed_hunks(&action_log, cx), vec![]);
    }

    // Rejecting part of a file the agent created used to delete the whole file:
    // the `Created` arm never looked at the ranges it was given. One line turned
    // down has to remove one line.
    #[gpui::test]
    async fn test_reject_one_line_of_created_file(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let file_path = project
            .read_with(cx, |project, cx| {
                project.find_project_path("dir/new_file", cx)
            })
            .unwrap();
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(file_path, cx))
            .await
            .unwrap();
        cx.update(|cx| {
            action_log.update(cx, |log, cx| log.buffer_created(buffer.clone(), cx));
            buffer.update(cx, |buffer, cx| {
                buffer.set_text(
                    "one
two
three
",
                    cx,
                )
            });
            action_log.update(cx, |log, cx| log.buffer_edited(buffer.clone(), cx));
        });
        project
            .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
            .await
            .unwrap();
        cx.run_until_parked();

        action_log
            .update(cx, |log, cx| {
                let (task, _) = log.reject_edits_in_ranges(
                    buffer.clone(),
                    vec![Point::new(1, 0)..Point::new(1, 3)],
                    None,
                    cx,
                );
                task
            })
            .await
            .unwrap();
        cx.run_until_parked();

        assert!(
            fs.is_file(path!("/dir/new_file").as_ref()).await,
            "rejecting one line must not delete the file"
        );
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "one
three
"
        );
        assert!(
            !unreviewed_hunks(&action_log, cx).is_empty(),
            "the lines that were not rejected are still waiting for an answer"
        );
    }

    #[gpui::test]
    async fn test_reject_created_file_with_user_edits(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let action_log = cx.new(|_| ActionLog::new(project.clone()));

        let file_path = project
            .read_with(cx, |project, cx| {
                project.find_project_path("dir/new_file", cx)
            })
            .unwrap();
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(file_path, cx))
            .await
            .unwrap();

        // AI creates file with initial content
        cx.update(|cx| {
            action_log.update(cx, |log, cx| log.buffer_created(buffer.clone(), cx));
            buffer.update(cx, |buffer, cx| buffer.set_text("ai content", cx));
            action_log.update(cx, |log, cx| log.buffer_edited(buffer.clone(), cx));
        });

        project
            .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
            .await
            .unwrap();

        cx.run_until_parked();

        // User makes additional edits
        cx.update(|cx| {
            buffer.update(cx, |buffer, cx| {
                buffer.edit([(10..10, "\nuser added this line")], None, cx);
            });
        });

        project
            .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
            .await
            .unwrap();

        assert!(fs.is_file(path!("/dir/new_file").as_ref()).await);

        // Reject all
        action_log
            .update(cx, |log, cx| {
                let (task, _) = log.reject_edits_in_ranges(
                    buffer.clone(),
                    vec![Point::new(0, 0)..Point::new(100, 0)],
                    None,
                    cx,
                );
                task
            })
            .await
            .unwrap();
        cx.run_until_parked();

        // Every line on screen was turned down, so every line goes -- but the
        // file stays, since the user has written in it.
        assert!(fs.is_file(path!("/dir/new_file").as_ref()).await);

        let content = buffer.read_with(cx, |buffer, _| buffer.text());
        assert_eq!(content, "");
    }

    #[gpui::test]
    async fn test_reject_after_accepting_hunk_on_created_file(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let action_log = cx.new(|_| ActionLog::new(project.clone()));

        let file_path = project
            .read_with(cx, |project, cx| {
                project.find_project_path("dir/new_file", cx)
            })
            .unwrap();
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(file_path.clone(), cx))
            .await
            .unwrap();

        // AI creates file with initial content
        cx.update(|cx| {
            action_log.update(cx, |log, cx| log.buffer_created(buffer.clone(), cx));
            buffer.update(cx, |buffer, cx| buffer.set_text("ai content v1", cx));
            action_log.update(cx, |log, cx| log.buffer_edited(buffer.clone(), cx));
        });
        project
            .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
            .await
            .unwrap();
        cx.run_until_parked();
        assert_ne!(unreviewed_hunks(&action_log, cx), vec![]);

        // User accepts the single hunk
        action_log.update(cx, |log, cx| {
            let buffer_range = Anchor::min_max_range_for_buffer(buffer.read(cx).remote_id());
            log.keep_edits_in_range(buffer.clone(), buffer_range, None, cx)
        });
        cx.run_until_parked();
        assert_eq!(unreviewed_hunks(&action_log, cx), vec![]);
        assert!(fs.is_file(path!("/dir/new_file").as_ref()).await);

        // AI modifies the file
        cx.update(|cx| {
            buffer.update(cx, |buffer, cx| buffer.set_text("ai content v2", cx));
            action_log.update(cx, |log, cx| log.buffer_edited(buffer.clone(), cx));
        });
        project
            .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
            .await
            .unwrap();
        cx.run_until_parked();
        assert_ne!(unreviewed_hunks(&action_log, cx), vec![]);

        // User rejects the hunk
        action_log
            .update(cx, |log, cx| {
                let (task, _) = log.reject_edits_in_ranges(
                    buffer.clone(),
                    vec![Anchor::min_max_range_for_buffer(
                        buffer.read(cx).remote_id(),
                    )],
                    None,
                    cx,
                );
                task
            })
            .await
            .unwrap();
        cx.run_until_parked();
        assert!(fs.is_file(path!("/dir/new_file").as_ref()).await,);
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "ai content v1"
        );
        assert_eq!(unreviewed_hunks(&action_log, cx), vec![]);
    }

    #[gpui::test]
    async fn test_reject_edits_on_previously_accepted_created_file(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let action_log = cx.new(|_| ActionLog::new(project.clone()));

        let file_path = project
            .read_with(cx, |project, cx| {
                project.find_project_path("dir/new_file", cx)
            })
            .unwrap();
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(file_path.clone(), cx))
            .await
            .unwrap();

        // AI creates file with initial content
        cx.update(|cx| {
            action_log.update(cx, |log, cx| log.buffer_created(buffer.clone(), cx));
            buffer.update(cx, |buffer, cx| buffer.set_text("ai content v1", cx));
            action_log.update(cx, |log, cx| log.buffer_edited(buffer.clone(), cx));
        });
        project
            .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
            .await
            .unwrap();
        cx.run_until_parked();

        // User clicks "Accept All"
        action_log.update(cx, |log, cx| log.keep_all_edits(None, cx));
        cx.run_until_parked();
        assert!(fs.is_file(path!("/dir/new_file").as_ref()).await);
        assert_eq!(unreviewed_hunks(&action_log, cx), vec![]); // Hunks are cleared

        // AI modifies file again
        cx.update(|cx| {
            buffer.update(cx, |buffer, cx| buffer.set_text("ai content v2", cx));
            action_log.update(cx, |log, cx| log.buffer_edited(buffer.clone(), cx));
        });
        project
            .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
            .await
            .unwrap();
        cx.run_until_parked();
        assert_ne!(unreviewed_hunks(&action_log, cx), vec![]);

        // User clicks "Reject All"
        action_log
            .update(cx, |log, cx| log.reject_all_edits(None, cx))
            .await;
        cx.run_until_parked();
        assert!(fs.is_file(path!("/dir/new_file").as_ref()).await);
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "ai content v1"
        );
        assert_eq!(unreviewed_hunks(&action_log, cx), vec![]);
    }

    #[gpui::test(iterations = 100)]
    async fn test_random_diffs(mut rng: StdRng, cx: &mut TestAppContext) {
        init_test(cx);

        let operations = env::var("OPERATIONS")
            .map(|i| i.parse().expect("invalid `OPERATIONS` variable"))
            .unwrap_or(20);

        let text = RandomCharIter::new(&mut rng).take(50).collect::<String>();
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), json!({"file": text})).await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let file_path = project
            .read_with(cx, |project, cx| project.find_project_path("dir/file", cx))
            .unwrap();
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(file_path, cx))
            .await
            .unwrap();

        action_log.update(cx, |log, cx| log.buffer_read(buffer.clone(), cx));

        for _ in 0..operations {
            match rng.random_range(0..100) {
                0..25 => {
                    action_log.update(cx, |log, cx| {
                        let range = buffer.read(cx).random_byte_range(0, &mut rng);
                        log::info!("keeping edits in range {:?}", range);
                        log.keep_edits_in_range(buffer.clone(), range, None, cx)
                    });
                }
                25..50 => {
                    action_log
                        .update(cx, |log, cx| {
                            let range = buffer.read(cx).random_byte_range(0, &mut rng);
                            log::info!("rejecting edits in range {:?}", range);
                            let (task, _) =
                                log.reject_edits_in_ranges(buffer.clone(), vec![range], None, cx);
                            task
                        })
                        .await
                        .unwrap();
                }
                _ => {
                    let is_agent_edit = rng.random_bool(0.5);
                    if is_agent_edit {
                        log::info!("agent edit");
                    } else {
                        log::info!("user edit");
                    }
                    cx.update(|cx| {
                        buffer.update(cx, |buffer, cx| buffer.randomly_edit(&mut rng, 1, cx));
                        if is_agent_edit {
                            action_log.update(cx, |log, cx| log.buffer_edited(buffer.clone(), cx));
                        }
                    });
                }
            }

            if rng.random_bool(0.2) {
                quiesce(&action_log, &buffer, cx);
            }
        }

        quiesce(&action_log, &buffer, cx);

        fn quiesce(
            action_log: &Entity<ActionLog>,
            buffer: &Entity<Buffer>,
            cx: &mut TestAppContext,
        ) {
            log::info!("quiescing...");
            cx.run_until_parked();
            action_log.update(cx, |log, cx| {
                let tracked_buffer = log.tracked_buffers.get(buffer).unwrap();
                let mut old_text = tracked_buffer.diff_base.clone();
                let new_text = buffer.read(cx).as_rope();
                for edit in tracked_buffer.unreviewed_edits.edits() {
                    let old_start = old_text.point_to_offset(Point::new(edit.new.start, 0));
                    let old_end = old_text.point_to_offset(cmp::min(
                        Point::new(edit.new.start + edit.old_len(), 0),
                        old_text.max_point(),
                    ));
                    old_text.replace(
                        old_start..old_end,
                        &new_text.slice_rows(edit.new.clone()).to_string(),
                    );
                }
                pretty_assertions::assert_eq!(old_text.to_string(), new_text.to_string());
            })
        }
    }

    #[gpui::test]
    async fn test_undo_last_reject(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/dir"),
            json!({
                "file1": "abc\ndef\nghi"
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let action_log = cx.new(|_| ActionLog::new(project.clone()));
        let file_path = project
            .read_with(cx, |project, cx| project.find_project_path("dir/file1", cx))
            .unwrap();

        let buffer = project
            .update(cx, |project, cx| project.open_buffer(file_path, cx))
            .await
            .unwrap();

        // Track the buffer and make an agent edit
        cx.update(|cx| {
            action_log.update(cx, |log, cx| log.buffer_read(buffer.clone(), cx));
            buffer.update(cx, |buffer, cx| {
                buffer
                    .edit(
                        [(Point::new(1, 0)..Point::new(1, 3), "AGENT_EDIT")],
                        None,
                        cx,
                    )
                    .unwrap()
            });
            action_log.update(cx, |log, cx| log.buffer_edited(buffer.clone(), cx));
        });
        cx.run_until_parked();

        // Verify the agent edit is there
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "abc\nAGENT_EDIT\nghi"
        );
        assert!(!unreviewed_hunks(&action_log, cx).is_empty());

        // Reject all edits
        action_log
            .update(cx, |log, cx| log.reject_all_edits(None, cx))
            .await;
        cx.run_until_parked();

        // Verify the buffer is back to original
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "abc\ndef\nghi"
        );
        assert!(unreviewed_hunks(&action_log, cx).is_empty());

        // Verify undo state is available
        assert!(action_log.read_with(cx, |log, _| log.has_pending_undo()));

        // Undo the reject
        action_log
            .update(cx, |log, cx| log.undo_last_reject(cx))
            .await;

        cx.run_until_parked();

        // Verify the agent edit is restored
        assert_eq!(
            buffer.read_with(cx, |buffer, _| buffer.text()),
            "abc\nAGENT_EDIT\nghi"
        );

        // Verify undo state is cleared
        assert!(!action_log.read_with(cx, |log, _| log.has_pending_undo()));
    }

    #[gpui::test]
    async fn test_linked_action_log_buffer_read(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), json!({"file": "hello world"}))
            .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let parent_log = cx.new(|_| ActionLog::new(project.clone()));
        let child_log =
            cx.new(|_| ActionLog::new(project.clone()).with_linked_action_log(parent_log.clone()));

        let file_path = project
            .read_with(cx, |project, cx| project.find_project_path("dir/file", cx))
            .unwrap();
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(file_path, cx))
            .await
            .unwrap();

        cx.update(|cx| {
            child_log.update(cx, |log, cx| log.buffer_read(buffer.clone(), cx));
        });

        // Neither log considers the buffer stale immediately after reading it.
        let child_stale = cx.read(|cx| {
            child_log
                .read(cx)
                .stale_buffers(cx)
                .cloned()
                .collect::<Vec<_>>()
        });
        let parent_stale = cx.read(|cx| {
            parent_log
                .read(cx)
                .stale_buffers(cx)
                .cloned()
                .collect::<Vec<_>>()
        });
        assert!(child_stale.is_empty());
        assert!(parent_stale.is_empty());

        // Simulate a user edit after the agent read the file.
        cx.update(|cx| {
            buffer.update(cx, |buffer, cx| {
                buffer.edit([(0..5, "goodbye")], None, cx).unwrap();
            });
        });
        cx.run_until_parked();

        // Both child and parent should see the buffer as stale because both tracked
        // it at the pre-edit version via buffer_read forwarding.
        let child_stale = cx.read(|cx| {
            child_log
                .read(cx)
                .stale_buffers(cx)
                .cloned()
                .collect::<Vec<_>>()
        });
        let parent_stale = cx.read(|cx| {
            parent_log
                .read(cx)
                .stale_buffers(cx)
                .cloned()
                .collect::<Vec<_>>()
        });
        assert_eq!(child_stale, vec![buffer.clone()]);
        assert_eq!(parent_stale, vec![buffer]);
    }

    #[gpui::test]
    async fn test_linked_action_log_buffer_edited(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), json!({"file": "abc\ndef\nghi"}))
            .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let parent_log = cx.new(|_| ActionLog::new(project.clone()));
        let child_log =
            cx.new(|_| ActionLog::new(project.clone()).with_linked_action_log(parent_log.clone()));

        let file_path = project
            .read_with(cx, |project, cx| project.find_project_path("dir/file", cx))
            .unwrap();
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(file_path, cx))
            .await
            .unwrap();

        cx.update(|cx| {
            child_log.update(cx, |log, cx| log.buffer_read(buffer.clone(), cx));
            buffer.update(cx, |buffer, cx| {
                buffer
                    .edit([(Point::new(1, 0)..Point::new(1, 3), "DEF")], None, cx)
                    .unwrap();
            });
            child_log.update(cx, |log, cx| log.buffer_edited(buffer.clone(), cx));
        });
        cx.run_until_parked();

        let expected_hunks = vec![(
            buffer,
            vec![HunkStatus {
                range: Point::new(1, 0)..Point::new(2, 0),
                diff_status: DiffHunkStatusKind::Modified,
                old_text: "def\n".into(),
            }],
        )];
        assert_eq!(
            unreviewed_hunks(&child_log, cx),
            expected_hunks,
            "child should track the agent edit"
        );
        assert_eq!(
            unreviewed_hunks(&parent_log, cx),
            expected_hunks,
            "parent should also track the agent edit via linked log forwarding"
        );
    }

    #[gpui::test]
    async fn test_linked_action_log_buffer_created(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), json!({})).await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let parent_log = cx.new(|_| ActionLog::new(project.clone()));
        let child_log =
            cx.new(|_| ActionLog::new(project.clone()).with_linked_action_log(parent_log.clone()));

        let file_path = project
            .read_with(cx, |project, cx| {
                project.find_project_path("dir/new_file", cx)
            })
            .unwrap();
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(file_path, cx))
            .await
            .unwrap();

        cx.update(|cx| {
            child_log.update(cx, |log, cx| log.buffer_created(buffer.clone(), cx));
            buffer.update(cx, |buffer, cx| buffer.set_text("hello", cx));
            child_log.update(cx, |log, cx| log.buffer_edited(buffer.clone(), cx));
        });
        project
            .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))
            .await
            .unwrap();
        cx.run_until_parked();

        let expected_hunks = vec![(
            buffer.clone(),
            vec![HunkStatus {
                range: Point::new(0, 0)..Point::new(0, 5),
                diff_status: DiffHunkStatusKind::Added,
                old_text: "".into(),
            }],
        )];
        assert_eq!(
            unreviewed_hunks(&child_log, cx),
            expected_hunks,
            "child should track the created file"
        );
        assert_eq!(
            unreviewed_hunks(&parent_log, cx),
            expected_hunks,
            "parent should also track the created file via linked log forwarding"
        );
    }

    #[gpui::test]
    async fn test_linked_action_log_will_delete_buffer(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), json!({"file": "hello\n"}))
            .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let parent_log = cx.new(|_| ActionLog::new(project.clone()));
        let child_log =
            cx.new(|_| ActionLog::new(project.clone()).with_linked_action_log(parent_log.clone()));

        let file_path = project
            .read_with(cx, |project, cx| project.find_project_path("dir/file", cx))
            .unwrap();
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(file_path.clone(), cx))
            .await
            .unwrap();

        cx.update(|cx| {
            child_log.update(cx, |log, cx| log.will_delete_buffer(buffer.clone(), cx));
        });
        project
            .update(cx, |project, cx| project.delete_file(file_path, cx))
            .unwrap()
            .await
            .unwrap();
        cx.run_until_parked();

        let expected_hunks = vec![(
            buffer.clone(),
            vec![HunkStatus {
                range: Point::new(0, 0)..Point::new(0, 0),
                diff_status: DiffHunkStatusKind::Deleted,
                old_text: "hello\n".into(),
            }],
        )];
        assert_eq!(
            unreviewed_hunks(&child_log, cx),
            expected_hunks,
            "child should track the deleted file"
        );
        assert_eq!(
            unreviewed_hunks(&parent_log, cx),
            expected_hunks,
            "parent should also track the deleted file via linked log forwarding"
        );
    }

    /// Simulates the subagent scenario: two child logs linked to the same parent, each
    /// editing a different file. The parent accumulates all edits while each child
    /// only sees its own.
    #[gpui::test]
    async fn test_linked_action_log_independent_tracking(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/dir"),
            json!({
                "file_a": "content of a",
                "file_b": "content of b",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let parent_log = cx.new(|_| ActionLog::new(project.clone()));
        let child_log_1 =
            cx.new(|_| ActionLog::new(project.clone()).with_linked_action_log(parent_log.clone()));
        let child_log_2 =
            cx.new(|_| ActionLog::new(project.clone()).with_linked_action_log(parent_log.clone()));

        let file_a_path = project
            .read_with(cx, |project, cx| {
                project.find_project_path("dir/file_a", cx)
            })
            .unwrap();
        let file_b_path = project
            .read_with(cx, |project, cx| {
                project.find_project_path("dir/file_b", cx)
            })
            .unwrap();
        let buffer_a = project
            .update(cx, |project, cx| project.open_buffer(file_a_path, cx))
            .await
            .unwrap();
        let buffer_b = project
            .update(cx, |project, cx| project.open_buffer(file_b_path, cx))
            .await
            .unwrap();

        cx.update(|cx| {
            child_log_1.update(cx, |log, cx| log.buffer_read(buffer_a.clone(), cx));
            buffer_a.update(cx, |buffer, cx| {
                buffer.edit([(0..0, "MODIFIED: ")], None, cx).unwrap();
            });
            child_log_1.update(cx, |log, cx| log.buffer_edited(buffer_a.clone(), cx));

            child_log_2.update(cx, |log, cx| log.buffer_read(buffer_b.clone(), cx));
            buffer_b.update(cx, |buffer, cx| {
                buffer.edit([(0..0, "MODIFIED: ")], None, cx).unwrap();
            });
            child_log_2.update(cx, |log, cx| log.buffer_edited(buffer_b.clone(), cx));
        });
        cx.run_until_parked();

        let child_1_changed: Vec<_> = cx.read(|cx| {
            child_log_1
                .read(cx)
                .changed_buffers(cx)
                .map(|(buffer, _)| buffer)
                .collect()
        });
        let child_2_changed: Vec<_> = cx.read(|cx| {
            child_log_2
                .read(cx)
                .changed_buffers(cx)
                .map(|(buffer, _)| buffer)
                .collect()
        });
        let parent_changed: Vec<_> = cx.read(|cx| {
            parent_log
                .read(cx)
                .changed_buffers(cx)
                .map(|(buffer, _)| buffer)
                .collect()
        });

        assert_eq!(
            child_1_changed,
            vec![buffer_a.clone()],
            "child 1 should only track file_a"
        );
        assert_eq!(
            child_2_changed,
            vec![buffer_b.clone()],
            "child 2 should only track file_b"
        );
        assert_eq!(parent_changed.len(), 2, "parent should track both files");
        assert!(
            parent_changed.contains(&buffer_a) && parent_changed.contains(&buffer_b),
            "parent should contain both buffer_a and buffer_b"
        );
    }

    #[gpui::test]
    async fn test_file_read_time_recorded_on_buffer_read(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), json!({"file": "hello world"}))
            .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let action_log = cx.new(|_| ActionLog::new(project.clone()));

        let file_path = project
            .read_with(cx, |project, cx| project.find_project_path("dir/file", cx))
            .unwrap();
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(file_path, cx))
            .await
            .unwrap();

        let abs_path = PathBuf::from(path!("/dir/file"));
        assert!(
            action_log.read_with(cx, |log, _| log.file_read_time(&abs_path).is_none()),
            "file_read_time should be None before buffer_read"
        );

        cx.update(|cx| {
            action_log.update(cx, |log, cx| log.buffer_read(buffer.clone(), cx));
        });

        assert!(
            action_log.read_with(cx, |log, _| log.file_read_time(&abs_path).is_some()),
            "file_read_time should be recorded after buffer_read"
        );
    }

    #[gpui::test]
    async fn test_file_read_time_recorded_on_buffer_edited(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), json!({"file": "hello world"}))
            .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let action_log = cx.new(|_| ActionLog::new(project.clone()));

        let file_path = project
            .read_with(cx, |project, cx| project.find_project_path("dir/file", cx))
            .unwrap();
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(file_path, cx))
            .await
            .unwrap();

        let abs_path = PathBuf::from(path!("/dir/file"));
        assert!(
            action_log.read_with(cx, |log, _| log.file_read_time(&abs_path).is_none()),
            "file_read_time should be None before buffer_edited"
        );

        cx.update(|cx| {
            action_log.update(cx, |log, cx| log.buffer_edited(buffer.clone(), cx));
        });

        assert!(
            action_log.read_with(cx, |log, _| log.file_read_time(&abs_path).is_some()),
            "file_read_time should be recorded after buffer_edited"
        );
    }

    #[gpui::test]
    async fn test_file_read_time_recorded_on_buffer_created(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), json!({"file": "existing content"}))
            .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let action_log = cx.new(|_| ActionLog::new(project.clone()));

        let file_path = project
            .read_with(cx, |project, cx| project.find_project_path("dir/file", cx))
            .unwrap();
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(file_path, cx))
            .await
            .unwrap();

        let abs_path = PathBuf::from(path!("/dir/file"));
        assert!(
            action_log.read_with(cx, |log, _| log.file_read_time(&abs_path).is_none()),
            "file_read_time should be None before buffer_created"
        );

        cx.update(|cx| {
            action_log.update(cx, |log, cx| log.buffer_created(buffer.clone(), cx));
        });

        assert!(
            action_log.read_with(cx, |log, _| log.file_read_time(&abs_path).is_some()),
            "file_read_time should be recorded after buffer_created"
        );
    }

    #[gpui::test]
    async fn test_file_read_time_removed_on_delete(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), json!({"file": "hello world"}))
            .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let action_log = cx.new(|_| ActionLog::new(project.clone()));

        let file_path = project
            .read_with(cx, |project, cx| project.find_project_path("dir/file", cx))
            .unwrap();
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(file_path, cx))
            .await
            .unwrap();

        let abs_path = PathBuf::from(path!("/dir/file"));

        cx.update(|cx| {
            action_log.update(cx, |log, cx| log.buffer_read(buffer.clone(), cx));
        });
        assert!(
            action_log.read_with(cx, |log, _| log.file_read_time(&abs_path).is_some()),
            "file_read_time should exist after buffer_read"
        );

        cx.update(|cx| {
            action_log.update(cx, |log, cx| log.will_delete_buffer(buffer.clone(), cx));
        });
        assert!(
            action_log.read_with(cx, |log, _| log.file_read_time(&abs_path).is_none()),
            "file_read_time should be removed after will_delete_buffer"
        );
    }

    #[gpui::test]
    async fn test_file_read_time_not_forwarded_to_linked_action_log(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), json!({"file": "hello world"}))
            .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let parent_log = cx.new(|_| ActionLog::new(project.clone()));
        let child_log =
            cx.new(|_| ActionLog::new(project.clone()).with_linked_action_log(parent_log.clone()));

        let file_path = project
            .read_with(cx, |project, cx| project.find_project_path("dir/file", cx))
            .unwrap();
        let buffer = project
            .update(cx, |project, cx| project.open_buffer(file_path, cx))
            .await
            .unwrap();

        let abs_path = PathBuf::from(path!("/dir/file"));

        cx.update(|cx| {
            child_log.update(cx, |log, cx| log.buffer_read(buffer.clone(), cx));
        });
        assert!(
            child_log.read_with(cx, |log, _| log.file_read_time(&abs_path).is_some()),
            "child should record file_read_time on buffer_read"
        );
        assert!(
            parent_log.read_with(cx, |log, _| log.file_read_time(&abs_path).is_none()),
            "parent should NOT get file_read_time from child's buffer_read"
        );

        cx.update(|cx| {
            child_log.update(cx, |log, cx| log.buffer_edited(buffer.clone(), cx));
        });
        assert!(
            parent_log.read_with(cx, |log, _| log.file_read_time(&abs_path).is_none()),
            "parent should NOT get file_read_time from child's buffer_edited"
        );

        cx.update(|cx| {
            child_log.update(cx, |log, cx| log.buffer_created(buffer.clone(), cx));
        });
        assert!(
            parent_log.read_with(cx, |log, _| log.file_read_time(&abs_path).is_none()),
            "parent should NOT get file_read_time from child's buffer_created"
        );
    }

    #[derive(Debug, PartialEq)]
    struct HunkStatus {
        range: Range<Point>,
        diff_status: DiffHunkStatusKind,
        old_text: String,
    }

    fn unreviewed_hunks(
        action_log: &Entity<ActionLog>,
        cx: &TestAppContext,
    ) -> Vec<(Entity<Buffer>, Vec<HunkStatus>)> {
        cx.read(|cx| {
            action_log
                .read(cx)
                .changed_buffers(cx)
                .map(|(buffer, diff)| {
                    let snapshot = buffer.read(cx).snapshot();
                    (
                        buffer,
                        diff.read(cx)
                            .snapshot(cx)
                            .hunks(&snapshot)
                            .map(|hunk| HunkStatus {
                                diff_status: hunk.status().kind,
                                range: hunk.range,
                                old_text: diff
                                    .read(cx)
                                    .base_text(cx)
                                    .text_for_range(hunk.diff_base_byte_range)
                                    .collect(),
                            })
                            .collect(),
                    )
                })
                .collect()
        })
    }
}
