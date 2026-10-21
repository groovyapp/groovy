//! The Settings screen: profile, appearance, network, identity, storage and advanced.
//!
//! Each section says what it does today. Network and storage options that
//! depend on later issues are described, not offered, so nothing is shown as
//! a control that does nothing.

use std::{path::PathBuf, sync::Arc};

use gpui::{App, Context, Entity, FontWeight, SharedString, Window, div, prelude::*, px};
use groovy_core::{Appearance, DataDir, Database, RelaySetting, storage::StorageUsage};
use tracing::warn;

use crate::{
    backup::BackupSection,
    data::AppData,
    devices::DevicesSection,
    files::format_bytes,
    node::{NodeState, abbreviate_id},
    settings_profile::ProfileEditor,
    theme::{self, ActiveTheme},
    ui::{
        button::{Button, Variant},
        icon::Icon,
        input::TextInput,
    },
};

/// The Settings screen's sections, built once and kept while the window is open.
pub struct SettingsScreen {
    profile: Entity<ProfileEditor>,
    backup: Entity<BackupSection>,
    devices: Entity<DevicesSection>,
    relay_input: Entity<TextInput>,
    /// The last result of saving relays: whether it was an error, and the message.
    relay_status: Option<(bool, SharedString)>,
    cap_input: Entity<TextInput>,
    /// What the node holds, as last read from it.
    storage: Option<StorageUsage>,
    /// The last result of a storage action: whether it was an error, and the message.
    storage_status: Option<(bool, SharedString)>,
}

impl SettingsScreen {
    pub fn new(database: Arc<Database>, cx: &mut Context<Self>) -> Self {
        let mut screen = Self {
            profile: cx.new(|cx| ProfileEditor::new(database, cx)),
            backup: cx.new(BackupSection::new),
            devices: cx.new(DevicesSection::new),
            relay_input: cx.new(|cx| {
                TextInput::new("https://relay.example.org./ (space or comma separated)", cx)
            }),
            relay_status: None,
            cap_input: cx.new(|cx| TextInput::new("Cap in MB, for example 10240", cx)),
            storage: None,
            storage_status: None,
        };
        screen.refresh_storage(cx);
        screen
    }

