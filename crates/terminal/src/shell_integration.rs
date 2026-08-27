//! Installing shell integration without touching the user's rc files.
//!
//! The editor needs shells to emit OSC 133 markers, and the obvious way to
//! arrange that — appending a `source` line to `.zshrc` — is the one thing not
//! to do. It edits a file the user owns, survives uninstall, and breaks in ways
//! they cannot attribute to us.
//!
//! Instead each shell is started in a way that loads our script *and then* the
//! user's own configuration, using the mechanism that shell already provides:
//!
//! | Shell | Mechanism | User's own config |
//! | --- | --- | --- |
//! | zsh | `ZDOTDIR` pointed at a shim directory | shim sources their `.zshrc`, then restores `ZDOTDIR` |
//! | bash | `--init-file` | our file sources their `~/.bashrc` first |
//! | fish | `XDG_DATA_DIRS` + `vendor_conf.d` | fish loads both on its own |
//! | PowerShell | inline `-Command` | `-NoExit` keeps the session, profile still loads |
//!
//! PowerShell is deliberately **not** given a `.ps1` to dot-source.
//! ExecutionPolicy governs script files, and the Windows default blocks
//! unsigned ones — so a file would silently fail on exactly the platform this
//! fork treats as first-class. Inline `-Command` is not subject to it.
//!
//! # Never break the shell
//!
//! Every script is defensive and every failure path here degrades to "no
//! integration" rather than "no shell". If a script cannot be written, the
//! terminal spawns exactly as it would have without this module.

use std::path::{Path, PathBuf};

use settings::ShellIntegrationMode;
use util::shell::ShellKind;

/// The scripts, embedded so a broken or missing install directory cannot leave
/// the editor unable to find them.
const ZSH_SCRIPT: &str = include_str!("../../../assets/shell_integration/acuto.zsh");
const BASH_SCRIPT: &str = include_str!("../../../assets/shell_integration/acuto.bash");
const FISH_SCRIPT: &str = include_str!("../../../assets/shell_integration/acuto.fish");
const POWERSHELL_SCRIPT: &str = include_str!("../../../assets/shell_integration/acuto.ps1");

/// The shells this integration supports.
///
/// `ShellKind` has no zsh variant — `Posix` covers sh, bash and zsh alike — so
/// the distinction is recovered from the program name. zsh and bash need
/// genuinely different mechanisms (`ZDOTDIR` versus `--init-file`), so lumping
/// them together would silently install the wrong one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntegrationShell {
    Zsh,
    Bash,
    Fish,
    PowerShell,
}

impl IntegrationShell {
    /// Resolves the shell from its program path and parsed kind.
    ///
    /// The program name wins where it is decisive, because `ShellKind::Posix`
    /// is the default and therefore also what an unrecognised shell parses as.
    pub fn detect(program: &str, kind: ShellKind) -> Option<Self> {
        let name = Path::new(program)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or(program)
            .to_ascii_lowercase();

        match name.as_str() {
            "zsh" => return Some(Self::Zsh),
            "bash" | "sh" => return Some(Self::Bash),
            "fish" => return Some(Self::Fish),
            "pwsh" | "powershell" => return Some(Self::PowerShell),
            _ => {}
        }

        match kind {
            ShellKind::Fish => Some(Self::Fish),
            ShellKind::PowerShell | ShellKind::Pwsh => Some(Self::PowerShell),
            // Posix is also the fallback for shells we do not recognise, so it
            // is not evidence of bash on its own — but bash's mechanism is the
            // safest guess for a POSIX-ish shell, and the script no-ops if the
            // hooks it needs are missing.
            ShellKind::Posix => Some(Self::Bash),
            _ => None,
        }
    }

    pub fn script(self) -> &'static str {
        match self {
            Self::Zsh => ZSH_SCRIPT,
            Self::Bash => BASH_SCRIPT,
            Self::Fish => FISH_SCRIPT,
            Self::PowerShell => POWERSHELL_SCRIPT,
        }
    }
}

/// Script text for a shell, for `acuto shell-integration <shell>` to print.
pub fn script_for(shell: IntegrationShell) -> &'static str {
    shell.script()
}

/// How a shell should be started so it loads the integration.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Injection {
    /// Arguments for the child.
    pub args: Vec<String>,
    /// Whether [`Self::args`] replaces the user's arguments or precedes them.
    ///
    /// PowerShell has to replace them: `-Command` consumes the remainder of the
    /// command line, so a second one cannot follow the first. Everything else
    /// prepends, which keeps the user's arguments untouched.
    pub replaces_args: bool,
    /// Environment to set for the child.
    pub env: Vec<(String, String)>,
}

