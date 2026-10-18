//! First-run onboarding: welcome, a display name, the endpoint ID, and a note
//! on backing up the identity. Shown until it is finished once.
//!
//! The identity is created at startup, because the node needs it to start.
//! Onboarding is gated on a completion flag in the settings table instead.

use gpui::{
    App, Context, Entity, EventEmitter, FontWeight, Subscription, Window, div, prelude::*, px,
};
use groovy_core::{Database, validate_display_name};
use tracing::warn;

use crate::{
    node::{NodeState, abbreviate_id},
    theme::ActiveTheme,
    ui::{
        button::{Button, Variant},
        icon::Icon,
        input::{TextInput, TextInputEvent},
    },
};

/// Settings-table keys written when onboarding finishes.
pub const COMPLETED_KEY: &str = "onboarding.completed";
const CONTENT_WIDTH: gpui::Pixels = px(480.);

/// Whether onboarding has been finished on this machine.
pub fn is_completed(database: &Database) -> bool {
    match database.get_setting(COMPLETED_KEY) {
        Ok(value) => value.is_some(),
        Err(error) => {
            warn!(%error, "could not read onboarding state; showing onboarding");
            false
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Welcome,
    Name,
    Identity,
    Backup,
}

impl Step {
    const ALL: [Step; 4] = [Step::Welcome, Step::Name, Step::Identity, Step::Backup];

    fn index(self) -> usize {
        Self::ALL.iter().position(|step| *step == self).unwrap_or(0)
    }

    fn next(self) -> Option<Step> {
        Self::ALL.get(self.index() + 1).copied()
    }

    fn previous(self) -> Option<Step> {
        self.index().checked_sub(1).map(|index| Self::ALL[index])
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnboardingEvent {
    Finished { display_name: String },
}

pub struct Onboarding {
    step: Step,
    /// Set when the name step is entered, so render can focus the field.
    focus_name: bool,
    name: Entity<TextInput>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<OnboardingEvent> for Onboarding {}

impl Onboarding {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| TextInput::new("Your name", cx));
        let _subscriptions = vec![cx.subscribe(&name, |this, _, event: &TextInputEvent, cx| {
            match event {
                // Redraw so Next enables as the name becomes valid.
                TextInputEvent::Changed => cx.notify(),
                // Enter advances, as the Next button does.
                TextInputEvent::Submitted => this.advance(cx),
            }
        })];
        Self {
            step: Step::Welcome,
            focus_name: false,
            name,
            _subscriptions,
        }
    }

    fn valid_name(&self, cx: &App) -> Option<String> {
        validate_display_name(self.name.read(cx).text())
    }

    fn go_to(&mut self, step: Step, cx: &mut Context<Self>) {
        self.step = step;
        self.focus_name = step == Step::Name;
        cx.notify();
    }

    fn back(&mut self, cx: &mut Context<Self>) {
        if let Some(previous) = self.step.previous() {
            self.go_to(previous, cx);
        }
    }

    fn advance(&mut self, cx: &mut Context<Self>) {
        match self.step.next() {
            Some(next) => {
                // The name step cannot be left until the name is valid.
                if self.step == Step::Name && self.valid_name(cx).is_none() {
                    return;
                }
                self.go_to(next, cx);
            }
            None => {
                if let Some(display_name) = self.valid_name(cx) {
                    cx.emit(OnboardingEvent::Finished { display_name });
                }
            }
        }
    }
}

impl Render for Onboarding {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.focus_name) {
            // Typing starts straight away on the name step.
            let focus = self.name.read(cx).focus_handle().clone();
            window.focus(&focus, cx);
        }
        let theme = cx.theme();
        let step = self.step;
        let valid = self.valid_name(cx).is_some();

        let heading = |text: &'static str| {
            div()
                .text_xl()
                .font_weight(FontWeight::SEMIBOLD)
                .child(text)
        };
        let paragraph =
            |text: &'static str| div().text_sm().text_color(theme.text_muted).child(text);

        let body = match step {
            Step::Welcome => div()
                .flex()
                .flex_col()
                .gap_3()
                .child(heading("Welcome to Groovy"))
                .child(paragraph(
                    "Groovy shares files, storage and messages directly between your devices. \
                     No central server holds your data.",
                ))
                .child(paragraph(
                    "Groovy is pre-alpha. Don't rely on it to protect data you can't afford to lose.",
                )),
            Step::Name => div()
                .flex()
                .flex_col()
                .gap_3()
                .child(heading("What should contacts call you?"))
                .child(self.name.clone())
                .child(paragraph("You can change this later in Settings.")),
            Step::Identity => {
                let node = cx.global::<NodeState>();
                let endpoint_id = node.endpoint_id().to_string();
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(heading("This is your identity"))
                    .child(paragraph(
                        "Peers find you by this ID. It stays the same across launches.",
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .p_3()
                            .rounded_md()
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.input_background)
                            .child(
                                div()
                                    .flex_1()
                                    .text_sm()
                                    .child(abbreviate_id(&endpoint_id)),
                            )
                            .child(
                                Button::icon_only("onboarding-copy-id", Icon::Copy)
                                    .variant(Variant::Ghost)
                                    .tooltip("Copy endpoint ID")
                                    .on_click(move |_, cx| {
                                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                            endpoint_id.clone(),
                                        ));
                                    }),
                            ),
                    )
            }
            Step::Backup => div()
                .flex()
                .flex_col()
                .gap_3()
                .child(heading("Back up your identity"))
                .child(paragraph(
                    "If you lose this identity, your contacts can't reach you as the same peer. \
                     Backup and restore are coming soon.",
                )),
        };

        let view = cx.entity().clone();
        let back = (step != Step::Welcome).then(|| {
            Button::new("onboarding-back", "Back")
                .variant(Variant::Secondary)
                .on_click({
                    let view = view.clone();
                    move |_, cx| view.update(cx, |this, cx| this.back(cx))
                })
        });

        let next_label = if step.next().is_none() {
            "Finish"
        } else {
            "Next"
        };
        let next_enabled = step != Step::Name || valid;
        let next = Button::new("onboarding-next", next_label)
            .disabled(!next_enabled)
            .on_click(move |_, cx| view.update(cx, |this, cx| this.advance(cx)));

        let progress = format!("Step {} of {}", step.index() + 1, Step::ALL.len());

        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .size_full()
            .bg(theme.editor_background)
            .text_color(theme.text)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_6()
                    .w(CONTENT_WIDTH)
                    .max_w_full()
                    .p_8()
                    .child(div().text_xs().text_color(theme.text_muted).child(progress))
                    .child(body)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .children(back)
                            .child(next),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_run_in_order_and_stop_at_the_ends() {
        assert_eq!(Step::Welcome.previous(), None);
        assert_eq!(Step::Welcome.next(), Some(Step::Name));
        assert_eq!(Step::Name.next(), Some(Step::Identity));
        assert_eq!(Step::Identity.next(), Some(Step::Backup));
        assert_eq!(Step::Backup.next(), None);
        assert_eq!(Step::Backup.previous(), Some(Step::Identity));
    }

    #[test]
    fn onboarding_is_completed_once_its_flag_is_saved() {
        let tmp = tempfile::tempdir().unwrap();
        let database = Database::open(&tmp.path().join("groovy.sqlite")).unwrap();
        assert!(!is_completed(&database));

        database.set_setting(COMPLETED_KEY, "true").unwrap();

        assert!(is_completed(&database));
    }
}
