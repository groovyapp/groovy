//! The Peers screen: this device's ticket and pairing, then the contacts, with
//! how each is reached, and a detail view for each contact.

use std::sync::Arc;
use std::time::Duration;

use gpui::{App, Context, Entity, FontWeight, Subscription, Window, div, prelude::*, px};
use groovy_core::{Contact, Database, EndpointId, Route, Trust};
use tracing::warn;

use crate::{
    node::{NodeState, abbreviate_id},
    theme::ActiveTheme,
    tickets::TicketPanel,
    ui::{
        avatar::Avatar,
        button::{Button, Variant},
        dialog::ConfirmDialog,
        icon::Icon,
        input::{TextInput, TextInputEvent},
    },
};

/// How often the contact list reloads from the database, to pick up new contacts and trust changes.
const RELOAD_EVERY: Duration = Duration::from_secs(15);

pub struct PeersView {
    database: Arc<Database>,
    tickets: Entity<TicketPanel>,
    search: Entity<TextInput>,
    nickname: Entity<TextInput>,
    contacts: Vec<Contact>,
    selected: Option<EndpointId>,
    confirm_remove: bool,
    _subscriptions: Vec<Subscription>,
}

impl PeersView {
    pub fn new(database: Arc<Database>, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| TextInput::new("Search contacts", cx));
        let nickname = cx.new(|cx| TextInput::new("Nickname (optional)", cx));
        let _subscriptions = vec![cx.subscribe(&search, |_, _, event: &TextInputEvent, cx| {
            if *event == TextInputEvent::Changed {
                cx.notify();
            }
        })];
        let mut view = Self {
            database,
            tickets: cx.new(TicketPanel::new),
            search,
            nickname,
            contacts: Vec::new(),
            selected: None,
            confirm_remove: false,
            _subscriptions,
        };
        view.reload(cx);

        // Keep the list current with new contacts and trust changes, without a restart.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(RELOAD_EVERY).await;
                if this.update(cx, |view, cx| view.reload(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        view
    }

    /// Reads the contacts in the background, and shows them when they arrive.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let database = self.database.clone();
        let read = cx.background_spawn(async move { database.contacts() });
        cx.spawn(async move |this, cx| match read.await {
            Ok(contacts) => {
                this.update(cx, |view, cx| {
                    view.contacts = contacts;
                    cx.notify();
                })
                .ok();
            }
            Err(error) => warn!(%error, "could not read the contacts"),
        })
        .detach();
    }

    fn select(&mut self, id: EndpointId, cx: &mut Context<Self>) {
        self.selected = Some(id);
        let nickname = self
            .contacts
            .iter()
            .find(|contact| contact.endpoint_id == id)
            .and_then(|contact| contact.nickname.clone())
            .unwrap_or_default();
        self.nickname
            .update(cx, |input, cx| input.set_text(nickname, cx));
        cx.notify();
    }

    /// Runs a database write in the background, then reloads the list.
    fn write(
        &mut self,
        cx: &mut Context<Self>,
        change: impl FnOnce(&Database) -> Result<(), groovy_core::DbError> + Send + 'static,
    ) {
        let database = self.database.clone();
        let written = cx.background_spawn(async move { change(&database) });
        cx.spawn(async move |this, cx| {
            if let Err(error) = written.await {
                warn!(%error, "could not save a contact change");
            }
            this.update(cx, |view, cx| view.reload(cx)).ok();
        })
        .detach();
    }

    fn save_nickname(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected else {
            return;
        };
        let typed = self.nickname.read(cx).text().trim().to_owned();
        let nickname = (!typed.is_empty()).then_some(typed);
        self.write(cx, move |database| {
            database.set_nickname(id, nickname.as_deref()).map(|_| ())
        });
    }

    /// Blocking refuses the peer's connections from now on. Unblocking returns it to
    /// pending, so the pairing has to be done again to trust it.
    fn set_blocked(&mut self, blocked: bool, cx: &mut Context<Self>) {
        let Some(id) = self.selected else {
            return;
        };
        let trust = if blocked {
            Trust::Blocked
        } else {
            Trust::Pending
        };
        self.write(cx, move |database| {
            database.set_trust(id, trust).map(|_| ())
        });
    }

    fn remove_selected(&mut self, cx: &mut Context<Self>) {
        self.confirm_remove = false;
        let Some(id) = self.selected.take() else {
            return;
        };
        self.write(cx, move |database| database.remove_contact(id).map(|_| ()));
        cx.notify();
    }

    /// Fills in the pairing input with a link that was opened. Pairing still waits for the user.
    pub fn prefill_pairing(&mut self, text: String, cx: &mut Context<Self>) {
        self.tickets
            .update(cx, |panel, cx| panel.set_input(text, cx));
        cx.notify();
    }

    fn presence(cx: &App, id: &EndpointId) -> Option<Route> {
        cx.try_global::<NodeState>().and_then(|node| node.route(id))
    }
}

fn trust_label(trust: Trust) -> &'static str {
    match trust {
        Trust::Pending => "Pending",
        Trust::Trusted => "Trusted",
        Trust::Blocked => "Blocked",
    }
}

