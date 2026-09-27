//! What makes a Claude Code thread look and behave like Claude Code.
//!
//! Kept together rather than scattered through the thread view because these
//! are all answers to the same question -- "does this feel like the tool the
//! user already knows?" -- and because a second agent with its own identity
//! should be able to sit beside this file rather than thread another set of
//! conditionals through the view.

use std::time::Duration;

use gpui::{AnyElement, FontWeight, Global, Hsla, Task, hsla, img, svg};
use ui::{DotPattern, DotSpinner, Tooltip, prelude::*};

/// Claude's clay, `#D97757`.
///
/// Hard-coded rather than taken from the theme because it identifies the agent,
/// not the editor. A user switching themes still needs Claude to look like
/// Claude, and a theme's accent belongs to whatever the theme wants to accent.
pub(crate) fn claude_clay() -> Hsla {
    hsla(0.041, 0.631, 0.596, 1.0)
}

/// Clay pushed to a warning: same family, unmistakably an alarm.
///
/// Not the theme's error colour, because nothing has gone wrong -- this is a
/// standing condition the user chose, and it should read as "the safety is off"
/// rather than "something failed".
pub(crate) fn relaxed_permissions_red() -> Hsla {
    hsla(0.02, 0.72, 0.55, 1.0)
}

/// Whether a session mode lets the agent act without stopping to ask.
///
/// `auto` hands permission decisions to the model and `bypassPermissions`
/// grants everything outright, so in both the next shell command can run with
/// the user's credentials without anything appearing on screen first. That is
/// worth a standing signal.
///
/// `acceptEdits` is deliberately not in this set. It waives the prompt for file
/// edits only and still stops for commands -- and it is a common default, so
/// colouring it would leave the border red permanently and the signal would
/// stop meaning anything.
pub(crate) fn mode_skips_permission_prompts(mode_id: &str) -> bool {
    let mode_id = mode_id.trim();
    mode_id.eq_ignore_ascii_case("auto") || mode_id.eq_ignore_ascii_case("bypassPermissions")
}

/// How often the machine is checked for a Remote Control bridge.
///
/// Enumerating every process is not free, so this is deliberately lazy: the
/// bridge is started and stopped by hand, minutes apart, and a light that is
/// correct within a few seconds is correct enough.
const REMOTE_CONTROL_POLL: Duration = Duration::from_secs(30);

/// Whether a Remote Control bridge is running on this machine.
///
/// Remote Control does not travel over the agent connection -- it is a separate
/// `claude remote-control` process that exposes this machine's sessions to
/// claude.ai, and the ACP adapter neither knows nor reports that it exists. So
/// the only honest signal available here is the process itself.
///
/// One global rather than a poller per thread: the answer is a property of the
/// machine, and every open Claude tab asking the same question independently
/// would enumerate every process N times over.
pub(crate) struct RemoteControlStatus {
    bridge_running: bool,
    _poll: Task<()>,
}

impl Global for RemoteControlStatus {}

impl RemoteControlStatus {
    pub(crate) fn init(cx: &mut App) {
        // Registered before the poll is spawned: the task's first act is to
        // update this global, and a foreground task that outran its own
        // registration would be updating something that does not exist yet.
        cx.set_global(RemoteControlStatus {
            bridge_running: false,
            _poll: Task::ready(()),
        });

        let poll = cx.spawn(async move |cx| {
            loop {
                let running = cx.background_spawn(async { bridge_is_running() }).await;

                let changed = cx.update_global(|status: &mut RemoteControlStatus, _| {
                    let changed = status.bridge_running != running;
                    status.bridge_running = running;
                    changed
                });

                // Only when it flips: this runs forever, and redrawing every
                // window every five seconds to report "still the same" would
                // keep the machine awake for nothing.
                if changed {
                    cx.update(|cx| cx.refresh_windows());
                }

                cx.background_executor().timer(REMOTE_CONTROL_POLL).await;
            }
        });

        cx.update_global(|status: &mut RemoteControlStatus, _| {
            status._poll = poll;
        });
    }

