//! The Files screen: send files and folders to contacts, answer offers from them,
//! follow transfers, and open what has been received.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, Context, Entity, ExternalPaths, FontWeight, PathPromptOptions, SharedString, Window, div,
    prelude::*, px,
};
use groovy_core::shares::Share;
use groovy_core::transfers::{Transfer, TransferState};
use groovy_core::{Database, EndpointAddr, EndpointId, SharePreview, Trust};
use tracing::warn;

use crate::{
    node::{self, NodeState, abbreviate_id},
    theme::ActiveTheme,
    ui::{
        button::{Button, Variant},
        icon::Icon,
        input::TextInput,
        progress::ProgressBar,
    },
};

pub struct FilesView {
    database: Arc<Database>,
    /// Files and folders chosen or dropped, waiting for a contact to send them to.
    chosen: Vec<PathBuf>,
    status: Option<(bool, SharedString)>,
    /// The ticket of the share made most recently, to copy.
    new_ticket: Option<String>,
    /// Where a share ticket is pasted.
    ticket_input: Entity<TextInput>,
    /// A ticket that has been read, and what it offers, waiting for the user to download it.
    preview: Option<(String, SharePreview)>,
}

impl FilesView {
    pub fn new(database: Arc<Database>, cx: &mut Context<Self>) -> Self {
        let ticket_input = cx.new(|cx| TextInput::new("Paste a share ticket", cx));
        Self {
            database,
            chosen: Vec::new(),
            status: None,
            new_ticket: None,
            ticket_input,
            preview: None,
        }
    }

