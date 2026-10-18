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
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let items = Screen::ALL.into_iter().enumerate().map(|(index, screen)| {
            let selected = screen == self.current;
            let on_select = self.on_select.clone();
            let color = if selected {
                theme.text
            } else {
                theme.text_muted
            };
            div()
                .id(("nav", index))
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