    pub(crate) fn bridge_running(cx: &App) -> bool {
        cx.try_global::<Self>()
            .is_some_and(|status| status.bridge_running)
    }
}

/// Whether one process's arguments start a Remote Control bridge.
///
/// Split out from the process walk so the matching can be tested without
/// spawning anything. The first argument is the executable, which is why the
/// name is checked separately from the flags.
fn args_start_a_bridge(args: &[String]) -> bool {
    let Some((executable, flags)) = args.split_first() else {
        return false;
    };

    let is_claude = std::path::Path::new(executable)
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_ascii_lowercase())
        .is_some_and(|stem| stem == "claude");

    is_claude
        && flags
            .iter()
            .any(|flag| matches!(flag.trim(), "remote-control" | "--remote-control" | "--rc"))
}

fn bridge_is_running() -> bool {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System, UpdateKind};

    let refresh = ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always);
    let mut system = System::new_with_specifics(RefreshKind::nothing().with_processes(refresh));
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);

    system.processes().values().any(|process| {
        let args = process
            .cmd()
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        args_start_a_bridge(&args)
    })
}

/// The green light shown while a Remote Control bridge is up.
///
/// Deliberately a dot and not a labelled pill: the composer has room for one
/// more glyph, not one more word, and what this says is binary.
pub(crate) fn remote_control_indicator() -> impl IntoElement {
    div()
        .id("remote-control-indicator")
        .flex()
        .items_center()
        .justify_center()
        .size_4()
        .rounded_full()
        .bg(hsla(0.39, 0.6, 0.45, 0.16))
        .child(
            div()
                .size(gpui::px(6.))
                .rounded_full()
                .bg(hsla(0.39, 0.72, 0.45, 1.0)),
        )
        .tooltip(Tooltip::text(
            "Remote Control is running. Claude sessions on this machine can be \
             driven from claude.ai/code and the Claude mobile app.",
        ))
}

/// What Claude says it is doing while it works.
///
/// Claude Code shows a gerund that changes every few seconds rather than a
/// fixed "Thinking…". It is the difference between a machine that is busy and
/// one that is having a go, and it is most of why waiting on Claude does not
/// feel like waiting on a spinner. Taken from the set the CLI itself cycles.
const THINKING_WORDS: &[&str] = &[
    "Brewing",
    "Cerebrating",
    "Churning",
    "Coalescing",
    "Cogitating",
    "Computing",
    "Conjuring",
    "Considering",
    "Cooking",
    "Crafting",
    "Deliberating",
    "Determining",
    "Forging",
    "Hatching",
    "Ideating",
    "Inferring",
    "Marinating",
    "Mulling",
    "Musing",
    "Noodling",
    "Percolating",
    "Pondering",
    "Processing",
    "Puttering",
    "Reticulating",
    "Ruminating",
    "Simmering",
    "Spinning",
    "Stewing",
    "Synthesizing",
    "Thinking",
    "Transmuting",
    "Vibing",
    "Working",
];

/// How long each word stays up.
///
/// Long enough to read, short enough that a slow turn does not sit on one word
/// and look stuck.
const THINKING_WORD_SECONDS: u64 = 4;

/// The word for a turn that has been running `elapsed_seconds` seconds.
///
/// Derived from elapsed time rather than held as state: this is re-rendered on
/// every frame while generating, so a counter would need somewhere to live and
/// a timer to advance it, for a value that is already a function of the clock.
pub(crate) fn thinking_word(elapsed_seconds: u64) -> &'static str {
    let index = (elapsed_seconds / THINKING_WORD_SECONDS) as usize % THINKING_WORDS.len();
    THINKING_WORDS[index]
}

