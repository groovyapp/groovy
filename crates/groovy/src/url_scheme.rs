//! Whether the desktop sends `groovy://` links to Groovy. On Linux this is a desktop entry that
//! handles the scheme, kept in step at startup, as an installer would set it up. Elsewhere it is
//! not available yet.
//!
//! Only a run on the default data directory registers. A run with the data directory overridden,
//! such as a test copy, leaves the registration alone, so it never takes the user's links.

use std::io;
use std::path::Path;

/// The desktop entry's file name, for a build run from anywhere but an AppImage.
const ENTRY: &str = "groovy-url.desktop";

/// The packaged desktop entry, which an AppImage installs as its launcher entry.
const PACKAGED_ENTRY: &str = include_str!("../../../assets/io.github.groovyapp.Groovy.desktop");
const ICON_PNG: &[u8] = include_bytes!("../../../assets/icons/app-icon-256.png");
const ICON_SVG: &[u8] = include_bytes!("../../../assets/icons/app-icon.svg");

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
    if let Some(appimage) = std::env::var_os("APPIMAGE").map(std::path::PathBuf::from)
        && appimage.is_absolute()
        && std::env::var_os("FLATPAK_ID").is_none()
    {
        return integrate_appimage(&appimage, on);
    }
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
    let Some(program) = crate::launch::program()? else {
        return Ok(());
    };
    let contents = entry_contents(&program);
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

/// The launcher entry an AppImage installs: the packaged entry, running the AppImage file. It
/// handles `groovy://` links only when `links` is on.
pub fn appimage_entry(appimage: &Path, links: bool) -> String {
    let exe = appimage
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    PACKAGED_ENTRY
        .lines()
        .filter(|line| links || !line.starts_with("MimeType="))
        .map(|line| {
            if line.starts_with("Exec=") {
                format!("Exec=\"{exe}\" %u")
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// Puts an AppImage in the app launcher, with its icon, as a package would on install. The entry
/// points at the AppImage file, so it keeps working however often the AppImage is run. The
/// hidden link entry of a source build is not needed then, and is removed.
fn integrate_appimage(appimage: &Path, links: bool) -> io::Result<()> {
    let Some(data) = data_home() else {
        return Ok(());
    };
    let applications = data.join("applications");
    std::fs::create_dir_all(&applications)?;
    let entry_path = applications.join(format!("{}.desktop", crate::APP_ID));
    let contents = appimage_entry(appimage, links);
    if std::fs::read_to_string(&entry_path).ok().as_deref() != Some(contents.as_str()) {
        std::fs::write(&entry_path, contents)?;
    }
    for (dir, name, bytes) in [
        ("256x256", format!("{}.png", crate::APP_ID), ICON_PNG),
        ("scalable", format!("{}.svg", crate::APP_ID), ICON_SVG),
    ] {
        let icon = data.join("icons/hicolor").join(dir).join("apps").join(name);
        if std::fs::read(&icon).ok().as_deref() != Some(bytes) {
            if let Some(parent) = icon.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&icon, bytes)?;
        }
    }
    let _ = std::fs::remove_file(applications.join(ENTRY));
    if links {
        let wanted = format!("{}.desktop", crate::APP_ID);
        let current = std::process::Command::new("xdg-mime")
            .args(["query", "default", "x-scheme-handler/groovy"])
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
            .unwrap_or_default();
        if current != wanted {
            let _ = std::process::Command::new("xdg-mime")
                .args(["default", &wanted, "x-scheme-handler/groovy"])
                .status();
        }
    }
    refresh_desktop(&entry_path);
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
fn data_home() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".local/share"))
        })
}

#[cfg(not(target_os = "linux"))]
fn data_home() -> Option<std::path::PathBuf> {
    None
}

fn entry_path() -> Option<std::path::PathBuf> {
    data_home().map(|base| base.join("applications").join(ENTRY))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_appimage_entry_runs_the_appimage_and_handles_links_only_when_asked() {
        let with_links = appimage_entry(Path::new("/home/a/Apps/Groovy.AppImage"), true);
        assert!(with_links.contains("Exec=\"/home/a/Apps/Groovy.AppImage\" %u"));
        assert!(with_links.contains("MimeType=x-scheme-handler/groovy;"));
        assert!(with_links.contains("Icon=io.github.groovyapp.Groovy"));
        assert!(
            !with_links.contains("NoDisplay"),
            "it shows in the launcher"
        );
        let without = appimage_entry(Path::new("/home/a/Apps/Groovy.AppImage"), false);
        assert!(!without.contains("MimeType"));
    }

    #[test]
    fn the_entry_runs_this_executable_with_the_link_and_handles_the_scheme() {
        let entry = entry_contents(Path::new("/opt/My Apps/groovy"));
        assert!(entry.contains("Exec=\"/opt/My Apps/groovy\" %u"));
        assert!(entry.contains("MimeType=x-scheme-handler/groovy;"));
        assert!(entry.contains("NoDisplay=true"));
    }
}
