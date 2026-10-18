//! Groovy's component kit, built on GPUI and styled by the active theme.
//!
//! Components are `RenderOnce` builders, so a view creates them in its
//! `render`. Stateful pieces (text input, toasts, context menu) are entities
//! that a view holds. Icons come from Tabler (see [`icon`]).
//!
//! The kit is consumed as views are built on top of it, so unused parts are
//! not reported as dead code. The gallery (debug builds) shows every component.

#![allow(dead_code)]

/// A click or dismiss handler. Stored in an `Rc` so one handler can serve several callers.
pub type Callback = std::rc::Rc<dyn Fn(&mut gpui::Window, &mut gpui::App)>;

pub mod avatar;
pub mod badge;
pub mod button;
pub mod context_menu;
pub mod dialog;
#[cfg(debug_assertions)]
pub mod gallery;
pub mod icon;
pub mod input;
pub mod list;
pub mod progress;
pub mod spinner;
pub mod toast;
pub mod tooltip;
