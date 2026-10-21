//! Starting Groovy when the session starts. On Linux this is an entry in the user's autostart
//! folder, which starts Groovy with `--background` so it opens to the tray rather than a window.
//! Elsewhere nothing is written.

use std::io;

/// Writes or removes the login entry, so Groovy starts (or stops starting) with the session.
pub fn set_enabled(enabled: bool) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let Some(path) = entry_path() else {
            return Ok(());
        };
        if enabled {
            let Some(exe) = crate::launch::program()? else {
                return Ok(());
            };
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
                    "[Desktop Entry]\nType=Application\nName=Groovy\nComment=Keep receiving files and messages in the background\nExec=\"{exe}\" --background\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
                ),
            )
        } else {
            match std::fs::remove_file(&path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
                _ => Ok(()),
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = enabled;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn entry_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".config"))
        })?;
    Some(base.join("autostart").join("groovy.desktop"))
}
