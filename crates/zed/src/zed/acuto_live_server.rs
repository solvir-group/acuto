//! A static file server for the open project, started from the status bar.
//!
//! The thing VS Code's "Live Server" extension does: serve the folder over
//! `http://127.0.0.1`, open a browser at it, and reload the page when a file
//! changes. Editing a page and alt-tabbing to a stale browser is the single
//! most repeated action in front-end work, and everything needed to remove it
//! is already in the process -- a filesystem watcher, an async executor, and a
//! worktree root.
//!
//! It is deliberately not a general web server. It binds the loopback address
//! only, it speaks enough HTTP/1.1 to answer a browser and no more, and it
//! closes every connection after one response rather than keeping a keep-alive
//! state machine. Anything beyond serving a directory to the machine it runs on
//! is out of scope.

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use futures::{AsyncReadExt as _, AsyncWriteExt as _, StreamExt as _};
use gpui::{Anchor, Task};
use task::{RevealStrategy, SpawnInTerminal};
use terminal_view::terminal_panel::TerminalPanel;
use ui::prelude::*;
use ui::{ContextMenu, Tooltip, right_click_menu};
use util::{ResultExt as _, maybe};
use workspace::{HideStatusItem, StatusItemView, Workspace, item::ItemHandle};

/// How long a reload poll waits before answering "nothing yet".
///
/// Long enough that an idle page is not making constant requests, short enough
/// that the browser's own connection timeouts never fire first.
const RELOAD_POLL_TIMEOUT: Duration = Duration::from_secs(25);

/// How long the watcher coalesces filesystem events before reporting them.
///
/// A single save from an editor is several events -- a temp file, a rename, a
/// metadata change -- and reloading the page once per event makes the browser
/// flash three times for one keystroke.
const WATCH_LATENCY: Duration = Duration::from_millis(150);

/// Injected into every HTML response so a saved file reaches the open page.
///
/// Written as a long poll rather than a websocket because it is a dozen lines
/// instead of a handshake implementation, and because a poll that fails simply
/// retries -- there is no connection to lose. The `id` guard means a page that
/// somehow receives two copies of the script only runs one loop.
const RELOAD_SCRIPT: &str = r#"<script id="acuto-live-reload">
(function () {
  if (window.__acutoLiveReload) { return; }
  window.__acutoLiveReload = true;

  let since = 0;

  // Stylesheets are re-fetched in place rather than reloading the document.
  // A reload throws away scroll position, form state, open dialogs and any
  // JavaScript state the page had built up -- which for the CSS tweak you are
  // in the middle of is the entire reason you were looking at the page.
  function reloadStyles() {
    const links = document.querySelectorAll('link[rel="stylesheet"][href]');
    for (const link of links) {
      const url = new URL(link.href, location.href);
      if (url.origin !== location.origin) { continue; }
      url.searchParams.set("__acuto", String(Date.now()));
      // A fresh element that replaces the old one only after it has loaded,
      // so the page is never briefly unstyled.
      const replacement = link.cloneNode();
      replacement.href = url.href;
      replacement.addEventListener("load", () => link.remove(), { once: true });
      replacement.addEventListener("error", () => replacement.remove(), { once: true });
      link.after(replacement);
    }
  }

  async function poll() {
    try {
      const response = await fetch("/__acuto_live_reload?since=" + since, { cache: "no-store" });
      const body = await response.json();
      if (since !== 0 && body.generation > since) {
        if (body.styles_only) { reloadStyles(); } else { location.reload(); return; }
      }
      since = body.generation;
    } catch (error) {
      await new Promise((resolve) => setTimeout(resolve, 1000));
    }
    poll();
  }
  poll();
})();
</script>"#;

/// What changed since a given generation.
///
/// The client needs to know whether it can swap stylesheets or has to reload,
/// and only the watcher knows which files moved. Kept as a single atomic pair
/// so a reader never sees a generation without its kind.
struct ChangeLog {
    generation: AtomicU64,
    /// True while every change since the last full reload was a stylesheet.
    styles_only: AtomicBool,
}

impl ChangeLog {
    fn new() -> Self {
        Self {
            generation: AtomicU64::new(1),
            styles_only: AtomicBool::new(false),
        }
    }

