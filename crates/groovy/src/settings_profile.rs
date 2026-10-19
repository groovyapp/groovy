//! The profile section of Settings: the display name and the picture peers see.
//!
//! Database work runs on the background executor. The view only changes once
//! the result comes back, so the UI never waits on the database or the disk.

use std::sync::Arc;

use gpui::{
    App, Context, Entity, FontWeight, Image, ImageFormat, PathPromptOptions, SharedString,
    Subscription, Window, div, prelude::*, px,
};
use groovy_core::{Database, PeerProfile, Profile, validate_display_name};
use tracing::warn;

use crate::{
    theme::ActiveTheme,
    ui::{
        avatar::Avatar,
        button::{Button, Variant},
        input::{TextInput, TextInputEvent},
    },
};

const AVATAR_SIZE: gpui::Pixels = px(64.);

pub struct ProfileEditor {
    database: Arc<Database>,
    name: Entity<TextInput>,
    avatar: Option<Arc<Image>>,
    /// The hash of the saved picture, which peers see instead of the picture.
    avatar_hash: Option<[u8; 32]>,
    /// The last result, shown under the form: whether it was an error, and the message.
    status: Option<(bool, SharedString)>,
    busy: bool,
    _subscription: Subscription,
}

impl ProfileEditor {
    pub fn new(database: Arc<Database>, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| TextInput::new("Display name", cx));
        let _subscription = cx.subscribe(&name, |_, _, event: &TextInputEvent, cx| {
            if *event == TextInputEvent::Changed {
                cx.notify();
            }
        });

        let load = {
            let database = database.clone();
            cx.background_spawn(async move { database.profile() })
        };
        cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |editor, cx| editor.apply_loaded(result, cx))
                .ok();
        })
        .detach();

        Self {
            database,
            name,
            avatar: None,
            avatar_hash: None,
            status: None,
            busy: false,
            _subscription,
        }
    }

    fn apply_loaded(
        &mut self,
        result: Result<Option<Profile>, groovy_core::DbError>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(Some(profile)) => {
                self.name
                    .update(cx, |input, cx| input.set_text(profile.display_name, cx));
                self.avatar_hash = profile.avatar_png.as_deref().map(groovy_core::avatar_hash);
                self.avatar = profile.avatar_png.map(picture);
            }
            Ok(None) => {}
            Err(error) => {
                warn!(%error, "could not load the profile");
                self.set_status(true, "Could not load your profile.", cx);
            }
        }
        cx.notify();
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

    /// Tells peers the saved name and picture. Uses the name in the field, which
    /// is what was just saved.
    fn publish(&self, cx: &App) {
        if let Some(display_name) = validate_display_name(self.name.read(cx).text()) {
            crate::node::publish_profile(
                cx,
                PeerProfile {
                    display_name,
                    avatar_hash: self.avatar_hash,
                },
            );
        }
    }

    fn save_name(&mut self, cx: &mut Context<Self>) {
        let Some(name) = validate_display_name(self.name.read(cx).text()) else {
            return;
        };
        self.busy = true;
        let database = self.database.clone();
        let save = cx.background_spawn(async move { database.save_display_name(&name) });
        cx.spawn(async move |this, cx| {
            let result = save.await;
            this.update(cx, |editor, cx| {
                editor.busy = false;
                match result {
                    Ok(()) => {
                        editor.publish(cx);
                        editor.set_status(false, "Name saved.", cx);
                    }
                    Err(error) => {
                        warn!(%error, "could not save the display name");
                        editor.set_status(true, "Could not save your name.", cx);
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    fn choose_picture(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose a picture".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            this.update(cx, |editor, cx| editor.store_picture(path, cx))
                .ok();
        })
        .detach();
    }

    /// Makes a thumbnail of the picture at `path` and stores it.
    fn store_picture(&mut self, path: std::path::PathBuf, cx: &mut Context<Self>) {
        self.busy = true;
        let database = self.database.clone();
        let stored = cx.background_spawn(async move {
            let png = groovy_core::thumbnail_from_file(&path).map_err(|error| error.to_string())?;
            match database.save_avatar(Some(&png)) {
                Ok(true) => Ok(png),
                Ok(false) => Err("Save your name before choosing a picture.".to_owned()),
                Err(error) => Err(error.to_string()),
            }
        });
        cx.spawn(async move |this, cx| {
            let result = stored.await;
            this.update(cx, |editor, cx| {
                editor.busy = false;
                match result {
                    Ok(png) => {
                        editor.avatar_hash = Some(groovy_core::avatar_hash(&png));
                        editor.avatar = Some(picture(png));
                        editor.publish(cx);
                        editor.set_status(false, "Picture saved.", cx);
                    }
                    Err(message) => {
                        warn!(%message, "could not save the picture");
                        editor.set_status(true, message, cx);
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    fn remove_picture(&mut self, cx: &mut Context<Self>) {
        self.busy = true;
        let database = self.database.clone();
        let removed = cx.background_spawn(async move { database.save_avatar(None) });
        cx.spawn(async move |this, cx| {
            let result = removed.await;
            this.update(cx, |editor, cx| {
                editor.busy = false;
                match result {
                    Ok(_) => {
                        editor.avatar_hash = None;
                        editor.avatar = None;
                        editor.publish(cx);
                        editor.set_status(false, "Picture removed.", cx);
                    }
                    Err(error) => {
                        warn!(%error, "could not remove the picture");
                        editor.set_status(true, "Could not remove the picture.", cx);
                    }
                }
            })
            .ok();
        })
        .detach();
    }
}

/// The stored PNG as an image the UI can draw.
fn picture(png: Vec<u8>) -> Arc<Image> {
    Arc::new(Image::from_bytes(ImageFormat::Png, png))
}

impl Render for ProfileEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let name_text = self.name.read(cx).text().to_owned();
        let valid = validate_display_name(&name_text).is_some();
        let has_picture = self.avatar.is_some();
        let busy = self.busy;

        let preview_name = if name_text.trim().is_empty() {
            "?".to_owned()
        } else {
            name_text
        };

        let view = cx.entity().clone();
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
                    .child("Profile"),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_4()
                    .child(
                        Avatar::new(preview_name)
                            .size(AVATAR_SIZE)
                            .image(self.avatar.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(
                                Button::new("profile-choose-picture", "Choose picture…")
                                    .variant(Variant::Secondary)
                                    .disabled(busy)
                                    .on_click({
                                        let view = view.clone();
                                        move |_, cx| {
                                            view.update(cx, |editor, cx| editor.choose_picture(cx))
                                        }
                                    }),
                            )
                            .child(
                                Button::new("profile-remove-picture", "Remove")
                                    .variant(Variant::Ghost)
                                    .disabled(busy || !has_picture)
                                    .on_click({
                                        let view = view.clone();
                                        move |_, cx| {
                                            view.update(cx, |editor, cx| editor.remove_picture(cx))
                                        }
                                    }),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.text_muted)
                            .child("Display name"),
                    )
                    .child(self.name.clone()),
            )
            .child(
                div().flex().flex_row().child(
                    Button::new("profile-save-name", "Save name")
                        .disabled(busy || !valid)
                        .on_click(move |_, cx| view.update(cx, |editor, cx| editor.save_name(cx))),
                ),
            )
            .children(status)
    }
}
