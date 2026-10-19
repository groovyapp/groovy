//! A single-line text input with a caret, a placeholder, and IME support.
//!
//! Multiline editing (the chat composer) is not part of this component yet.

use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    ElementInputHandler, EntityInputHandler, EventEmitter, FocusHandle, KeyBinding, TextRun,
    UTF16Selection, actions, canvas, div, prelude::*, *,
};

use crate::theme::ActiveTheme;

actions!(
    text_input,
    [Backspace, Delete, Left, Right, Home, End, Enter, Paste]
);

const CONTEXT: &str = "TextInput";

/// Key bindings for [`TextInput`]. Call once at startup.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some(CONTEXT)),
        KeyBinding::new("delete", Delete, Some(CONTEXT)),
        KeyBinding::new("left", Left, Some(CONTEXT)),
        KeyBinding::new("right", Right, Some(CONTEXT)),
        KeyBinding::new("home", Home, Some(CONTEXT)),
        KeyBinding::new("end", End, Some(CONTEXT)),
        KeyBinding::new("enter", Enter, Some(CONTEXT)),
        // `secondary` is Ctrl on Linux and Windows, and Cmd on macOS.
        KeyBinding::new("secondary-v", Paste, Some(CONTEXT)),
    ]);
}

/// Emitted by a [`TextInput`] when its text changes or Enter is pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextInputEvent {
    Changed,
    Submitted,
}

pub struct TextInput {
    focus_handle: FocusHandle,
    content: String,
    /// Byte offset of the caret. Always on a character boundary.
    cursor: usize,
    placeholder: SharedString,
    /// Shows each character as a dot, for passphrases.
    masked: bool,
    /// The field's width, measured while painting. Used to keep the caret in view.
    view_width: Rc<Cell<Pixels>>,
}

impl EventEmitter<TextInputEvent> for TextInput {}

impl TextInput {
    pub fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: String::new(),
            cursor: 0,
            placeholder: placeholder.into(),
            masked: false,
            view_width: Rc::new(Cell::new(px(0.))),
        }
    }

    /// Hides the text behind dots, for passphrases.
    pub fn masked(mut self) -> Self {
        self.masked = true;
        self
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    /// Replaces the text and puts the caret at the end. Emits `Changed`.
    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.content = text.into();
        self.cursor = self.content.len();
        cx.emit(TextInputEvent::Changed);
        cx.notify();
    }

    fn replace_range(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) {
        self.content.replace_range(range.clone(), text);
        self.cursor = range.start + text.len();
        cx.emit(TextInputEvent::Changed);
        cx.notify();
    }

    fn previous_boundary(&self, index: usize) -> usize {
        self.content[..index]
            .char_indices()
            .next_back()
            .map_or(0, |(offset, _)| offset)
    }

    fn next_boundary(&self, index: usize) -> usize {
        self.content[index..]
            .chars()
            .next()
            .map_or(index, |ch| index + ch.len_utf8())
    }

    fn on_backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        if self.cursor > 0 {
            let start = self.previous_boundary(self.cursor);
            self.replace_range(start..self.cursor, "", cx);
        }
    }

    fn on_delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        if self.cursor < self.content.len() {
            let end = self.next_boundary(self.cursor);
            self.replace_range(self.cursor..end, "", cx);
        }
    }

    fn on_left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        self.cursor = self.previous_boundary(self.cursor);
        cx.notify();
    }

    fn on_right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        self.cursor = self.next_boundary(self.cursor);
        cx.notify();
    }

    fn on_home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.cursor = 0;
        cx.notify();
    }

    fn on_end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.cursor = self.content.len();
        cx.notify();
    }

    /// Inserts the clipboard text at the caret. The field is one line, so line
    /// breaks are removed and surrounding whitespace is trimmed.
    fn on_paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let line: String = text
            .trim()
            .chars()
            .filter(|ch| !matches!(ch, '\r' | '\n'))
            .collect();
        if !line.is_empty() {
            self.replace_range(self.cursor..self.cursor, &line, cx);
        }
    }

    fn on_enter(&mut self, _: &Enter, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(TextInputEvent::Submitted);
    }
}

