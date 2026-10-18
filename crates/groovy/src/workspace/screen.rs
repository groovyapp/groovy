//! The screens the sidebar switches between, their shortcuts, and remembering
//! the last one across launches.

use std::sync::Arc;

use gpui::{App, AppContext, KeyBinding, actions};
use groovy_core::Database;
use tracing::warn;

use crate::ui::icon::Icon;

actions!(
    workspace,
    [
        ShowFiles,
        ShowMessages,
        ShowPeers,
        ShowStorage,
        ShowSettings
    ]
);

/// The settings-table key holding the last screen the user chose.
const LAST_SCREEN_KEY: &str = "shell.last_screen";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Screen {
    #[default]
    Files,
    Messages,
    Peers,
    Storage,
    Settings,
}

impl Screen {
    pub const ALL: [Screen; 5] = [
        Screen::Files,
        Screen::Messages,
        Screen::Peers,
        Screen::Storage,
        Screen::Settings,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Screen::Files => "Files",
            Screen::Messages => "Messages",
            Screen::Peers => "Peers",
            Screen::Storage => "Storage",
            Screen::Settings => "Settings",
        }
    }

    pub fn icon(self) -> Icon {
        match self {
            Screen::Files => Icon::File,
            Screen::Messages => Icon::Message,
            Screen::Peers => Icon::Users,
            Screen::Storage => Icon::Folder,
            Screen::Settings => Icon::Settings,
        }
    }

    /// What the screen will do once it is built, shown until then.
    pub fn description(self) -> &'static str {
        match self {
            Screen::Files => "Send and receive files with your contacts.",
            Screen::Messages => "Direct and group conversations.",
            Screen::Peers => "Contacts, pairing requests and how to reach each peer.",
            Screen::Storage => "Shared folders, replication and disk usage.",
            Screen::Settings => "Profile, appearance, network and identity.",
        }
    }

    /// The number pressed with the platform's shortcut modifier to open this screen.
    pub fn shortcut(self) -> &'static str {
        match self {
            Screen::Files => "1",
            Screen::Messages => "2",
            Screen::Peers => "3",
            Screen::Storage => "4",
            Screen::Settings => "5",
        }
    }

    /// The label for the shortcut in tooltips, such as "Ctrl+1".
    pub fn shortcut_label(self) -> String {
        let modifier = if cfg!(target_os = "macos") {
            "Cmd"
        } else {
            "Ctrl"
        };
        format!("{modifier}+{}", self.shortcut())
    }

    fn key(self) -> &'static str {
        match self {
            Screen::Files => "files",
            Screen::Messages => "messages",
            Screen::Peers => "peers",
            Screen::Storage => "storage",
            Screen::Settings => "settings",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|screen| screen.key() == key)
    }

    /// The screen to open at launch: the last one chosen, or Files.
    pub fn load(database: &Database) -> Self {
        match database.get_setting(LAST_SCREEN_KEY) {
            Ok(Some(key)) => Self::from_key(&key).unwrap_or_default(),
            Ok(None) => Self::default(),
            Err(error) => {
                warn!(%error, "could not read the last screen; opening Files");
                Self::default()
            }
        }
    }

    /// Saves this screen as the one to open next launch. The write happens on
    /// the background executor, so the UI thread never waits on the database.
    pub fn remember(self, database: Arc<Database>, cx: &App) {
        cx.background_spawn(async move {
            if let Err(error) = database.set_setting(LAST_SCREEN_KEY, self.key()) {
                warn!(%error, "could not save the last screen");
            }
        })
        .detach();
    }
}

/// Key bindings for the screen shortcuts. Call once at startup.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("secondary-1", ShowFiles, None),
        KeyBinding::new("secondary-2", ShowMessages, None),
        KeyBinding::new("secondary-3", ShowPeers, None),
        KeyBinding::new("secondary-4", ShowStorage, None),
        KeyBinding::new("secondary-5", ShowSettings, None),
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_screen_round_trips_through_its_key() {
        for screen in Screen::ALL {
            assert_eq!(Screen::from_key(screen.key()), Some(screen));
        }
    }

    #[test]
    fn unknown_keys_are_not_screens() {
        assert_eq!(Screen::from_key("nope"), None);
    }

    #[test]
    fn shortcuts_are_unique() {
        let mut shortcuts: Vec<_> = Screen::ALL.iter().map(|s| s.shortcut()).collect();
        shortcuts.sort_unstable();
        shortcuts.dedup();
        assert_eq!(shortcuts.len(), Screen::ALL.len());
    }

    #[test]
    fn the_last_saved_screen_is_loaded_back() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("groovy.sqlite");
        let database = Database::open(&path).unwrap();
        assert_eq!(Screen::load(&database), Screen::Files, "defaults to Files");

        database
            .set_setting(LAST_SCREEN_KEY, Screen::Peers.key())
            .unwrap();

        assert_eq!(Screen::load(&database), Screen::Peers);
    }

    #[test]
    fn an_unknown_saved_screen_falls_back_to_files() {
        let tmp = tempfile::tempdir().unwrap();
        let database = Database::open(&tmp.path().join("groovy.sqlite")).unwrap();
        database
            .set_setting(LAST_SCREEN_KEY, "removed-screen")
            .unwrap();

        assert_eq!(Screen::load(&database), Screen::Files);
    }
}