/// How one agent identifies itself in the panel.
///
/// Colour, mark and name together, because any one of them alone is ambiguous:
/// two agents can share an icon family, and a name in grey placeholder text is
/// not something anyone reads twice.
/// How one agent identifies itself in the panel.
///
/// Colour, mark, name and motion together. Any one alone is ambiguous: two
/// agents can share an icon family, a name in grey placeholder text is not
/// something anyone reads twice, and a colour is useless to a reader who does
/// not separate that pair of hues. The motion is the one that survives being
/// small and being glanced at, so it carries most of the load.
#[derive(Clone, Copy)]
pub(crate) struct AgentBrand {
    pub name: &'static str,
    pub icon: IconName,
    pub accent: Hsla,
    /// The figure this agent's loader traces through the nine-dot grid.
    pub pattern: DotPattern,
    /// The vendor's own wordmark, where Acuto ships one. Without it the
    /// greeting sets the mark beside the name instead.
    pub wordmark: Option<Wordmark>,
}

/// A wordmark image, in a version for each kind of theme: the lettering is
/// dark on light themes and light on dark ones, while the mark keeps its
/// colour in both.
#[derive(Clone, Copy)]
pub(crate) struct Wordmark {
    pub light: &'static str,
    pub dark: &'static str,
    /// Width over height, so the image is laid out at its own proportions.
    pub aspect_ratio: f32,
}

/// Clay on a dark theme; the theme's ink on a light one, like every other
/// agent colour (see `AgentBrand::for_agent_in`).
pub(crate) fn claude_clay_in(cx: &gpui::App) -> Hsla {
    use theme::ActiveTheme as _;
    if cx.theme().appearance().is_light() {
        cx.theme().colors().text
    } else {
        claude_clay()
    }
}

impl AgentBrand {
    /// The brand for an agent id, in the colours of the active theme.
    ///
    /// On a light theme every agent is drawn in the theme's ink: loaders,
    /// marks, the send button. Four vendor colours on white made the panel look
    /// like a sticker sheet; ink is how a professional light app draws its
    /// chat. The agent is still told apart by its mark and its name. Dark
    /// themes keep the vendor colours, which read well against a dark panel.
    ///
    /// Every drawing site goes through this rather than `for_agent`.
    pub(crate) fn for_agent_in(agent_id: &str, cx: &gpui::App) -> Option<Self> {
        use theme::ActiveTheme as _;
        let mut brand = Self::for_agent(agent_id)?;
        if cx.theme().appearance().is_light() {
            brand.accent = cx.theme().colors().text;
        }
        Some(brand)
    }

    /// The brand for an agent id, or `None` for the built-in agent.
    ///
    /// The native agent deliberately has none: it is the house agent in the
    /// house style, and branding it would imply it is another third party.
    ///
    /// Every colour here is the vendor's own published brand value, not an
    /// approximation chosen to sit nicely with the theme -- the point of these
    /// is to match what the user has seen everywhere else that agent appears.
    /// Lightness is the one thing allowed to move, and only where the published
    /// value is too dark to read against a dark panel.
    pub(crate) fn for_agent(agent_id: &str) -> Option<Self> {
        match agent_id {
            agent_servers::CLAUDE_AGENT_ID => Some(Self {
                name: "Claude Code",
                icon: IconName::AiClaude,
                // Anthropic's clay, #D97757, exactly.
                accent: hsla(0.041, 0.631, 0.596, 1.0),
                // Anthropic's mark is a radial burst, so the loader opens from
                // the middle outwards.
                pattern: DotPattern::Bloom,
                wordmark: Some(Wordmark {
                    light: "images/agents/claude_wordmark_light.png",
                    dark: "images/agents/claude_wordmark_dark.png",
                    aspect_ratio: 186. / 40.,
                }),
            }),
            agent_servers::CODEX_ID => Some(Self {
                name: "Codex",
                icon: IconName::AiOpenAi,
                // OpenAI draws Codex in black and white, not colour. On a dark
                // panel that is near-white.
                accent: hsla(0., 0., 0.92, 1.0),
                // OpenAI's mark is a rotational knot, so one highlight runs
                // round the ring with the centre held still.
                pattern: DotPattern::Orbit,
                wordmark: None,
            }),
            agent_servers::COPILOT_ID => Some(Self {
                name: "GitHub Copilot",
                icon: IconName::Copilot,
                // GitHub's Copilot purple, #6E40C9, lifted slightly for the
                // same reason.
                accent: hsla(0.7225, 0.559, 0.60, 1.0),
                // Copilot completes ahead of the cursor, so the loader scans
                // left to right.
                pattern: DotPattern::Sweep,
                wordmark: None,
            }),
            agent_servers::GEMINI_ID => Some(Self {
                name: "Gemini",
                icon: IconName::AiGemini,
                // Google blue, #4285F4, the predominant hue of the Gemini
                // sparkle.
                accent: hsla(0.604, 0.82, 0.62, 1.0),
                // The sparkle is a four-point star on the diagonals, so the
                // loader travels corner to corner.
                pattern: DotPattern::Diagonal,
                wordmark: None,
            }),
            agent_servers::ANTIGRAVITY_ID => Some(Self {
                name: "Antigravity",
                icon: IconName::AiAntigravity,
                // Google blue, #3186FF, the colour the Antigravity arch is
                // drawn in.
                accent: hsla(0.6, 1.0, 0.596, 1.0),
                // The arch lifts off the ground, so the whole grid rises and
                // settles together.
                pattern: DotPattern::Pulse,
                wordmark: None,
            }),
            _ => None,
        }
    }

