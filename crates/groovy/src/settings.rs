//! The Settings screen: profile, appearance, network, identity, storage and advanced.
//!
//! Each section says what it does today. Network and storage options that
//! depend on later issues are described, not offered, so nothing is shown as
//! a control that does nothing.

use std::{path::PathBuf, sync::Arc};

use gpui::{App, Context, Entity, FontWeight, Window, div, prelude::*, px};
use groovy_core::{Appearance, DataDir, Database};
use tracing::warn;

use crate::{
    backup::BackupSection,
    data::AppData,
    node::abbreviate_id,
    settings_profile::ProfileEditor,
    theme::{self, ActiveTheme},
    ui::{
        button::{Button, Variant},
        icon::Icon,
    },
};

/// The Settings screen's sections, built once and kept while the window is open.
pub struct SettingsScreen {
    profile: Entity<ProfileEditor>,
    backup: Entity<BackupSection>,
}

impl SettingsScreen {
    pub fn new(database: Arc<Database>, cx: &mut Context<Self>) -> Self {
        Self {
            profile: cx.new(|cx| ProfileEditor::new(database, cx)),
            backup: cx.new(BackupSection::new),
        }
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

/// A switch for one way of finding peers. Takes effect the next time Groovy starts.
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
                "Network",
                "How this device reaches peers. Discovery changes take effect the next time Groovy starts.",
                cx,
            ))
            .child(info_row(
                "Relay",
                "n0's public relays. Self-hosted relays arrive in #23.".to_owned(),
                None::<gpui::Div>,
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
            .child(info_row(
                "Usage",
                "Shared storage and cleanup arrive with blob storage (#26).".to_owned(),
                None::<gpui::Div>,
                cx,
            ))
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
