//! Reading the history the user's shell already keeps.
//!
//! Suggestions that only know about commands run since the editor started are
//! useless on the first prompt of every session, which is exactly when someone
//! forms an opinion about whether the feature works. Shells have been recording
//! this for years; the file is already on disk.
//!
//! Nothing here writes to those files. They belong to the shell.

use std::path::PathBuf;

/// Ceiling on how much of a history file is read.
///
/// A years-old `.bash_history` can hold tens of thousands of lines, and the
/// oldest are the least useful. Reading the tail bounds both the work and the
/// memory without losing anything anyone would miss.
const MAX_SEEDED_COMMANDS: usize = 5_000;

/// Where each shell keeps its history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellHistoryKind {
    PowerShell,
    Bash,
    Zsh,
    Fish,
}

impl ShellHistoryKind {
    /// Guesses from a shell program path, so the caller can pass whatever it
    /// was configured to launch.
    pub fn from_program(program: &str) -> Option<Self> {
        let name = std::path::Path::new(program)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or(program)
            .to_ascii_lowercase();

        match name.as_str() {
            "pwsh" | "powershell" => Some(Self::PowerShell),
            "bash" | "sh" => Some(Self::Bash),
            "zsh" => Some(Self::Zsh),
            "fish" => Some(Self::Fish),
            _ => None,
        }
    }

    fn path(self) -> Option<PathBuf> {
        match self {
            Self::PowerShell => {
                // PSReadLine's location, which is where anything typed at a
                // Windows prompt in the last decade has been recorded.
                let appdata = std::env::var_os("APPDATA")?;
                Some(
                    PathBuf::from(appdata)
                        .join("Microsoft")
                        .join("Windows")
                        .join("PowerShell")
                        .join("PSReadLine")
                        .join("ConsoleHost_history.txt"),
                )
            }
            Self::Bash => Some(dirs::home_dir()?.join(".bash_history")),
            Self::Zsh => Some(dirs::home_dir()?.join(".zsh_history")),
            Self::Fish => Some(
                dirs::data_local_dir()
                    .or_else(dirs::home_dir)?
                    .join("fish")
                    .join("fish_history"),
            ),
        }
    }

    /// Reads the shell's history, most recent last.
    ///
    /// Returns an empty list rather than an error for anything unreadable: a
    /// missing or malformed history file is a reason to have fewer suggestions,
    /// never a reason to fail.
    pub fn read(self) -> Vec<String> {
        let Some(path) = self.path() else {
            return Vec::new();
        };
        // Lossy: history files accumulate whatever encoding the shell was using
        // at the time, and one bad byte should not discard the rest.
        let Ok(bytes) = std::fs::read(&path) else {
            return Vec::new();
        };
        let text = String::from_utf8_lossy(&bytes);

        let mut commands: Vec<String> = match self {
            Self::PowerShell | Self::Bash => text
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect(),
            Self::Zsh => text
                .lines()
                .filter_map(parse_zsh_line)
                .filter(|line| !line.is_empty())
                .collect(),
            Self::Fish => text
                .lines()
                .filter_map(|line| {
                    line.trim_start()
                        .strip_prefix("- cmd: ")
                        .map(|command| command.trim().to_string())
                })
                .filter(|line| !line.is_empty())
                .collect(),
        };

        if commands.len() > MAX_SEEDED_COMMANDS {
            commands.drain(..commands.len() - MAX_SEEDED_COMMANDS);
        }
        commands
    }
}

/// zsh writes either a bare command or `: <started>:<elapsed>;<command>` when
/// extended history is on.
fn parse_zsh_line(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    match line.strip_prefix(':') {
        Some(rest) => rest.split_once(';').map(|(_, command)| command.trim().to_string()),
        None => Some(line.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_shells_by_program_name() {
        assert_eq!(
            ShellHistoryKind::from_program("C:/Windows/System32/powershell.exe"),
            Some(ShellHistoryKind::PowerShell)
        );
        assert_eq!(
            ShellHistoryKind::from_program("/usr/bin/zsh"),
            Some(ShellHistoryKind::Zsh)
        );
        assert_eq!(ShellHistoryKind::from_program("/usr/bin/nu"), None);
    }

    #[test]
    fn zsh_extended_history_keeps_only_the_command() {
        assert_eq!(
            parse_zsh_line(": 1700000000:0;cargo build --release"),
            Some("cargo build --release".to_string())
        );
        // Plain format, written when extended history is off.
        assert_eq!(
            parse_zsh_line("git status"),
            Some("git status".to_string())
        );
        assert_eq!(parse_zsh_line("   "), None);
    }
}
