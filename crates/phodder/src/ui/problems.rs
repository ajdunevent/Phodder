//! The window's single banner: offline state, a failing sync, failing feeds,
//! and other background errors, with a Details dialog listing them.

use super::*;
use crate::feed_health::FeedProblem;
use crate::gpodder_sync_runner::SyncProblem;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

/// Older background errors are dropped past this many.
const MAX_OTHER_ERRORS: usize = 10;

#[derive(Default)]
struct State {
    offline: bool,
    feed_problems: Vec<FeedProblem>,
    other_errors: Vec<String>,
    /// Feeds hidden from the banner by Dismiss; a new failing feed shows it again.
    dismissed_feeds: HashSet<i64>,
    /// Stays until a sync succeeds.
    sync_problem: Option<SyncProblem>,
    /// Hidden by Dismiss until the sync's problem changes.
    sync_dismissed: bool,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::default();
}

/// Failing feeds by subscription ID, with their reason, for the
/// Subscriptions page's warning icons.
pub(super) fn failing_feeds() -> HashMap<i64, String> {
    STATE.with(|s| s.borrow().feed_problems.iter().map(|p| (p.subscription_id, p.reason.clone())).collect())
}

pub(super) fn set_refresh_status(offline: bool, feed_problems: Vec<FeedProblem>) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.offline = offline;
        let ids: HashSet<i64> = feed_problems.iter().map(|p| p.subscription_id).collect();
        s.dismissed_feeds.retain(|id| ids.contains(id));
        s.feed_problems = feed_problems;
    });
}

pub(super) fn set_sync_status(problem: Option<SyncProblem>) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if s.sync_problem != problem {
            s.sync_dismissed = false;
        }
        s.sync_problem = problem;
    });
}

fn sync_text(problem: &SyncProblem) -> String {
    match problem.failed_feeds {
        None => gettext_f("{service} sync failed for all feeds", &[("service", &problem.service)]),
        Some(n) => ngettext_f(
            "{service} sync failed for {n} feed",
            "{service} sync failed for {n} feeds",
            n as u32,
            &[("service", &problem.service), ("n", &n.to_string())],
        ),
    }
}

pub(super) fn push_error(message: String) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if !s.other_errors.contains(&message) {
            s.other_errors.push(message);
            let excess = s.other_errors.len().saturating_sub(MAX_OTHER_ERRORS);
            s.other_errors.drain(..excess);
        }
    });
}

fn dismiss() {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        s.other_errors.clear();
        s.dismissed_feeds = s.feed_problems.iter().map(|p| p.subscription_id).collect();
        s.sync_dismissed = true;
    });
}

/// Banner title, and whether the Details button applies. None hides it.
fn banner_text() -> Option<(String, bool)> {
    STATE.with(|s| {
        let s = s.borrow();
        if s.offline {
            return Some((gettext("You're offline. Refreshing will resume when you reconnect."), false));
        }
        let feeds: Vec<&FeedProblem> = s.feed_problems.iter().filter(|p| !s.dismissed_feeds.contains(&p.subscription_id)).collect();
        let others = s.other_errors.len();
        // A failing sync leads, since the account needs fixing; the rest is
        // counted after it.
        if let Some(sync) = s.sync_problem.as_ref().filter(|_| !s.sync_dismissed) {
            let sync = sync_text(sync);
            let rest = feeds.len() + others;
            let text = if rest == 0 {
                sync
            } else {
                // {sync} is the "… sync failed for …" sentence above.
                ngettext_f("{sync}, and {n} other problem", "{sync}, and {n} other problems", rest as u32, &[("sync", &sync), ("n", &rest.to_string())])
            };
            return Some((text, true));
        }
        let feed_part = match feeds.as_slice() {
            [] => None,
            [one] => Some(gettext_f("Couldn't refresh “{title}”", &[("title", &one.title)])),
            many => Some(ngettext_f("Couldn't refresh {n} feed", "Couldn't refresh {n} feeds", many.len() as u32, &[("n", &many.len().to_string())])),
        };
        let text = match (feed_part, others) {
            (None, 0) => return None,
            (None, 1) => s.other_errors[0].clone(),
            (None, n) => ngettext_f("{n} problem needs attention", "{n} problems need attention", n as u32, &[("n", &n.to_string())]),
            (Some(feeds), 0) => feeds,
            // {feeds} is the "Couldn't refresh…" sentence above.
            (Some(feeds), n) => ngettext_f("{feeds}, and {n} other problem", "{feeds}, and {n} other problems", n as u32, &[("feeds", &feeds), ("n", &n.to_string())]),
        };
        Some((text, true))
    })
}