/// Where the generated scripts live.
///
/// Under the data directory rather than a temp dir: a temp file swept mid-
/// session would break every subsequently opened terminal, and the failure
/// would look like a shell bug.
fn integration_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("shell_integration")
}

/// Writes a file only when its content differs.
///
/// Rewriting unconditionally would touch mtimes on every terminal open, which
/// makes the directory look busier than it is and defeats any caching layered
/// on top later.
fn write_if_changed(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Ok(existing) = std::fs::read_to_string(path)
        && existing == contents
    {
        return Ok(());
    }
    std::fs::write(path, contents)
}

/// Materialises the scripts and returns how to start the shell.
///
/// Returns `None` when integration is disabled, the shell is unsupported, or
/// anything at all goes wrong writing the files — in every one of those cases
/// the terminal must spawn normally.
pub fn prepare(
    shell: IntegrationShell,
    mode: ShellIntegrationMode,
    data_dir: &Path,
    user_home: Option<&Path>,
    existing_args: &[String],
) -> Option<Injection> {
    if mode != ShellIntegrationMode::Auto {
        return None;
    }

    let dir = integration_dir(data_dir);
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }

    // TERM_PROGRAM gates every script: copied elsewhere they become no-ops
    // rather than painting escape sequences into an unrelated terminal.
    let mut env = vec![("TERM_PROGRAM".to_string(), "Acuto".to_string())];

    match shell {
        IntegrationShell::Zsh => {
            // zsh reads .zshrc from ZDOTDIR. The shim sources the user's real
            // one first, restores ZDOTDIR so nothing downstream sees the shim,
            // and only then loads ours — so a user's `setopt` or framework is
            // already in place and our PS1 append lands last.
            let script = dir.join("acuto.zsh");
            write_if_changed(&script, ZSH_SCRIPT).ok()?;

            let home = user_home?;
            let shim_dir = dir.join("zdotdir");
            std::fs::create_dir_all(&shim_dir).ok()?;

            let previous = std::env::var("ZDOTDIR").unwrap_or_else(|_| home.display().to_string());
            let shim = format!(
                r#"# Generated by Acuto. Sources your own configuration, then the integration.
ACUTO_PREVIOUS_ZDOTDIR="{previous}"
if [[ -f "$ACUTO_PREVIOUS_ZDOTDIR/.zshrc" ]]; then
  ZDOTDIR="$ACUTO_PREVIOUS_ZDOTDIR"
  source "$ACUTO_PREVIOUS_ZDOTDIR/.zshrc"
fi
ZDOTDIR="$ACUTO_PREVIOUS_ZDOTDIR"
unset ACUTO_PREVIOUS_ZDOTDIR
[[ -f "{script}" ]] && source "{script}"
"#,
                previous = previous.replace('"', r#"\""#),
                script = script.display().to_string().replace('\\', "/"),
            );
            write_if_changed(&shim_dir.join(".zshrc"), &shim).ok()?;

            env.push((
                "ZDOTDIR".to_string(),
                shim_dir.display().to_string(),
            ));
            Some(Injection {
                args: Vec::new(),
                replaces_args: false,
                env,
            })
        }

        IntegrationShell::Bash => {
            // --init-file replaces ~/.bashrc, so ours sources theirs first.
            // Interactive bash ignores it without -i, which the shell already
            // gets by virtue of having a tty.
            let script = dir.join("acuto.bash");
            let user_rc = user_home
                .map(|home| home.join(".bashrc").display().to_string().replace('\\', "/"))
                .unwrap_or_default();
            let wrapper = format!(
                "# Generated by Acuto. Sources your own configuration, then the integration.\n\
                 [ -f \"{user_rc}\" ] && source \"{user_rc}\"\n\
                 {body}\n",
                user_rc = user_rc,
                body = BASH_SCRIPT,
            );
            write_if_changed(&script, &wrapper).ok()?;

            Some(Injection {
                args: vec!["--init-file".to_string(), script.display().to_string()],
                replaces_args: false,
                env,
            })
        }

        IntegrationShell::Fish => {
            // fish sources conf.d from every XDG_DATA_DIRS entry, so this needs
            // no wrapper and cannot clobber the user's own config.
            let vendor = dir.join("fish").join("vendor_conf.d");
            std::fs::create_dir_all(&vendor).ok()?;
            write_if_changed(&vendor.join("acuto.fish"), FISH_SCRIPT).ok()?;

            let base = dir.join("fish").display().to_string();
            let combined = match std::env::var("XDG_DATA_DIRS") {
                Ok(existing) if !existing.is_empty() => {
                    let separator = if cfg!(windows) { ';' } else { ':' };
                    format!("{base}{separator}{existing}")
                }
                _ => base,
            };
            env.push(("XDG_DATA_DIRS".to_string(), combined));
            Some(Injection {
                args: Vec::new(),
                replaces_args: false,
                env,
            })
        }

        IntegrationShell::PowerShell => {
            // Inline, not a file: ExecutionPolicy blocks unsigned .ps1 by
            // default on Windows, and would take the integration with it.
            Some(Injection {
                args: merge_powershell_args(existing_args, POWERSHELL_SCRIPT),
                replaces_args: true,
                env,
            })
        }
    }
}

