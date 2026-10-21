//! What starts this copy of Groovy again, for the entries the app writes: the login entry and
//! the `groovy://` link handler.

use std::io;
use std::path::PathBuf;

/// The program an entry should run to start this copy, or `None` when the package manages its
/// own entries.
///
/// - In an AppImage the running binary sits in a mount that changes on every launch, so the
///   AppImage file itself is used.
/// - In a Flatpak the package exports its own desktop entry, which handles links, and the
///   sandbox cannot write the session's entries, so nothing is written.
/// - Otherwise it is the running binary.
pub fn program() -> io::Result<Option<PathBuf>> {
    if std::env::var_os("FLATPAK_ID").is_some() {
        return Ok(None);
    }
    if let Some(appimage) = std::env::var_os("APPIMAGE").map(PathBuf::from)
        && appimage.is_absolute()
    {
        return Ok(Some(appimage));
    }
    std::env::current_exe().map(Some)
}
