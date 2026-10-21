//! A text input with a caret, a placeholder, and IME support. One line by default; the chat
//! composer turns on several lines, where Shift+Enter starts a new one.

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
    [
        Backspace, Delete, Left, Right, Up, Down, Home, End, Enter, Newline, Paste
    ]
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
        KeyBinding::new("shift-enter", Newline, Some(CONTEXT)),
        KeyBinding::new("up", Up, Some(CONTEXT)),
        KeyBinding::new("down", Down, Some(CONTEXT)),
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
    /// Takes several lines: Shift+Enter starts a new one, and the field grows to fit.
    multiline: bool,
    /// The field's width, measured while painting. Used to keep the caret in view.
    view_width: Rc<Cell<Pixels>>,
}

impl EventEmitter<TextInputEvent> for TextInput {}

impl TextInput {
    pub fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle().tab_stop(true),
            content: String::new(),
            cursor: 0,
            placeholder: placeholder.into(),
            masked: false,
            multiline: false,
            view_width: Rc::new(Cell::new(px(0.))),
        }
    }

    /// Hides the text behind dots, for passphrases.
    pub fn masked(mut self) -> Self {
        self.masked = true;
        self
    }

    /// Lets the text take several lines. Enter still submits; Shift+Enter starts a new line.
    pub fn multiline(mut self) -> Self {
        self.multiline = true;
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

    /// The byte offset where the line holding `index` starts.
    fn line_start(&self, index: usize) -> usize {
        self.content[..index].rfind('\n').map_or(0, |at| at + 1)
    }

    /// The byte offset where the line holding `index` ends, before its line break.
    fn line_end(&self, index: usize) -> usize {
        self.content[index..]
            .find('\n')
            .map_or(self.content.len(), |at| index + at)
    }

    /// The byte offset `column` characters into the line starting at `start`, or its end.
    fn offset_in_line(&self, start: usize, column: usize) -> usize {
        let end = self.line_end(start);
        self.content[start..end]
            .char_indices()
            .nth(column)
            .map_or(end, |(at, _)| start + at)
    }

    fn column(&self) -> usize {
        self.content[self.line_start(self.cursor)..self.cursor]
            .chars()
            .count()
    }

    fn on_newline(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        if self.multiline {
            self.replace_range(self.cursor..self.cursor, "\n", cx);
        } else {
            cx.emit(TextInputEvent::Submitted);
        }
    }

    fn on_up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        if !self.multiline {
            return;
        }
        let start = self.line_start(self.cursor);
        self.cursor = if start == 0 {
            0
        } else {
            let column = self.column();
            self.offset_in_line(self.line_start(start - 1), column)
        };
        cx.notify();
    }

    fn on_down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        if !self.multiline {
            return;
        }
        let end = self.line_end(self.cursor);
        self.cursor = if end == self.content.len() {
            end
        } else {
            let column = self.column();
            self.offset_in_line(end + 1, column)
        };
        cx.notify();
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

    /// To the start of the line, or of the text in a one-line field.
    fn on_home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.cursor = self.line_start(self.cursor);
        cx.notify();
    }

    fn on_end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.cursor = self.line_end(self.cursor);
        cx.notify();
    }

    /// Inserts the clipboard text at the caret. A one-line field drops line breaks and trims
    /// surrounding whitespace; a multi-line field keeps the breaks.
    fn on_paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let line: String = if self.multiline {
            text.replace("\r\n", "\n").replace('\r', "\n")
        } else {
            text.trim()
                .chars()
                .filter(|ch| !matches!(ch, '\r' | '\n'))
                .collect()
        };
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

