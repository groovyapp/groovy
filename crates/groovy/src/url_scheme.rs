//! Whether the desktop sends `groovy://` links to Groovy. On Linux this is a desktop entry that
//! handles the scheme; packaging will do this on install later. Elsewhere it is not available.

use std::io;

/// Whether Groovy is registered for the scheme.
pub fn is_registered() -> bool {
    entry_path().is_some_and(|path| path.exists())
}

/// Registers Groovy for `groovy://` links, or removes the registration.
pub fn set_registered(on: bool) -> io::Result<()> {
    let Some(path) = entry_path() else {
        return Ok(());
    };
    if !on {
        return match std::fs::remove_file(&path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        };
    }
    let exe = std::env::current_exe()?;
    let exe = exe
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        &path,
        format!(
            "[Desktop Entry]\nType=Application\nName=Groovy\nComment=Open groovy:// links\nExec=\"{exe}\" %u\nNoDisplay=true\nTerminal=false\nMimeType=x-scheme-handler/groovy;\n"
        ),
    )?;
    // Makes this the handler for the scheme. Best effort: the entry alone is enough on desktops
    // that read it.
    let _ = std::process::Command::new("xdg-mime")
        .args(["default", "groovy-url.desktop", "x-scheme-handler/groovy"])
        .status();
    Ok(())
}

#[cfg(target_os = "linux")]
fn entry_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".local/share"))
        })?;
    Some(base.join("applications").join("groovy-url.desktop"))
}

#[cfg(not(target_os = "linux"))]
fn entry_path() -> Option<std::path::PathBuf> {
    None
}
