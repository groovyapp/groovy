//! Modal dialogs: a centered panel over a dimmed backdrop, and a confirmation
//! dialog built from it.

use std::rc::Rc;

use super::Callback;
use gpui::{AnyElement, App, ElementId, Pixels, SharedString, Window, prelude::*, *};

use super::button::{Button, Variant};
use crate::theme::ActiveTheme;

const WIDTH: Pixels = px(420.);

/// A panel over a dimmed backdrop. Clicking the backdrop calls `on_dismiss`.
#[derive(IntoElement)]
pub struct Modal {
    id: ElementId,
    title: SharedString,
    body: Option<AnyElement>,
    footer: Vec<AnyElement>,
    on_dismiss: Option<Callback>,
}

impl Modal {
    pub fn new(id: impl Into<ElementId>, title: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            body: None,
            footer: Vec::new(),
            on_dismiss: None,
        }
    }

    pub fn body(mut self, body: impl IntoElement) -> Self {
        self.body = Some(body.into_any_element());
        self
    }

    /// Adds a button or other element to the bottom of the dialog, in order.
    pub fn action(mut self, action: impl IntoElement) -> Self {
        self.footer.push(action.into_any_element());
        self
    }

    pub fn on_dismiss(mut self, listener: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_dismiss = Some(Rc::new(listener));
        self
    }
}

impl RenderOnce for Modal {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let on_dismiss = self.on_dismiss;

        let mut panel = div()
            .id(self.id.clone())
            .w(WIDTH)
            .max_w_full()
            .p_4()
            .gap_3()
            .flex()
            .flex_col()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .text_color(theme.text)
            .shadow_lg()
            // Clicks inside the panel must not reach the backdrop.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(self.title),
            );

        if let Some(body) = self.body {
            panel = panel.child(div().text_sm().text_color(theme.text_muted).child(body));
        }
        if !self.footer.is_empty() {
            panel = panel.child(
                div()
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap_2()
                    .children(self.footer),
            );
        }

        let mut backdrop = div()
            .id(ElementId::Name(format!("{}-backdrop", self.id).into()))
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme.backdrop)
            .occlude()
            .child(panel);

        if let Some(on_dismiss) = on_dismiss {
            backdrop = backdrop.on_mouse_down(MouseButton::Left, move |_, window, cx| {
                on_dismiss(window, cx)
            });
        }
        backdrop
    }
}

/// A modal that asks the user to confirm an action, with Cancel and Confirm buttons.
#[derive(IntoElement)]
pub struct ConfirmDialog {
    id: ElementId,
    title: SharedString,
    message: SharedString,
    confirm_label: SharedString,
    destructive: bool,
    on_confirm: Option<Callback>,
    on_cancel: Option<Callback>,
}

impl ConfirmDialog {
    pub fn new(
        id: impl Into<ElementId>,
        title: impl Into<SharedString>,
        message: impl Into<SharedString>,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            message: message.into(),
            confirm_label: "Confirm".into(),
            destructive: false,
            on_confirm: None,
            on_cancel: None,
        }
    }

    pub fn confirm_label(mut self, label: impl Into<SharedString>) -> Self {
        self.confirm_label = label.into();
        self
    }

    /// Shows the confirm button in the danger style, for actions that cannot be undone.
    pub fn destructive(mut self, destructive: bool) -> Self {
        self.destructive = destructive;
        self
    }

    pub fn on_confirm(mut self, listener: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_confirm = Some(Rc::new(listener));
        self
    }

    pub fn on_cancel(mut self, listener: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_cancel = Some(Rc::new(listener));
        self
    }
}

impl RenderOnce for ConfirmDialog {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let id = self.id.clone();
        let on_confirm = self.on_confirm;
        let on_cancel = self.on_cancel;
        let dismiss = on_cancel.clone();

        let cancel = {
            let on_cancel = on_cancel.clone();
            Button::new(ElementId::Name(format!("{id}-cancel").into()), "Cancel")
                .variant(Variant::Secondary)
                .on_click(move |window, cx| {
                    if let Some(on_cancel) = &on_cancel {
                        on_cancel(window, cx);
                    }
                })
        };
        let confirm = Button::new(
            ElementId::Name(format!("{id}-confirm").into()),
            self.confirm_label,
        )
        .variant(if self.destructive {
            Variant::Danger
        } else {
            Variant::Primary
        })
        .on_click(move |window, cx| {
            if let Some(on_confirm) = &on_confirm {
                on_confirm(window, cx);
            }
        });

        let mut modal = Modal::new(id, self.title)
            .body(self.message)
            .action(cancel)
            .action(confirm);
        if let Some(on_cancel) = dismiss {
            modal = modal.on_dismiss(move |window, cx| on_cancel(window, cx));
        }
        modal
    }
}
