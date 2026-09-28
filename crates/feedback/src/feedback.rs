use client::telemetry;
use extension_host::ExtensionStore;
use gpui::{App, ClipboardItem, PromptLevel, actions};
use system_specs::{CopySystemSpecsIntoClipboard, SystemSpecs};
use util::ResultExt;
use workspace::Workspace;
use zed_actions::feedback::{EmailZed, FileBugReport, RequestFeature};

actions!(
    zed,
    [
        /// Opens the Acuto repository on GitHub.
        OpenZedRepo,
        /// Copies installed extensions to the clipboard for bug reports.
        CopyInstalledExtensionsIntoClipboard
    ]
);

/// Acuto's own repository, not Zed's: reports and requests about Acuto go
/// to the people who can act on them.
const ACUTO_REPO_URL: &str = "https://github.com/solvir-group/acuto";

const FEEDBACK_URL: &str = "https://acuto.dev/feedback";

fn file_bug_report_url(specs: &SystemSpecs) -> String {
    let body = format!(
        "**What happened**\n\n\n**What you expected**\n\n\n**System information**\n\n{specs}"
    );
    format!(
        "{ACUTO_REPO_URL}/issues/new?body={}",
        urlencoding::encode(&body)
    )
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace
            .register_action(|_, _: &CopySystemSpecsIntoClipboard, window, cx| {
                let specs =
                    SystemSpecs::new(window, cx, telemetry::os_name(), telemetry::os_version());

                cx.spawn_in(window, async move |_, cx| {
                    let specs = specs.await.to_string();

                    cx.update(|_, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(specs.clone()))
                    })
                    .log_err();

                    cx.prompt(
                        PromptLevel::Info,
                        "Copied into clipboard",
                        Some(&specs),
                        &["OK"],
                    )
                    .await
                })
                .detach();
            })
            .register_action(|_, _: &CopyInstalledExtensionsIntoClipboard, window, cx| {
                let clipboard_text = format_installed_extensions_for_clipboard(cx);
                cx.write_to_clipboard(ClipboardItem::new_string(clipboard_text.clone()));
                drop(window.prompt(
                    PromptLevel::Info,
                    "Copied into clipboard",
                    Some(&clipboard_text),
                    &["OK"],
                    cx,
                ));
            })
            .register_action(|_, _: &RequestFeature, _, cx| {
                cx.open_url(FEEDBACK_URL);
            })
            .register_action(move |_, _: &FileBugReport, window, cx| {
                let specs =
                    SystemSpecs::new(window, cx, telemetry::os_name(), telemetry::os_version());
                cx.spawn_in(window, async move |_, cx| {
                    let specs = specs.await;
                    cx.update(|_, cx| {
                        cx.open_url(&file_bug_report_url(&specs));
                    })
                    .log_err();
                })
                .detach();
            })
            // There is no Acuto mailbox; the feedback page is where messages go.
            .register_action(move |_, _: &EmailZed, _, cx| {
                cx.open_url(FEEDBACK_URL);
            })
            .register_action(move |_, _: &OpenZedRepo, _, cx| {
                cx.open_url(ACUTO_REPO_URL);
            });
    })
    .detach();
}

fn format_installed_extensions_for_clipboard(cx: &mut App) -> String {
    let store = ExtensionStore::global(cx);
    let store = store.read(cx);
    let mut lines = Vec::with_capacity(store.extension_index.extensions.len());

    for (extension_id, entry) in store.extension_index.extensions.iter() {
        let line = format!(
            "- {} ({}) v{}{}",
            entry.manifest.name,
            extension_id,
            entry.manifest.version,
            if entry.dev { " (dev)" } else { "" }
        );
        lines.push(line);
    }

    lines.sort();

    if lines.is_empty() {
        return "No extensions installed.".to_string();
    }

    format!(
        "Installed extensions ({}):\n{}",
        lines.len(),
        lines.join("\n")
    )
}
