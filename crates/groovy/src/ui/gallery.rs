//! A window that shows every component in the kit, so each can be checked in
//! light and dark themes. Debug builds only: open it with Ctrl+Shift+G.

use gpui::{
    App, Context, Entity, KeyBinding, MouseButton, MouseDownEvent, Subscription, TitlebarOptions,
    Window, WindowOptions, actions, div, prelude::*, px,
};

use super::avatar::Avatar;
use super::badge::Badge;
use super::button::{Button, Variant};
use super::context_menu::{ContextMenu, ContextMenuEvent, MenuItem};
use super::dialog::ConfirmDialog;
use super::icon::{Icon, icon};
use super::input::TextInput;
use super::list::list;
use super::progress::ProgressBar;
use super::spinner::Spinner;
use super::toast::{ToastKind, Toasts};
use crate::theme::{self, ActiveTheme, Mode};

actions!(gallery, [Open]);

/// Registers the shortcut that opens the gallery.
pub fn init(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("ctrl-shift-g", Open, None)]);
    cx.on_action(|_: &Open, cx: &mut App| open(cx));
}

fn open(cx: &mut App) {
    let options = WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some("Component gallery".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    if let Err(error) = cx.open_window(options, |window, cx| cx.new(|cx| Gallery::new(window, cx)))
    {
        tracing::warn!(%error, "failed to open the component gallery");
    }
}

struct Gallery {
    input: Entity<TextInput>,
    toasts: Entity<Toasts>,
    confirm_open: bool,
    menu: Option<(Entity<ContextMenu>, Subscription)>,
}

impl Gallery {
    fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            input: cx.new(|cx| TextInput::new("Type something", cx)),
            toasts: cx.new(|_| Toasts::default()),
            confirm_open: false,
            menu: None,
        }
    }

    fn open_menu(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        let toasts = self.toasts.clone();
        let items = vec![
            MenuItem::new("Copy", |_, _| {}).icon(Icon::Copy),
            MenuItem::new("Show a toast", move |_, cx| {
                toasts.update(cx, |toasts, cx| {
                    toasts.push(ToastKind::Success, "Copied from the context menu", cx)
                });
            })
            .icon(Icon::Bell),
            MenuItem::new("Delete", |_, _| {})
                .icon(Icon::Trash)
                .danger(),
        ];
        let menu = cx.new(|_| ContextMenu::new(event.position, items));
        let subscription = cx.subscribe(&menu, |this, _, _: &ContextMenuEvent, cx| {
            this.menu = None;
            cx.notify();
        });
        self.menu = Some((menu, subscription));
        cx.notify();
    }
}

fn section(title: &'static str, body: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .child(
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(title),
        )
        .child(body)
}

impl Render for Gallery {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let entity = cx.entity().clone();

        let theme_buttons = div()
            .flex()
            .flex_row()
            .gap_2()
            .child(
                Button::new("gallery-theme-system", "Follow system")
                    .variant(Variant::Secondary)
                    .on_click(|window, cx| {
                        theme::set_mode_override(None, cx);
                        window.refresh();
                    }),
            )
            .child(
                Button::new("gallery-theme-light", "Light")
                    .variant(Variant::Secondary)
                    .on_click(|window, cx| {
                        theme::set_mode_override(Some(Mode::Light), cx);
                        window.refresh();
                    }),
            )
            .child(
                Button::new("gallery-theme-dark", "Dark")
                    .variant(Variant::Secondary)
                    .on_click(|window, cx| {
                        theme::set_mode_override(Some(Mode::Dark), cx);
                        window.refresh();
                    }),
            );

        let buttons = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_2()
            .items_center()
            .child(Button::new("b-primary", "Primary"))
            .child(Button::new("b-secondary", "Secondary").variant(Variant::Secondary))
            .child(Button::new("b-danger", "Danger").variant(Variant::Danger))
            .child(Button::new("b-ghost", "Ghost").variant(Variant::Ghost))
            .child(Button::new("b-icon", "With icon").icon(Icon::Send))
            .child(Button::new("b-disabled", "Disabled").disabled(true))
            .child(
                Button::icon_only("b-icon-only", Icon::Settings)
                    .variant(Variant::Secondary)
                    .tooltip("Settings"),
            );