    /// Makes a share of the chosen files and folders. Anyone with its ticket can download
    /// them while this node is online.
    fn make_share(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = cx.try_global::<NodeState>().and_then(|node| node.handle()) else {
            self.set_status(true, "The node is not running yet.", cx);
            return;
        };
        let paths = self.chosen.clone();
        cx.spawn(async move |this, cx| {
            let outcome = handle.create_share(paths).await;
            this.update(cx, |view, cx| match outcome {
                Ok(shared) => {
                    view.new_ticket = Some(shared.ticket);
                    view.set_status(
                        false,
                        "Share made. Anyone with its ticket can download it while this device is online.",
                        cx,
                    );
                }
                Err(error) => {
                    warn!(%error, "could not make a share");
                    view.set_status(true, format!("The share could not be made: {error}"), cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Stops a share. Its content is refused to anyone who is not a contact from now on.
    fn revoke_share(&mut self, id: i64, cx: &mut Context<Self>) {
        let Some(handle) = cx.try_global::<NodeState>().and_then(|node| node.handle()) else {
            self.set_status(true, "The node is not running yet.", cx);
            return;
        };
        cx.spawn(async move |this, cx| {
            let outcome = handle.revoke_share(id).await;
            this.update(cx, |view, cx| match outcome {
                Ok(()) => view.set_status(false, "Share stopped.", cx),
                Err(error) => {
                    view.set_status(true, format!("The share could not be stopped: {error}"), cx)
                }
            })
            .ok();
        })
        .detach();
    }

    /// Reads a pasted ticket: its names and sizes, without downloading anything.
    fn read_ticket(&mut self, cx: &mut Context<Self>) {
        let ticket = self.ticket_input.read(cx).text().trim().to_owned();
        if ticket.is_empty() {
            self.set_status(true, "Paste a share ticket first.", cx);
            return;
        }
        let Some(handle) = cx.try_global::<NodeState>().and_then(|node| node.handle()) else {
            self.set_status(true, "The node is not running yet.", cx);
            return;
        };
        self.set_status(false, "Reading the share...", cx);
        cx.spawn(async move |this, cx| {
            let outcome = handle.preview_share(ticket.clone()).await;
            this.update(cx, |view, cx| match outcome {
                Ok(preview) => {
                    view.preview = Some((ticket, preview));
                    view.status = None;
                    cx.notify();
                }
                Err(error) => {
                    view.preview = None;
                    view.set_status(true, format!("That share cannot be read: {error}"), cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Downloads the share that was previewed. It then appears under Transfers.
    fn download_preview(&mut self, cx: &mut Context<Self>) {
        let Some((ticket, _)) = self.preview.clone() else {
            return;
        };
        let Some(handle) = cx.try_global::<NodeState>().and_then(|node| node.handle()) else {
            self.set_status(true, "The node is not running yet.", cx);
            return;
        };
        cx.spawn(async move |this, cx| {
            let outcome = handle.receive_share(ticket).await;
            this.update(cx, |view, cx| match outcome {
                Ok(_) => {
                    view.preview = None;
                    view.ticket_input
                        .update(cx, |input, cx| input.set_text("", cx));
                    view.set_status(false, "Downloading. It appears under Transfers.", cx);
                }
                Err(error) => {
                    view.set_status(true, format!("The download could not start: {error}"), cx)
                }
            })
            .ok();
        })
        .detach();
    }

    fn add_chosen(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        for path in paths {
            if !self.chosen.contains(path) {
                self.chosen.push(path.clone());
            }
        }
        self.status = None;
        cx.notify();
    }

    fn choose(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: true,
            prompt: Some("Choose files or folders to send".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            this.update(cx, |view, cx| view.add_chosen(&paths, cx)).ok();
        })
        .detach();
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

    /// Sends the chosen files to a contact. The offer goes out in the background, and the
    /// contact decides whether to accept it.
    fn send(&mut self, peer: EndpointId, cx: &mut Context<Self>) {
        let Some(handle) = cx.try_global::<NodeState>().and_then(|node| node.handle()) else {
            self.set_status(true, "The node is not running yet.", cx);
            return;
        };
        let name = self.label_for(peer);
        let paths = self.chosen.clone();
        self.set_status(false, format!("Offering files to {name}..."), cx);
        let sending = async move { handle.send_files(EndpointAddr::from(peer), paths).await };
        cx.spawn(async move |this, cx| {
            let outcome = sending.await;
            this.update(cx, |view, cx| match outcome {
                Ok(()) => {
                    view.chosen.clear();
                    view.set_status(false, format!("{name} accepted the files."), cx);
                }
                Err(groovy_core::NodeError::OfferDeclined) => {
                    view.set_status(true, format!("{name} declined the files."), cx);
                }
                Err(error) => {
                    warn!(%error, "could not send files");
                    view.set_status(true, format!("The files could not be sent: {error}"), cx);
                }
            })
            .ok();
        })
        .detach();
    }

    fn label_for(&self, peer: EndpointId) -> String {
        match self.database.contact(peer) {
            Ok(Some(contact)) => contact.label().to_owned(),
            _ => abbreviate_id(&peer.to_string()),
        }
    }

    fn trusted_contacts(&self) -> Vec<(EndpointId, String)> {
        match self.database.contacts() {
            Ok(contacts) => contacts
                .into_iter()
                .filter(|contact| contact.trust == Trust::Trusted)
                .map(|contact| (contact.endpoint_id, contact.label().to_owned()))
                .collect(),
            Err(error) => {
                warn!(%error, "could not read the contacts");
                Vec::new()
            }
        }
    }
}

/// What a transfer control does to a transfer.
#[derive(Clone, Copy)]
enum Control {
    Pause,
    Resume,
    Cancel,
}

/// Runs a transfer control in the background. Failures are logged, since the transfer
/// list shows the state that results.
fn control(cx: &mut App, id: i64, action: Control) {
    let Some(handle) = cx.try_global::<NodeState>().and_then(|node| node.handle()) else {
        return;
    };
    cx.spawn(async move |_| {
        let outcome = match action {
            Control::Pause => handle.pause_transfer(id).await,
            Control::Resume => handle.resume_transfer(id).await,
            Control::Cancel => handle.cancel_transfer(id).await,
        };
        if let Err(error) = outcome {
            warn!(%error, "could not change the transfer");
        }
    })
    .detach();
}

/// Bytes as a short size, such as `12.4 MB`.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// How long something will take, such as `2m 5s`.
pub fn format_eta(remaining: Duration) -> String {
    let seconds = remaining.as_secs();
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m {}s", seconds / 60, seconds % 60),
        _ => format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60),
    }
}

fn state_label(state: &TransferState) -> SharedString {
    match state {
        TransferState::Queued => "Queued".into(),
        TransferState::Active => "Receiving".into(),
        TransferState::Paused => "Paused".into(),
        TransferState::Completed => "Received".into(),
        TransferState::Failed(error) => format!("Failed: {error}").into(),
        TransferState::Cancelled => "Cancelled".into(),
    }
}

fn describe(transfer: &Transfer) -> String {
    let names = transfer.offer.names.join(", ");
    format!(
        "{names} ({} files, {})",
        transfer.offer.files,
        format_bytes(transfer.offer.bytes)
    )
}

impl Render for FilesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let view = cx.entity().clone();
        let contacts = self.trusted_contacts();
        let has_chosen = !self.chosen.is_empty();

        // Offers from contacts, answered with accept or decline.
        let offers: Vec<_> = cx
            .try_global::<NodeState>()
            .map(|node| node.offers().to_vec())
            .unwrap_or_default();
        let offer_rows = offers.into_iter().map(|request| {
            let peer = request.peer.endpoint_id;
            let sender = if request.peer.profile.display_name.is_empty() {
                abbreviate_id(&peer.to_string())
            } else {
                request.peer.profile.display_name.clone()
            };
            let offer = request.offer;
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_3()
                .py_2()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w_0()
                        .child(div().text_sm().child(format!("{sender} is offering files")))
                        .child(div().text_xs().text_color(theme.text_muted).child(format!(
                            "{} ({} files, {})",
                            offer.names.join(", "),
                            offer.files,
                            format_bytes(offer.bytes)
                        ))),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .child(
                            Button::new(format!("offer-decline-{peer}"), "Decline")
                                .variant(Variant::Secondary)
                                .on_click(move |_, cx| node::answer_offer(cx, peer, false)),
                        )
                        .child(
                            Button::new(format!("offer-accept-{peer}"), "Accept")
                                .on_click(move |_, cx| node::answer_offer(cx, peer, true)),
                        ),
                )
        });

        // Transfers, with progress, speed and the controls each state allows.
        let node = cx.try_global::<NodeState>();
        let transfers: Vec<_> = node
            .map(|node| node.transfers().cloned().collect())
            .unwrap_or_default();
        let speeds: Vec<(i64, Option<f64>)> = transfers
            .iter()
            .map(|transfer| (transfer.id, node.and_then(|node| node.speed(transfer.id))))
            .collect();
        let transfer_rows = transfers
            .iter()
            .filter(|transfer| transfer.state != TransferState::Completed)
            .map(|transfer| {
                let id = transfer.id;
                let fraction = if transfer.offer.bytes == 0 {
                    1.0
                } else {
                    transfer.done as f32 / transfer.offer.bytes as f32
                };
                let speed = speeds
                    .iter()
                    .find(|(speed_id, _)| *speed_id == id)
                    .and_then(|(_, speed)| *speed);
                let detail = match (&transfer.state, speed) {
                    (TransferState::Active, Some(bytes_per_second)) if bytes_per_second > 0. => {
                        let remaining = transfer.offer.bytes.saturating_sub(transfer.done);
                        let eta = Duration::from_secs_f64(remaining as f64 / bytes_per_second);
                        format!(
                            "{}/s, {} left",
                            format_bytes(bytes_per_second as u64),
                            format_eta(eta)
                        )
                    }
                    _ => format!(
                        "{} of {}",
                        format_bytes(transfer.done),
                        format_bytes(transfer.offer.bytes)
                    ),
                };

                let mut controls = div().flex().flex_row().gap_2();
                if matches!(
                    transfer.state,
                    TransferState::Queued | TransferState::Active
                ) {
                    controls = controls.child(
                        Button::new(format!("transfer-pause-{id}"), "Pause")
                            .variant(Variant::Secondary)
                            .on_click(move |_, cx| control(cx, id, Control::Pause)),
                    );
                }
                if matches!(
                    transfer.state,
                    TransferState::Paused | TransferState::Failed(_)
                ) {
                    controls = controls.child(
                        Button::new(format!("transfer-resume-{id}"), "Resume")
                            .variant(Variant::Secondary)
                            .on_click(move |_, cx| control(cx, id, Control::Resume)),
                    );
                }
                if matches!(
                    transfer.state,
                    TransferState::Queued | TransferState::Active | TransferState::Paused
                ) {
                    controls = controls.child(
                        Button::new(format!("transfer-cancel-{id}"), "Cancel")
                            .variant(Variant::Danger)
                            .on_click(move |_, cx| control(cx, id, Control::Cancel)),
                    );
                }

                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .py_2()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_between()
                            .gap_3()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .min_w_0()
                                    .child(div().text_sm().child(describe(transfer)))
                                    .child(div().text_xs().text_color(theme.text_muted).child(
                                        format!(
                                            "From {} · {}",
                                            abbreviate_id(&transfer.peer.to_string()),
                                            state_label(&transfer.state)
                                        ),
                                    )),
                            )
                            .child(controls),
                    )
                    .child(ProgressBar::new(fraction))
                    .child(div().text_xs().text_color(theme.text_muted).child(detail))
            });

        // Received files: each one can be opened, or shown in its folder.
        let received = node.map(|node| {
            node.transfers()
                .filter(|transfer| transfer.state == TransferState::Completed)
                .flat_map(|transfer| node.saved_files(transfer.id).to_vec())
                .map(|path| {
                    let open_path = path.clone();
                    let reveal_path = path.clone();
                    let key = path.display().to_string();
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        .py_1()
                        .child(
                            div()
                                .text_sm()
                                .min_w_0()
                                .overflow_hidden()
                                .child(key.clone()),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .gap_2()
                                .child(
                                    Button::new(format!("received-open-{key}"), "Open")
                                        .variant(Variant::Secondary)
                                        .on_click(move |_, cx| cx.open_with_system(&open_path)),
                                )
                                .child(
                                    Button::new(format!("received-reveal-{key}"), "Show in folder")
                                        .variant(Variant::Secondary)
                                        .on_click(move |_, cx| cx.reveal_path(&reveal_path)),
                                ),
                        )
                })
                .collect::<Vec<_>>()
        });

        let drop_hint = if has_chosen {
            "Drop more files here, or choose a contact below to send them to."
        } else {
            "Drop files or folders here to send them."
        };
        let chosen_rows = self
            .chosen
            .iter()
            .map(|path| div().text_sm().child(path.display().to_string()));
        let contact_rows = contacts.into_iter().map(|(peer, label)| {
            let send_view = view.clone();
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_3()
                .py_1()
                .child(div().text_sm().child(label))
                .child(
                    Button::new(format!("send-to-{peer}"), "Send")
                        .disabled(!has_chosen)
                        .on_click(move |_, cx| {
                            send_view.update(cx, |view, cx| view.send(peer, cx));
                        }),
                )
        });
        let status = self.status.as_ref().map(|(error, message)| {
            let color = if *error { theme.danger } else { theme.success };
            div().text_sm().text_color(color).child(message.clone())
        });

        let choose_view = view.clone();
        let clear_view = view.clone();
        let make_view = view.clone();
        let read_view = view.clone();
        let download_view = view.clone();

        // Shares, as the database has them, with a stop button for each active one.
        let share_rows = self
            .database
            .shares()
            .unwrap_or_default()
            .into_iter()
            .rev()
            .map(|share| share_row(share, view.clone(), theme.text_muted));

        let preview_rows = self.preview.as_ref().map(|(_, preview)| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .py_2()
                .child(div().text_sm().child(preview.names.join(", ")))
                .child(div().text_xs().text_color(theme.text_muted).child(format!(
                    "{} files, {}, from {}",
                    preview.files,
                    format_bytes(preview.bytes),
                    abbreviate_id(&preview.from.to_string())
                )))
                .child(
                    Button::new("share-download", "Download").on_click(move |_, cx| {
                        download_view.update(cx, |view, cx| view.download_preview(cx));
                    }),
                )
        });
        div()
            .flex()
            .flex_col()
            .gap_5()
            .max_w(px(640.))
            .child(
                div()
                    .id("files-drop")
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_4()
                    .border_1()
                    .border_color(theme.border)
                    .rounded_md()
                    .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                        this.add_chosen(paths.paths(), cx);
                    }))
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.text_muted)
                            .child(drop_hint),
                    )
                    .children(chosen_rows)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(
                                Button::new("files-choose", "Choose files or folders")
                                    .variant(Variant::Secondary)
                                    .icon(Icon::Folder)
                                    .on_click(move |_, cx| {
                                        choose_view.update(cx, |view, cx| view.choose(cx));
                                    }),
                            )
                            .when(has_chosen, |row| {
                                row.child(
                                    Button::new("files-clear", "Clear")
                                        .variant(Variant::Ghost)
                                        .on_click(move |_, cx| {
                                            clear_view.update(cx, |view, cx| {
                                                view.chosen.clear();
                                                cx.notify();
                                            });
                                        }),
                                )
                            }),
                    ),
            )
            .children(status)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(section_title("Share a link", theme.text))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.text_muted)
                            .child(
                                "A link lets anyone who has it download these files while this device is online. Stop the share to take them back.",
                            ),
                    )
                    .child(
                        Button::new("share-make", "Make a share link")
                            .variant(Variant::Secondary)
                            .icon(Icon::Upload)
                            .disabled(!has_chosen)
                            .on_click(move |_, cx| {
                                make_view.update(cx, |view, cx| view.make_share(cx));
                            }),
                    )
                    .when_some(self.new_ticket.clone(), |column, ticket| {
                        let copy = ticket.clone();
                        column
                            .child(div().text_xs().child(ticket))
                            .child(
                                Button::new("share-copy", "Copy ticket")
                                    .variant(Variant::Secondary)
                                    .icon(Icon::Copy)
                                    .on_click(move |_, cx| {
                                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                            copy.clone(),
                                        ));
                                    }),
                            )
                    }),
            )
            .child(section("Your shares", theme.text_muted, share_rows))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(section_title("Receive a share", theme.text))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .items_center()
                            .child(div().flex_1().child(self.ticket_input.clone()))
                            .child(
                                Button::new("share-preview", "Preview")
                                    .variant(Variant::Secondary)
                                    .on_click(move |_, cx| {
                                        read_view.update(cx, |view, cx| view.read_ticket(cx));
                                    }),
                            ),
                    )
                    .children(preview_rows),
            )
            .child(section("Send to", theme.text_muted, contact_rows))
            .child(section("Offers", theme.text_muted, offer_rows))
            .child(section("Transfers", theme.text_muted, transfer_rows))
            .child(section(
                "Received",
                theme.text_muted,
                received.unwrap_or_default(),
            ))
    }
}

