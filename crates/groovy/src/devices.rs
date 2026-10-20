//! The Devices section of Settings: link your devices so contacts trust all of them, and revoke
//! one that is lost. See `docs/identity.md`.

use std::time::Duration;

use gpui::{Context, Entity, FontWeight, SharedString, Window, div, prelude::*, px};
use groovy_core::EndpointId;
use groovy_core::user::UserDevice;
use tracing::warn;

use crate::{
    node::{NodeState, abbreviate_id},
    theme::ActiveTheme,
    ui::{
        button::{Button, Variant},
        icon::Icon,
        input::TextInput,
    },
};

/// How often the device list is read again.
const REFRESH_EVERY: Duration = Duration::from_secs(3);

pub struct DevicesSection {
    devices: Vec<UserDevice>,
    own_id: Option<EndpointId>,
    /// What this device is called when it is added to a user.
    name: Entity<TextInput>,
    /// Where a link ticket from another device is pasted.
    ticket_input: Entity<TextInput>,
    /// The ticket made most recently, to copy.
    new_ticket: Option<String>,
    /// The device the user has asked to revoke, waiting for the second click.
    revoking: Option<EndpointId>,
    status: Option<(bool, SharedString)>,
}

fn default_name() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "this device".to_owned())
}

impl DevicesSection {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| TextInput::new("Name for this device", cx));
        name.update(cx, |input, cx| input.set_text(default_name(), cx));
        let section = Self {
            devices: Vec::new(),
            own_id: None,
            name,
            ticket_input: cx
                .new(|cx| TextInput::new("Paste a link ticket from another device", cx)),
            new_ticket: None,
            revoking: None,
            status: None,
        };

        cx.spawn(async move |this, cx| {
            loop {
                let node = cx.update(|cx| {
                    cx.try_global::<NodeState>()
                        .map(|node| (node.handle(), node.endpoint_id()))
                });
                if let Some((Some(handle), own_id)) = node
                    && let Ok(devices) = handle.user_devices().await
                {
                    let shown = this.update(cx, |section, cx| {
                        section.devices = devices;
                        section.own_id = Some(own_id);
                        cx.notify();
                    });
                    if shown.is_err() {
                        break;
                    }
                }
                cx.background_executor().timer(REFRESH_EVERY).await;
            }
        })
        .detach();
        section
    }

    fn set_status(
        &mut self,
        error: bool,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.status = Some((error, message.into()));
        cx.notify();
    }

    fn handle(cx: &Context<Self>) -> Option<groovy_core::NodeHandle> {
        cx.try_global::<NodeState>().and_then(|node| node.handle())
    }

    /// Opens a link for another device. This device becomes a user the first time.
    fn make_ticket(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = Self::handle(cx) else {
            self.set_status(true, "The node is not running yet.", cx);
            return;
        };
        let name = self.name.read(cx).text().trim().to_owned();
        if name.is_empty() {
            self.set_status(true, "Give this device a name first.", cx);
            return;
        }
        cx.spawn(async move |this, cx| {
            let outcome = handle.link_device(name).await;
            this.update(cx, |section, cx| match outcome {
                Ok(ticket) => {
                    section.new_ticket = Some(ticket);
                    section.status = None;
                    cx.notify();
                }
                Err(error) => {
                    warn!(%error, "could not open a link");
                    section.set_status(true, format!("The link could not be made: {error}"), cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Links this device to the user in a ticket made on another of their devices.
    fn link_this(&mut self, cx: &mut Context<Self>) {
        let ticket = self.ticket_input.read(cx).text().trim().to_owned();
        if ticket.is_empty() {
            self.set_status(true, "Paste a link ticket first.", cx);
            return;
        }
        let name = self.name.read(cx).text().trim().to_owned();
        if name.is_empty() {
            self.set_status(true, "Give this device a name first.", cx);
            return;
        }
        let Some(handle) = Self::handle(cx) else {
            self.set_status(true, "The node is not running yet.", cx);
            return;
        };
        self.set_status(false, "Linking...", cx);
        cx.spawn(async move |this, cx| {
            let outcome = handle.accept_link(ticket, name).await;
            this.update(cx, |section, cx| match outcome {
                Ok(_) => {
                    section
                        .ticket_input
                        .update(cx, |input, cx| input.set_text("", cx));
                    section.set_status(false, "Linked. Your contacts will trust this device.", cx);
                }
                Err(error) => {
                    warn!(%error, "could not link this device");
                    section.set_status(
                        true,
                        format!("This device could not be linked: {error}"),
                        cx,
                    );
                }
            })
            .ok();
        })
        .detach();
    }

    /// Revokes a device, after the second click.
    fn revoke(&mut self, device: EndpointId, cx: &mut Context<Self>) {
        if self.revoking != Some(device) {
            self.revoking = Some(device);
            cx.notify();
            return;
        }
        self.revoking = None;
        let Some(handle) = Self::handle(cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let outcome = handle.revoke_device(device).await;
            this.update(cx, |section, cx| match outcome {
                Ok(()) => section.set_status(
                    false,
                    "Revoked. Your contacts refuse it once one of your devices connects to them.",
                    cx,
                ),
                Err(error) => section.set_status(
                    true,
                    format!("The device could not be revoked: {error}"),
                    cx,
                ),
            })
            .ok();
        })
        .detach();
    }
}

impl Render for DevicesSection {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let view = cx.entity().clone();

        let rows: Vec<_> = self
            .devices
            .iter()
            .map(|entry| {
                let device = entry.cert.device;
                let id = abbreviate_id(&device.to_string());
                let is_this = Some(device) == self.own_id;
                let revoke_view = view.clone();
                let keep_view = view.clone();
                let confirming = self.revoking == Some(device);

                let mut row = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .py_1()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .child(div().text_sm().child(entry.cert.name.clone()))
                            .child(div().text_xs().text_color(theme.text_muted).child(format!(
                                "{id}{}",
                                if is_this {
                                    " · this device"
                                } else if entry.revoked.is_some() {
                                    " · revoked"
                                } else {
                                    ""
                                }
                            ))),
                    );
                if !is_this && entry.revoked.is_none() {
                    row = row.child(if confirming {
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(
                                Button::new(
                                    format!("device-revoke-confirm-{id}"),
                                    "Revoke for good",
                                )
                                .variant(Variant::Danger)
                                .on_click(move |_, cx| {
                                    revoke_view
                                        .update(cx, |section, cx| section.revoke(device, cx));
                                }),
                            )
                            .child(
                                Button::new(format!("device-revoke-keep-{id}"), "Keep")
                                    .variant(Variant::Ghost)
                                    .on_click(move |_, cx| {
                                        keep_view.update(cx, |section, cx| {
                                            section.revoking = None;
                                            cx.notify();
                                        });
                                    }),
                            )
                    } else {
                        div().child(
                            Button::new(format!("device-revoke-{id}"), "Revoke")
                                .variant(Variant::Secondary)
                                .on_click(move |_, cx| {
                                    revoke_view
                                        .update(cx, |section, cx| section.revoke(device, cx));
                                }),
                        )
                    });
                }
                row
            })
            .collect();

        let make_view = view.clone();
        let link_view = view.clone();
        let status = self.status.as_ref().map(|(error, message)| {
            let color = if *error { theme.danger } else { theme.success };
            div().text_sm().text_color(color).child(message.clone())
        });

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Devices"))
            .child(
                div().text_xs().text_color(theme.text_muted).child(
                    "Link your devices so your contacts trust all of them after pairing with any one. Every linked device holds your user key, so revoke a lost device here. A revocation reaches your contacts the next time one of your devices connects to them.",
                ),
            )
            .children(rows)
            .child(self.name.clone())
            .child(
                Button::new("device-make-ticket", "Link another device")
                    .variant(Variant::Secondary)
                    .on_click(move |_, cx| make_view.update(cx, |section, cx| section.make_ticket(cx))),
            )
            .when_some(self.new_ticket.clone(), |column, ticket| {
                let copy = ticket.clone();
                column
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.text_muted)
                            .child("Paste this on the other device. It works once, for ten minutes."),
                    )
                    .child(div().text_xs().child(ticket))
                    .child(
                        Button::new("device-copy-ticket", "Copy ticket")
                            .variant(Variant::Secondary)
                            .icon(Icon::Copy)
                            .on_click(move |_, cx| {
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy.clone()));
                            }),
                    )
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .items_center()
                    .max_w(px(560.))
                    .child(div().flex_1().child(self.ticket_input.clone()))
                    .child(
                        Button::new("device-link-this", "Link this device")
                            .on_click(move |_, cx| link_view.update(cx, |section, cx| section.link_this(cx))),
                    ),
            )
            .children(status)
    }
}