        let badges = div()
            .flex()
            .flex_row()
            .gap_2()
            .items_center()
            .child(Badge::new(3))
            .child(Badge::new(42))
            .child(Badge::new(120))
            .child(Avatar::new("Ada Lovelace"))
            .child(Avatar::new("Grace Hopper").size(px(24.)))
            .child(Avatar::new("linus"));

        let progress = div()
            .flex()
            .flex_row()
            .gap_4()
            .items_center()
            .child(div().w(px(240.)).child(ProgressBar::new(0.35)))
            .child(Spinner::new("gallery-spinner"));

        let icons = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_3()
            .children(Icon::ALL.iter().map(|glyph| {
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_1()
                    .w(px(64.))
                    .text_xs()
                    .child(icon(*glyph, px(20.), theme.text))
                    .child(format!("{glyph:?}"))
            }));

        let rows = list("gallery-list", 200, |index, _, cx| {
            div()
                .flex()
                .items_center()
                .h(px(28.))
                .px_2()
                .text_sm()
                .border_b_1()
                .border_color(cx.theme().border)
                .child(format!("Row {index}"))
                .into_any_element()
        })
        .h(px(160.))
        .border_1()
        .border_color(theme.border)
        .rounded_md();

        let dialogs = Button::new("open-confirm", "Delete item…")
            .variant(Variant::Danger)
            .on_click({
                let entity = entity.clone();
                move |_, cx| {
                    entity.update(cx, |gallery, cx| {
                        gallery.confirm_open = true;
                        cx.notify();
                    });
                }
            });

        let toast_buttons = div().flex().flex_row().gap_2().children(
            [
                ("Info", ToastKind::Info),
                ("Success", ToastKind::Success),
                ("Warning", ToastKind::Warning),
                ("Danger", ToastKind::Danger),
            ]
            .into_iter()
            .enumerate()
            .map(|(index, (label, kind))| {
                let toasts = self.toasts.clone();
                Button::new(("toast", index), label)
                    .variant(Variant::Secondary)
                    .on_click(move |_, cx| {
                        toasts.update(cx, |toasts, cx| {
                            toasts.push(kind, format!("{label} toast"), cx)
                        });
                    })
            }),
        );

        let context_area = div()
            .id("context-area")
            .flex()
            .items_center()
            .justify_center()
            .h(px(64.))
            .w_full()
            .rounded_md()
            .border_1()
            .border_dashed()
            .border_color(theme.border)
            .text_sm()
            .text_color(theme.text_muted)
            .child("Right-click here")
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, _, cx| this.open_menu(event, cx)),
            );

        let confirm = self.confirm_open.then(|| {
            let close = {
                let entity = entity.clone();
                move |_window: &mut Window, cx: &mut App| {
                    entity.update(cx, |gallery, cx| {
                        gallery.confirm_open = false;
                        cx.notify();
                    });
                }
            };
            let confirmed = {
                let toasts = self.toasts.clone();
                let close = close.clone();
                move |window: &mut Window, cx: &mut App| {
                    toasts.update(cx, |toasts, cx| {
                        toasts.push(ToastKind::Danger, "Item deleted", cx)
                    });
                    close(window, cx);
                }
            };
            ConfirmDialog::new(
                "gallery-confirm",
                "Delete this item?",
                "This cannot be undone.",
            )
            .confirm_label("Delete")
            .destructive(true)
            .on_cancel(close)
            .on_confirm(confirmed)
        });

        let menu = self.menu.as_ref().map(|(menu, _)| menu.clone());

        div()
            .id("gallery")
            .relative()
            .size_full()
            .overflow_y_scroll()
            .bg(theme.editor_background)
            .text_color(theme.text)
            .p_6()
            .flex()
            .flex_col()
            .gap_6()
            .child(section("Theme", theme_buttons))
            .child(section("Buttons", buttons))
            .child(section(
                "Text input",
                div().w(px(320.)).child(self.input.clone()),
            ))
            .child(section("Badges and avatars", badges))
            .child(section("Progress and spinner", progress))
            .child(section("Icons", icons))
            .child(section("List (virtualised)", rows))
            .child(section("Dialogs", dialogs))
            .child(section("Toasts", toast_buttons))
            .child(section("Context menu", context_area))
            .child(section(
                "Tooltip",
                Button::new("tooltip-demo", "Hover me").tooltip("A tooltip"),
            ))
            .children(confirm)
            .children(menu)
            .child(self.toasts.clone())
    }
}
