//! A menu opened at the pointer, such as on right-click. It closes when the
//! user picks an item or clicks anywhere else.
//!
//! A view holds an `Entity<ContextMenu>` while the menu is open, subscribes to
//! [`ContextMenuEvent::Dismissed`] to drop it, and renders it as a child.

use std::rc::Rc;

use super::Callback;
use gpui::{
    Anchor, Context, EventEmitter, Pixels, Point, SharedString, Window, anchored, deferred, div,
    prelude::*, px,
};

use super::icon::{Icon, icon};
use crate::theme::ActiveTheme;

const WIDTH: Pixels = px(200.);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextMenuEvent {
    Dismissed,
}

pub struct MenuItem {
    label: SharedString,
    icon: Option<Icon>,
    danger: bool,
    action: Callback,
}

impl MenuItem {
    pub fn new(
        label: impl Into<SharedString>,
        action: impl Fn(&mut Window, &mut gpui::App) + 'static,
    ) -> Self {
        Self {
            label: label.into(),
            icon: None,
            danger: false,
            action: Rc::new(action),
        }
    }

    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }

    /// Shows the item in the danger color, for destructive actions.
    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }
}

pub struct ContextMenu {
    position: Point<Pixels>,
    items: Vec<MenuItem>,
}

impl EventEmitter<ContextMenuEvent> for ContextMenu {}

impl ContextMenu {
    /// A menu at window coordinates `position` (for example, the pointer's position).
    pub fn new(position: Point<Pixels>, items: Vec<MenuItem>) -> Self {
        Self { position, items }
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        cx.emit(ContextMenuEvent::Dismissed);
    }
}

impl Render for ContextMenu {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let entity = cx.entity().clone();

        let rows: Vec<_> = self
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let color = if item.danger {
                    theme.danger
                } else {
                    theme.text
                };
                let action = item.action.clone();
                let entity = entity.clone();
                let mut row = div()
                    .id(("context-menu-item", index))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .h(px(28.))
                    .rounded_sm()
                    .text_sm()
                    .text_color(color)
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.ghost_element_hover));
                if let Some(glyph) = item.icon {
                    row = row.child(icon(glyph, px(14.), color));
                }
                row.child(item.label.clone())
                    .on_click(move |_, window, cx| {
                        action(window, cx);
                        entity.update(cx, |menu, cx| menu.dismiss(cx));
                    })
            })
            .collect();

        deferred(
            anchored()
                .position(self.position)
                .anchor(Anchor::TopLeft)
                .snap_to_window_with_margin(px(8.))
                .child(
                    div()
                        .id("context-menu")
                        .w(WIDTH)
                        .p_1()
                        .flex()
                        .flex_col()
                        .rounded_md()
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.surface)
                        .shadow_md()
                        .occlude()
                        .on_mouse_down_out(cx.listener(|menu, _, _, cx| menu.dismiss(cx)))
                        .children(rows),
                ),
        )
        .with_priority(1)
    }
}