fn route_label(route: Option<Route>) -> (&'static str, bool) {
    match route {
        Some(Route::Direct) => ("Online, direct", true),
        Some(Route::Relayed) => ("Online, relayed", true),
        None => ("Offline", false),
    }
}

impl Render for PeersView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let view = cx.entity().clone();
        let query = self.search.read(cx).text().to_lowercase();

        let rows: Vec<_> = self
            .contacts
            .iter()
            .filter(|contact| {
                query.is_empty()
                    || contact.label().to_lowercase().contains(&query)
                    || contact.endpoint_id.to_string().contains(&query)
            })
            .enumerate()
            .map(|(index, contact)| {
                let id = contact.endpoint_id;
                let (presence_text, online) = route_label(Self::presence(cx, &id));
                let dot = if online {
                    theme.success
                } else {
                    theme.text_disabled
                };
                let selected = self.selected == Some(id);
                let view = view.clone();
                div()
                    .id(("contact", index))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected, |d| d.bg(theme.ghost_element_active))
                    .hover(|style| style.bg(theme.ghost_element_hover))
                    .on_click(move |_, _, cx| view.update(cx, |view, cx| view.select(id, cx)))
                    .child(Avatar::new(contact.label().to_owned()).size(px(28.)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(div().text_sm().child(contact.label().to_owned()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.text_muted)
                                    .child(trust_label(contact.trust)),
                            ),
                    )
                    .child(div().size(px(8.)).rounded_full().bg(dot))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.text_muted)
                            .child(presence_text),
                    )
            })
            .collect();

        let list = if self.contacts.is_empty() {
            div()
                .text_sm()
                .text_color(theme.text_muted)
                .child("No contacts yet. Pair with someone using a ticket above.")
                .into_any_element()
        } else if rows.is_empty() {
            div()
                .text_sm()
                .text_color(theme.text_muted)
                .child("No contacts match the search.")
                .into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .children(rows)
                .into_any_element()
        };

        let detail = self.selected.and_then(|id| {
            let contact = self.contacts.iter().find(|c| c.endpoint_id == id)?.clone();
            let blocked = contact.trust == Trust::Blocked;
            let endpoint_id = contact.endpoint_id.to_string();
            let copy_id = endpoint_id.clone();
            let block_view = view.clone();
            let remove_view = view.clone();
            let save_view = view.clone();
            Some(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .max_w(px(480.))
                    .child(div().h(px(1.)).w_full().bg(theme.border))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(contact.label().to_owned()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.text_muted)
                                    .child(abbreviate_id(&endpoint_id)),
                            )
                            .child(
                                Button::icon_only("contact-copy-id", Icon::Copy)
                                    .variant(Variant::Ghost)
                                    .tooltip("Copy endpoint ID")
                                    .on_click(move |_, cx| {
                                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                            copy_id.clone(),
                                        ));
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.text_muted)
                            .child(format!("Trust: {}", trust_label(contact.trust))),
                    )
                    .child(self.nickname.clone())
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(
                                Button::new("contact-save-nickname", "Save nickname")
                                    .variant(Variant::Secondary)
                                    .on_click(move |_, cx| {
                                        save_view.update(cx, |view, cx| view.save_nickname(cx))
                                    }),
                            )
                            .child(
                                Button::new(
                                    "contact-block",
                                    if blocked { "Unblock" } else { "Block" },
                                )
                                .variant(if blocked {
                                    Variant::Secondary
                                } else {
                                    Variant::Danger
                                })
                                .on_click(move |_, cx| {
                                    block_view.update(cx, |view, cx| view.set_blocked(!blocked, cx))
                                }),
                            )
                            .child(
                                Button::new("contact-remove", "Remove")
                                    .variant(Variant::Ghost)
                                    .on_click(move |_, cx| {
                                        remove_view.update(cx, |view, cx| {
                                            view.confirm_remove = true;
                                            cx.notify();
                                        })
                                    }),
                            ),
                    )
                    .into_any_element(),
            )
        });

        let remove_dialog = (self.confirm_remove && self.selected.is_some()).then(|| {
            let confirm = view.clone();
            let cancel = view.clone();
            ConfirmDialog::new(
                ("remove-contact", 0usize),
                "Remove this contact?",
                "They will be forgotten here. Their connections are not blocked, and pairing again adds them back.",
            )
            .confirm_label("Remove")
            .destructive(true)
            .on_confirm(move |_, cx| confirm.update(cx, |view, cx| view.remove_selected(cx)))
            .on_cancel(move |_, cx| {
                cancel.update(cx, |view, cx| {
                    view.confirm_remove = false;
                    cx.notify();
                })
            })
        });

        div()
            .relative()
            .flex()
            .flex_col()
            .gap_6()
            .max_w(px(560.))
            .child(self.tickets.clone())
            .child(div().h(px(1.)).w_full().bg(theme.border))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Contacts"),
                    )
                    .child(self.search.clone())
                    .child(list),
            )
            .children(detail)
            .children(remove_dialog)
    }
}
