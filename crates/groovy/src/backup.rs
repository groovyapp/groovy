//! Backing up and restoring the identity from the UI.
//!
//! Both views do their file and crypto work on the background executor. The
//! restore form is used during onboarding and in Settings.

use std::{
    fs,
    path::{Path, PathBuf},
};

use gpui::{
    App, Context, Entity, EventEmitter, FontWeight, PathPromptOptions, SharedString, Subscription,
    Window, div, prelude::*, px,
};
use groovy_core::{DataDir, Identity, SecretKey, backup};
use tracing::warn;

use crate::{
    APP_NAME,
    data::AppData,
    theme::ActiveTheme,
    ui::{
        button::{Button, Variant},
        input::{TextInput, TextInputEvent},
    },
};

/// Backups are small: the key plus encryption overhead. Larger files are not backups.
const MAX_BACKUP_BYTES: u64 = 1024 * 1024;
const DEFAULT_FILE_NAME: &str = "groovy-identity.age";

/// Restores a backup into the identity in [`AppData`]. The node must restart to use it.
pub fn restore_from_file(path: &Path, passphrase: &str, dir: &DataDir) -> Result<Identity, String> {
    let size = fs::metadata(path).map_err(|error| error.to_string())?.len();
    if size > MAX_BACKUP_BYTES {
        return Err("That file is too large to be a Groovy backup.".to_owned());
    }
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    let secret = backup::import(&bytes, passphrase).map_err(|error| error.to_string())?;
    Identity::restore(dir, APP_NAME, secret).map_err(|error| error.to_string())
}

/// Writes an encrypted backup of `secret` to `path`.
fn write_backup(secret: &SecretKey, passphrase: &str, path: &Path) -> Result<(), String> {
    let bytes = backup::export(secret, passphrase).map_err(|error| error.to_string())?;
    fs::write(path, bytes).map_err(|error| error.to_string())
}

/// The folder the save dialog starts in.
fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// The result of a restore, so the parent can move on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreEvent {
    Restored,
}

/// Chooses a backup file, takes its passphrase, and restores it.
pub struct RestoreForm {
    passphrase: Entity<TextInput>,
    file: Option<PathBuf>,
    status: Option<(bool, SharedString)>,
    busy: bool,
    _subscription: Subscription,
}

impl EventEmitter<RestoreEvent> for RestoreForm {}

impl RestoreForm {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let passphrase = cx.new(|cx| TextInput::new("Backup passphrase", cx).masked());
        let _subscription = cx.subscribe(&passphrase, |_, _, event: &TextInputEvent, cx| {
            if *event == TextInputEvent::Changed {
                cx.notify();
            }
        });
        Self {
            passphrase,
            file: None,
            status: None,
            busy: false,
            _subscription,
        }
    }

    fn can_restore(&self, cx: &App) -> bool {
        !self.busy && self.file.is_some() && !self.passphrase.read(cx).text().is_empty()
    }

    fn choose_file(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose a backup".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            this.update(cx, |form, cx| {
                form.file = Some(path);
                form.status = None;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn restore(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.file.clone() else {
            return;
        };
        let passphrase = self.passphrase.read(cx).text().to_owned();
        let dir = cx.global::<AppData>().dir.clone();
        self.busy = true;
        self.status = None;

        let restored =
            cx.background_spawn(async move { restore_from_file(&path, &passphrase, &dir) });
        cx.spawn(async move |this, cx| {
            let result = restored.await;
            this.update(cx, |form, cx| form.finish(result, cx)).ok();
        })
        .detach();
        cx.notify();
    }

    fn finish(&mut self, result: Result<Identity, String>, cx: &mut Context<Self>) {
        self.busy = false;
        match result {
            Ok(identity) => {
                let public_key = identity.public_key();
                cx.global_mut::<AppData>().identity = identity;
                self.passphrase
                    .update(cx, |input, cx| input.set_text("", cx));
                self.status = Some((
                    false,
                    format!(
                        "Identity restored: {}",
                        crate::node::abbreviate_id(&public_key.to_string())
                    )
                    .into(),
                ));
                cx.emit(RestoreEvent::Restored);
            }
            Err(message) => {
                warn!(%message, "could not restore the identity");
                self.status = Some((true, message.into()));
            }
        }
        cx.notify();
    }
}

