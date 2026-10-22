//! Tells the user when a newer release of Groovy is out. A check runs at startup, unless the
//! user has turned checks off, and the workspace shows a banner for what it finds.

use gpui::{App, AppContext, Global};
use groovy_core::updates::{self, Update};

/// What the check found, and whether the user has closed the banner for it.
#[derive(Default)]
pub struct UpdateState {
    pub available: Option<Update>,
    pub dismissed: bool,
}

impl Global for UpdateState {}

/// Checks for an update in the background, if the user has not turned checks off. A run with the
/// data directory overridden is a test or second copy, so it does not check.
pub fn start(cx: &mut App) {
    if std::env::var_os(groovy_core::override_env_var(crate::APP_NAME)).is_some() {
        return;
    }
    if !cx
        .global::<crate::data::AppData>()
        .config
        .background
        .check_updates
    {
        return;
    }
    cx.set_global(UpdateState::default());
    cx.spawn(async move |cx| {
        let outcome = cx
            .background_spawn(async move { updates::check_blocking(updates::CURRENT) })
            .await;
        cx.update(|cx| match outcome {
            Ok(Some(update)) => {
                tracing::info!(version = %update.version, "a newer release is out");
                cx.global_mut::<UpdateState>().available = Some(update);
                cx.refresh_windows();
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(%error, "could not check for a newer release"),
        });
    })
    .detach();
}

/// The release the banner is about, if the user has not closed it.
pub fn banner(cx: &App) -> Option<Update> {
    let state = cx.try_global::<UpdateState>()?;
    if state.dismissed {
        return None;
    }
    state.available.clone()
}
