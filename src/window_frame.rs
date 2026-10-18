//! Client-side window decorations for Wayland and X11: an invisible resize
//! border, rounded corners and a drop shadow. Passes the content through
//! untouched when the platform draws the frame itself (Windows).

use gpui::{prelude::*, *};

use crate::theme::ActiveTheme;

/// Width of the transparent margin that holds the shadow and resize handles.
const SHADOW_SIZE: Pixels = px(10.);
const CORNER_RADIUS: Pixels = px(10.);
const BORDER_SIZE: Pixels = px(1.);

pub fn client_side_decorations(
    content: impl IntoElement,
    window: &mut Window,
    cx: &mut App,
) -> Stateful<Div> {
    let decorations = window.window_decorations();
    window.set_client_inset(match decorations {
        Decorations::Client { .. } => SHADOW_SIZE,
        Decorations::Server => px(0.),
    });

    let backdrop = div().id("window-backdrop").size_full();
    let Decorations::Client { tiling } = decorations else {
        return backdrop.child(content);
    };

    let border = cx.theme().border;

    backdrop
        .bg(transparent_black())
        .child(
            canvas(
                |_, window, _| {
                    window.insert_hitbox(
                        Bounds::new(point(px(0.), px(0.)), window.viewport_size()),
                        HitboxBehavior::Normal,
                    )
                },
                move |_, hitbox, window, _| {
                    let Some(edge) =
                        resize_edge(window.mouse_position(), window.viewport_size(), tiling)
                    else {
                        return;
                    };
                    window.set_cursor_style(cursor_for(edge), &hitbox);
                },
            )
            .size_full()
            .absolute(),
        )
        .when(!(tiling.top || tiling.right), |d| {
            d.rounded_tr(CORNER_RADIUS)
        })
        .when(!(tiling.top || tiling.left), |d| {
            d.rounded_tl(CORNER_RADIUS)
        })
        .when(!(tiling.bottom || tiling.right), |d| {
            d.rounded_br(CORNER_RADIUS)
        })
        .when(!(tiling.bottom || tiling.left), |d| {
            d.rounded_bl(CORNER_RADIUS)
        })
        .when(!tiling.top, |d| d.pt(SHADOW_SIZE))
        .when(!tiling.bottom, |d| d.pb(SHADOW_SIZE))
        .when(!tiling.left, |d| d.pl(SHADOW_SIZE))
        .when(!tiling.right, |d| d.pr(SHADOW_SIZE))
        // Re-run the cursor logic above as the pointer moves.
        .on_mouse_move(|_, window, _| window.refresh())
        .on_mouse_down(MouseButton::Left, move |event, window, _| {
            if let Some(edge) = resize_edge(event.position, window.viewport_size(), tiling) {
                window.start_window_resize(edge);
            }
        })
        .child(
            div()
                .size_full()
                .cursor(CursorStyle::Arrow)
                .overflow_hidden()
                .when(!(tiling.top || tiling.right), |d| {
                    d.rounded_tr(CORNER_RADIUS)
                })
                .when(!(tiling.top || tiling.left), |d| {
                    d.rounded_tl(CORNER_RADIUS)
                })
                .when(!(tiling.bottom || tiling.right), |d| {
                    d.rounded_br(CORNER_RADIUS)
                })
                .when(!(tiling.bottom || tiling.left), |d| {
                    d.rounded_bl(CORNER_RADIUS)
                })
                .when(!tiling.top, |d| d.border_t(BORDER_SIZE))
                .when(!tiling.bottom, |d| d.border_b(BORDER_SIZE))
                .when(!tiling.left, |d| d.border_l(BORDER_SIZE))
                .when(!tiling.right, |d| d.border_r(BORDER_SIZE))
                .border_color(border)
                .when(!tiling.is_tiled(), |d| {
                    d.shadow(vec![BoxShadow {
                        color: hsla(0., 0., 0., 0.4),
                        blur_radius: SHADOW_SIZE / 2.,
                        spread_radius: px(0.),
                        offset: point(px(0.), px(0.)),
                    }])
                })
                // Don't let the resize logic above see moves inside the frame.
                .on_mouse_move(|_, _, cx| cx.stop_propagation())
                .child(content),
        )
}

/// Windows 11 outlines every window in the system accent color; paint that
/// outline with the theme's border instead. Windows 10 ignores this.
#[cfg(target_os = "windows")]
pub fn set_native_border_color(window: &Window, cx: &App) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::{
        Foundation::{COLORREF, HWND},
        Graphics::Dwm::{DWMWA_BORDER_COLOR, DwmSetWindowAttribute},
    };

    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };

    let color = Rgba::from(cx.theme().border);
    let to_byte = |channel: f32| (channel * 255.).round() as u32;
    // COLORREF is 0x00BBGGRR.
    let color = COLORREF(to_byte(color.r) | to_byte(color.g) << 8 | to_byte(color.b) << 16);

    unsafe {
        let _ = DwmSetWindowAttribute(
            HWND(handle.hwnd.get() as _),
            DWMWA_BORDER_COLOR,
            &color as *const _ as _,
            std::mem::size_of::<COLORREF>() as u32,
        );
    }
}

#[cfg(not(target_os = "windows"))]
pub fn set_native_border_color(_window: &Window, _cx: &App) {}

fn cursor_for(edge: ResizeEdge) -> CursorStyle {
    match edge {
        ResizeEdge::Top | ResizeEdge::Bottom => CursorStyle::ResizeUpDown,
        ResizeEdge::Left | ResizeEdge::Right => CursorStyle::ResizeLeftRight,
        ResizeEdge::TopLeft | ResizeEdge::BottomRight => CursorStyle::ResizeUpLeftDownRight,
        ResizeEdge::TopRight | ResizeEdge::BottomLeft => CursorStyle::ResizeUpRightDownLeft,
    }
}

/// Which edge or corner a point in the shadow margin grabs, if any.
fn resize_edge(pos: Point<Pixels>, size: Size<Pixels>, tiling: Tiling) -> Option<ResizeEdge> {
    let margin = SHADOW_SIZE * 1.5;
    if Bounds::new(Point::default(), size)
        .inset(margin)
        .contains(&pos)
    {
        return None;
    }

    let near_left = pos.x < margin && !tiling.left;
    let near_right = pos.x > size.width - margin && !tiling.right;
    let near_top = pos.y < margin && !tiling.top;
    let near_bottom = pos.y > size.height - margin && !tiling.bottom;

    match (near_top, near_bottom, near_left, near_right) {
        (true, _, true, _) => Some(ResizeEdge::TopLeft),
        (true, _, _, true) => Some(ResizeEdge::TopRight),
        (_, true, true, _) => Some(ResizeEdge::BottomLeft),
        (_, true, _, true) => Some(ResizeEdge::BottomRight),
        (true, ..) => Some(ResizeEdge::Top),
        (_, true, ..) => Some(ResizeEdge::Bottom),
        (_, _, true, _) => Some(ResizeEdge::Left),
        (_, _, _, true) => Some(ResizeEdge::Right),
        _ => None,
    }
}