impl Render for RestoreForm {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let file_name = self
            .file
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "No file chosen".to_owned());
        let can_restore = self.can_restore(cx);
        let view = cx.entity().clone();
        let status = self.status.as_ref().map(|(error, message)| {
            let color = if *error { theme.danger } else { theme.success };
            div().text_sm().text_color(color).child(message.clone())
        });

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .child(
                        Button::new("restore-choose-file", "Choose backup file…")
                            .variant(Variant::Secondary)
                            .disabled(self.busy)
                            .on_click({
                                let view = view.clone();
                                move |_, cx| view.update(cx, |form, cx| form.choose_file(cx))
                            }),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.text_muted)
                            .child(file_name),
                    ),
            )
            .child(self.passphrase.clone())
            .child(
                div().flex().flex_row().child(
                    Button::new("restore-identity", "Restore")
                        .disabled(!can_restore)
                        .on_click(move |_, cx| view.update(cx, |form, cx| form.restore(cx))),
                ),
            )
            .children(status)
    }
}

/// The identity section of Settings: back up the identity, and restore one.
pub struct BackupSection {
    restore: Entity<RestoreForm>,
    passphrase: Entity<TextInput>,
    confirm: Entity<TextInput>,
    status: Option<(bool, SharedString)>,
    _subscriptions: Vec<Subscription>,
}

impl BackupSection {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let restore = cx.new(RestoreForm::new);
        let passphrase = cx.new(|cx| TextInput::new("New passphrase", cx).masked());
        let confirm = cx.new(|cx| TextInput::new("Repeat passphrase", cx).masked());
        let _subscriptions = vec![cx.subscribe(&restore, |this, _, _: &RestoreEvent, cx| {
            this.status = Some((false, "Identity restored. Restart Groovy to use it.".into()));
            cx.notify();
        })];
        Self {
            restore,
            passphrase,
            confirm,
            status: None,
            _subscriptions,
        }
    }

    fn save_backup(&mut self, cx: &mut Context<Self>) {
        let passphrase = self.passphrase.read(cx).text().to_owned();
        let confirm = self.confirm.read(cx).text().to_owned();
        if passphrase != confirm {
            self.status = Some((true, "The passphrases do not match.".into()));
            cx.notify();
            return;
        }
        if let Err(error) = backup::validate_passphrase(&passphrase) {
            self.status = Some((true, error.to_string().into()));
            cx.notify();
            return;
        }

        let secret = cx.global::<AppData>().identity.secret_key().clone();
        let picked = cx.prompt_for_new_path(&home_dir(), Some(DEFAULT_FILE_NAME));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = picked.await else {
                return;
            };
            let written = cx
                .background_spawn(async move { write_backup(&secret, &passphrase, &path) })
                .await;
            this.update(cx, |section, cx| {
                match written {
                    Ok(()) => {
                        section.status = Some((false, "Backup saved.".into()));
                        section
                            .passphrase
                            .update(cx, |input, cx| input.set_text("", cx));
                        section
                            .confirm
                            .update(cx, |input, cx| input.set_text("", cx));
                    }
                    Err(message) => {
                        warn!(%message, "could not save the backup");
                        section.status = Some((true, message.into()));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

impl Render for BackupSection {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let view = cx.entity().clone();
        let can_save = {
            let passphrase = self.passphrase.read(cx).text();
            let confirm = self.confirm.read(cx).text();
            !passphrase.is_empty() && !confirm.is_empty()
        };
        let status = self.status.as_ref().map(|(error, message)| {
            let color = if *error { theme.danger } else { theme.success };
            div().text_sm().text_color(color).child(message.clone())
        });

        div()
            .flex()
            .flex_col()
            .gap_4()
            .max_w(px(480.))
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Identity"),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Back up"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.text_muted)
                            .child("A backup is an encrypted file. Anyone with the file and its passphrase can act as this device, so keep it somewhere safe and don't share it. The passphrase needs at least 12 characters."),
                    )
                    .child(self.passphrase.clone())
                    .child(self.confirm.clone())
                    .child(
                        div().flex().flex_row().child(
                            Button::new("backup-save", "Save backup…")
                                .disabled(!can_save)
                                .on_click(move |_, cx| {
                                    view.update(cx, |section, cx| section.save_backup(cx))
                                }),
                        ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Restore"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.text_muted)
                            .child("Restoring replaces this device's identity. Peers will know you by the restored ID, and you must restart Groovy for it to take effect."),
                    )
                    .child(self.restore.clone()),
            )
            .children(status)
    }
}
