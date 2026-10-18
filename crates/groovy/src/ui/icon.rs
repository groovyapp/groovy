//! Icons from Tabler Icons (MIT), embedded in the binary and loaded through
//! GPUI's asset source. See `assets/icons/tabler/README.md`.

use std::borrow::Cow;

use anyhow::Result;
use gpui::{AssetSource, Hsla, Pixels, SharedString, Svg, prelude::*, svg};

macro_rules! icons {
    ($($variant:ident => $file:literal),* $(,)?) => {
        /// An icon from the Tabler set.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Icon {
            $($variant),*
        }

        impl Icon {
            /// Every icon in the set, for the asset source and the gallery.
            pub const ALL: &'static [Icon] = &[$(Icon::$variant),*];

            /// The path the icon is loaded from.
            pub fn path(self) -> &'static str {
                match self {
                    $(Icon::$variant => concat!("icons/tabler/", $file, ".svg")),*
                }
            }

            fn bytes(self) -> &'static [u8] {
                match self {
                    $(Icon::$variant => include_bytes!(concat!(
                        "../../../../assets/icons/tabler/", $file, ".svg"
                    ))),*
                }
            }
        }
    };
}

icons! {
    Bell => "bell",
    Check => "check",
    ChevronDown => "chevron-down",
    CircleCheck => "circle-check",
    CircleX => "circle-x",
    Copy => "copy",
    Download => "download",
    File => "file",
    Folder => "folder",
    InfoCircle => "info-circle",
    Loader => "loader-2",
    Lock => "lock",
    Message => "message",
    Plus => "plus",
    Search => "search",
    Send => "send",
    Settings => "settings",
    Trash => "trash",
    TriangleAlert => "alert-triangle",
    Upload => "upload",
    User => "user",
    Users => "users",
    X => "x",
}

/// Serves the embedded icons to GPUI's `svg()` element.
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(Icon::ALL
            .iter()
            .find(|icon| icon.path() == path)
            .map(|icon| Cow::Borrowed(icon.bytes())))
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}

/// A square icon drawn in `color`.
pub fn icon(icon: Icon, size: Pixels, color: Hsla) -> Svg {
    svg()
        .path(icon.path())
        .size(size)
        .flex_none()
        .text_color(color)
}