    /// Reads what the node holds. Does nothing until the node is running.
    pub fn refresh_storage(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = cx.try_global::<NodeState>().and_then(|node| node.handle()) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            if let Ok(usage) = handle.storage_usage().await {
                this.update(cx, |screen, cx| {
                    screen.storage = Some(usage);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn set_storage_status(
        &mut self,
        error: bool,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.storage_status = Some((error, message.into()));
        cx.notify();
    }

    /// Sets the cap from the field, in megabytes. An empty field removes the cap.
    fn apply_cap(&mut self, cx: &mut Context<Self>) {
        let text = self.cap_input.read(cx).text().trim().to_owned();
        let cap = if text.is_empty() {
            None
        } else {
            match text.parse::<f64>() {
                Ok(megabytes) if megabytes > 0.0 => Some((megabytes * 1024.0 * 1024.0) as u64),
                _ => {
                    self.set_storage_status(
                        true,
                        "Enter a number of megabytes, or leave it empty for no limit.",
                        cx,
                    );
                    return;
                }
            }
        };
        let Some(handle) = cx.try_global::<NodeState>().and_then(|node| node.handle()) else {
            self.set_storage_status(true, "The node is not running yet.", cx);
            return;
        };
        cx.spawn(async move |this, cx| {
            let outcome = handle.set_storage_cap(cap).await;
            this.update(cx, |screen, cx| {
                match outcome {
                    Ok(()) => screen.set_storage_status(
                        false,
                        match cap {
                            Some(_) => "The cap is set.",
                            None => "There is no cap.",
                        },
                        cx,
                    ),
                    Err(error) => screen.set_storage_status(
                        true,
                        format!("The cap could not be saved: {error}"),
                        cx,
                    ),
                }
                screen.refresh_storage(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Clears the cache: the received files' history and the sent files' tags. The files
    /// already saved to disk, and the shared folders, are not touched.
    fn clear_cache(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = cx.try_global::<NodeState>().and_then(|node| node.handle()) else {
            self.set_storage_status(true, "The node is not running yet.", cx);
            return;
        };
        cx.spawn(async move |this, cx| {
            let outcome = handle.clear_cache().await;
            this.update(cx, |screen, cx| {
                match outcome {
                    Ok(()) => screen.set_storage_status(
                        false,
                        "The cache is cleared. Files already saved to disk are kept.",
                        cx,
                    ),
                    Err(error) => screen.set_storage_status(
                        true,
                        format!("The cache could not be cleared: {error}"),
                        cx,
                    ),
                }
                screen.refresh_storage(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Uses the relay URLs typed in the field. Any token already set in config.toml is kept.
    fn use_typed_relays(&mut self, cx: &mut Context<Self>) {
        let text = self.relay_input.read(cx).text().to_owned();
        match groovy_core::parse_relay_urls(&text) {
            Ok(urls) => {
                let auth_token = match &cx.global::<AppData>().config.relay {
                    RelaySetting::Custom { auth_token, .. } => auth_token.clone(),
                    _ => None,
                };
                self.set_relay(RelaySetting::Custom { urls, auth_token }, cx);
                self.relay_input
                    .update(cx, |input, cx| input.set_text("", cx));
                self.relay_status = Some((
                    false,
                    "Saved. Takes effect the next time Groovy starts.".into(),
                ));
            }
            Err(message) => {
                self.relay_status = Some((true, message.into()));
            }
        }
        cx.notify();
    }

    /// Changes which relays the node uses and saves it. Takes effect the next time Groovy starts.
    fn set_relay(&mut self, relay: RelaySetting, cx: &mut Context<Self>) {
        let config = {
            let data = cx.global_mut::<AppData>();
            data.config.relay = relay;
            data.config.clone()
        };
        let dir: DataDir = cx.global::<AppData>().dir.clone();
        cx.background_spawn(async move {
            if let Err(error) = config.save(&dir) {
                warn!(%error, "could not save the relay setting");
            }
        })
        .detach();
        cx.notify();
    }

    /// Changes how this node finds peers and saves it. The node reads the change the next time Groovy starts.
    fn set_discovery(
        &mut self,
        change: impl FnOnce(&mut groovy_core::Discovery),
        cx: &mut Context<Self>,
    ) {
        let config = {
            let data = cx.global_mut::<AppData>();
            change(&mut data.config.discovery);
            data.config.clone()
        };
        let dir: DataDir = cx.global::<AppData>().dir.clone();
        cx.background_spawn(async move {
            if let Err(error) = config.save(&dir) {
                warn!(%error, "could not save the discovery setting");
            }
        })
        .detach();
        cx.notify();
    }

    /// Saves the appearance to `config.toml` and applies it straight away.
    fn set_appearance(&mut self, choice: Appearance, cx: &mut Context<Self>) {
        let config = {
            let data = cx.global_mut::<AppData>();
            data.config.appearance = choice;
            data.config.clone()
        };
        let dir: DataDir = cx.global::<AppData>().dir.clone();
        theme::set_mode_override(theme::mode_for(choice), cx);

        cx.background_spawn(async move {
            if let Err(error) = config.save(&dir) {
                warn!(%error, "could not save the appearance setting");
            }
        })
        .detach();
        cx.notify();
    }
}

/// A heading for a section, with a short line under it.
fn section_heading(title: &'static str, detail: &'static str, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child(title),
        )
        .child(div().text_xs().text_color(theme.text_muted).child(detail))
}

/// Which relays are used: n0's public relays, none, or a custom list. Public and
/// Off choose at once; a custom list is typed in the field and saved with its button.
fn relay_row(
    relay: &RelaySetting,
    view: Entity<SettingsScreen>,
    input: Entity<TextInput>,
    status: Option<(bool, SharedString)>,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    let (current, detail) = match relay {
        RelaySetting::Public => (
            "Public",
            "n0's public relays help peers connect when a direct connection is not possible.",
        ),
        RelaySetting::Disabled => (
            "Off",
            "No relays. Peers must be reachable directly, so some connections will fail.",
        ),
        RelaySetting::Custom { urls, .. } => (
            "Custom",
            if urls.len() == 1 {
                "Your own relay."
            } else {
                "Your own relays."
            },
        ),
    };
    let choose = |label: &'static str, id: &'static str, value: RelaySetting| {
        let selected = std::mem::discriminant(&value) == std::mem::discriminant(relay);
        let view = view.clone();
        Button::new(id, label)
            .variant(if selected {
                Variant::Primary
            } else {
                Variant::Secondary
            })
            .on_click(move |_, cx| {
                let value = value.clone();
                view.update(cx, |screen, cx| screen.set_relay(value, cx))
            })
    };
    let custom_urls = match relay {
        RelaySetting::Custom { urls, .. } => urls.join("  "),
        _ => String::new(),
    };
    let save_view = view.clone();
    let status_line = status.map(|(error, message)| {
        let color = if error { theme.danger } else { theme.success };
        div().text_xs().text_color(color).child(message)
    });

    div()
        .flex()
        .flex_col()
        .gap_3()
        .child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(div().text_sm().child(format!("Relays: {current}")))
                        .child(div().text_xs().text_color(theme.text_muted).child(detail)),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .child(choose(
                            "Public",
                            "settings-relay-public",
                            RelaySetting::Public,
                        ))
                        .child(choose("Off", "settings-relay-off", RelaySetting::Disabled)),
                ),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(div().text_xs().text_color(theme.text_muted).child(
                    if custom_urls.is_empty() {
                        "Use your own relays. Enter their URLs, then save.".to_owned()
                    } else {
                        format!("Current: {custom_urls}")
                    },
                ))
                .child(input)
                .child(
                    div().flex().flex_row().child(
                        Button::new("settings-relay-save", "Use these relays")
                            .variant(Variant::Secondary)
                            .on_click(move |_, cx| {
                                save_view.update(cx, |screen, cx| screen.use_typed_relays(cx))
                            }),
                    ),
                )
                .children(status_line),
        )
}

/// A switch for one way of finding peers. Takes effect the next time Groovy starts.
/// A row with an On/Off switch for one notification setting, saved as soon as it changes.
fn notification_row(
    label: &'static str,
    detail: &'static str,
    id: &'static str,
    on: bool,
    set: fn(&mut groovy_core::Notifications, bool),
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    let button = Button::new(id, if on { "On" } else { "Off" })
        .variant(if on {
            Variant::Primary
        } else {
            Variant::Secondary
        })
        .on_click(move |_, cx| {
            crate::notifications::update(cx, |settings| set(settings, !on));
        });
    div()
        .flex()
        .flex_row()
        .items_start()
        .gap_3()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_sm().child(label))
                .child(div().text_xs().text_color(theme.text_muted).child(detail)),
        )
        .child(button)
}

fn discovery_row(
    label: &'static str,
    detail: &'static str,
    id: &'static str,
    on: bool,
    view: Entity<SettingsScreen>,
    set: fn(&mut groovy_core::Discovery, bool),
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    let button = Button::new(id, if on { "On" } else { "Off" })
        .variant(if on {
            Variant::Primary
        } else {
            Variant::Secondary
        })
        .on_click(move |_, cx| {
            view.update(cx, |screen, cx| {
                screen.set_discovery(|discovery| set(discovery, !on), cx)
            })
        });
    div()
        .flex()
        .flex_row()
        .items_start()
        .gap_3()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_sm().child(label))
                .child(div().text_xs().text_color(theme.text_muted).child(detail)),
        )
        .child(button)
}

impl SettingsScreen {
    /// What the node holds, the cap, the cap field, and clearing the cache.
    fn storage_usage_rows(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let view = cx.entity().clone();
        let clear_view = view.clone();
        let cap_view = view.clone();
        let no_cap_view = view.clone();

        let usage = self.storage.map(|usage| {
            let cap = match usage.cap_bytes {
                Some(bytes) => format_bytes(bytes),
                None => "No limit".to_owned(),
            };
            let warning = usage.near_cap().then(|| {
                div().text_sm().text_color(theme.warning).child(
                    "This device is near its storage cap. Clear the cache, or raise the cap.",
                )
            });
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(info_row(
                    "Received files",
                    format_bytes(usage.received_bytes),
                    None::<gpui::Div>,
                    cx,
                ))
                .child(info_row(
                    "Shared folders",
                    format_bytes(usage.folder_bytes),
                    None::<gpui::Div>,
                    cx,
                ))
                .child(info_row("Cap", cap, None::<gpui::Div>, cx))
                .children(warning)
        });
        let status = self.storage_status.as_ref().map(|(error, message)| {
            let color = if *error { theme.danger } else { theme.success };
            div().text_sm().text_color(color).child(message.clone())
        });

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(match usage {
                Some(rows) => rows.into_any_element(),
                None => div()
                    .text_sm()
                    .text_color(theme.text_muted)
                    .child("Storage is read once the node is running.")
                    .into_any_element(),
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .items_center()
                    .child(div().flex_1().child(self.cap_input.clone()))
                    .child(
                        Button::new("storage-set-cap", "Set cap")
                            .variant(Variant::Secondary)
                            .on_click(move |_, cx| {
                                cap_view.update(cx, |screen, cx| screen.apply_cap(cx))
                            }),
                    )
                    .child(
                        Button::new("storage-no-cap", "No limit")
                            .variant(Variant::Ghost)
                            .on_click(move |_, cx| {
                                no_cap_view.update(cx, |screen, cx| {
                                    screen
                                        .cap_input
                                        .update(cx, |input, cx| input.set_text("", cx));
                                    screen.apply_cap(cx);
                                })
                            }),
                    ),
            )
            .child(
                Button::new("storage-clear-cache", "Clear cache")
                    .variant(Variant::Secondary)
                    .icon(Icon::Trash)
                    .on_click(move |_, cx| {
                        clear_view.update(cx, |screen, cx| screen.clear_cache(cx))
                    }),
            )
            .children(status)
    }
}

fn divider(cx: &App) -> impl IntoElement {
    div().h(px(1.)).w_full().bg(cx.theme().border)
}

/// A row of label and value, with an optional button on the right.
fn info_row(
    label: &'static str,
    value: String,
    action: Option<impl IntoElement>,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .child(
            div()
                .w(px(96.))
                .text_xs()
                .text_color(theme.text_muted)
                .child(label),
        )
        .child(div().flex_1().min_w_0().text_sm().child(value))
        .children(action)
}

impl Render for SettingsScreen {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().clone();
        let data = cx.global::<AppData>();
        let current = data.config.appearance;
        let discovery = data.config.discovery;
        let notifications = data.config.notifications.clone();
        let relay = data.config.relay.clone();
        let data_dir: PathBuf = data.dir.root().to_path_buf();
        let logs_dir: PathBuf = data.dir.logs_dir();
        let endpoint_id = data.identity.public_key().to_string();

        let appearance_button = |choice: Appearance, label: &'static str| {
            let view = view.clone();
            let variant = if choice == current {
                Variant::Primary
            } else {
                Variant::Secondary
            };
            Button::new(label, label)
                .variant(variant)
                .on_click(move |_, cx| {
                    view.update(cx, |screen, cx| screen.set_appearance(choice, cx))
                })
        };

        let reveal_data = {
            let path = data_dir.clone();
            Button::new("settings-open-data", "Show in folder")
                .variant(Variant::Secondary)
                .on_click(move |_, cx| cx.reveal_path(&path))
        };
        let reveal_logs = {
            let path = logs_dir.clone();
            Button::new("settings-open-logs", "Open logs folder")
                .variant(Variant::Secondary)
                .on_click(move |_, cx| cx.reveal_path(&path))
        };
        let copy_id = {
            let id = endpoint_id.clone();
            Button::icon_only("settings-copy-id", Icon::Copy)
                .variant(Variant::Ghost)
                .tooltip("Copy endpoint ID")
                .on_click(move |_, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(id.clone()));
                })
        };

        div()
            .flex()
            .flex_col()
            .gap_6()
            .max_w(px(560.))
            .child(self.profile.clone())
            .child(divider(cx))
            .child(section_heading(
                "Appearance",
                "Follow the system, or always use one theme.",
                cx,
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .child(appearance_button(Appearance::System, "System"))
                    .child(appearance_button(Appearance::Light, "Light"))
                    .child(appearance_button(Appearance::Dark, "Dark")),
            )
            .child(divider(cx))
            .child(section_heading(
                "Notifications",
                "Told about messages, pairing requests and files while the window is away.",
                cx,
            ))
            .child(notification_row(
                "Do not disturb",
                "Turn every notification off.",
                "settings-dnd",
                notifications.do_not_disturb,
                |settings, on| settings.do_not_disturb = on,
                cx,
            ))
            .child(notification_row(
                "Hide content",
                "Say that something happened, without who or what.",
                "settings-hide-content",
                notifications.hide_content,
                |settings, on| settings.hide_content = on,
                cx,
            ))
            .child(divider(cx))
            .child(section_heading(
                "Network",
                "How this device reaches peers. Discovery changes take effect the next time Groovy starts.",
                cx,
            ))
            .child(discovery_row(
                "Local network",
                "Find devices on this network without the internet. Announces this device's ID and addresses there.",
                "settings-local-discovery",
                discovery.local_network,
                cx.entity().clone(),
                |discovery, on| discovery.local_network = on,
                cx,
            ))
            .child(discovery_row(
                "Internet directory",
                "Publish this device's ID and addresses to the public DHT, so anyone who knows the ID can find it.",
                "settings-dht-discovery",
                discovery.dht,
                cx.entity().clone(),
                |discovery, on| discovery.dht = on,
                cx,
            ))
            .child(relay_row(
                &relay,
                cx.entity().clone(),
                self.relay_input.clone(),
                self.relay_status.clone(),
                cx,
            ))
            .child(divider(cx))
            .child(section_heading(
                "Identity",
                "Your endpoint ID is how peers find you. Back up the identity from here.",
                cx,
            ))
            .child(info_row(
                "Endpoint ID",
                abbreviate_id(&endpoint_id),
                Some(copy_id),
                cx,
            ))
            .child(self.backup.clone())
            .child(divider(cx))
            .child(self.devices.clone())
            .child(divider(cx))
            .child(section_heading(
                "Storage",
                "Where Groovy keeps its data on this device.",
                cx,
            ))
            .child(info_row(
                "Data folder",
                data_dir.display().to_string(),
                Some(reveal_data),
                cx,
            ))
            .child(self.storage_usage_rows(cx))
            .child(divider(cx))
            .child(section_heading("Advanced", "Logs and version.", cx))
            .child(info_row(
                "Version",
                format!("Groovy {}", env!("CARGO_PKG_VERSION")),
                None::<gpui::Div>,
                cx,
            ))
            .child(info_row(
                "Logs",
                logs_dir.display().to_string(),
                Some(reveal_logs),
                cx,
            ))
    }
}
