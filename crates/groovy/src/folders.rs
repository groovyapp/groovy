//! The Storage screen: shared folders. Make one from a directory, invite members, join one,
//! see how big it is and who it is syncing with, and leave it.

use std::path::PathBuf;
use std::time::Duration;

use gpui::{
    Context, Entity, FontWeight, PathPromptOptions, SharedString, Window, div, prelude::*, px,
};
use groovy_core::folders::{Folder, FolderAccess, FolderStatus, NamespaceId};
use groovy_core::replicas::ReplicaHealth;
use tracing::warn;

use crate::{
    files::format_bytes,
    node::NodeState,
    theme::ActiveTheme,
    ui::{
        button::{Button, Variant},
        icon::Icon,
        input::TextInput,
    },
};

/// How often the folder list and each folder's status are read again.
const REFRESH_EVERY: Duration = Duration::from_secs(2);

struct Row {
    folder: Folder,
    status: Option<FolderStatus>,
    health: Option<ReplicaHealth>,
}

pub struct FoldersView {
    rows: Vec<Row>,
    new_name: Entity<TextInput>,
    new_path: Option<PathBuf>,
    join_ticket: Entity<TextInput>,
    join_name: Entity<TextInput>,
    join_path: Option<PathBuf>,
    /// The invitation made most recently: for which folder, what it allows, and the ticket.
    invite: Option<(NamespaceId, FolderAccess, String)>,
    /// The folder the user has asked to leave, waiting for the second click to confirm.
    leaving: Option<NamespaceId>,
    status: Option<(bool, SharedString)>,
}

