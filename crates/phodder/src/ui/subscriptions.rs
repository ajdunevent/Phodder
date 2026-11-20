use super::*;

/// The Subscriptions tab: the list or the empty placeholder, with Add Feed
/// pinned below. `subscriptions_slot` is passed in so deleting a
/// subscription, a local synchronous change, can rebuild the slot directly
/// instead of going through the backend.
pub(super) fn build_subscriptions_page_from_data(
    subscriptions: Vec<phodder_core::models::Subscription>,
    groups: Vec<phodder_core::groups::Group>,
    episode_counts: std::collections::HashMap<i64, i64>,
    db_path: &std::path::Path,
    subscriptions_slot: &gtk4::Box,
    tokio_handle: &tokio::runtime::Handle,
    is_narrow: &std::rc::Rc<std::cell::Cell<bool>>,
    command_tx: &UnboundedSender<UiCommand>,
    dialog_sheet: &libadwaita::BottomSheet,
) -> gtk4::Widget {
    // Add Feed is pinned below the empty state too; it's the only way out of
    // it.
    let list_content: gtk4::Widget = if subscriptions.is_empty() {
        placeholder_page(&gettext("Subscriptions"), "starred-symbolic").upcast()
    } else {
        build_subscriptions_list(&subscriptions, &groups, &episode_counts, db_path, subscriptions_slot, tokio_handle, is_narrow, command_tx, dialog_sheet)
    };

    // AdwButtonRow inside an AdwToolbarView bottom bar (Raised), so Add Feed
    // is part of the list with a managed undershoot shadow.
    let add_feed_row = libadwaita::ButtonRow::builder().title(gettext("Add Podcast")).start_icon_name("list-add-symbolic").build();
    {
        let db_path = db_path.to_path_buf();
        let tokio_handle = tokio_handle.clone();
        let command_tx = command_tx.clone();
        let subscriptions_slot = subscriptions_slot.clone();
        let is_narrow = is_narrow.clone();
        let dialog_sheet = dialog_sheet.clone();
        add_feed_row.connect_activated(move |row| {
            let Some(window) = row.root().and_then(|r| r.downcast::<libadwaita::ApplicationWindow>().ok())
            else {
                tracing::warn!("couldn't find a parent ApplicationWindow for Add Podcast");
                return;
            };
            show_discover_page(
                &window,
                db_path.clone(),
                tokio_handle.clone(),
                command_tx.clone(),
                subscriptions_slot.clone(),
                is_narrow.clone(),
                &dialog_sheet,
            );
        });
    }
    let add_feed_list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .margin_start(12)
        .margin_end(12)
        .margin_bottom(12)
        .margin_top(6)
        .build();
    add_feed_list.append(&add_feed_row);

    let toolbar_view = libadwaita::ToolbarView::new();
    toolbar_view.set_vexpand(true);
    toolbar_view.set_content(Some(&list_content));
    toolbar_view.add_bottom_bar(&add_feed_list);
    toolbar_view.set_bottom_bar_style(libadwaita::ToolbarStyle::Raised);
    toolbar_view.upcast()
}

/// Loads subscriptions and swaps in the tab's widget, as build_queue_page
/// does.
pub(super) fn build_subscriptions_page(
    tokio_handle: &tokio::runtime::Handle,
    db_path: &std::path::Path,
    subscriptions_slot: &gtk4::Box,
    is_narrow: &std::rc::Rc<std::cell::Cell<bool>>,
    command_tx: &UnboundedSender<UiCommand>,
    dialog_sheet: &libadwaita::BottomSheet,
) {
    let db_path_owned = db_path.to_path_buf();
    let subscriptions_slot_for_content = subscriptions_slot.clone();
    let subscriptions_slot_for_build = subscriptions_slot.clone();
    let tokio_handle_for_completion = tokio_handle.clone();
    let is_narrow = is_narrow.clone();
    let command_tx = command_tx.clone();
    let dialog_sheet = dialog_sheet.clone();
    run_db_op(
        tokio_handle,
        db_path.to_path_buf(),
        |conn| {
            let subscriptions = phodder_core::subscriptions::list_subscriptions(conn)?;
            let groups = phodder_core::groups::list_groups(conn)?;
            let mut episode_counts: Vec<(i64, i64)> = phodder_core::subscriptions::subscription_group_names_and_episode_counts(conn)?
                .into_iter()
                .map(|(id, info)| (id, info.episode_count))
                .collect();
            episode_counts.sort();
            Ok::<_, anyhow::Error>((subscriptions, groups, episode_counts))
        },
        move |result| match result {
            Ok((subscriptions, groups, episode_counts)) => {
                let mut failing: Vec<(i64, String)> = problems::failing_feeds().into_iter().collect();
                failing.sort();
                let inputs = (&subscriptions, &groups, &episode_counts, is_narrow.get(), failing);
                if slot_data_unchanged(&subscriptions_slot_for_content, &inputs) {
                    return;
                }
                let widget = build_subscriptions_page_from_data(
                    subscriptions,
                    groups,
                    episode_counts.into_iter().collect(),
                    &db_path_owned,
                    &subscriptions_slot_for_build,
                    &tokio_handle_for_completion,
                    &is_narrow,
                    &command_tx,
                    &dialog_sheet,
                );
                replace_slot_content(&subscriptions_slot_for_content, widget);
            }
            Err(e) => tracing::error!("failed to load subscriptions list: {e}"),
        },
    );
}