    /// Records a batch of changed paths.
    ///
    /// A batch counts as styles-only when every path in it is a stylesheet.
    /// One changed `.js` file in the same batch means the page has to reload,
    /// because swapping stylesheets would leave the old script running.
    fn record(&self, paths: impl IntoIterator<Item = PathBuf>) {
        let mut any = false;
        let mut all_styles = true;
        for path in paths {
            any = true;
            let is_style = path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("css"));
            all_styles &= is_style;
        }
        if !any {
            return;
        }
        self.styles_only.store(all_styles, Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Records that something changed without knowing what.
    ///
    /// Used when the filesystem disagrees with the watcher. A full reload
    /// rather than a stylesheet swap, because the whole point of arriving here
    /// is that the changed paths are unknown -- and swapping stylesheets when
    /// the change was actually a script leaves the old script running against
    /// new markup, which is worse than the reload it was avoiding.
    fn record_unknown(&self) {
        self.styles_only.store(false, Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Clone)]
enum ServerState {
    Stopped,
    Running { port: u16, root: Arc<Path> },
    Failed(SharedString),
}

/// Starts and stops the live server, and shows which it is.
pub struct LiveServerButton {
    workspace: gpui::WeakEntity<Workspace>,
    state: ServerState,
    /// Dropping this stops the server: the accept loop ends with the task, and
    /// the listener closes with the loop.
    _server: Option<Task<()>>,
    _watcher: Option<Task<()>>,
}

impl LiveServerButton {
    pub fn new(workspace: gpui::WeakEntity<Workspace>) -> Self {
        Self {
            workspace,
            state: ServerState::Stopped,
            _server: None,
            _watcher: None,
        }
    }

    /// The directory the server serves.
    ///
    /// The first visible worktree, which is the folder the window is "about".
    /// A window with several worktrees serves the first rather than inventing a
    /// merged tree that matches no path on disk.
    fn document_root(&self, cx: &App) -> Option<Arc<Path>> {
        let workspace = self.workspace.upgrade()?;
        let project = workspace.read(cx).project().read(cx);
        let worktree = project.visible_worktrees(cx).next()?;
        Some(worktree.read(cx).abs_path())
    }

    /// The page to open in the browser, as a URL path.
    ///
    /// The HTML file you are editing, when it is inside the served folder:
    /// that is the page you want to see, and a folder of loose pages has no
    /// `index.html` for `/` to find. Otherwise `/`, which serves `index.html`
    /// when there is one and a list of the folder's pages when there is not.
    fn page_to_open(&self, root: &Path, cx: &App) -> String {
        let active_page = maybe!({
            let workspace = self.workspace.upgrade()?;
            let workspace = workspace.read(cx);
            let project_path = workspace.active_item(cx)?.project_path(cx)?;
            let absolute = workspace
                .project()
                .read(cx)
                .absolute_path(&project_path, cx)?;
            let is_page = absolute
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| {
                    extension.eq_ignore_ascii_case("html") || extension.eq_ignore_ascii_case("htm")
                });
            if !is_page {
                return None;
            }
            let relative = absolute.strip_prefix(root).ok()?;
            Some(url_path_for(relative))
        });
        active_page.unwrap_or_else(|| "/".to_string())
    }

    fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.state {
            ServerState::Running { .. } => self.stop(cx),
            ServerState::Stopped | ServerState::Failed(_) => self.start(window, cx),
        }
    }

    /// Starts the project's own dev server in the terminal.
    ///
    /// Deliberately not supervised or proxied by this button. A dev server is a
    /// long-running process with output worth reading -- compile errors, the URL
    /// it chose, the port it fell back to -- and hiding it behind a status dot
    /// would throw all of that away. The terminal already linkifies the URL it
    /// prints, which is both more accurate than guessing a port and correct when
    /// the usual one was taken.
    fn start_dev_server(&mut self, script: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };

        let command = format!("npm run {script}");
        let label = format!("Dev server ({script})");
        let task = SpawnInTerminal {
            id: task::TaskId("acuto-dev-server".into()),
            full_label: label.clone(),
            label: label.clone(),
            command: Some(command.clone()),
            args: Vec::new(),
            command_label: command,
            cwd: None,
            env: Default::default(),
            use_new_terminal: true,
            allow_concurrent_runs: false,
            reveal: RevealStrategy::Always,
            reveal_target: zed_actions::RevealTarget::Dock,
            hide: task::HideStrategy::Never,
            shell: task::Shell::System,
            show_summary: true,
            show_command: true,
            show_rerun: true,
            save: task::SaveStrategy::None,
        };

        workspace.update(cx, |workspace, cx| {
            let Some(panel) = workspace.panel::<TerminalPanel>(cx) else {
                return;
            };
            panel.update(cx, |panel, cx| {
                panel
                    .add_terminal_task(task, RevealStrategy::Always, window, cx)
                    .detach_and_log_err(cx);
            });
        });

