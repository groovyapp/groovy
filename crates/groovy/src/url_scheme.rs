//! Whether the desktop sends `groovy://` links to Groovy. On Linux this is a desktop entry that
//! handles the scheme, kept in step at startup, as an installer would set it up. Elsewhere it is
//! not available yet.
//!
//! Only a run on the default data directory registers. A run with the data directory overridden,
//! such as a test copy, leaves the registration alone, so it never takes the user's links.

use std::io;
use std::path::Path;

/// The desktop entry's file name.
const ENTRY: &str = "groovy-url.desktop";

/// The desktop entry for `exe`.
pub fn entry_contents(exe: &Path) -> String {
    let exe = exe
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!(
        "[Desktop Entry]\nType=Application\nName=Groovy\nComment=Open groovy:// links\nExec=\"{exe}\" %u\nNoDisplay=true\nTerminal=false\nMimeType=x-scheme-handler/groovy;\n"
    )
}

/// Registers Groovy for `groovy://` links, or removes the registration, to match `on`. Writes
/// only when something changed, and tells the desktop to read the entry again.
pub fn sync(on: bool) -> io::Result<()> {
    let Some(path) = entry_path() else {
        return Ok(());
    };
    if !on {
        return match std::fs::remove_file(&path) {
            Ok(()) => {
                refresh_desktop(&path);
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
    }
    let contents = entry_contents(&std::env::current_exe()?);
    if std::fs::read_to_string(&path).ok().as_deref() != Some(contents.as_str()) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, contents)?;
    }
    let current = std::process::Command::new("xdg-mime")
        .args(["query", "default", "x-scheme-handler/groovy"])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .unwrap_or_default();
    if current != ENTRY {
        let _ = std::process::Command::new("xdg-mime")
            .args(["default", ENTRY, "x-scheme-handler/groovy"])
            .status();
    }
    refresh_desktop(&path);
    Ok(())
}

/// Asks the desktop to read its application entries again. KDE keeps a cache that does not see a
/// new entry until it is rebuilt. Best effort: either tool may be missing.
fn refresh_desktop(path: &Path) {
    if let Some(dir) = path.parent() {
        let _ = std::process::Command::new("update-desktop-database")
            .arg(dir)
            .status();
    }
    let _ = std::process::Command::new("kbuildsycoca6")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(target_os = "linux")]
fn entry_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".local/share"))
        })?;
    Some(base.join("applications").join(ENTRY))
}

#[cfg(not(target_os = "linux"))]
fn entry_path() -> Option<std::path::PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_entry_runs_this_executable_with_the_link_and_handles_the_scheme() {
        let entry = entry_contents(Path::new("/opt/My Apps/groovy"));
        assert!(entry.contains("Exec=\"/opt/My Apps/groovy\" %u"));
        assert!(entry.contains("MimeType=x-scheme-handler/groovy;"));
        assert!(entry.contains("NoDisplay=true"));
    }
}
