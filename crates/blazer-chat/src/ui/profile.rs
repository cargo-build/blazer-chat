//! A profile destination independent of virtualized message rows. @req M19_PROFILE_NAVIGATION

use gpui::{AnyElement, App, FontWeight, Role, SharedString, div, img, prelude::*, rems, rgb};

use super::message_pane::avatar;
use super::theme::Palette;
use crate::rc::{UserInfo, encode_query_param};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProfileAction {
    Close,
    Retry,
    OpenConversation,
    OpenOnServer,
}

pub(crate) struct ProfileView<'a> {
    pub(crate) username: &'a str,
    pub(crate) info: Option<&'a UserInfo>,
    pub(crate) avatar_path: Option<&'a str>,
    pub(crate) server: Option<&'a str>,
    pub(crate) is_self: bool,
    pub(crate) error: Option<&'a str>,
}

impl ProfileView<'_> {
    /// Use the original client's account/direct routes. @req M19_PROFILE_NAVIGATION
    pub(crate) fn server_url(&self) -> Option<String> {
        let origin = self.server?.trim_end_matches('/');
        Some(match self.is_self {
            true => format!("{origin}/account/profile"),
            false => format!("{origin}/direct/{}/user-info", encode_query_param(self.username)),
        })
    }
}

/// Navigation callbacks stay with the window owner. @req M19_PROFILE_NAVIGATION
pub(crate) fn panel(
    view: ProfileView<'_>,
    p: Palette,
    on_action: impl Fn(ProfileAction, &mut App) + Clone + 'static,
) -> AnyElement {
    let button = |id: &'static str, label: &'static str, action| {
        let on_action = on_action.clone();
        div()
            .id(id)
            .debug_selector(move || id.to_owned())
            .role(Role::Button)
            .aria_label(label)
            .tab_stop(true)
            .px_3()
            .py_2()
            .rounded_md()
            .text_sm()
            .cursor_pointer()
            .hover(|style| style.bg(rgb(p.row_hover)))
            .focus_visible(|style| style.border_1().border_color(rgb(p.accent)))
            .child(label)
            .on_click(move |_, _, cx| on_action(action, cx))
    };
    let heading = match view.info.and_then(|info| info.name.as_deref()).filter(|name| !name.is_empty()) {
        Some(name) => name,
        None => view.username,
    };
    let status = view.info.and_then(|info| info.status.as_deref());
    let status_color = match status {
        Some("online") => p.online,
        Some("away") => p.away,
        Some("busy") => p.busy,
        _ => p.muted,
    };
    let mut content = div()
        .id("profile-panel")
        .debug_selector(|| "profile-panel".to_owned())
        .role(Role::Complementary)
        .aria_label(format!("Profile for {}", view.username))
        .flex()
        .flex_col()
        .flex_none()
        .w(rems(22.0))
        .min_h_0()
        .border_l_1()
        .border_color(rgb(p.border))
        .bg(rgb(p.panel_alt))
        .text_color(rgb(p.text))
        .child(
            div().flex().items_center().justify_between().p_3()
                .border_b_1().border_color(rgb(p.border))
                .child(div().font_weight(FontWeight::SEMIBOLD).child(match view.is_self { true => "Your profile", false => "Profile" }))
                .child(button("profile-close", "Close profile", ProfileAction::Close)),
        );
    let identity = match view.avatar_path {
        Some(path) => img(std::path::PathBuf::from(path)).size(rems(5.0)).rounded_md().into_any_element(),
        None => avatar(view.username, p, rems(5.0)).into_any_element(),
    };
    let mut details = div().id("profile-details").flex_1().min_h_0().overflow_y_scroll()
        .flex().flex_col().p_4().gap_3()
        .child(identity)
        .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(SharedString::from(heading.to_owned())))
        .child(div().text_sm().text_color(rgb(p.muted)).child(format!("@{}", view.username)));
    if let Some(status) = status {
        details = details.child(div().text_sm().text_color(rgb(status_color)).child(SharedString::from(status.to_owned())));
    }
    details = match (view.info, view.error) {
        (_, Some(error)) => details
            .child(div().text_sm().text_color(rgb(p.danger_text)).child(format!("Could not load profile: {error}")))
            .child(button("profile-retry", "Retry profile", ProfileAction::Retry)),
        (None, None) => details.child(div().text_sm().text_color(rgb(p.muted)).child("Loading profile…")),
        (Some(info), None) => {
            let sections = [
                ("Status message", info.status_text.clone()),
                ("About", info.bio.clone()),
                ("Roles", match info.roles.is_empty() { true => None, false => Some(info.roles.join(", ")) }),
                ("Time zone", info.utc_offset.map(|offset| format!("UTC{offset:+}"))),
            ];
            details.children(sections.into_iter().filter_map(|(label, text)| {
                text.filter(|text| !text.is_empty()).map(|text| {
                    div().flex().flex_col().gap_1()
                        .child(div().text_xs().font_weight(FontWeight::MEDIUM).text_color(rgb(p.muted)).child(label))
                        .child(div().text_sm().child(SharedString::from(text)))
                })
            }))
        }
    };
    if let Some(server) = view.server {
        details = details.child(div().flex().flex_col().gap_1()
            .child(div().text_xs().text_color(rgb(p.muted)).child("Workspace"))
            .child(div().text_sm().child(SharedString::from(server.to_owned()))));
    }
    content = content.child(details).child(
        div().flex().flex_col().p_3().gap_1().border_t_1().border_color(rgb(p.border))
            .when(!view.is_self, |actions| actions.child(button("profile-conversation", "Open conversation", ProfileAction::OpenConversation)))
            .when(view.server.is_some(), |actions| actions.child(button("profile-server", "Open on server", ProfileAction::OpenOnServer))),
    );
    content.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_links_use_the_server_and_escape_user_path_components() {
        let mut view = ProfileView {
            username: "bob/elsewhere?query",
            info: None,
            avatar_path: None,
            server: Some("https://chat.example/"),
            is_self: false,
            error: None,
        };
        assert_eq!(view.server_url().as_deref(), Some("https://chat.example/direct/bob%2Felsewhere%3Fquery/user-info"));
        view.is_self = true;
        assert_eq!(view.server_url().as_deref(), Some("https://chat.example/account/profile"));
        view.server = None;
        assert!(view.server_url().is_none(), "an absent session origin creates no invented link");
    }
}