        // Not `Running`: nothing is being served from here, so claiming a port
        // and offering to stop it would both be lies.
        self.state = ServerState::Stopped;
        cx.notify();
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        self._server = None;
        self._watcher = None;
        self.state = ServerState::Stopped;
        cx.notify();
    }

    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.document_root(cx) else {
            self.state = ServerState::Failed("Open a folder first".into());
            cx.notify();
            return;
        };

        if let Some(script) = dev_server_script(&root) {
            self.start_dev_server(script, window, cx);
            return;
        }

        let Some(fs) = self
            .workspace
            .upgrade()
            .map(|workspace| workspace.read(cx).project().read(cx).fs().clone())
        else {
            self.state = ServerState::Failed("No project".into());
            cx.notify();
            return;
        };

        let changes = Arc::new(ChangeLog::new());
        let executor = cx.background_executor().clone();

        // Bound on the foreground so the port is known before the button
        // redraws: a button that says "starting" and then a port one frame
        // later reads as a glitch for something this fast.
        let listener = match std::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        {
            Ok(listener) => listener,
            Err(error) => {
                self.state = ServerState::Failed(format!("Could not bind: {error}").into());
                cx.notify();
                return;
            }
        };

        let port = match listener.local_addr() {
            Ok(address) => address.port(),
            Err(error) => {
                self.state = ServerState::Failed(format!("No local address: {error}").into());
                cx.notify();
                return;
            }
        };

        if let Err(error) = listener.set_nonblocking(true) {
            self.state = ServerState::Failed(format!("Could not configure: {error}").into());
            cx.notify();
            return;
        }

        self._server = Some(cx.background_spawn({
            let root = root.clone();
            let changes = changes.clone();
            let executor = executor.clone();
            async move {
                let Some(listener) = smol::net::TcpListener::try_from(listener).log_err() else {
                    return;
                };
                let mut incoming = listener.incoming();
                while let Some(stream) = incoming.next().await {
                    let Some(stream) = stream.log_err() else {
                        continue;
                    };
                    executor
                        .spawn(serve_connection(stream, root.clone(), changes.clone()))
                        .detach();
                }
            }
        }));

        self._watcher = Some(cx.background_spawn({
            let root = root.clone();
            let changes = changes.clone();
            async move {
                let (mut events, _watcher) = fs.watch(&root, WATCH_LATENCY).await;
                while let Some(batch) = events.next().await {
                    // The editor's own git status refresh touches `.git`
                    // constantly, and none of it is part of a page.
                    changes.record(
                        batch
                            .into_iter()
                            .map(|event| event.path)
                            .filter(|path| is_served_change(&root, path)),
                    );
                }
            }
        }));

        let page = self.page_to_open(&root, cx);
        self.state = ServerState::Running {
            port,
            root: root.clone(),
        };
        cx.open_url(&format!("http://127.0.0.1:{port}{page}"));
        cx.notify();
    }

    fn open_in_browser(&self, cx: &mut App) {
        if let ServerState::Running { port, root } = &self.state {
            let page = self.page_to_open(root, cx);
            cx.open_url(&format!("http://127.0.0.1:{port}{page}"));
        }
    }
}

impl Render for LiveServerButton {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let entity_for_menu = entity.clone();

        let (label, color, tooltip) = match &self.state {
            ServerState::Stopped => (
                "Go Live".to_string(),
                Color::Muted,
                "Serve this folder and open it in a browser".to_string(),
            ),
            ServerState::Running { port, root } => (
                format!("Live :{port}"),
                Color::Accent,
                format!("Serving {} - click to stop", root.display()),
            ),
            ServerState::Failed(reason) => (
                "Go Live".to_string(),
                Color::Error,
                format!("Live server failed: {reason}"),
            ),
        };

        let running = matches!(self.state, ServerState::Running { .. });

        right_click_menu("live-server-menu")
            // Opens upward. The default drops the menu below its trigger,
            // which for anything in the status bar is off the bottom of the
            // window: the menu opens and is never seen.
            .anchor(Anchor::BottomRight)
            .attach(Anchor::TopRight)
            .trigger(move |_, _, _| {
                Button::new("live-server", label.clone())
                    .label_size(LabelSize::Small)
                    .color(color)
                    .tooltip(Tooltip::text(tooltip.clone()))
                    .on_click({
                        let entity = entity.clone();
                        move |_, window, cx| {
                            entity.update(cx, |this, cx| this.toggle(window, cx));
                        }
                    })
            })
            .menu(move |window, cx| {
                let entity = entity_for_menu.clone();
                ContextMenu::build(window, cx, move |menu, _, _| {
                    menu.header("Live Server")
                        .entry(if running { "Stop" } else { "Start" }, None, {
                            let entity = entity.clone();
                            move |window, cx| {
                                entity.update(cx, |this, cx| this.toggle(window, cx));
                            }
                        })
                        .when(running, |menu| {
                            menu.entry("Open in Browser", None, {
                                let entity = entity.clone();
                                move |_, cx| {
                                    entity.update(cx, |this, cx| this.open_in_browser(cx));
                                }
                            })
                        })
                })
            })
    }
}