impl FoldersView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let view = Self {
            rows: Vec::new(),
            new_name: cx.new(|cx| TextInput::new("Name (optional)", cx)),
            new_path: None,
            join_ticket: cx.new(|cx| TextInput::new("Paste a folder ticket", cx)),
            join_name: cx.new(|cx| TextInput::new("Name (optional)", cx)),
            join_path: None,
            invite: None,
            leaving: None,
            status: None,
        };

        // Keep the list and each folder's size and peers current.
        cx.spawn(async move |this, cx| {
            loop {
                let handle = cx.update(|cx| cx.try_global::<NodeState>().and_then(|n| n.handle()));
                if let Some(handle) = handle
                    && let Ok(folders) = handle.folders().await
                {
                    let mut rows = Vec::with_capacity(folders.len());
                    for folder in folders {
                        let status = handle.folder_status(folder.namespace).await.ok();
                        let health = handle.folder_health(folder.namespace).await.ok();
                        rows.push(Row {
                            folder,
                            status,
                            health,
                        });
                    }
                    let shown = this.update(cx, |view, cx| {
                        view.rows = rows;
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
        view
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

    fn pick_directory(&mut self, for_join: bool, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose a folder".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            this.update(cx, |view, cx| {
                if for_join {
                    view.join_path = Some(path);
                } else {
                    view.new_path = Some(path);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Makes a shared folder from the chosen directory. Its files are shared at once.
    fn create(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.new_path.clone() else {
            self.set_status(true, "Choose the folder to share first.", cx);
            return;
        };
        let Some(handle) = cx.try_global::<NodeState>().and_then(|node| node.handle()) else {
            self.set_status(true, "The node is not running yet.", cx);
            return;
        };
        let typed = self.new_name.read(cx).text().trim().to_owned();
        let name = if typed.is_empty() {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Shared folder".to_owned())
        } else {
            typed
        };
        cx.spawn(async move |this, cx| {
            let outcome = handle.create_folder(name.clone(), Some(path)).await;
            this.update(cx, |view, cx| match outcome {
                Ok(_) => {
                    view.new_path = None;
                    view.new_name.update(cx, |input, cx| input.set_text("", cx));
                    view.set_status(
                        false,
                        format!("Sharing {name}. Invite someone to give them access."),
                        cx,
                    );
                }
                Err(error) => {
                    warn!(%error, "could not make a shared folder");
                    view.set_status(true, format!("The folder could not be made: {error}"), cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Joins a folder from its ticket, and saves its files to the chosen directory.
    fn join(&mut self, cx: &mut Context<Self>) {
        let ticket = self.join_ticket.read(cx).text().trim().to_owned();
        if ticket.is_empty() {
            self.set_status(true, "Paste a folder ticket first.", cx);
            return;
        }
        let Some(path) = self.join_path.clone() else {
            self.set_status(true, "Choose where to save the folder first.", cx);
            return;
        };
        let Some(handle) = cx.try_global::<NodeState>().and_then(|node| node.handle()) else {
            self.set_status(true, "The node is not running yet.", cx);
            return;
        };
        let typed = self.join_name.read(cx).text().trim().to_owned();
        let name = if typed.is_empty() {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "Shared folder".to_owned())
        } else {
            typed
        };
        cx.spawn(async move |this, cx| {
            let outcome = handle.join_folder(ticket, Some(path)).await;
            let named = match &outcome {
                Ok(folder) => handle.rename_folder(folder.namespace, name).await,
                Err(_) => Ok(()),
            };
            this.update(cx, |view, cx| match (outcome, named) {
                (Ok(_), _) => {
                    view.join_path = None;
                    view.join_ticket
                        .update(cx, |input, cx| input.set_text("", cx));
                    view.join_name
                        .update(cx, |input, cx| input.set_text("", cx));
                    view.set_status(false, "Joined. Its files appear as they sync.", cx);
                }
                (Err(error), _) => {
                    warn!(%error, "could not join a shared folder");
                    view.set_status(true, format!("The folder could not be joined: {error}"), cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Makes a ticket for a folder, with the access it gives.
    fn invite_to(&mut self, namespace: NamespaceId, access: FolderAccess, cx: &mut Context<Self>) {
        let Some(handle) = cx.try_global::<NodeState>().and_then(|node| node.handle()) else {
            self.set_status(true, "The node is not running yet.", cx);
            return;
        };
        cx.spawn(async move |this, cx| {
            let outcome = handle.folder_ticket(namespace, access).await;
            this.update(cx, |view, cx| match outcome {
                Ok(ticket) => {
                    view.invite = Some((namespace, access, ticket));
                    view.status = None;
                    cx.notify();
                }
                Err(error) => view.set_status(
                    true,
                    format!("The invitation could not be made: {error}"),
                    cx,
                ),
            })
            .ok();
        })
        .detach();
    }

    /// Chooses whether this device keeps a full copy of a folder.
    fn set_keep_copy(&mut self, namespace: NamespaceId, keep: bool, cx: &mut Context<Self>) {
        let Some(handle) = cx.try_global::<NodeState>().and_then(|node| node.handle()) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let outcome = handle.set_keep_copy(namespace, keep).await;
            this.update(cx, |view, cx| match outcome {
                Ok(()) => view.set_status(
                    false,
                    if keep {
                        "This device keeps a full copy of the folder."
                    } else {
                        "This device no longer keeps the folder's files. Files already saved stay."
                    },
                    cx,
                ),
                Err(error) => {
                    view.set_status(true, format!("The setting was not saved: {error}"), cx)
                }
            })
            .ok();
        })
        .detach();
    }

    /// Checks this device's copy of a folder now.
    fn check_copy(&mut self, namespace: NamespaceId, cx: &mut Context<Self>) {
        let Some(handle) = cx.try_global::<NodeState>().and_then(|node| node.handle()) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let outcome = handle.verify_folder(namespace).await;
            this.update(cx, |view, cx| match outcome {
                Ok(info) if info.complete() => {
                    view.set_status(false, "Checked: this device has every file in full.", cx)
                }
                Ok(info) => view.set_status(
                    false,
                    format!(
                        "Checked: {} of {} files are here in full.",
                        info.held, info.files
                    ),
                    cx,
                ),
                Err(error) => view.set_status(true, format!("The check failed: {error}"), cx),
            })
            .ok();
        })
        .detach();
    }

    /// Leaves a folder, after the second click. Files already saved to disk are kept.
    fn leave(&mut self, namespace: NamespaceId, cx: &mut Context<Self>) {
        if self.leaving != Some(namespace) {
            self.leaving = Some(namespace);
            cx.notify();
            return;
        }
        self.leaving = None;
        let Some(handle) = cx.try_global::<NodeState>().and_then(|node| node.handle()) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let outcome = handle.leave_folder(namespace).await;
            this.update(cx, |view, cx| match outcome {
                Ok(()) => {
                    view.rows.retain(|row| row.folder.namespace != namespace);
                    view.set_status(
                        false,
                        "You left the folder. Files already saved are kept.",
                        cx,
                    );
                }
                Err(error) => {
                    view.set_status(true, format!("The folder could not be left: {error}"), cx)
                }
            })
            .ok();
        })
        .detach();
    }
}

fn short(namespace: &NamespaceId) -> String {
    namespace.to_string().chars().take(8).collect()
}

/// What to show about how well a folder is kept: how many complete copies exist, then one
/// line for each device.
fn copies_line(health: &ReplicaHealth) -> Vec<String> {
    let headline = match health.copies {
        0 => "No device has a complete copy".to_owned(),
        1 => "1 complete copy".to_owned(),
        n => format!("{n} complete copies"),
    };
    let now = groovy_core::contacts::now();
    let mut lines = vec![headline];
    for info in &health.devices {
        let who = if info.you {
            "This device".to_owned()
        } else {
            let id = info.device.to_string();
            format!("Device {}", &id[..8.min(id.len())])
        };
        let state = if !info.keeps_copy {
            "follows the file list only".to_owned()
        } else if info.complete() {
            format!("complete, checked {}", ago(now - info.verified))
        } else {
            format!(
                "{} of {} files, checked {}",
                info.held,
                info.files,
                ago(now - info.verified)
            )
        };
        lines.push(format!("{who}: {state}"));
    }
    lines
}

/// How long ago, in the largest whole unit.
fn ago(seconds: i64) -> String {
    match seconds.max(0) {
        s if s < 60 => "just now".to_owned(),
        s if s < 3600 => format!("{} min ago", s / 60),
        s if s < 86_400 => format!("{} h ago", s / 3600),
        s => format!("{} days ago", s / 86_400),
    }
}

impl Render for FoldersView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let view = cx.entity().clone();

        let rows: Vec<_> = self
            .rows
            .iter()
            .map(|row| {
                let namespace = row.folder.namespace;
                let id = short(&namespace);
                let name = if row.folder.name.is_empty() {
                    format!("Shared folder {id}")
                } else {
                    row.folder.name.clone()
                };
                let location = match &row.folder.path {
                    Some(path) => format!("Saved in {}", path.display()),
                    None => "Not saved on this device".to_owned(),
                };
                let detail = match row.status {
                    Some(status) => {
                        let peers = match status.sync_peers {
                            0 => "not syncing with anyone now".to_owned(),
                            1 => "syncing with 1 peer".to_owned(),
                            n => format!("syncing with {n} peers"),
                        };
                        format!(
                            "{} files, {} · {peers}",
                            status.files,
                            format_bytes(status.bytes)
                        )
                    }
                    None => "Reading the folder...".to_owned(),
                };

                let copies = row.health.as_ref().map(copies_line);
                let keeps = row.folder.keep_copy;
                let keep_toggle_view = view.clone();
                let check_view = view.clone();
                let edit_view = view.clone();
                let read_view = view.clone();
                let leave_view = view.clone();
                let keep_view = view.clone();
                let confirming = self.leaving == Some(namespace);
                let invite_ticket = self
                    .invite
                    .as_ref()
                    .filter(|(for_folder, _, _)| *for_folder == namespace)
                    .cloned();

                let mut actions = div().flex().flex_row().flex_wrap().gap_2();
                actions = actions
                    .child(
                        Button::new(format!("folder-invite-edit-{id}"), "Invite to edit")
                            .variant(Variant::Secondary)
                            .on_click(move |_, cx| {
                                edit_view.update(cx, |view, cx| {
                                    view.invite_to(namespace, FolderAccess::Write, cx)
                                });
                            }),
                    )
                    .child(
                        Button::new(format!("folder-invite-read-{id}"), "Invite to read")
                            .variant(Variant::Secondary)
                            .on_click(move |_, cx| {
                                read_view.update(cx, |view, cx| {
                                    view.invite_to(namespace, FolderAccess::Read, cx)
                                });
                            }),
                    );
                actions = actions
                    .child(
                        Button::new(
                            format!("folder-keep-{id}"),
                            if keeps {
                                "Don't keep a copy here"
                            } else {
                                "Keep a full copy here"
                            },
                        )
                        .variant(Variant::Secondary)
                        .on_click(move |_, cx| {
                            keep_toggle_view
                                .update(cx, |view, cx| view.set_keep_copy(namespace, !keeps, cx));
                        }),
                    )
                    .child(
                        Button::new(format!("folder-check-{id}"), "Check copy")
                            .variant(Variant::Secondary)
                            .on_click(move |_, cx| {
                                check_view.update(cx, |view, cx| view.check_copy(namespace, cx));
                            }),
                    );
                if let Some(path) = row.folder.path.clone() {
                    actions = actions.child(
                        Button::new(format!("folder-open-{id}"), "Show in folder")
                            .variant(Variant::Secondary)
                            .icon(Icon::Folder)
                            .on_click(move |_, cx| cx.reveal_path(&path)),
                    );
                }
                actions = if confirming {
                    actions
                        .child(
                            Button::new(format!("folder-leave-confirm-{id}"), "Leave for good")
                                .variant(Variant::Danger)
                                .on_click(move |_, cx| {
                                    leave_view.update(cx, |view, cx| view.leave(namespace, cx));
                                }),
                        )
                        .child(
                            Button::new(format!("folder-leave-keep-{id}"), "Stay")
                                .variant(Variant::Ghost)
                                .on_click(move |_, cx| {
                                    keep_view.update(cx, |view, cx| {
                                        view.leaving = None;
                                        cx.notify();
                                    });
                                }),
                        )
                } else {
                    actions.child(
                        Button::new(format!("folder-leave-{id}"), "Leave")
                            .variant(Variant::Ghost)
                            .on_click(move |_, cx| {
                                leave_view.update(cx, |view, cx| view.leave(namespace, cx));
                            }),
                    )
                };

                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .py_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(name),
                    )
                    .child(div().text_xs().text_color(theme.text_muted).child(location))
                    .child(div().text_xs().text_color(theme.text_muted).child(detail))
                    .when_some(copies, |column, lines| {
                        lines.into_iter().fold(column, |column, line| {
                            column.child(div().text_xs().text_color(theme.text_muted).child(line))
                        })
                    })
                    .child(actions)
                    .when_some(invite_ticket, |column, (_, access, ticket)| {
                        let copy = ticket.clone();
                        let kind = match access {
                            FolderAccess::Write => "Anyone with this ticket can join and edit.",
                            FolderAccess::Read => "Anyone with this ticket can join and read.",
                        };
                        column
                            .child(div().text_xs().text_color(theme.text_muted).child(kind))
                            .child(div().text_xs().child(ticket))
                            .child(
                                Button::new(format!("folder-copy-{id}"), "Copy ticket")
                                    .variant(Variant::Secondary)
                                    .icon(Icon::Copy)
                                    .on_click(move |_, cx| {
                                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                            copy.clone(),
                                        ));
                                    }),
                            )
                    })
            })
            .collect();

        let create_view = view.clone();
        let create_pick = view.clone();
        let join_view = view.clone();
        let join_pick = view.clone();
        let new_path = self
            .new_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "No folder chosen".to_owned());
        let join_path = self
            .join_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "No folder chosen".to_owned());
        let status = self.status.as_ref().map(|(error, message)| {
            let color = if *error { theme.danger } else { theme.success };
            div().text_sm().text_color(color).child(message.clone())
        });

        div()
            .flex()
            .flex_col()
            .gap_5()
            .max_w(px(640.))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.text_muted)
                    .child("Shared folders keep a folder in step between the people you invite, while their devices are online. Access is set by each invitation. It cannot be changed afterwards."),
            )
            .children(status)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Your shared folders"))
                    .children(rows)
                    .when(self.rows.is_empty(), |d| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(theme.text_muted)
                                .child("You are not in any shared folder yet."),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Share a folder"))
                    .child(self.new_name.clone())
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new("folder-new-pick", "Choose folder")
                                    .variant(Variant::Secondary)
                                    .icon(Icon::Folder)
                                    .on_click(move |_, cx| {
                                        create_pick.update(cx, |view, cx| view.pick_directory(false, cx));
                                    }),
                            )
                            .child(div().text_xs().text_color(theme.text_muted).child(new_path)),
                    )
                    .child(
                        Button::new("folder-new-create", "Share this folder").on_click(move |_, cx| {
                            create_view.update(cx, |view, cx| view.create(cx));
                        }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Join a folder"))
                    .child(self.join_ticket.clone())
                    .child(self.join_name.clone())
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new("folder-join-pick", "Choose where to save")
                                    .variant(Variant::Secondary)
                                    .icon(Icon::Folder)
                                    .on_click(move |_, cx| {
                                        join_pick.update(cx, |view, cx| view.pick_directory(true, cx));
                                    }),
                            )
                            .child(div().text_xs().text_color(theme.text_muted).child(join_path)),
                    )
                    .child(
                        Button::new("folder-join", "Join").on_click(move |_, cx| {
                            join_view.update(cx, |view, cx| view.join(cx));
                        }),
                    ),
            )
    }
}
