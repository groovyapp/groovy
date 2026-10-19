//! The ticket panel on the Peers screen: this device's ticket as a QR code,
//! and a way to connect to a peer by pasting theirs.

use std::sync::Arc;

use gpui::{
    App, Context, Entity, FontWeight, Image, ImageFormat, SharedString, Subscription, Window, div,
    img, prelude::*, px,
};
use groovy_core::ticket;
use tracing::warn;

use crate::{
    node::{NodeState, abbreviate_id},
    theme::ActiveTheme,
    ui::{
        button::{Button, Variant},
        icon::Icon,
        input::{TextInput, TextInputEvent},
    },
};

const QR_SIZE: gpui::Pixels = px(176.);

pub struct TicketPanel {
    input: Entity<TextInput>,
    status: Option<(bool, SharedString)>,
    busy: bool,
    /// The ticket last drawn, and its QR code, so the picture is not redrawn every frame.
    qr: Option<(String, Arc<Image>)>,
    _subscription: Subscription,
}

impl TicketPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| TextInput::new("Paste a ticket or groovy:// link", cx));
        let _subscription =
            cx.subscribe(&input, |this, _, event: &TextInputEvent, cx| match event {
                TextInputEvent::Changed => cx.notify(),
                TextInputEvent::Submitted => this.connect(cx),
            });
        Self {
            input,
            status: None,
            busy: false,
            qr: None,
            _subscription,
        }
    }

    /// This device's ticket, once its address is known.
    fn my_ticket(cx: &App) -> Option<String> {
        cx.try_global::<NodeState>()
            .and_then(|node| node.addr().map(ticket::encode))
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let text = self.input.read(cx).text().to_owned();
        let Some(node) = cx.try_global::<NodeState>() else {
            self.set_status(true, "The node is not running yet.", cx);
            return;
        };
        let our_id = node.endpoint_id();
        let Some(handle) = node.handle() else {
            self.set_status(true, "The node is not running yet.", cx);
            return;
        };

        let addr = match ticket::parse(&text, our_id) {
            Ok(addr) => addr,
            Err(error) => {
                self.set_status(true, error.to_string(), cx);
                return;
            }
        };

        // Both sides show this code, so each can check the other sees the same one.
        let code = groovy_core::verification_code(&our_id, &addr.id);
        self.busy = true;
        self.set_status(
            false,
            format!("Waiting for them to accept. Check that they see {code}."),
            cx,
        );
        let connecting = async move { handle.request_pairing(addr).await };
        cx.spawn(async move |this, cx| {
            let result = connecting.await;
            this.update(cx, |panel, cx| panel.finish_connect(result, cx))
                .ok();
        })
        .detach();
    }

    fn finish_connect(
        &mut self,
        result: Result<groovy_core::PeerInfo, groovy_core::NodeError>,
        cx: &mut Context<Self>,
    ) {
        self.busy = false;
        match result {
            Ok(peer) => {
                let name = if peer.profile.display_name.is_empty() {
                    "a peer".to_owned()
                } else {
                    peer.profile.display_name
                };
                self.input.update(cx, |input, cx| input.set_text("", cx));
                self.set_status(false, format!("Paired with {name}."), cx);
            }
            Err(groovy_core::NodeError::PairingDeclined) => {
                self.set_status(true, "They declined the pairing.", cx);
            }
            Err(error) => {
                warn!(%error, "could not connect to a peer");
                self.set_status(true, connect_message(&error), cx);
            }
        }
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
}

/// The message to show for a failed connection.
fn connect_message(error: &groovy_core::NodeError) -> String {
    match error {
        groovy_core::NodeError::Connect(protocol) => protocol.to_string(),
        other => other.to_string(),
    }
}

impl Render for TicketPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let view = cx.entity().clone();
        let my_ticket = Self::my_ticket(cx);

        // Redraw the QR code only when the ticket changes.
        if let Some(text) = my_ticket
            .as_ref()
            .filter(|text| self.qr.as_ref().is_none_or(|(drawn, _)| drawn != *text))
        {
            match ticket::qr_png(text) {
                Ok(png) => {
                    let picture = Arc::new(Image::from_bytes(ImageFormat::Png, png));
                    self.qr = Some((text.clone(), picture));
                }
                Err(error) => warn!(%error, "could not draw the ticket's QR code"),
            }
        }

        let share = match my_ticket {
            Some(text) => {
                let copy = text.clone();
                let link = format!("groovy://{text}");
                let mut row = div().flex().flex_row().items_start().gap_4();
                if let Some((_, picture)) = &self.qr {
                    row = row.child(
                        img(picture.clone())
                            .size(QR_SIZE)
                            .flex_none()
                            .rounded_md(),
                    );
                }
                row.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .child(abbreviate_id(&text)),
                        )
                        .child(
                            div().text_xs().text_color(theme.text_muted).child(
                                "Anyone with this ticket can connect to this device.",
                            ),
                        )
                        .child(
                            Button::new("ticket-copy", "Copy ticket")
                                .variant(Variant::Secondary)
                                .icon(Icon::Copy)
                                .on_click(move |_, cx| {
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                        copy.clone(),
                                    ));
                                }),
                        )
                        .child(
                            Button::new("ticket-copy-link", "Copy link")
                                .variant(Variant::Secondary)
                                .icon(Icon::Copy)
                                .on_click({
                                    let link = link.clone();
                                    move |_, cx| {
                                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                            link.clone(),
                                        ));
                                    }
                                }),
                        ),
                )
                .into_any_element()
            }
            None => div()
                .text_sm()
                .text_color(theme.text_muted)
                .child("This device's address is not known yet. Your ticket appears once the node is online.")
                .into_any_element(),
        };

        let can_connect = !self.busy && !self.input.read(cx).text().trim().is_empty();
        let status = self.status.as_ref().map(|(error, message)| {
            let color = if *error { theme.danger } else { theme.success };
            div().text_sm().text_color(color).child(message.clone())
        });

        div()
            .flex()
            .flex_col()
            .gap_4()
            .max_w(px(560.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Your ticket"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.text_muted)
                            .child("Share this so someone can connect to you."),
                    ),
            )
            .child(share)
            .child(div().h(px(1.)).w_full().bg(theme.border))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Add a peer"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.text_muted)
                            .child("Paste a ticket someone gave you, then connect."),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_2()
                    .items_center()
                    .child(div().flex_1().child(self.input.clone()))
                    .child(
                        Button::new("ticket-connect", "Pair")
                            .disabled(!can_connect)
                            .on_click(move |_, cx| view.update(cx, |panel, cx| panel.connect(cx))),
                    ),
            )
            .children(status)
    }
}