impl gpui::EventEmitter<()> for LiveServerButton {}

impl StatusItemView for LiveServerButton {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        // What is serving does not depend on what is open.
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}

/// Answers one request and closes the connection.
///
/// One response per connection rather than keep-alive: the difference is
/// invisible over loopback, and it removes the only piece of state a connection
/// would otherwise carry.
async fn serve_connection(
    mut stream: smol::net::TcpStream,
    root: Arc<Path>,
    changes: Arc<ChangeLog>,
) {
    let Some(RequestHead {
        is_head,
        target: request_target,
        host,
        range,
    }) = read_request_target(&mut stream).await
    else {
        return;
    };

    // A page on another site can point its own hostname at 127.0.0.1 and then
    // read anything served here as same-origin. The Host it sends is still
    // its own, so only loopback names are answered.
    if !host.as_deref().is_some_and(is_loopback_host) {
        stream
            .write_all(&http_response(
                403,
                "text/plain; charset=utf-8",
                b"Forbidden".to_vec(),
            ))
            .await
            .log_err();
        stream.close().await.log_err();
        return;
    }

    // Split before decoding, so an escaped `?` in a file name stays part of
    // the name instead of starting the query.
    let (path, query) = match request_target.split_once('?') {
        Some((path, query)) => (percent_decode(path), query),
        None => (percent_decode(&request_target), ""),
    };
    let path = path.as_str();

    let response = if path == "/__acuto_live_reload" {
        let since = query
            .split('&')
            .find_map(|pair| pair.strip_prefix("since="))
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        await_change(since, &changes, &root).await
    } else {
        serve_path(path, range.as_deref(), &root).await
    };
    // HEAD answers with the headers GET would send, Content-Length included,
    // and no body.
    let response = if is_head {
        match response.windows(4).position(|window| window == b"\r\n\r\n") {
            Some(end) => response[..end + 4].to_vec(),
            None => response,
        }
    } else {
        response
    };

    stream.write_all(&response).await.log_err();
    stream.close().await.log_err();
}

/// Reads the request line and discards the headers.
///
/// Bounded so a client that never sends a blank line cannot grow this buffer
/// without limit; a request target longer than this is not one a browser sends.
/// The parts of a request this server acts on.
struct RequestHead {
    is_head: bool,
    target: String,
    host: Option<String>,
    range: Option<String>,
}

async fn read_request_target(stream: &mut smol::net::TcpStream) -> Option<RequestHead> {
    /// Enough for any real URL plus the headers a browser sends.
    const MAX_REQUEST_BYTES: usize = 16 * 1024;

    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).await.log_err()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);

        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if buffer.len() > MAX_REQUEST_BYTES {
            return None;
        }
    }

    let head = String::from_utf8_lossy(&buffer);
    let mut lines = head.lines();
    let request_line = lines.next()?;
    let mut parts = request_line.split(' ');
    let method = parts.next()?;
    if method != "GET" && method != "HEAD" {
        return None;
    }
    let target = parts.next()?.to_string();
    let headers = lines
        .take_while(|line| !line.is_empty())
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect::<Vec<_>>();
    let header = |wanted: &str| {
        headers
            .iter()
            .find(|(name, _)| name == wanted)
            .map(|(_, value)| value.clone())
    };
    Some(RequestHead {
        is_head: method == "HEAD",
        target,
        host: header("host"),
        range: header("range"),
    })
}

