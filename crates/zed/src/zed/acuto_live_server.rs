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
use ui::prelude::*;
use ui::{ContextMenu, Tooltip, right_click_menu};
use util::ResultExt as _;
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

    fn toggle(&mut self, cx: &mut Context<Self>) {
        match self.state {
            ServerState::Running { .. } => self.stop(cx),
            ServerState::Stopped | ServerState::Failed(_) => self.start(cx),
        }
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        self._server = None;
        self._watcher = None;
        self.state = ServerState::Stopped;
        cx.notify();
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.document_root(cx) else {
            self.state = ServerState::Failed("Open a folder first".into());
            cx.notify();
            return;
        };

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
        let listener = match std::net::TcpListener::bind(SocketAddr::from((
            Ipv4Addr::LOCALHOST,
            0,
        ))) {
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
                    changes.record(batch.into_iter().map(|event| event.path));
                }
            }
        }));

        self.state = ServerState::Running {
            port,
            root: root.clone(),
        };
        cx.open_url(&format!("http://127.0.0.1:{port}/"));
        cx.notify();
    }

    fn open_in_browser(&self, cx: &mut App) {
        if let ServerState::Running { port, .. } = self.state {
            cx.open_url(&format!("http://127.0.0.1:{port}/"));
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
                        move |_, _, cx| {
                            entity.update(cx, |this, cx| this.toggle(cx));
                        }
                    })
            })
            .menu(move |window, cx| {
                let entity = entity_for_menu.clone();
                ContextMenu::build(window, cx, move |menu, _, _| {
                    menu.header("Live Server")
                        .entry(if running { "Stop" } else { "Start" }, None, {
                            let entity = entity.clone();
                            move |_, cx| {
                                entity.update(cx, |this, cx| this.toggle(cx));
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
    let Some(request_target) = read_request_target(&mut stream).await else {
        return;
    };

    let (path, query) = match request_target.split_once('?') {
        Some((path, query)) => (path, query),
        None => (request_target.as_str(), ""),
    };

    let response = if path == "/__acuto_live_reload" {
        let since = query
            .split('&')
            .find_map(|pair| pair.strip_prefix("since="))
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        await_change(since, &changes).await
    } else {
        serve_path(path, &root).await
    };

    stream.write_all(&response).await.log_err();
    stream.close().await.log_err();
}

/// Reads the request line and discards the headers.
///
/// Bounded so a client that never sends a blank line cannot grow this buffer
/// without limit; a request target longer than this is not one a browser sends.
async fn read_request_target(stream: &mut smol::net::TcpStream) -> Option<String> {
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
    let request_line = head.lines().next()?;
    let mut parts = request_line.split(' ');
    let method = parts.next()?;
    if method != "GET" && method != "HEAD" {
        return None;
    }
    Some(percent_decode(parts.next()?))
}

/// Holds a reload poll open until something changes, or until it times out.
async fn await_change(since: u64, changes: &ChangeLog) -> Vec<u8> {
    /// How often the poll re-checks. Small enough to feel immediate, large
    /// enough that an idle page costs nothing measurable.
    const TICK: Duration = Duration::from_millis(100);

    let deadline = std::time::Instant::now() + RELOAD_POLL_TIMEOUT;
    loop {
        let current = changes.generation.load(Ordering::SeqCst);
        if current != since || std::time::Instant::now() >= deadline {
            let styles_only = changes.styles_only.load(Ordering::SeqCst);
            return http_response(
                200,
                "application/json",
                format!("{{\"generation\":{current},\"styles_only\":{styles_only}}}")
                    .into_bytes(),
            );
        }
        smol::Timer::after(TICK).await;
    }
}

async fn serve_path(request_path: &str, root: &Path) -> Vec<u8> {
    let Some(mut path) = resolve_within(root, request_path) else {
        return http_response(403, "text/plain; charset=utf-8", b"Forbidden".to_vec());
    };

    if path.is_dir() {
        path = path.join("index.html");
    }

    let Ok(bytes) = smol::fs::read(&path).await else {
        return http_response(
            404,
            "text/html; charset=utf-8",
            not_found_page(request_path).into_bytes(),
        );
    };

    let content_type = content_type_for(&path);
    if content_type.starts_with("text/html") {
        return http_response(200, content_type, inject_reload_script(bytes));
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

fn inject_reload_script(mut html: Vec<u8>) -> Vec<u8> {
    let text = String::from_utf8_lossy(&html).into_owned();
    // Before `</body>` if there is one, so the page's own scripts have run;
    // appended otherwise, because a fragment without a body tag still executes.
    match text.rfind("</body>") {
        Some(index) => {
            let mut out = text;
            out.insert_str(index, RELOAD_SCRIPT);
            out.into_bytes()
        }
        None => {
            html.extend_from_slice(RELOAD_SCRIPT.as_bytes());
            html
        }
    }
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
        "html" | "htm" => "text/html; charset=utf-8",
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
