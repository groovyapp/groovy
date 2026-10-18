//! The strip along the bottom: node status, relay, and this node's endpoint ID.

use gpui::{App, ClipboardItem, Window, div, prelude::*, px};

use crate::{
    node::{NodeState, NodeStatus, abbreviate_id},
    theme::ActiveTheme,
    ui::{button::Button, icon::Icon, spinner::Spinner},
};

const HEIGHT: gpui::Pixels = px(26.);

#[derive(IntoElement)]
pub struct StatusBar;

impl RenderOnce for StatusBar {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let node = cx.global::<NodeState>();
        let status = node.status();
        let endpoint_id = node.endpoint_id().to_string();
        let relay = node.relay();

        let (status_text, status_color) = match status {
            NodeStatus::Starting => ("Starting", theme.text_muted),
            NodeStatus::Online => ("Online", theme.success),
            NodeStatus::Offline => ("Offline", theme.danger),
        };
        let relay_text = match (status, relay) {
            (NodeStatus::Online, Some(relay)) => format!("Relay: {relay}"),
            (NodeStatus::Online, None) => "No relay yet".to_owned(),
            _ => String::new(),
        };

        let status_item = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1p5()
            .child(match status {
                NodeStatus::Starting => Spinner::new("node-starting")
                    .size(px(12.))
                    .into_any_element(),
                _ => div()
                    .size(px(8.))
                    .rounded_full()
                    .bg(status_color)
                    .into_any_element(),
            })
            .child(status_text);

        let identity_item = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .child(div().text_color(theme.text_muted).child("Endpoint"))
            .child(abbreviate_id(&endpoint_id))
            .child(
                Button::icon_only("copy-endpoint-id", Icon::Copy)
                    .variant(crate::ui::button::Variant::Ghost)
                    .tooltip("Copy endpoint ID")
                    .on_click(move |_, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(endpoint_id.clone()));
                    }),
            );

        div()
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .justify_between()
            .h(HEIGHT)
            .px_3()
            .gap_4()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.editor_background)
            .text_xs()
            .text_color(theme.text)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_4()
                    .child(status_item)
                    .child(div().text_color(theme.text_muted).child(relay_text)),
            )
            .child(identity_item)
    }
}