/// The byte range a `Range: bytes=...` header asks for within `length`
/// bytes, or `None` to send the whole file. Only single ranges, which is what
/// a media element asks for when it seeks.
fn requested_range(range: &str, length: usize) -> Option<std::ops::Range<usize>> {
    let spec = range.trim().strip_prefix("bytes=")?;
    if spec.contains(',') || length == 0 {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    let (start, end) = match (start.trim(), end.trim()) {
        ("", suffix) => {
            let suffix = suffix.parse::<usize>().ok()?.min(length);
            (length - suffix, length - 1)
        }
        (start, "") => (start.parse::<usize>().ok()?, length - 1),
        (start, end) => (start.parse::<usize>().ok()?, end.parse::<usize>().ok()?.min(length - 1)),
    };
    (start <= end && start < length).then_some(start..end + 1)
}

/// Whether a Host header names this machine.
fn is_loopback_host(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next().unwrap_or_default(),
        None => host.rsplit_once(':').map_or(host, |(name, _)| name),
    };
    name.eq_ignore_ascii_case("localhost")
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// Holds a reload poll open until something changes, or until it times out.
async fn await_change(since: u64, changes: &ChangeLog, root: &Path) -> Vec<u8> {
    /// How often the poll re-checks. Small enough to feel immediate, large
    /// enough that an idle page costs nothing measurable.
    const TICK: Duration = Duration::from_millis(100);

    /// How often the poll checks the filesystem itself rather than only the
    /// watcher's counter.
    ///
    /// Fifty times less often than the tick: walking the tree is far more
    /// expensive than reading an atomic, and this is a safety net for events
    /// the watcher dropped, not the mechanism anyone should be relying on.
    const VERIFY_EVERY: u32 = 50;

    let deadline = std::time::Instant::now() + RELOAD_POLL_TIMEOUT;
    let baseline = fingerprint(root).await;
    let mut ticks: u32 = 0;

    loop {
        let mut current = changes.generation.load(Ordering::SeqCst);

        // The watcher missed it. Bump the counter so every other client polling
        // this server agrees a change happened, rather than each discovering it
        // separately and reporting a different generation.
        if current == since && ticks > 0 && ticks % VERIFY_EVERY == 0 {
            if fingerprint(root).await != baseline {
                changes.record_unknown();
                current = changes.generation.load(Ordering::SeqCst);
            }
        }

        if current != since || std::time::Instant::now() >= deadline {
            let styles_only = changes.styles_only.load(Ordering::SeqCst);
            return http_response(
                200,
                "application/json",
                format!("{{\"generation\":{current},\"styles_only\":{styles_only}}}").into_bytes(),
            );
        }

        ticks = ticks.saturating_add(1);
        smol::Timer::after(TICK).await;
    }
}

/// A cheap summary of the served tree: how many files it holds and the latest
/// modification time among them.
///
/// Deliberately not a hash of the contents. This runs several times a second
/// while a page is open, and reading every file to answer "did anything change"
/// would make the live server the most expensive thing in the process. Count
/// plus newest mtime catches edits, creations and deletions, which is every
/// change that should reload a page.
///
/// Directories that never belong to a served site are skipped, because walking
/// `node_modules` on every poll costs more than the whole rest of the feature.
async fn fingerprint(root: &Path) -> (u64, u64) {
    const SKIP: [&str; 5] = ["node_modules", ".git", "target", "dist", ".next"];
    /// Depth beyond which a tree is assumed not to be a static site worth
    /// watching, so a poll can never walk an unbounded hierarchy.
    const MAX_DEPTH: usize = 8;

    let mut count = 0u64;
    let mut newest = 0u64;
    let mut stack = vec![(root.to_path_buf(), 0usize)];

    while let Some((directory, depth)) = stack.pop() {
        let Ok(mut entries) = smol::fs::read_dir(&directory).await else {
            continue;
        };
        while let Some(Ok(entry)) = smol::stream::StreamExt::next(&mut entries).await {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') && name != ".well-known" {
                continue;
            }
            if SKIP.contains(&name.as_ref()) {
                continue;
            }

            let Ok(metadata) = entry.metadata().await else {
                continue;
            };
            if metadata.is_dir() {
                if depth < MAX_DEPTH {
                    stack.push((entry.path(), depth + 1));
                }
                continue;
            }

            count += 1;
            if let Ok(modified) = metadata.modified()
                && let Ok(since_epoch) = modified.duration_since(std::time::UNIX_EPOCH)
            {
                newest = newest.max(since_epoch.as_millis() as u64);
            }
        }
    }

    (count, newest)
}