/// Converts a UTF-16 offset, as used by the platform text input, to a byte offset.
fn utf16_to_byte(text: &str, offset: usize) -> usize {
    let mut units = 0;
    for (byte, ch) in text.char_indices() {
        if units >= offset {
            return byte;
        }
        units += ch.len_utf16();
    }
    text.len()
}

fn byte_to_utf16(text: &str, byte: usize) -> usize {
    text[..byte].encode_utf16().count()
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let start = utf16_to_byte(&self.content, range.start);
        let end = utf16_to_byte(&self.content, range.end);
        *adjusted_range = Some(range);
        Some(self.content[start..end].to_owned())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let caret = byte_to_utf16(&self.content, self.cursor);
        Some(UTF16Selection {
            range: caret..caret,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        None
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = match range {
            Some(range) => {
                utf16_to_byte(&self.content, range.start)..utf16_to_byte(&self.content, range.end)
            }
            None => self.cursor..self.cursor,
        };
        self.replace_range(range, text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Marked (composing) text is not shown separately yet, so it is inserted as final text.
        self.replace_text_in_range(range, new_text, window, cx);
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        // Click-to-position is not implemented yet, so the caret stays where it is.
        None
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(element_bounds)
    }
}

/// The field's text size, which is `text_sm` in the theme's scale.
fn text_size() -> Rems {
    rems(0.875)
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let focused = self.focus_handle.is_focused(window);
        let text_style = window.text_style();
        // The caret is measured at the same size the text is drawn at. Measuring
        // at the window's default size made the caret drift further from the
        // text with every character.
        let font_size = text_size().to_pixels(window.rem_size());

        let empty = self.content.is_empty();
        let shown = if self.masked {
            "•".repeat(self.content.chars().count())
        } else {
            self.content.clone()
        };
        let display: SharedString = if empty {
            self.placeholder.clone()
        } else {
            shown.into()
        };
        // The caret's byte offset in what is displayed, which differs when masked.
        let caret = if self.masked {
            self.content[..self.cursor].chars().count() * '•'.len_utf8()
        } else {
            self.cursor
        };
        let color = if empty { theme.text_muted } else { theme.text };
        let view_width = self.view_width.clone();

        // Where the caret sits: shape the same text the user sees and ask for the x position.
        let caret_x = if empty {
            px(0.)
        } else {
            let run = TextRun {
                len: display.len(),
                font: text_style.font(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            window
                .text_system()
                .shape_line(display.clone(), font_size, &[run], None)
                .x_for_index(caret)
        };

        let entity = cx.entity();
        let focus = self.focus_handle.clone();
        let border = if focused {
            theme.focus_ring
        } else {
            theme.border
        };
        let caret = theme.text;

        // While focused, text longer than the field scrolls so the caret stays at
        // the right edge. Unfocused, it shows from the start.
        const PADDING: Pixels = px(8.);
        let inner_width = (view_width.get() - PADDING * 2.).max(px(0.));
        let scroll = if focused {
            (caret_x - inner_width + px(2.)).max(px(0.))
        } else {
            px(0.)
        };

        div()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_backspace))
            .on_action(cx.listener(Self::on_delete))
            .on_action(cx.listener(Self::on_left))
            .on_action(cx.listener(Self::on_right))
            .on_action(cx.listener(Self::on_home))
            .on_action(cx.listener(Self::on_end))
            .on_action(cx.listener(Self::on_enter))
            .on_action(cx.listener(Self::on_paste))
            .relative()
            .flex()
            .items_center()
            .w_full()
            .h(px(28.))
            .px_2()
            .rounded_md()
            .border_1()
            .border_color(border)
            .bg(theme.input_background)
            .text_size(text_size())
            .text_color(color)
            .overflow_hidden()
            .child(
                div()
                    .absolute()
                    .top_0()
                    .h_full()
                    .left(PADDING - scroll)
                    .flex()
                    .items_center()
                    .whitespace_nowrap()
                    .child(display),
            )
            .when(focused, |d| {
                d.child(
                    div()
                        .absolute()
                        .top(px(6.))
                        .bottom(px(6.))
                        .left(PADDING + caret_x - scroll)
                        .w(px(1.))
                        .bg(caret),
                )
            })
            .child(
                // Registers the input handler for the platform (typing, IME, paste)
                // over the field's bounds, while it is focused.
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, cx| {
                        view_width.set(bounds.size.width);
                        if focus.is_focused(window) {
                            window.handle_input(
                                &focus,
                                ElementInputHandler::new(bounds, entity.clone()),
                                cx,
                            );
                        }
                    },
                )
                .absolute()
                .size_full(),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{byte_to_utf16, utf16_to_byte};

    #[test]
    fn utf16_offsets_map_to_byte_offsets() {
        let text = "aé😀b";
        // a=1 unit, é=1 unit (2 bytes), 😀=2 units (4 bytes), b=1 unit.
        assert_eq!(utf16_to_byte(text, 0), 0);
        assert_eq!(utf16_to_byte(text, 1), 1);
        assert_eq!(utf16_to_byte(text, 2), 3);
        assert_eq!(utf16_to_byte(text, 4), 7);
        assert_eq!(utf16_to_byte(text, 5), 8);
        assert_eq!(byte_to_utf16(text, 7), 4);
        assert_eq!(byte_to_utf16(text, 8), 5);
    }
}

#[cfg(test)]
mod gpui_tests {
    use std::{cell::RefCell, rc::Rc};

    use gpui::{TestAppContext, WindowAppearance};

    use super::{TextInput, TextInputEvent, bind_keys};
    use crate::theme::{self, ModeOverride};

    fn setup(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.set_global(ModeOverride::default());
            theme::sync_with_appearance(WindowAppearance::Light, cx);
            bind_keys(cx);
        });
    }

    #[test]
    fn typing_and_backspace_edit_the_text() {
        let mut cx = TestAppContext::single();
        setup(&mut cx);
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("Name", cx));
        let focus = input.read_with(cx, |input, _| input.focus_handle().clone());
        cx.update(|window, cx| focus.focus(window, cx));

        cx.simulate_input("Ada");
        cx.simulate_keystrokes("backspace");

        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "Ad"
        );
    }

    #[test]
    fn enter_submits_and_the_text_is_kept() {
        let mut cx = TestAppContext::single();
        setup(&mut cx);
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("Name", cx));
        let submitted = Rc::new(RefCell::new(0));
        let counter = submitted.clone();
        cx.update(|_, cx| {
            cx.subscribe(&input, move |_, event: &TextInputEvent, _| {
                if *event == TextInputEvent::Submitted {
                    *counter.borrow_mut() += 1;
                }
            })
            .detach();
        });
        let focus = input.read_with(cx, |input, _| input.focus_handle().clone());
        cx.update(|window, cx| focus.focus(window, cx));

        cx.simulate_input("Grace");
        cx.simulate_keystrokes("enter");

        assert_eq!(*submitted.borrow(), 1);
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "Grace"
        );
    }

    #[test]
    fn masked_input_keeps_the_text_but_hides_it() {
        let mut cx = TestAppContext::single();
        setup(&mut cx);
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("Passphrase", cx).masked());
        let focus = input.read_with(cx, |input, _| input.focus_handle().clone());
        cx.update(|window, cx| focus.focus(window, cx));

        cx.simulate_input("secret");

        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "secret"
        );
    }

    #[test]
    fn ctrl_v_pastes_the_clipboard_at_the_caret() {
        let mut cx = TestAppContext::single();
        setup(&mut cx);
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("Ticket", cx));
        let focus = input.read_with(cx, |input, _| input.focus_handle().clone());
        cx.update(|window, cx| focus.focus(window, cx));
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
            "  endpointabc\n".to_owned(),
        ));

        cx.simulate_keystrokes("ctrl-v");

        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "endpointabc"
        );
    }
}