/// Builds a PowerShell command line that runs the integration and then
/// whatever the user configured.
///
/// `-Command` consumes the entire remainder of the command line, so prepending
/// a second one produces a command line PowerShell reads as one giant script
/// argument — the user's own command silently never runs. The two have to be
/// merged into a single `-Command` instead, with the integration first so the
/// prompt it installs is in place before the user's profile customises it.
fn merge_powershell_args(existing: &[String], script: &str) -> Vec<String> {
    let command_at = existing
        .iter()
        .position(|arg| arg.eq_ignore_ascii_case("-Command") || arg.eq_ignore_ascii_case("-c"));

    match command_at {
        Some(index) => {
            let mut args: Vec<String> = existing[..index].to_vec();
            // Everything after -Command is the user's command, however many
            // argv entries it happens to occupy.
            let user_command = existing[index + 1..].join(" ");
            args.push("-Command".to_string());
            args.push(if user_command.trim().is_empty() {
                script.to_string()
            } else {
                format!("{script}\n{user_command}")
            });
            args
        }
        None => {
            let mut args: Vec<String> = existing.to_vec();
            // Only added when absent: passing it twice is harmless but makes
            // the command line confusing to read in a process list.
            if !args.iter().any(|arg| arg.eq_ignore_ascii_case("-NoExit")) {
                args.push("-NoExit".to_string());
            }
            args.push("-Command".to_string());
            args.push(script.to_string());
            args
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("acuto-shell-integration-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }


    #[test]
    fn powershell_merges_into_a_single_command() {
        // The shape this fork actually ships: the user already passes -Command
        // to source their own profile.
        let existing: Vec<String> = ["-NoLogo", "-NoExit", "-Command", ". 'C:/x/acuto.ps1'"]
            .iter()
            .map(|arg| arg.to_string())
            .collect();

        let merged = merge_powershell_args(&existing, "INTEGRATION");

        assert_eq!(
            merged.iter().filter(|arg| arg.eq_ignore_ascii_case("-Command")).count(),
            1,
            "-Command consumes the rest of the command line; a second one is unreachable"
        );
        assert_eq!(merged[0], "-NoLogo", "the user's other flags survive");
        assert_eq!(merged[1], "-NoExit");

        let command = merged.last().expect("a command");
        assert!(command.starts_with("INTEGRATION"), "integration installs first");
        assert!(
            command.contains(". 'C:/x/acuto.ps1'"),
            "the user's own command must still run"
        );
    }

    #[test]
    fn powershell_without_an_existing_command_gets_its_own() {
        let merged = merge_powershell_args(&["-NoLogo".to_string()], "INTEGRATION");
        assert_eq!(merged, vec!["-NoLogo", "-NoExit", "-Command", "INTEGRATION"]);
    }

    #[test]
    fn powershell_does_not_duplicate_no_exit() {
        let existing: Vec<String> = ["-NoExit".to_string()].to_vec();
        let merged = merge_powershell_args(&existing, "INTEGRATION");
        assert_eq!(
            merged.iter().filter(|arg| arg.eq_ignore_ascii_case("-NoExit")).count(),
            1
        );
    }

    #[test]
    fn disabled_modes_never_inject() {
        let dir = temp_dir("disabled");
        for mode in [ShellIntegrationMode::Manual, ShellIntegrationMode::Off] {
            assert!(
                prepare(IntegrationShell::Zsh, mode, &dir, Some(Path::new("/home/dev")), &[]).is_none(),
                "{mode:?} must not inject"
            );
        }
    }

    #[test]
    fn zsh_shim_sources_the_users_rc_before_ours() {
        let dir = temp_dir("zsh");
        let injection = prepare(
            IntegrationShell::Zsh,
            ShellIntegrationMode::Auto,
            &dir,
            Some(Path::new("/home/dev")),
            &[],
        )
        .expect("zsh is supported");

        assert!(
            injection.args.is_empty(),
            "zsh is redirected by ZDOTDIR, not by arguments"
        );
        assert!(
            injection.env.iter().any(|(key, _)| key == "ZDOTDIR"),
            "ZDOTDIR must point at the shim"
        );

        let shim = std::fs::read_to_string(dir.join("shell_integration/zdotdir/.zshrc"))
            .expect("shim written");
        let user_rc = shim.find(".zshrc\"").expect("sources the user's rc");
        let ours = shim.find("acuto.zsh").expect("sources ours");
        assert!(
            user_rc < ours,
            "the user's configuration must load first, so our prompt marker lands last"
        );
        assert!(
            shim.contains("ZDOTDIR=\"$ACUTO_PREVIOUS_ZDOTDIR\""),
            "ZDOTDIR must be restored so nothing downstream sees the shim"
        );
    }

    #[test]
    fn bash_wrapper_sources_the_users_rc_before_ours() {
        let dir = temp_dir("bash");
        let injection = prepare(
            IntegrationShell::Bash,
            ShellIntegrationMode::Auto,
            &dir,
            Some(Path::new("/home/dev")),
            &[],
        )
        .expect("bash is supported");

        assert_eq!(injection.args.first().map(String::as_str), Some("--init-file"));

        let script = std::fs::read_to_string(dir.join("shell_integration/acuto.bash"))
            .expect("script written");
        let user_rc = script.find(".bashrc").expect("sources the user's rc");
        let ours = script.find("__acuto_precmd").expect("contains the integration");
        assert!(user_rc < ours, "the user's configuration must load first");
    }

    #[test]
    fn powershell_is_inline_never_a_file() {
        let dir = temp_dir("pwsh");
        let injection = prepare(
            IntegrationShell::PowerShell,
            ShellIntegrationMode::Auto,
            &dir,
            Some(Path::new("C:/Users/dev")),
            &[],
        )
        .expect("powershell is supported");

        assert!(
            injection.args.iter().any(|arg| arg == "-Command"),
            "must be passed inline"
        );
        assert!(
            injection.args.iter().any(|arg| arg == "-NoExit"),
            "the session must stay interactive"
        );
        assert!(
            !injection.args.iter().any(|arg| arg.ends_with(".ps1")),
            "a .ps1 would be blocked by the default ExecutionPolicy on Windows"
        );
    }

    #[test]
    fn fish_extends_rather_than_replaces_data_dirs() {
        let dir = temp_dir("fish");
        let injection = prepare(
            IntegrationShell::Fish,
            ShellIntegrationMode::Auto,
            &dir,
            Some(Path::new("/home/dev")),
            &[],
        )
        .expect("fish is supported");

        let (_, value) = injection
            .env
            .iter()
            .find(|(key, _)| key == "XDG_DATA_DIRS")
            .expect("XDG_DATA_DIRS set");
        assert!(value.contains("shell_integration"));
        assert!(
            dir.join("shell_integration/fish/vendor_conf.d/acuto.fish").exists(),
            "fish loads vendor_conf.d automatically"
        );
    }

    #[test]
    fn every_supported_shell_gates_on_term_program() {
        // A script copied into another terminal must be inert rather than
        // printing escape sequences into it.
        for script in [ZSH_SCRIPT, BASH_SCRIPT, FISH_SCRIPT, POWERSHELL_SCRIPT] {
            assert!(
                script.contains("TERM_PROGRAM"),
                "every script must check it is running inside Acuto"
            );
        }
    }

    #[test]
    fn every_supported_shell_guards_against_double_loading() {
        for script in [ZSH_SCRIPT, BASH_SCRIPT, FISH_SCRIPT, POWERSHELL_SCRIPT] {
            assert!(
                script.contains("ACUTO_SHELL_INTEGRATION_LOADED"),
                "loading twice would emit duplicate markers and desynchronise the phase"
            );
        }
    }
}