async fn serve_path(request_path: &str, range: Option<&str>, root: &Path) -> Vec<u8> {
    let Some(mut path) = resolve_within(root, request_path) else {
        return http_response(403, "text/plain; charset=utf-8", b"Forbidden".to_vec());
    };

    let is_directory = path.is_dir();
    // `/site` must become `/site/` before its page is served: the browser
    // resolves the page's relative links against the URL, and without the
    // slash `style.css` would be looked for next to the folder, not in it.
    if is_directory && !request_path.ends_with('/') {
        let relative = path.strip_prefix(root).unwrap_or(Path::new(""));
        return redirect_response(&format!("{}/", url_path_for(relative).trim_end_matches('/')));
    }
    if is_directory {
        let index = path.join("index.html");
        if index.is_file() {
            path = index;
        }
    }

    // A symlink or junction inside the project is followed by the filesystem,
    // so a path built only from plain names can still lead outside the folder.
    // Both are resolved to where they really are, and anything outside the
    // real root is refused: a page served from here must not be able to read
    // the rest of the disk.
    let (Ok(real_root), Ok(real_path)) = (
        smol::fs::canonicalize(root).await,
        smol::fs::canonicalize(&path).await,
    ) else {
        return http_response(
            404,
            "text/html; charset=utf-8",
            not_found_page(request_path).into_bytes(),
        );
    };
    if !real_path.starts_with(&real_root) {
        return http_response(403, "text/plain; charset=utf-8", b"Forbidden".to_vec());
    }

    if path.is_dir() {
        return http_response(
            200,
            "text/html; charset=utf-8",
            directory_listing(&path, request_path).await.into_bytes(),
        );
    }

    let Ok(bytes) = smol::fs::read(&path).await else {
        return http_response(
            404,
            "text/html; charset=utf-8",
            not_found_page(request_path).into_bytes(),
        );
    };

    let content_type = content_type_for(&path);
    if content_type == "text/html" {
        return http_response(200, content_type, inject_reload_script(bytes));
    }
    if let Some(range) = range.and_then(|range| requested_range(range, bytes.len())) {
        let total = bytes.len();
        let mut response = format!(
            "HTTP/1.1 206 Partial Content\r\n\
             Content-Type: {content_type}\r\n\
             Content-Length: {}\r\n\
             Content-Range: bytes {}-{}/{total}\r\n\
             Accept-Ranges: bytes\r\n\
             Cache-Control: no-store\r\n\
             Connection: close\r\n\r\n",
            range.len(),
            range.start,
            range.end - 1,
        )
        .into_bytes();
        response.extend_from_slice(&bytes[range]);
        return response;
    }
    http_response(200, content_type, bytes)
}

/// Maps a request path onto a file inside `root`, or refuses.
///
/// Traversal is rejected by rebuilding the path from its components rather than
/// by canonicalising and comparing prefixes: a symlink inside the tree that
/// points outside it would pass a prefix check on the pre-resolution path, and
/// `..` never survives this at all because it is dropped rather than applied.
fn resolve_within(root: &Path, request_path: &str) -> Option<PathBuf> {
    let mut resolved = root.to_path_buf();
    for segment in request_path.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            return None;
        }
        // Hidden files are where secrets live -- `.env`, `.git/config` -- and
        // no page needs them. `.well-known` is the one hidden path the web uses.
        if segment.starts_with('.') && segment != ".well-known" {
            return None;
        }
        let candidate = Path::new(segment);
        // A segment that is anything other than one plain name -- a drive
        // letter, a UNC prefix, a root -- would reset the path being built.
        let mut components = candidate.components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(name)), None) => resolved.push(name),
            _ => return None,
        }
    }
    Some(resolved)
}

/// Whether a changed path belongs to what is being served, rather than to
/// version control or build output beside it.
fn is_served_change(root: &Path, path: &Path) -> bool {
    const SKIP: [&str; 4] = ["node_modules", "target", "dist", ".next"];
    let relative = path.strip_prefix(root).unwrap_or(path);
    !relative.components().any(|component| match component {
        Component::Normal(name) => {
            let name = name.to_string_lossy();
            (name.starts_with('.') && name != ".well-known") || SKIP.contains(&name.as_ref())
        }
        _ => false,
    })
}

fn inject_reload_script(mut html: Vec<u8>) -> Vec<u8> {
    const BODY_END: &[u8] = b"</body>";
    // Spliced into the bytes rather than a decoded copy, so a page in another
    // encoding comes back byte for byte. Before `</body>` if there is one, so
    // the page's own scripts have run; appended otherwise, because a fragment
    // without a body tag still executes.
    let body_end = html
        .windows(BODY_END.len())
        .rposition(|window| window.eq_ignore_ascii_case(BODY_END));
    match body_end {
        Some(index) => {
            html.splice(index..index, RELOAD_SCRIPT.bytes());
        }
        None => html.extend_from_slice(RELOAD_SCRIPT.as_bytes()),
    }
    html
}