/// The list itself, split out so the empty placeholder shares the Add Feed
/// bar.
pub(super) fn build_subscriptions_list(
    subscriptions: &[phodder_core::models::Subscription],
    groups: &[phodder_core::groups::Group],
    episode_counts: &std::collections::HashMap<i64, i64>,
    db_path: &std::path::Path,
    subscriptions_slot: &gtk4::Box,
    tokio_handle: &tokio::runtime::Handle,
    is_narrow: &std::rc::Rc<std::cell::Cell<bool>>,
    command_tx: &UnboundedSender<UiCommand>,
    dialog_sheet: &libadwaita::BottomSheet,
) -> gtk4::Widget {
    let list_box = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .margin_start(12)
        .margin_end(12)
        .margin_top(12)
        .margin_bottom(12)
        .build();

    // 64px narrow, 96px wide.
    let thumbnail_size: i32 = if is_narrow.get() { 64 } else { 96 };

    // Grouped like the Queue: groups in their order, shows by title within.
    let mut ordered: Vec<(usize, &phodder_core::models::Subscription)> = subscriptions
        .iter()
        .map(|subscription| (groups.iter().position(|g| g.id == subscription.group_id).unwrap_or(usize::MAX), subscription))
        .collect();
    ordered.sort_by_cached_key(|(group_index, subscription)| (*group_index, subscription.title.to_lowercase()));

    let failing_feeds = problems::failing_feeds();
    let mut last_group_index: Option<usize> = None;
    for (group_index, subscription) in ordered {
        let group = groups.get(group_index);
        let group_rgb = group.and_then(|g| g.color.as_deref()).and_then(parse_rgb_components);
        if last_group_index != Some(group_index) {
            last_group_index = Some(group_index);
            let header_label = gtk4::Label::builder()
                .label(group.map(|g| g.name.clone()).unwrap_or_else(|| gettext("Ungrouped")))
                .halign(gtk4::Align::Start)
                .accessible_role(gtk4::AccessibleRole::Heading)
                .css_classes(["heading"])
                .margin_start(6)
                .margin_end(6)
                .margin_bottom(2)
                .build();
            let header_row = gtk4::ListBoxRow::builder().child(&header_label).selectable(false).activatable(false).build();
            set_accessible_label(&header_row, &header_label.label());
            if let (Some((r, g, b)), Some(group)) = (group_rgb, group) {
                // A low-opacity tint of the group color, as in the Queue.
                let widget_name = format!("subscriptions-group-header-{}", group.id);
                header_row.set_widget_name(&widget_name);
                add_display_css(&format!("#{widget_name} {{ background-color: rgba({r},{g},{b},0.15); }}"));
            }
            list_box.append(&header_row);
        }

        // The wrapper row below is the one focused and announced.
        let subscription_row = libadwaita::ActionRow::builder()
            .activatable(true)
            .focusable(false)
            .accessible_role(gtk4::AccessibleRole::Presentation)
            .title(&escape_markup(&subscription.title))
            .build();
        let episode_count = episode_counts.get(&subscription.id).copied().unwrap_or(0);
        subscription_row.set_subtitle(&format!("{episode_count} {}", if episode_count == 1 { "episode" } else { "episodes" }));

        // From the cached file only: GTK-thread code has no Tokio runtime to
        // fetch with (downloader.rs caches it during refresh). Image with
        // pixel_size, not Picture: Picture sizes itself from its paintable
        // and ignores size requests. Non-square art letterboxes instead of
        // cropping.
        let thumbnail = load_thumbnail_image(&[subscription.artwork_local_path.as_deref()], "image-x-generic-symbolic", thumbnail_size);
        subscription_row.add_prefix(&thumbnail);

        if let Some(reason) = failing_feeds.get(&subscription.id) {
            let warning = gtk4::Image::builder()
                .icon_name("dialog-warning-symbolic")
                .tooltip_text(gettext_f("Last refresh failed: {reason}", &[("reason", reason)]))
                .accessible_role(gtk4::AccessibleRole::Img)
                .css_classes(["warning"])
                .build();
            subscription_row.add_suffix(&warning);
        }
        {
            let db_path = db_path.to_path_buf();
            let subscriptions_slot = subscriptions_slot.clone();
            let subscription_id = subscription.id;
            let tokio_handle = tokio_handle.clone();
            let is_narrow = is_narrow.clone();
            let command_tx = command_tx.clone();
            let dialog_sheet = dialog_sheet.clone();
            subscription_row.connect_activated(move |row| {
                let Some(window) = row.root().and_then(|r| r.downcast::<libadwaita::ApplicationWindow>().ok()) else {
                    tracing::warn!("couldn't find a parent ApplicationWindow for subscription detail view");
                    return;
                };
                show_subscription_detail(
                    &window,
                    db_path.clone(),
                    subscription_id,
                    subscriptions_slot.clone(),
                    tokio_handle.clone(),
                    is_narrow.clone(),
                    command_tx.clone(),
                    false,
                    &dialog_sheet,
                    "details",
                );
            });
        }

        // The Queue's grab-handle column without the handle: a color bar,
        // flush left as a sibling of the row.
        let color_bar = gtk4::Box::builder().width_request(8).vexpand(true).build();
        if let Some((r, g, b)) = group_rgb {
            let widget_name = format!("subscriptions-color-bar-{}", subscription.id);
            color_bar.set_widget_name(&widget_name);
            add_display_css(&format!("#{widget_name} {{ background-color: rgba({r},{g},{b},0.25); }}"));
        }
        subscription_row.set_hexpand(true);
        let row_with_bar = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).build();
        row_with_bar.append(&color_bar);
        row_with_bar.append(&subscription_row);
        // Padding zeroed by id selector, beating boxed-list's class rule.
        let list_row = gtk4::ListBoxRow::builder().child(&row_with_bar).build();
        set_accessible_label(&list_row, &subscription.title);
        let mut description = subscription_row.subtitle().map(|s| s.to_string()).unwrap_or_default();
        if let Some(reason) = failing_feeds.get(&subscription.id) {
            description.push_str(". ");
            description.push_str(&gettext_f("Last refresh failed: {reason}", &[("reason", reason)]));
        }
        list_row.update_property(&[gtk4::accessible::Property::Description(&description)]);
        let list_row_widget_name = format!("subscriptions-row-{}", subscription.id);
        list_row.set_widget_name(&list_row_widget_name);
        add_display_css(&format!("#{list_row_widget_name} {{ padding: 0; margin: 0; }}"));
        list_box.append(&list_row);
    }
    // The subscription row is nested in a wrapper, so activation arrives
    // here rather than on the inner row.
    list_box.connect_row_activated(|_, row| {
        if let Some(inner) = row.child().and_then(|c| c.last_child()).and_downcast::<libadwaita::ActionRow>() {
            libadwaita::prelude::ActionRowExt::activate(&inner);
        }
    });

    // As in Library: see its comment.
    let bottom_divider_row = gtk4::ListBoxRow::builder()
        .activatable(false)
        .selectable(false)
        .focusable(false)
        .accessible_role(gtk4::AccessibleRole::Presentation)
        .child(&gtk4::Box::new(gtk4::Orientation::Horizontal, 0))
        .build();
    list_box.append(&bottom_divider_row);

    gtk4::ScrolledWindow::builder().child(&list_box).vexpand(true).build().upcast()
}