pub(super) fn update_banner(banner: &libadwaita::Banner) {
    match banner_text() {
        Some((title, has_details)) => {
            // Announced only when it changes: it's rebuilt on every refresh.
            if !banner.is_revealed() || banner.title() != title {
                announce(banner, &title);
            }
            banner.set_use_markup(false);
            banner.set_title(&title);
            let details = gettext("Details");
            banner.set_button_label(if has_details { Some(details.as_str()) } else { None });
            banner.set_revealed(true);
        }
        None => banner.set_revealed(false),
    }
}

pub(super) fn show_details(
    banner: &libadwaita::Banner,
    window: &libadwaita::ApplicationWindow,
    dialog_sheet: &libadwaita::BottomSheet,
    command_tx: &UnboundedSender<UiCommand>,
) {
    let (feeds, others, offline, sync) = STATE.with(|s| {
        let s = s.borrow();
        (s.feed_problems.clone(), s.other_errors.clone(), s.offline, s.sync_problem.clone())
    });
    let content = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(18)
        .margin_start(18)
        .margin_end(18)
        .margin_top(18)
        .margin_bottom(18)
        .build();
    let list = gtk4::ListBox::builder().selection_mode(gtk4::SelectionMode::None).css_classes(["boxed-list"]).build();
    let mut sync_row = None;
    if let Some(sync) = &sync {
        let row = libadwaita::ActionRow::builder()
            .title(escape_markup(&sync_text(sync)))
            .subtitle(escape_markup(&sync.reason))
            .subtitle_lines(6)
            .activatable(true)
            .build();
        let icon = gtk4::Image::from_icon_name("dialog-warning-symbolic");
        set_accessible_label(&icon, &gettext("Sync problem"));
        row.add_prefix(&icon);
        row.add_suffix(&gtk4::Image::from_icon_name("go-next-symbolic"));
        row.set_tooltip_text(Some(&gettext("Open sync settings")));
        list.append(&row);
        sync_row = Some(row);
    }
    for problem in &feeds {
        let row = libadwaita::ActionRow::builder().title(escape_markup(&problem.title)).subtitle(escape_markup(&problem.reason)).build();
        let icon = gtk4::Image::from_icon_name("dialog-warning-symbolic");
        set_accessible_label(&icon, &gettext("Feed problem"));
        row.add_prefix(&icon);
        list.append(&row);
    }
    for message in &others {
        let row = libadwaita::ActionRow::builder().title(escape_markup(message)).title_lines(3).build();
        let icon = gtk4::Image::from_icon_name("dialog-error-symbolic");
        set_accessible_label(&icon, &gettext("Error"));
        row.add_prefix(&icon);
        list.append(&row);
    }
    if feeds.is_empty() && others.is_empty() && sync.is_none() {
        list.append(&libadwaita::ActionRow::builder().title(gettext("No problems")).build());
    }
    content.append(&list);

    let buttons = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(12).halign(gtk4::Align::Center).build();
    let retry = gtk4::Button::builder().label(gettext("Try Again")).css_classes(["pill", "suggested-action"]).sensitive(!offline && !feeds.is_empty()).build();
    let dismiss_button = gtk4::Button::builder().label(gettext("Dismiss")).css_classes(["pill"]).build();
    buttons.append(&retry);
    buttons.append(&dismiss_button);
    content.append(&buttons);

    let presentation = present_dialog_adaptively(&content, &gettext("Problems"), 420, 360, window, dialog_sheet, true);
    if let Some(row) = sync_row {
        let window = window.clone();
        let presentation = presentation.clone();
        row.connect_activated(move |_| {
            presentation.close();
            let _ = gtk4::prelude::WidgetExt::activate_action(&window, "win.show-settings", None);
        });
    }
    {
        let command_tx = command_tx.clone();
        let presentation = presentation.clone();
        retry.connect_clicked(move |_| {
            if let Err(e) = command_tx.send(UiCommand::RefreshAll) {
                tracing::error!("failed to send RefreshAll: {e}");
            }
            presentation.close();
        });
    }
    {
        let banner = banner.clone();
        dismiss_button.connect_clicked(move |_| {
            dismiss();
            update_banner(&banner);
            presentation.close();
        });
    }
}