/// A page linking to what is in `directory`, for a folder with no
/// `index.html`.
///
/// A folder of loose pages is the usual shape of a first project, and a 404 at
/// `/` told its owner nothing about why; this shows them their pages instead.
/// Hidden entries are left out: `.git` and `.acuto` are not pages.
async fn directory_listing(directory: &Path, request_path: &str) -> String {
    let mut folders = Vec::new();
    let mut files = Vec::new();
    if let Ok(mut entries) = smol::fs::read_dir(directory).await {
        while let Some(Ok(entry)) = entries.next().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            match entry.file_type().await {
                Ok(kind) if kind.is_dir() => folders.push(name),
                Ok(_) => files.push(name),
                Err(_) => continue,
            }
        }
    }
    folders.sort_by_key(|name| name.to_lowercase());
    files.sort_by_key(|name| name.to_lowercase());

    let base = if request_path.ends_with('/') {
        request_path.to_string()
    } else {
        format!("{request_path}/")
    };
    let mut items = String::new();
    for name in &folders {
        items.push_str(&format!(
            "<li><a href=\"{}{}/\">{}/</a></li>",
            html_escape(&base),
            html_escape(&url_segment(name)),
            html_escape(name)
        ));
    }
    for name in &files {
        items.push_str(&format!(
            "<li><a href=\"{}{}\">{}</a></li>",
            html_escape(&base),
            html_escape(&url_segment(name)),
            html_escape(name)
        ));
    }
    if items.is_empty() {
        items.push_str("<li>This folder is empty.</li>");
    }

    format!(
        "<!doctype html><meta charset=\"utf-8\"><title>{title}</title>\
         <body style=\"font-family:system-ui;padding:2rem 3rem;line-height:1.7\">\
         <h1 style=\"font-size:1.3rem\">{title}</h1>\
         <p style=\"color:#666\">No index.html here, so these are the files in this folder.</p>\
         <ul>{items}</ul>{RELOAD_SCRIPT}</body>",
        title = html_escape(request_path)
    )
}

/// A worktree-relative path as a URL path, each segment escaped.
fn url_path_for(relative: &Path) -> String {
    let mut path = String::new();
    for component in relative.components() {
        if let Component::Normal(segment) = component {
            path.push('/');
            path.push_str(&url_segment(&segment.to_string_lossy()));
        }
    }
    if path.is_empty() {
        path.push('/');
    }
    path
}

/// Escapes the characters a file name can hold that would change a URL's
/// meaning, and leaves the rest readable.
fn url_segment(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for character in name.chars() {
        match character {
            ' ' => out.push_str("%20"),
            '#' => out.push_str("%23"),
            '?' => out.push_str("%3F"),
            '%' => out.push_str("%25"),
            '"' => out.push_str("%22"),
            _ => out.push(character),
        }
    }
    out
}

fn not_found_page(request_path: &str) -> String {
    format!(
        "<!doctype html><meta charset=\"utf-8\"><title>404</title>\
         <body style=\"font-family:system-ui;padding:3rem\">\
         <h1>404</h1><p>No file at <code>{}</code>.</p>{RELOAD_SCRIPT}</body>",
        html_escape(request_path)
    )
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn content_type_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        // No charset: the page's own `<meta charset>` decides, and a header
        // would override it.
        "html" | "htm" => "text/html",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "txt" | "md" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

fn redirect_response(location: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 301 Moved Permanently\r\n\
         Location: {location}\r\n\
         Content-Length: 0\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n"
    )
    .into_bytes()
}

fn http_response(status: u16, content_type: &str, body: Vec<u8>) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "OK",
    };
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(&body);
    response
}