/// A heading for a block of controls.
fn section_title(title: &'static str, color: gpui::Hsla) -> impl IntoElement {
    div()
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(color)
        .child(title)
}

/// One of your shares: its names, and whether it is still being served.
fn share_row(share: Share, view: Entity<FilesView>, muted: gpui::Hsla) -> impl IntoElement {
    let id = share.id;
    let revoke_view = view;
    let active = share.is_active();
    let status = if active { "Active" } else { "Stopped" };
    div()
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
                .child(div().text_sm().child(share.names.join(", ")))
                .child(div().text_xs().text_color(muted).child(format!(
                    "{} · {} files, {}",
                    status,
                    share.files,
                    format_bytes(share.bytes)
                ))),
        )
        .when(active, |row| {
            row.child(
                Button::new(format!("share-stop-{id}"), "Stop sharing")
                    .variant(Variant::Danger)
                    .on_click(move |_, cx| {
                        revoke_view.update(cx, |view, cx| view.revoke_share(id, cx));
                    }),
            )
        })
}

/// A titled list of rows. An empty list says so.
fn section<E: IntoElement>(
    title: &'static str,
    muted: gpui::Hsla,
    rows: impl IntoIterator<Item = E>,
) -> impl IntoElement {
    let rows: Vec<_> = rows
        .into_iter()
        .map(IntoElement::into_any_element)
        .collect();
    let empty = rows.is_empty();
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
        .children(rows)
        .when(empty, |d| {
            d.child(div().text_xs().text_color(muted).child("Nothing here yet."))
        })
}

#[cfg(test)]
mod tests {
    use super::{format_bytes, format_eta};
    use std::time::Duration;

    #[test]
    fn sizes_are_shown_in_the_largest_fitting_unit() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024 * 1024), "5.0 GB");
    }

    #[test]
    fn times_are_shown_in_seconds_minutes_or_hours() {
        assert_eq!(format_eta(Duration::from_secs(9)), "9s");
        assert_eq!(format_eta(Duration::from_secs(125)), "2m 5s");
        assert_eq!(format_eta(Duration::from_secs(3725)), "1h 2m");
    }
}
