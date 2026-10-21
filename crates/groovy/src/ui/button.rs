//! Buttons, with primary, secondary, danger and ghost styles, optional icons,
//! and an icon-only form.

use std::rc::Rc;

use super::Callback;
use gpui::{prelude::*, *};

use super::icon::{Icon, icon};
use super::tooltip;
use crate::theme::ActiveTheme;

const HEIGHT: Pixels = px(28.);
const ICON_SIZE: Pixels = px(14.);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Variant {
    #[default]
    Primary,
    Secondary,
    Danger,
    Ghost,
}

#[derive(IntoElement)]
pub struct Button {
    id: ElementId,
    label: Option<SharedString>,
    icon: Option<Icon>,
    variant: Variant,
    disabled: bool,
    tooltip: Option<SharedString>,
    focus_handle: Option<FocusHandle>,
    on_activate: Option<Callback>,
}

impl Button {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: Some(label.into()),
            icon: None,
            variant: Variant::default(),
            disabled: false,
            tooltip: None,
            focus_handle: None,
            on_activate: None,
        }
    }

    /// A square button showing only an icon. Give it a tooltip so its purpose is clear.
    pub fn icon_only(id: impl Into<ElementId>, icon: Icon) -> Self {
        Self {
            label: None,
            icon: Some(icon),
            ..Self::new(id, "")
        }
    }

    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }

    pub fn variant(mut self, variant: Variant) -> Self {
        self.variant = variant;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn tooltip(mut self, text: impl Into<SharedString>) -> Self {
        self.tooltip = Some(text.into());
        self
    }

    /// Makes the button reachable with Tab and shows a focus ring. Pass a handle
    /// owned by the view, so focus survives re-renders.
    pub fn focus_handle(mut self, handle: &FocusHandle) -> Self {
        self.focus_handle = Some(handle.clone());
        self
    }

    /// Runs when the button is clicked, or when it is focused and Enter or Space is pressed.
    pub fn on_click(mut self, listener: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_activate = Some(Rc::new(listener));
        self
    }
}

impl RenderOnce for Button {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let (background, hover, foreground, border) = match self.variant {
            Variant::Primary => (
                theme.accent,
                theme.accent_hover,
                theme.on_accent,
                theme.accent,
            ),
            Variant::Danger => (
                theme.danger,
                theme.danger_hover,
                theme.on_accent,
                theme.danger,
            ),
            Variant::Secondary => (
                theme.surface,
                theme.ghost_element_hover,
                theme.text,
                theme.border,
            ),
            Variant::Ghost => (
                transparent_black(),
                theme.ghost_element_hover,
                theme.text,
                transparent_black(),
            ),
        };
        let active = match self.variant {
            Variant::Primary => theme.accent_hover,
            Variant::Danger => theme.danger_hover,
            _ => theme.ghost_element_active,
        };
        let focus_ring = theme.focus_ring;
        let disabled_text = theme.text_disabled;

        // Every button can take keyboard focus: a caller's handle if it gave one, otherwise one
        // kept for this button's id, so Tab reaches it without the caller doing anything.
        let focus_handle = match self.focus_handle.clone() {
            Some(handle) => handle,
            None => window
                .use_keyed_state(self.id.clone(), cx, |_, cx| cx.focus_handle())
                .read(cx)
                .clone(),
        };
        // Screen readers announce the label, or the tooltip for a button with only an icon.
        let accessible_name = self.label.clone().or_else(|| self.tooltip.clone());

        let icon_only = self.label.is_none();
        let mut button = div()
            .id(self.id.clone())
            .role(gpui::Role::Button)
            .aria_label(accessible_name.unwrap_or_default())
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .gap_1p5()
            .h(HEIGHT)
            .min_w(HEIGHT)
            .px(if icon_only { px(0.) } else { px(12.) })
            .w(if icon_only { HEIGHT } else { px(0.) })
            .when(!icon_only, |d| d.w_auto())
            .rounded_md()
            .border_1()
            .border_color(border)
            .bg(background)
            .text_sm()
            .text_color(foreground);

        if let Some(icon_kind) = self.icon {
            button = button.child(icon(icon_kind, ICON_SIZE, foreground));
        }
        if let Some(label) = self.label {
            button = button.child(label);
        }

        if let Some(text) = self.tooltip {
            button = button.tooltip(tooltip::text(text));
        }

        button = button
            .track_focus(&focus_handle)
            .tab_stop(!self.disabled)
            .focus_visible(|style| style.border_color(focus_ring));

        if self.disabled {
            return button
                .text_color(disabled_text)
                .opacity(0.6)
                .cursor_not_allowed()
                .into_any_element();
        }

        let on_activate = self.on_activate;
        let mut button = button
            .cursor_pointer()
            .hover(move |style| style.bg(hover))
            .active(move |style| style.bg(active));
        if let Some(on_activate) = on_activate {
            let on_mouse = on_activate.clone();
            button = button
                .on_click(move |_, window, cx| on_mouse(window, cx))
                .on_key_down(move |event, window, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                        on_activate(window, cx);
                        cx.stop_propagation();
                    }
                });
        }
        button.into_any_element()
    }
}