    /// This agent's loader: the nine-dot grid, in its colour, tracing its
    /// figure.
    pub(crate) fn loader(&self, id: impl Into<ElementId>) -> DotSpinner {
        DotSpinner::new(id).color(self.accent).pattern(self.pattern)
    }
}

/// What sits above the composer on an empty thread: the agent's full logo.
///
/// An empty panel is otherwise a box and a caret, which says nothing about who
/// is about to reply -- and with several agents open at once that is the first
/// thing worth knowing. The logo is the one people know from everywhere else
/// the agent appears, at the size a logo is shown at, not a glyph.
pub(crate) fn agent_greeting(brand: AgentBrand, cx: &App) -> AnyElement {
    use theme::ActiveTheme as _;
    const WORDMARK_HEIGHT: f32 = 40.;

    let lockup = match brand.wordmark {
        Some(wordmark) => {
            let source = if cx.theme().appearance().is_light() {
                wordmark.light
            } else {
                wordmark.dark
            };
            img(source)
                .h(px(WORDMARK_HEIGHT))
                .w(px(WORDMARK_HEIGHT * wordmark.aspect_ratio))
                .into_any_element()
        }
        None => h_flex()
            .gap_3()
            .items_center()
            .child(
                svg()
                    .path(brand.icon.path())
                    .size(px(32.))
                    .flex_none()
                    .text_color(brand.accent),
            )
            .child(
                div()
                    .text_size(px(30.))
                    .line_height(px(36.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(cx.theme().colors().text)
                    .child(brand.name),
            )
            .into_any_element(),
    };

    v_flex()
        .items_center()
        .gap_2()
        .pb_6()
        .child(lockup)
        .into_any_element()
}

/// Shown beside the mode selector while the agent may act without asking.
///
/// This replaces the coloured composer border. A tinted box around the input is
/// on screen constantly and stops being read within a day; an explicit warning
/// sits next to the control that caused it, so the fix is where the notice is.
pub(crate) fn relaxed_permissions_warning() -> impl IntoElement {
    div()
        .id("relaxed-permissions-warning")
        .flex()
        .items_center()
        .child(
            Icon::new(IconName::Warning)
                .size(IconSize::Small)
                .color(Color::Custom(relaxed_permissions_red())),
        )
        .tooltip(Tooltip::text(
            "This mode lets the agent act without asking. Commands can run \
             against your files and credentials with nothing shown first.",
        ))
}

/// A usage limit reported by Claude, and when it lifts.
///
/// Claude Code reports a spent subscription window as an ordinary error string,
/// so without this a user who has run out of usage sees "An Error Happened" and
/// a stack of prose, and has no way to tell it apart from a crash. It is not an
/// error in any useful sense -- nothing is broken and retrying cannot help --
/// so it is worth recognising and saying plainly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UsageLimit {
    /// When the limit lifts, as a Unix timestamp in seconds, if Claude said.
    pub resets_at: Option<i64>,
    /// Which cap was hit, which decides whether waiting is worth it.
    pub kind: UsageLimitKind,
}

/// The cap that was reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UsageLimitKind {
    /// The rolling five-hour window. Rolls over on its own, usually soon.
    Session,
    /// The weekly allowance. Not worth waiting for.
    Weekly,
    /// Opus specifically, with the other models still available.
    Model,
    /// Requests arriving too fast, rather than an allowance being spent.
    Rate,
}