/// Decodes `%XX` escapes, leaving anything malformed as written.
///
/// A path containing a stray `%` is a path someone actually created, and
/// refusing to decode it is better than mangling it.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
            if let Some(byte) = hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_batch_is_styles_only_when_every_path_in_it_is() {
        let changes = ChangeLog::new();
        let start = changes.generation.load(Ordering::SeqCst);

        changes.record([PathBuf::from("site/main.css")]);
        assert!(changes.styles_only.load(Ordering::SeqCst));
        assert_eq!(changes.generation.load(Ordering::SeqCst), start + 1);

        // One script in the batch means the page has to reload: swapping
        // stylesheets would leave the old script running.
        changes.record([PathBuf::from("site/a.css"), PathBuf::from("site/app.js")]);
        assert!(!changes.styles_only.load(Ordering::SeqCst));
        assert_eq!(changes.generation.load(Ordering::SeqCst), start + 2);

        // An empty batch is not a change, and must not wake every open page.
        changes.record([]);
        assert_eq!(changes.generation.load(Ordering::SeqCst), start + 2);

        // Extensions are compared without regard to case.
        changes.record([PathBuf::from("site/Theme.CSS")]);
        assert!(changes.styles_only.load(Ordering::SeqCst));
    }

    #[test]
    fn a_page_path_becomes_an_escaped_url_path() {
        assert_eq!(url_path_for(Path::new("hello.html")), "/hello.html");
        assert_eq!(
            url_path_for(Path::new("site").join("my page.html").as_path()),
            "/site/my%20page.html"
        );
        assert_eq!(url_segment("a#b?c"), "a%23b%3Fc");
    }

    #[test]
    fn traversal_is_refused_rather_than_clamped() {
        let root = Path::new("C:/project");
        assert_eq!(
            resolve_within(root, "/index.html"),
            Some(root.join("index.html"))
        );
        assert_eq!(
            resolve_within(root, "/nested/page.html"),
            Some(root.join("nested").join("page.html"))
        );
        assert_eq!(resolve_within(root, "/../secrets"), None);
        assert_eq!(resolve_within(root, "/nested/../../secrets"), None);
        // An absolute segment would otherwise replace everything built so far.
        // Only Windows reads "C:" as a prefix rather than a plain name.
        #[cfg(windows)]
        assert_eq!(resolve_within(root, "/C:/Windows"), None);
        // Empty and `.` segments are noise, not an error.
        assert_eq!(resolve_within(root, "//./a"), Some(root.join("a")));
        // Hidden files are never served.
        assert_eq!(resolve_within(root, "/.env"), None);
        assert_eq!(resolve_within(root, "/.git/config"), None);
        assert_eq!(
            resolve_within(root, "/.well-known/x"),
            Some(root.join(".well-known").join("x"))
        );
    }

    #[test]
    fn byte_ranges_are_read_the_way_media_elements_ask() {
        assert_eq!(requested_range("bytes=0-", 10), Some(0..10));
        assert_eq!(requested_range("bytes=2-4", 10), Some(2..5));
        assert_eq!(requested_range("bytes=-3", 10), Some(7..10));
        assert_eq!(requested_range("bytes=8-99", 10), Some(8..10));
        assert_eq!(requested_range("bytes=10-", 10), None);
        assert_eq!(requested_range("bytes=0-1,4-5", 10), None);
    }

    #[test]
    fn only_loopback_hosts_are_answered() {
        assert!(is_loopback_host("127.0.0.1:5173"));
        assert!(is_loopback_host("localhost:5173"));
        assert!(is_loopback_host("[::1]:5173"));
        assert!(!is_loopback_host("attacker.example:5173"));
        assert!(!is_loopback_host("attacker.example"));
    }

    #[test]
    fn percent_escapes_decode_and_malformed_ones_survive() {
        assert_eq!(percent_decode("/a%20b.html"), "/a b.html");
        assert_eq!(percent_decode("/100%"), "/100%");
        assert_eq!(percent_decode("/%zz"), "/%zz");
    }

    #[test]
    fn reload_script_goes_inside_the_body() {
        let page = b"<html><body><h1>hi</h1></body></html>".to_vec();
        let injected = String::from_utf8(inject_reload_script(page)).unwrap();
        assert!(injected.contains("acuto-live-reload"));
        assert!(injected.find("acuto-live-reload").unwrap() < injected.find("</body>").unwrap());

        // A fragment with no body tag still gets the script.
        let fragment = b"<h1>hi</h1>".to_vec();
        let injected = String::from_utf8(inject_reload_script(fragment)).unwrap();
        assert!(injected.contains("acuto-live-reload"));
    }
}

/// The npm script that serves this project, when it is not a directory of files.
///
/// Read from `package.json` rather than inferred from a framework directory,
/// because the script is what the user would run and what they will recognise.
///
/// A project with an `index.html` at its root is treated as static even when it
/// has a dev script: that is a hand-written site with tooling attached, and
/// serving it directly is what the button is for.
fn dev_server_script(root: &Path) -> Option<String> {
    if root.join("index.html").exists() {
        return None;
    }

    let manifest = std::fs::read_to_string(root.join("package.json")).ok()?;
    let manifest: serde_json::Value = serde_json::from_str(&manifest).ok()?;
    let scripts = manifest.get("scripts")?.as_object()?;

    // In the order a project is most likely to name it.
    ["dev", "start", "serve"]
        .into_iter()
        .find(|name| scripts.contains_key(*name))
        .map(str::to_string)
}
