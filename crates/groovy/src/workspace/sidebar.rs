//! The navigation column on the left: one entry per screen.

use std::rc::Rc;

use gpui::{App, Window, div, prelude::*, px};

use super::screen::Screen;
use crate::{
    theme::ActiveTheme,
    ui::{icon::icon, tooltip},
};

const WIDTH: gpui::Pixels = px(200.);

type SelectHandler = Rc<dyn Fn(Screen, &mut Window, &mut App)>;

#[derive(IntoElement)]
pub struct Sidebar {
    current: Screen,
    on_select: SelectHandler,
}

impl Sidebar {
    pub fn new(
        current: Screen,
        on_select: impl Fn(Screen, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            current,
            on_select: Rc::new(on_select),
        }
    }
}

impl RenderOnce for Sidebar {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        // Each item takes keyboard focus, from a handle kept for it across frames.
        let focus_handles: Vec<gpui::FocusHandle> = (0..Screen::ALL.len())
            .map(|index| {
                window
                    .use_keyed_state(("nav-focus", index), cx, |_, cx| {
                        cx.focus_handle().tab_stop(true)
                    })
                    .read(cx)
                    .clone()
            })
            .collect();
        let theme = cx.theme();
        let focus_ring = theme.focus_ring;
        let items = Screen::ALL.into_iter().enumerate().map(|(index, screen)| {
            let selected = screen == self.current;
            let on_select = self.on_select.clone();
            let color = if selected {
                theme.text
            } else {
                theme.text_muted
            };
            let focus = focus_handles[index].clone();
            let on_key = self.on_select.clone();
            div()
                .id(("nav", index))
                .role(gpui::Role::Button)
                .aria_label(screen.label())
                .track_focus(&focus)
                .tab_stop(true)
                .border_1()
                .border_color(gpui::transparent_black())
                .focus_visible(|style| style.border_color(focus_ring))
                .on_key_down(move |event, window, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                        on_key(screen, window, cx);
                        cx.stop_propagation();
                    }
                })
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .h(px(32.))
                .px_2()
                .rounded_md()
                .text_sm()
                .text_color(color)
                .cursor_pointer()
                .when(selected, |d| {
                    d.bg(theme.ghost_element_active).text_color(theme.text)
                })
                .hover(|style| style.bg(theme.ghost_element_hover))
                .tooltip(tooltip::text(format!(
                    "{} ({})",
                    screen.label(),
                    screen.shortcut_label()
                )))
                .on_click(move |_, window, cx| on_select(screen, window, cx))
                .child(icon(screen.icon(), px(16.), color))
                .child(screen.label())
        });

        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(WIDTH)
            .h_full()
            .gap_1()
            .p_2()
            .border_r_1()
            .border_color(theme.border)
            .bg(theme.editor_background)
            .children(items)
    }
}