impl UsageLimitKind {
    /// The headline, which is the part read first and often the only part read.
    pub(crate) fn title(self) -> &'static str {
        match self {
            UsageLimitKind::Session => "Session Limit Reached",
            UsageLimitKind::Weekly => "Weekly Limit Reached",
            UsageLimitKind::Model => "Model Limit Reached",
            UsageLimitKind::Rate => "Sending Too Fast",
        }
    }

    /// What has happened and what to do, in that order.
    pub(crate) fn description(self) -> &'static str {
        match self {
            UsageLimitKind::Session => {
                "Claude's usage for this five-hour window is spent. The thread is saved and \
                 picks up exactly where it left off when the window rolls over."
            }
            UsageLimitKind::Weekly => {
                "Claude's weekly allowance is spent. This one does not roll over shortly, so \
                 another agent is the faster route -- the thread here is saved either way."
            }
            UsageLimitKind::Model => {
                "This model's allowance is spent. Claude's other models are still available, \
                 and switching to one keeps the thread going."
            }
            UsageLimitKind::Rate => {
                "Requests are arriving faster than Claude will accept them. Nothing is spent \
                 and nothing is lost; waiting a moment is enough."
            }
        }
    }
}

/// Recognises Claude's usage-limit message.
///
/// Matches on the phrase rather than on a structured field because the adapter
/// forwards the CLI's text: there is no error code to key off. The phrasing has
/// been stable across Claude Code releases, and a false negative only costs the
/// user the generic error callout they would have had anyway.
pub(crate) fn parse_usage_limit(message: &str) -> Option<UsageLimit> {
    let lowered = message.to_ascii_lowercase();

    // Ordered most specific first: a weekly message also contains "limit
    // reached", so testing for the session case first would swallow it and tell
    // the user to wait five hours for something that lifts next week.
    let kind = if lowered.contains("weekly limit") || lowered.contains("weekly usage") {
        UsageLimitKind::Weekly
    } else if lowered.contains("opus limit")
        || lowered.contains("model limit")
        || (lowered.contains("limit") && lowered.contains("switch to"))
    {
        UsageLimitKind::Model
    } else if lowered.contains("rate limit") || lowered.contains("too many requests") {
        UsageLimitKind::Rate
    } else if lowered.contains("usage limit reached")
        || lowered.contains("limit reached|")
        || lowered.contains("session limit")
    {
        UsageLimitKind::Session
    } else {
        return None;
    };

    // `Claude AI usage limit reached|1740009600` -- the CLI appends the reset
    // time after a pipe, in Unix seconds.
    let resets_at = message
        .rsplit('|')
        .next()
        .map(str::trim)
        .filter(|tail| !tail.is_empty() && tail.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|tail| tail.parse::<i64>().ok())
        .filter(|timestamp| *timestamp > 0);

    Some(UsageLimit { resets_at, kind })
}

