//! A scrollable list that only builds the rows on screen.

use gpui::{AnyElement, App, ElementId, UniformList, Window, uniform_list};

/// A virtualised list of `count` rows. Every row must have the same height,
/// because the list uses that to decide which rows are visible.
pub fn list(
    id: impl Into<ElementId>,
    count: usize,
    render_row: impl Fn(usize, &mut Window, &mut App) -> AnyElement + 'static,
) -> UniformList {
    uniform_list(id, count, move |range, window, cx| {
        range
            .map(|index| render_row(index, window, cx))
            .collect::<Vec<_>>()
    })
}