/// Most lines a multi-line field shows before it scrolls.
const MAX_LINES: usize = 6;

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let focused = self.focus_handle.is_focused(window);
        let text_style = window.text_style();
        // The caret is measured at the same size the text is drawn at. Measuring
        // at the window's default size made the caret drift further from the
        // text with every character.
        let font_size = text_size().to_pixels(window.rem_size());
        // Each line has a fixed height, so the caret lines up with it at any text size.
        let line_height = (font_size * 1.43).round();
        let vertical_padding = ((px(28.) - px(20.)) / 2.) * (font_size / px(14.));

        let empty = self.content.is_empty();
        let shown = if self.masked {
            "•".repeat(self.content.chars().count())
        } else {
            self.content.clone()
        };
        let display: String = if empty {
            self.placeholder.to_string()
        } else {
            shown
        };
        // The caret's byte offset in what is displayed, which differs when masked.
        let caret = if self.masked {
            self.content[..self.cursor].chars().count() * '•'.len_utf8()
        } else {
            self.cursor
        };
        let color = if empty { theme.text_muted } else { theme.text };
        let view_width = self.view_width.clone();

        // The lines shown: one for a one-line field, every line otherwise.
        let lines: Vec<String> = if self.multiline {
            display.split('\n').map(str::to_owned).collect()
        } else {
            vec![display.clone()]
        };
        let (caret_line, caret_in_line) = if self.multiline && !empty {
            let before = &display[..caret];
            let line = before.matches('\n').count();
            let start = before.rfind('\n').map_or(0, |at| at + 1);
            (line, caret - start)
        } else {
            (0, if empty { 0 } else { caret })
        };

        // Where the caret sits: shape the line it is on and ask for the x position.
        let caret_x = if empty {
            px(0.)
        } else {
            let line: SharedString = lines[caret_line].clone().into();
            let run = TextRun {
                len: line.len(),
                font: text_style.font(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            window
                .text_system()
                .shape_line(line, font_size, &[run], None)
                .x_for_index(caret_in_line)
        };

        let entity = cx.entity();
        let focus = self.focus_handle.clone();
        let border = if focused {
            theme.focus_ring
        } else {
            theme.border
        };
        let caret_color = theme.text;

        // While focused, text longer than the field scrolls so the caret stays at
        // the right edge. Unfocused, it shows from the start.
        const PADDING: Pixels = px(8.);
        let inner_width = (view_width.get() - PADDING * 2.).max(px(0.));
        let scroll = if focused {
            (caret_x - inner_width + px(2.)).max(px(0.))
        } else {
            px(0.)
        };
        // A multi-line field grows to MAX_LINES, then scrolls to keep the caret's line in view.
        let shown_lines = lines.len().clamp(1, MAX_LINES);
        let first_line = if focused && lines.len() > MAX_LINES {
            caret_line
                .saturating_sub(MAX_LINES - 1)
                .min(lines.len() - MAX_LINES)
        } else {
            0
        };
        let height = line_height * shown_lines as f32 + vertical_padding * 2.;

        let text = div()
            .absolute()
            .top(vertical_padding - line_height * first_line as f32)
            .left(PADDING - scroll)
            .flex()
            .flex_col()
            .children(lines.into_iter().map(|line| {
                div()
                    .h(line_height)
                    .flex()
                    .items_center()
                    .whitespace_nowrap()
                    .child(line)
            }));

        div()
            .id(("text-input", entity.entity_id().as_u64()))
            .role(gpui::Role::TextInput)
            .aria_label(self.placeholder.clone())
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .tab_stop(true)
            .on_action(cx.listener(Self::on_backspace))
            .on_action(cx.listener(Self::on_delete))
            .on_action(cx.listener(Self::on_left))
            .on_action(cx.listener(Self::on_right))
            .on_action(cx.listener(Self::on_up))
            .on_action(cx.listener(Self::on_down))
            .on_action(cx.listener(Self::on_home))
            .on_action(cx.listener(Self::on_end))
            .on_action(cx.listener(Self::on_enter))
            .on_action(cx.listener(Self::on_newline))
            .on_action(cx.listener(Self::on_paste))
            .relative()
            .w_full()
            .h(height)
            .rounded_md()
            .border_1()
            .border_color(border)
            .bg(theme.input_background)
            .text_size(text_size())
            .text_color(color)
            .overflow_hidden()
            .child(text)
            .when(focused, |d| {
                d.child(
                    div()
                        .absolute()
                        .top(
                            vertical_padding
                                + line_height * (caret_line - first_line) as f32
                                + px(3.),
                        )
                        .h(line_height - px(6.))
                        .left(PADDING + caret_x - scroll)
                        .w(px(1.))
                        .bg(caret_color),
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

    use gpui::{AppContext, TestAppContext, WindowAppearance};

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

    #[test]
    fn a_multi_line_field_takes_new_lines_and_enter_still_submits() {
        let mut cx = TestAppContext::single();
        setup(&mut cx);
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("Message", cx).multiline());
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

        cx.simulate_input("first");
        cx.simulate_keystrokes("shift-enter");
        cx.simulate_input("second");
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "first\nsecond"
        );
        assert_eq!(*submitted.borrow(), 0, "a new line does not send");

        // Up keeps the column: the caret goes from after "second" to after "first".
        cx.simulate_keystrokes("up");
        cx.simulate_input("!");
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "first!\nsecond"
        );

        cx.simulate_keystrokes("enter");
        assert_eq!(*submitted.borrow(), 1);
    }

    #[test]
    fn a_multi_line_field_keeps_pasted_line_breaks() {
        let mut cx = TestAppContext::single();
        setup(&mut cx);
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("Message", cx).multiline());
        let focus = input.read_with(cx, |input, _| input.focus_handle().clone());
        cx.update(|window, cx| focus.focus(window, cx));
        cx.write_to_clipboard(gpui::ClipboardItem::new_string("one\r\ntwo".to_owned()));

        cx.simulate_keystrokes("ctrl-v");

        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "one\ntwo"
        );
    }

    gpui::actions!(input_tests, [NextField]);

    /// Two inputs and a Tab binding, as the workspace has.
    struct TwoFields {
        first: gpui::Entity<TextInput>,
        second: gpui::Entity<TextInput>,
    }

    impl gpui::Render for TwoFields {
        fn render(
            &mut self,
            _window: &mut gpui::Window,
            _cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            use gpui::{InteractiveElement, ParentElement};
            gpui::div()
                .on_action(|_: &NextField, window, cx| window.focus_next(cx))
                .child(self.first.clone())
                .child(self.second.clone())
        }
    }

    #[test]
    fn tab_moves_from_one_field_to_the_next() {
        let mut cx = TestAppContext::single();
        setup(&mut cx);
        cx.update(|cx| cx.bind_keys([gpui::KeyBinding::new("tab", NextField, None)]));
        let (view, cx) = cx.add_window_view(|_, cx| TwoFields {
            first: cx.new(|cx| TextInput::new("First", cx)),
            second: cx.new(|cx| TextInput::new("Second", cx)),
        });
        let (first, second) = view.read_with(cx, |view, cx| {
            (
                view.first.read(cx).focus_handle().clone(),
                view.second.read(cx).focus_handle().clone(),
            )
        });
        cx.update(|window, cx| first.focus(window, cx));
        cx.run_until_parked();

        cx.simulate_keystrokes("tab");

        assert!(
            cx.update(|window, _| second.is_focused(window)),
            "Tab reaches the second field"
        );
    }
}