/// "resets at 3:00 PM", in the user's own timezone.
///
/// Returns `None` when Claude did not say, in which case the callout leaves the
/// sentence off rather than guessing at a time the user would then plan around.
pub(crate) fn usage_limit_reset_label(limit: &UsageLimit) -> Option<String> {
    let resets_at = limit.resets_at?;
    let reset = chrono::DateTime::from_timestamp(resets_at, 0)?.with_timezone(&chrono::Local);
    Some(format!(
        "Resets at {}.",
        reset.format("%-I:%M %p on %-d %b")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_the_cli_usage_limit_message() {
        assert_eq!(
            parse_usage_limit("Claude AI usage limit reached|1740009600"),
            Some(UsageLimit {
                resets_at: Some(1740009600),
                kind: UsageLimitKind::Session,
            })
        );
    }

    #[test]
    fn recognises_a_usage_limit_without_a_reset_time() {
        assert_eq!(
            parse_usage_limit("Claude usage limit reached. Try again later."),
            Some(UsageLimit {
                resets_at: None,
                kind: UsageLimitKind::Session,
            })
        );
    }

    #[test]
    fn tells_the_weekly_cap_apart_from_the_session_window() {
        // Both contain "limit reached", and reporting a weekly cap as a
        // five-hour window would have the user wait for something that lifts
        // next week.
        let weekly = parse_usage_limit("Claude AI weekly limit reached").expect("weekly");
        assert_eq!(weekly.kind, UsageLimitKind::Weekly);

        let session = parse_usage_limit("Claude AI usage limit reached").expect("session");
        assert_eq!(session.kind, UsageLimitKind::Session);
    }

    #[test]
    fn tells_a_rate_limit_apart_from_a_spent_allowance() {
        // Nothing is spent here, so the advice is the opposite: wait a moment
        // rather than go and find another agent.
        let rate = parse_usage_limit("rate limit exceeded").expect("rate");
        assert_eq!(rate.kind, UsageLimitKind::Rate);
    }

    #[test]
    fn the_permissive_modes_are_the_ones_that_do_not_ask() {
        assert!(mode_skips_permission_prompts("auto"));
        assert!(mode_skips_permission_prompts("bypassPermissions"));
        assert!(mode_skips_permission_prompts("  auto  "));
        // Casing varies between adapters; the meaning does not.
        assert!(mode_skips_permission_prompts("BypassPermissions"));
    }

    #[test]
    fn the_asking_modes_are_left_alone() {
        assert!(!mode_skips_permission_prompts("default"));
        assert!(!mode_skips_permission_prompts("plan"));
        // Waives edits only, still stops for commands, and is a common default.
        assert!(!mode_skips_permission_prompts("acceptEdits"));
        assert!(!mode_skips_permission_prompts(""));
    }

    #[test]
    fn a_bridge_is_recognised_however_it_was_started() {
        let bridge = |args: &[&str]| {
            args_start_a_bridge(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>())
        };

        assert!(bridge(&[
            "C:\\Users\\me\\.local\\bin\\claude.exe",
            "remote-control"
        ]));
        assert!(bridge(&[
            "/usr/local/bin/claude",
            "remote-control",
            "--name",
            "laptop"
        ]));
        assert!(bridge(&["claude", "--remote-control"]));
        assert!(bridge(&["claude", "--rc"]));
    }

    #[test]
    fn an_ordinary_claude_session_is_not_a_bridge() {
        let bridge = |args: &[&str]| {
            args_start_a_bridge(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>())
        };

        // The thread this editor is talking to, which is a Claude process but
        // not a bridge -- the case that would light the indicator permanently.
        assert!(!bridge(&["claude.exe", "--output-format", "stream-json"]));
        assert!(!bridge(&["claude"]));
        // Another program that merely takes a similar flag.
        assert!(!bridge(&["vncviewer", "--remote-control"]));
        assert!(!bridge(&[]));
    }

    #[test]
    fn ignores_an_unrelated_error() {
        assert_eq!(parse_usage_limit("connection reset by peer"), None);
    }

    #[test]
    fn ignores_a_trailing_pipe_that_is_not_a_timestamp() {
        assert_eq!(
            parse_usage_limit("Claude AI usage limit reached|soon"),
            Some(UsageLimit {
                resets_at: None,
                kind: UsageLimitKind::Session,
            })
        );
    }

    #[test]
    fn thinking_words_cycle_and_never_index_out_of_bounds() {
        let first = thinking_word(0);
        assert_eq!(thinking_word(THINKING_WORD_SECONDS - 1), first);
        assert_ne!(thinking_word(THINKING_WORD_SECONDS), first);
        assert_eq!(
            thinking_word(THINKING_WORD_SECONDS * THINKING_WORDS.len() as u64),
            first
        );
        // A turn that has been running for a very long time still resolves.
        thinking_word(u64::MAX);
    }
}
