//! Desktop notifications for messages, pairing requests and transfers.
//!
//! A notification is shown only while the window is not focused, and never while
//! do-not-disturb is on or the conversation is muted. With "hide content" on, it says that
//! something happened, without who or what. Clicking one brings the window forward on the place
//! it was about, which the workspace takes from [`take_opened`].
//!
//! Click handling is available where the notification server reports actions (Linux). Elsewhere
//! notifications are shown but a click only brings the app forward.

use std::sync::{Mutex, PoisonError};

use gpui::{App, AppContext};
use groovy_core::groups::GroupId;
use groovy_core::{Contact, Database, EndpointId, NodeEvent, Notifications};
use tracing::warn;

use crate::data::AppData;

/// Where a notification leads when it is clicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Destination {
    Contact(EndpointId),
    Group(GroupId),
    Files,
    Peers,
}

/// Clicked notifications the workspace has not taken yet.
static OPENED: Mutex<Vec<Destination>> = Mutex::new(Vec::new());

/// The destinations of notifications clicked since the last call, oldest first.
pub fn take_opened() -> Vec<Destination> {
    std::mem::take(&mut *OPENED.lock().unwrap_or_else(PoisonError::into_inner))
}

/// The key a conversation is muted under in the config.
pub fn mute_key(destination: Destination) -> Option<String> {
    match destination {
        Destination::Contact(id) => Some(format!("contact:{id}")),
        Destination::Group(id) => Some(format!("group:{}", hex(&id))),
        Destination::Files | Destination::Peers => None,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Whether a conversation is muted in these settings.
pub fn is_muted(settings: &Notifications, destination: Destination) -> bool {
    mute_key(destination).is_some_and(|key| settings.muted.contains(&key))
}

/// Whether a notification may be shown at all: the window is away, and notifications are not
/// off for everything or for this conversation.
pub fn allowed(settings: &Notifications, focused: bool, muted: bool) -> bool {
    !focused && !settings.do_not_disturb && !muted
}

/// The title and body to show: the real ones, or a generic line when content is hidden.
pub fn text(settings: &Notifications, title: &str, body: &str) -> (String, String) {
    if settings.hide_content {
        (
            "Groovy".to_owned(),
            "You have a new notification.".to_owned(),
        )
    } else {
        (title.to_owned(), body.to_owned())
    }
}

/// Shows a notification for a node event, if this event is one the user is told about and the
/// window is away.
pub fn on_event(cx: &App, event: &NodeEvent) {
    let focused = cx.active_window().is_some();
    let Some(data) = cx.try_global::<AppData>() else {
        return;
    };
    let settings = data.config.notifications.clone();
    let database = data.database.clone();
    let notice = match event {
        NodeEvent::MessageReceived(message) => {
            let destination = Destination::Contact(message.author);
            let muted = is_muted(&settings, destination);
            if !allowed(&settings, focused, muted) {
                return;
            }
            let who = contact_label(&database, &message.author);
            let body = if message.attachments.is_empty() {
                preview(&message.body)
            } else {
                format!("Sent a file: {}", message.attachments[0].name)
            };
            Some((who, body, destination))
        }
        NodeEvent::GroupMessageReceived(message) => {
            let destination = Destination::Group(message.group);
            let muted = is_muted(&settings, destination);
            if !allowed(&settings, focused, muted) {
                return;
            }
            let title = group_name(&database, &message.group);
            let who = contact_label(&database, &message.author);
            Some((
                title,
                format!("{who}: {}", preview(&message.body)),
                destination,
            ))
        }
        NodeEvent::PairingRequested { peer, .. } => {
            if !allowed(&settings, focused, false) {
                return;
            }
            Some((
                "Pairing request".to_owned(),
                peer.profile.display_name.clone(),
                Destination::Peers,
            ))
        }
        NodeEvent::FileOffered { peer, offer } => {
            if !allowed(&settings, focused, false) {
                return;
            }
            Some((
                "File offered".to_owned(),
                format!(
                    "{} offered {}",
                    peer.profile.display_name,
                    offer.names.join(", ")
                ),
                Destination::Files,
            ))
        }
        NodeEvent::FileReceived { names, .. } => {
            if !allowed(&settings, focused, false) {
                return;
            }
            Some((
                "File received".to_owned(),
                names.join(", "),
                Destination::Files,
            ))
        }
        _ => None,
    };
    if let Some((title, body, destination)) = notice {
        let (title, body) = text(&settings, &title, &body);
        show(&title, &body, destination);
    }
}

/// Shows one notification. Clicking it queues its destination, on platforms that report clicks.
fn show(title: &str, body: &str, destination: Destination) {
    let mut note = notify_rust::Notification::new();
    note.appname("Groovy").summary(title).body(body);
    #[cfg(all(unix, not(target_os = "macos")))]
    note.action("default", "Open");
    match note.show() {
        Ok(handle) => {
            #[cfg(all(unix, not(target_os = "macos")))]
            std::thread::spawn(move || {
                handle.wait_for_action(|action| {
                    if action == "default" {
                        OPENED
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(destination);
                    }
                });
            });
            #[cfg(not(all(unix, not(target_os = "macos"))))]
            {
                let _ = (handle, destination);
            }
        }
        Err(error) => warn!(%error, "a notification could not be shown"),
    }
}

/// Turns a conversation's notifications on or off, and saves the config.
pub fn set_muted(cx: &mut App, destination: Destination, muted: bool) {
    let Some(key) = mute_key(destination) else {
        return;
    };
    let config = {
        let data = cx.global_mut::<AppData>();
        let list = &mut data.config.notifications.muted;
        list.retain(|existing| *existing != key);
        if muted {
            list.push(key);
        }
        data.config.clone()
    };
    let dir = cx.global::<AppData>().dir.clone();
    cx.background_spawn(async move {
        if let Err(error) = config.save(&dir) {
            warn!(%error, "could not save the notification settings");
        }
    })
    .detach();
}

/// Changes the notification settings, and saves the config.
pub fn update(cx: &mut App, change: impl FnOnce(&mut Notifications)) {
    let config = {
        let data = cx.global_mut::<AppData>();
        change(&mut data.config.notifications);
        data.config.clone()
    };
    let dir = cx.global::<AppData>().dir.clone();
    cx.background_spawn(async move {
        if let Err(error) = config.save(&dir) {
            warn!(%error, "could not save the notification settings");
        }
    })
    .detach();
}

fn contact_label(database: &Database, id: &EndpointId) -> String {
    database
        .contacts()
        .ok()
        .and_then(|contacts| {
            contacts
                .into_iter()
                .find(|c: &Contact| c.endpoint_id == *id)
        })
        .map(|contact| contact.label().to_owned())
        .unwrap_or_else(|| "A contact".to_owned())
}

fn group_name(database: &Database, id: &GroupId) -> String {
    database
        .groups()
        .ok()
        .and_then(|groups| groups.into_iter().find(|group| group.id == *id))
        .map(|group| group.name)
        .unwrap_or_else(|| "Group".to_owned())
}

/// The start of a message, for a notification body.
fn preview(body: &str) -> String {
    let single: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if single.chars().count() > 120 {
        format!("{}…", single.chars().take(120).collect::<String>())
    } else {
        single
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_notification_is_shown_only_while_the_window_is_away() {
        let settings = Notifications::default();
        assert!(allowed(&settings, false, false));
        assert!(
            !allowed(&settings, true, false),
            "not while the window is focused"
        );
        assert!(
            !allowed(&settings, false, true),
            "not for a muted conversation"
        );
    }

    #[test]
    fn do_not_disturb_silences_everything() {
        let settings = Notifications {
            do_not_disturb: true,
            ..Notifications::default()
        };
        assert!(!allowed(&settings, false, false));
    }

    #[test]
    fn hidden_content_says_only_that_something_happened() {
        let shown = Notifications::default();
        assert_eq!(
            text(&shown, "Ada", "lunch?"),
            ("Ada".to_owned(), "lunch?".to_owned())
        );
        let hidden = Notifications {
            hide_content: true,
            ..Notifications::default()
        };
        let (title, body) = text(&hidden, "Ada", "lunch?");
        assert!(!title.contains("Ada") && !body.contains("lunch"));
    }

    #[test]
    fn a_muted_conversation_is_found_by_its_key() {
        let id = groovy_core::SecretKey::generate().public();
        let settings = Notifications {
            muted: vec![mute_key(Destination::Contact(id)).unwrap()],
            ..Notifications::default()
        };
        assert!(is_muted(&settings, Destination::Contact(id)));
        assert!(!is_muted(&settings, Destination::Group([1; 32])));
        assert!(!is_muted(&settings, Destination::Files));
    }
}
