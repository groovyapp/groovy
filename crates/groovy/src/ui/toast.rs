//! Short messages in the corner of the window that dismiss themselves.
//!
//! A view holds a [`Toasts`] entity, pushes messages with [`Toasts::push`],
//! and renders it as its last child so toasts sit above the content.

use std::time::Duration;

use gpui::{Context, SharedString, Window, div, prelude::*, px};

use super::button::Button;
use super::icon::{Icon, icon};
use crate::theme::ActiveTheme;

/// How long a toast stays before it dismisses itself.
pub const DISMISS_AFTER: Duration = Duration::from_secs(5);
const MAX_VISIBLE: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Warning,
    Danger,
}

struct Toast {
    id: usize,
    kind: ToastKind,
    message: SharedString,
}

#[derive(Default)]
pub struct Toasts {
    next_id: usize,
    toasts: Vec<Toast>,
}

impl Toasts {
    /// Shows a toast. Older toasts are dropped once more than four are visible.
    pub fn push(
        &mut self,
        kind: ToastKind,
        message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_id;
        self.next_id += 1;
        self.toasts.push(Toast {
            id,
            kind,
            message: message.into(),
        });
        if self.toasts.len() > MAX_VISIBLE {
            self.toasts.remove(0);
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DISMISS_AFTER).await;
            this.update(cx, |toasts, cx| toasts.dismiss(id, cx)).ok();
        })
        .detach();
        cx.notify();
    }

    pub fn dismiss(&mut self, id: usize, cx: &mut Context<Self>) {
        self.toasts.retain(|toast| toast.id != id);
        cx.notify();
    }
}

impl Render for Toasts {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let toasts = cx.entity().clone();
        let rows: Vec<_> = self
            .toasts
            .iter()
            .map(|toast| {
                let (glyph, color) = match toast.kind {
                    ToastKind::Info => (Icon::InfoCircle, theme.accent),
                    ToastKind::Success => (Icon::CircleCheck, theme.success),
                    ToastKind::Warning => (Icon::TriangleAlert, theme.warning),
                    ToastKind::Danger => (Icon::CircleX, theme.danger),
                };
                let id = toast.id;
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .w(px(360.))
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.surface)
                    .text_color(theme.text)
                    .text_sm()
                    .shadow_md()
                    .child(icon(glyph, px(16.), color))
                    .child(div().flex_1().child(toast.message.clone()))
                    .child(
                        Button::icon_only(("toast-dismiss", id), Icon::X)
                            .variant(super::button::Variant::Ghost)
                            .tooltip("Dismiss")
                            .on_click({
                                let toasts = toasts.clone();
                                move |_window, cx| {
                                    toasts.update(cx, |this, cx| this.dismiss(id, cx));
                                }
                            }),
                    )
            })
            .collect();

        div()
            .absolute()
            .right_4()
            .bottom_4()
            .flex()
            .flex_col()
            .gap_2()
            .children(rows)
    }
}
