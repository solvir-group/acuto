//! Every command the machine can actually run.
//!
//! History only knows what this user has typed before, which leaves the
//! completion blind to everything they have installed but not yet used --
//! precisely the commands they most need help spelling. A hardcoded list of
//! "common commands" would be worse than nothing: it goes stale, it is wrong
//! per-machine, and it suggests things that are not installed.
//!
//! `PATH` is the authoritative answer to "what can I run here", and the
//! operating system keeps it current for free.

use std::{collections::BTreeSet, path::Path};

/// Ceiling on how many names are taken.
///
/// A developer machine with several toolchains on `PATH` can expose tens of
/// thousands of executables. Past a few thousand the tail is vendored helper
/// binaries nobody types by hand, and every extra entry costs ranking time on
/// every keystroke.
const MAX_COMMANDS: usize = 4_000;

/// Names of executables found on `PATH`, sorted and deduplicated.
///
/// Sorted because `PATH` order encodes precedence, not usefulness, and the
/// caller ranks these below anything the user has actually run. Deduplicated
/// because the same command appearing in three directories is one suggestion.
///
/// Returns what it managed to read. An unreadable directory is a reason for
/// fewer suggestions, never an error: `PATH` routinely contains entries that do
/// not exist.
pub fn read() -> Vec<String> {
    let Some(path) = std::env::var_os("PATH") else {
        return Vec::new();
    };

    let executable_extensions = executable_extensions();
    let mut commands = BTreeSet::new();

    for directory in std::env::split_paths(&path) {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };

        for entry in entries.flatten() {
            if commands.len() >= MAX_COMMANDS {
                return commands.into_iter().collect();
            }
            if let Some(name) = command_name(&entry.path(), &executable_extensions) {
                commands.insert(name);
            }
        }
    }

    commands.into_iter().collect()
}

/// What counts as executable here.
///
/// On Windows this is `PATHEXT`, lowercased, because a `.dll` sitting beside a
/// `.exe` is not something anyone types at a prompt. Elsewhere the extension
/// carries no meaning and the permission bits decide.
fn executable_extensions() -> Vec<String> {
    if !cfg!(windows) {
        return Vec::new();
    }

    std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
        .split(';')
        .filter_map(|extension| {
            let extension = extension.trim().trim_start_matches('.').to_ascii_lowercase();
            (!extension.is_empty()).then_some(extension)
        })
        .collect()
}

/// The name someone would type, or `None` if this is not a command.
fn command_name(path: &Path, executable_extensions: &[String]) -> Option<String> {
    if cfg!(windows) {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        if !executable_extensions.contains(&extension) {
            return None;
        }
        // The stem, because Windows resolves `git` to `git.exe` itself.
        let stem = path.file_stem()?.to_str()?;
        return (!stem.is_empty()).then(|| stem.to_string());
    }

    if !is_executable(path) {
        return None;
    }
    let name = path.file_name()?.to_str()?;
    (!name.is_empty()).then(|| name.to_string())
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn windows_executables_are_named_without_their_extension() {
        if !cfg!(windows) {
            return;
        }
        let extensions = vec!["exe".to_string(), "cmd".to_string()];
        assert_eq!(
            command_name(&PathBuf::from(r"C:\bin\git.exe"), &extensions),
            Some("git".to_string()),
            "Windows resolves `git` to `git.exe` itself, so the stem is what gets typed"
        );
        assert_eq!(
            command_name(&PathBuf::from(r"C:\bin\npm.cmd"), &extensions),
            Some("npm".to_string())
        );
    }

    #[test]
    fn windows_ignores_files_that_are_not_executable() {
        if !cfg!(windows) {
            return;
        }
        let extensions = vec!["exe".to_string()];
        // Sitting beside an executable, but not something anyone types.
        assert_eq!(
            command_name(&PathBuf::from(r"C:\bin\vcruntime.dll"), &extensions),
            None
        );
        assert_eq!(
            command_name(&PathBuf::from(r"C:\bin\README"), &extensions),
            None
        );
    }

    #[test]
    fn pathext_is_parsed_leniently() {
        if !cfg!(windows) {
            return;
        }
        // Real PATHEXT values are upper case and dot-prefixed; some are padded.
        unsafe { std::env::set_var("PATHEXT", ".COM; .EXE ;.BAT;;") };
        let extensions = executable_extensions();
        assert!(extensions.contains(&"exe".to_string()));
        assert!(extensions.contains(&"bat".to_string()));
        assert!(
            !extensions.iter().any(|extension| extension.is_empty()),
            "an empty entry would match every extensionless file"
        );
    }
}
