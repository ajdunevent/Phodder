use super::*;

/// One filter rule's rows, read back on save. Vec order is the priority
/// order `filter::evaluate` uses: an Allow above a broader Block carves out
/// an exception. Each rule is an expander titled by its pattern, so narrow
/// windows don't squeeze five controls onto one line.
#[derive(Clone)]
pub(super) struct FilterRuleRow {
    container: libadwaita::ExpanderRow,
    pattern_row: libadwaita::EntryRow,
    target_row: libadwaita::ComboRow,
    action_row: libadwaita::ComboRow,
    move_up_button: gtk4::Button,
    move_down_button: gtk4::Button,
}

impl FilterRuleRow {
    fn rule(&self) -> Option<phodder_core::filter::FilterRule> {
        let pattern = self.pattern_row.text().to_string();
        if pattern.trim().is_empty() {
            return None;
        }
        let target = match self.target_row.selected() {
            0 => phodder_core::filter::MatchTarget::Title,
            1 => phodder_core::filter::MatchTarget::Description,
            _ => phodder_core::filter::MatchTarget::Both,
        };
        let action = match self.action_row.selected() {
            0 => phodder_core::filter::Action::Allow,
            _ => phodder_core::filter::Action::Block,
        };
        Some(phodder_core::filter::FilterRule { pattern, target, action })
    }

    /// The collapsed row: the pattern, then "Block · Title".
    fn describe(&self) {
        let pattern = self.pattern_row.text();
        self.container.set_title(if pattern.trim().is_empty() { gettext("New rule") } else { pattern.to_string() }.as_str());
        let action = if self.action_row.selected() == 0 { gettext("Allow") } else { gettext("Block") };
        let target = match self.target_row.selected() {
            0 => gettext("Title"),
            1 => gettext("Notes"),
            _ => gettext("Both"),
        };
        self.container.set_subtitle(&gettext_f("{action} · {target}", &[("action", &action), ("target", &target)]));
    }
}

/// Disables the first row's up arrow and the last row's down arrow; called
/// after every build, move and removal.
pub(super) fn update_filter_rule_edge_sensitivity(rows: &[FilterRuleRow]) {
    for (index, row) in rows.iter().enumerate() {
        row.move_up_button.set_sensitive(index != 0);
        row.move_down_button.set_sensitive(index != rows.len() - 1);
    }
}

/// Re-appends every row in Vec order, add_rule_row last, so the visual order
/// matches the saved priority.
pub(super) fn resync_filter_rule_visual_order(rules_container: &gtk4::ListBox, rows: &[FilterRuleRow], add_rule_row: &libadwaita::ButtonRow) {
    while let Some(child) = rules_container.first_child() {
        rules_container.remove(&child);
    }
    for row in rows {
        rules_container.append(&row.container);
    }
    rules_container.append(add_rule_row);
}

/// Moves a row up (-1) or down (1) in both the Vec and the layout; a no-op at
/// the edges.
pub(super) fn move_filter_rule(
    rules_container: &gtk4::ListBox,
    rows: &std::rc::Rc<std::cell::RefCell<Vec<FilterRuleRow>>>,
    container: &libadwaita::ExpanderRow,
    direction: i32,
    on_change: &std::rc::Rc<dyn Fn()>,
    add_rule_row: &libadwaita::ButtonRow,
) {
    let mut rows_mut = rows.borrow_mut();
    let Some(index) = rows_mut.iter().position(|r| &r.container == container) else { return };
    let new_index = index as i32 + direction;
    if new_index < 0 || new_index as usize >= rows_mut.len() {
        return;
    }
    rows_mut.swap(index, new_index as usize);
    resync_filter_rule_visual_order(rules_container, &rows_mut, add_rule_row);
    update_filter_rule_edge_sensitivity(&rows_mut);
    drop(rows_mut);
    on_change();
}

/// Builds one rule and tracks it in `rows`; the caller puts it in place
/// with resync_filter_rule_visual_order. `initial` is None for a new rule,
/// which opens expanded.
pub(super) fn build_filter_rule_row(
    initial: Option<&phodder_core::filter::FilterRule>,
    rules_container: &gtk4::ListBox,
    rows: &std::rc::Rc<std::cell::RefCell<Vec<FilterRuleRow>>>,
    on_change: &std::rc::Rc<dyn Fn()>,
    add_rule_row: &libadwaita::ButtonRow,
) -> FilterRuleRow {
    let container = libadwaita::ExpanderRow::builder().use_markup(false).expanded(initial.is_none()).build();
    let pattern_row = libadwaita::EntryRow::builder().title(gettext("Pattern, e.g. *bonus* or interview")).show_apply_button(true).build();
    let target_row = libadwaita::ComboRow::builder()
        .title(gettext("Match in"))
        .model(&gtk4::StringList::new(&[gettext("Title").as_str(), gettext("Notes").as_str(), gettext("Both").as_str()]))
        .build();
    let action_row = libadwaita::ComboRow::builder()
        .title(gettext("Action"))
        .model(&gtk4::StringList::new(&[gettext("Allow").as_str(), gettext("Block").as_str()]))
        .build();
    if let Some(rule) = initial {
        pattern_row.set_text(&rule.pattern);
        target_row.set_selected(match rule.target {
            phodder_core::filter::MatchTarget::Title => 0,
            phodder_core::filter::MatchTarget::Description => 1,
            phodder_core::filter::MatchTarget::Both => 2,
        });
        action_row.set_selected(match rule.action {
            phodder_core::filter::Action::Allow => 0,
            phodder_core::filter::Action::Block => 1,
        });
    }

    // Priority order matters (see filter::evaluate), so rules can be
    // reordered.
    let icon_button = |icon: &str, tooltip: &str| {
        let button = gtk4::Button::builder().icon_name(icon).tooltip_text(tooltip).valign(gtk4::Align::Center).build();
        button.add_css_class("flat");
        button
    };
    let move_up_button = icon_button("go-up-symbolic", &gettext("Move rule up (higher priority)"));
    let move_down_button = icon_button("go-down-symbolic", &gettext("Move rule down (lower priority)"));
    let remove_button = icon_button("user-trash-symbolic", &gettext("Remove rule"));
    let order_row = libadwaita::ActionRow::builder().title(gettext("Priority")).build();
    order_row.add_suffix(&move_up_button);
    order_row.add_suffix(&move_down_button);
    order_row.add_suffix(&remove_button);
    container.add_row(&pattern_row);
    container.add_row(&target_row);
    container.add_row(&action_row);
    container.add_row(&order_row);

    let rule_row = FilterRuleRow {
        container: container.clone(),
        pattern_row: pattern_row.clone(),
        target_row: target_row.clone(),
        action_row: action_row.clone(),
        move_up_button: move_up_button.clone(),
        move_down_button: move_down_button.clone(),
    };
    rule_row.describe();

    // The pattern saves on Enter or the apply button, not per keystroke;
    // the title follows as it's typed.
    {
        let rule_row = rule_row.clone();
        pattern_row.connect_changed(move |_| rule_row.describe());
    }
    {
        let on_change = on_change.clone();
        pattern_row.connect_apply(move |_| on_change());
    }
    for combo in [&target_row, &action_row] {
        let rule_row = rule_row.clone();
        let on_change = on_change.clone();
        combo.connect_selected_notify(move |_| {
            rule_row.describe();
            on_change();
        });
    }
    rows.borrow_mut().push(rule_row.clone());
    update_filter_rule_edge_sensitivity(&rows.borrow());

    for (button, direction) in [(&move_up_button, -1), (&move_down_button, 1)] {
        let rules_container = rules_container.clone();
        let rows = rows.clone();
        let container = container.clone();
        let on_change = on_change.clone();
        let add_rule_row = add_rule_row.clone();
        button.connect_clicked(move |_| move_filter_rule(&rules_container, &rows, &container, direction, &on_change, &add_rule_row));
    }
    {
        let rules_container = rules_container.clone();
        let rows = rows.clone();
        let on_change = on_change.clone();
        remove_button.connect_clicked(move |_| {
            rules_container.remove(&container);
            // Widgets compare by GObject identity.
            rows.borrow_mut().retain(|r| r.container != container);
            update_filter_rule_edge_sensitivity(&rows.borrow());
            on_change();
        });
    }
    rule_row
}

/// A subscription's Details, Episodes and Settings tabs, with Delete (two
/// taps to confirm). Opened by tapping a Subscriptions row.
pub(super) fn show_subscription_detail(
    parent: &libadwaita::ApplicationWindow,
    db_path: std::path::PathBuf,
    subscription_id: i64,
    subscriptions_slot: gtk4::Box,
    tokio_handle: tokio::runtime::Handle,
    is_narrow: std::rc::Rc<std::cell::Cell<bool>>,
    command_tx: UnboundedSender<UiCommand>,
    // True right after Add Feed: settings can be set before anything
    // downloads, and Done starts the first refresh.
    is_new_subscription: bool,
    dialog_sheet: &libadwaita::BottomSheet,
    // "details", "episodes" or "settings".
    start_tab: &'static str,
) {
    let sub = match load_subscription(&db_path, subscription_id) {
        Ok(Some(s)) => s,
        Ok(None) => {
            tracing::warn!("subscription {subscription_id} no longer exists, can't show detail");
            return;
        }
        Err(e) => {
            tracing::error!("failed to load subscription {subscription_id} detail: {e}");
            return;
        }
    };

    // Scales continuously with the window width up to 265px
    // (hero_thumbnail_size_for_width). Image, not Picture, for
    // application-controlled size.
    let subscription_thumbnail = load_thumbnail_image(
        &[sub.artwork_local_path.as_deref()],
        "image-x-generic-symbolic",
        hero_thumbnail_size_for_width(parent.width()),
    );
    subscription_thumbnail.set_halign(gtk4::Align::Center);
    subscription_thumbnail.set_margin_bottom(4);

    // Resized on every window width change; set_pixel_size rescales without
    // reloading. Disconnected when the thumbnail is destroyed, since the main
    // window outlives each dialog.
    let width_notify_handler_id = {
        let subscription_thumbnail = subscription_thumbnail.clone();
        parent.connect_default_width_notify(move |win| {
            // Deferred: changing size inside the resize notify, mid-layout,
            // logged "Allocating size to AdwBin without calling
            // gtk_widget_measure()".
            let subscription_thumbnail = subscription_thumbnail.clone();
            let width = win.width();
            gtk4::glib::source::idle_add_local(move || {
                subscription_thumbnail.set_pixel_size(hero_thumbnail_size_for_width(width));
                gtk4::glib::ControlFlow::Break
            });
        })
    };
    // SignalHandlerId isn't Clone and connect_destroy needs an Fn, so it's
    // taken from a RefCell<Option<_>>.
    let width_notify_handler_id = std::rc::Rc::new(std::cell::RefCell::new(Some(width_notify_handler_id)));
    {
        let parent = parent.clone();
        subscription_thumbnail.connect_destroy(move |_| {
            if let Some(handler_id) = width_notify_handler_id.borrow_mut().take() {
                parent.disconnect(handler_id);
            }
        });
    }

    let title_label = gtk4::Label::builder()
        .label(&sub.title)
        .halign(gtk4::Align::Center)
        .justify(gtk4::Justification::Center)
        .wrap(true)
        .css_classes(["title-2"])
        .build();
    // Edit is a toggle: on makes the title editable, off saves. Phones have
    // no Escape key, so focus-loss/Escape conventions don't work.
    let edit_title_button = gtk4::ToggleButton::builder()
        .icon_name("document-edit-symbolic")
        .css_classes(["flat", "circular"])
        .valign(gtk4::Align::Center)
        .tooltip_text(gettext("Rename"))
        .build();
    let title_display_row = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(4).halign(gtk4::Align::Center).build();
    title_display_row.append(&title_label);

    let title_entry = gtk4::Entry::builder().text(&sub.title).halign(gtk4::Align::Center).hexpand(false).visible(false).build();
    set_accessible_label(&title_entry, &gettext("Podcast title"));
    // Two-stage undo, shown while editing: first back to the title at the
    // start of this edit, then to the feed's title (or the current title if
    // the feed's is unknown).
    let undo_title_button = gtk4::Button::builder()
        .icon_name("edit-undo-symbolic")
        .css_classes(["flat", "circular"])
        .valign(gtk4::Align::Center)
        .tooltip_text(gettext("Undo"))
        .visible(false)
        .build();
    let title_edit_row = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(4).halign(gtk4::Align::Center).visible(false).build();
    title_edit_row.append(&title_entry);

    let title_toggle_row = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(4).halign(gtk4::Align::Center).build();
    title_toggle_row.append(&title_display_row);
    title_toggle_row.append(&title_edit_row);
    title_toggle_row.append(&edit_title_button);
    title_toggle_row.append(&undo_title_button);

    // Captured each time editing starts.
    let title_pre_session_text = std::rc::Rc::new(std::cell::RefCell::new(sub.title.clone()));
    // 0 = unused, 1 = reverted to the session start, 2 = reverted to the feed
    // title. Reset on editing start and on keystrokes.
    let title_undo_stage = std::rc::Rc::new(std::cell::Cell::new(0u8));
    // Falls back to the current title for a never-refreshed subscription.
    let feed_default_title = sub.feed_title.clone().unwrap_or_else(|| sub.title.clone());


    {
        // A key controller, not "changed": undo's own set_text mustn't reset
        // the undo stage.
        let title_undo_stage = title_undo_stage.clone();
        let key_controller = gtk4::EventControllerKey::new();
        key_controller.connect_key_pressed(move |_, _, _, _| {
            title_undo_stage.set(0);
            gtk4::glib::Propagation::Proceed
        });
        title_entry.add_controller(key_controller);
    }
    {
        let title_entry = title_entry.clone();
        let title_pre_session_text = title_pre_session_text.clone();
        let title_undo_stage = title_undo_stage.clone();
        let feed_default_title = feed_default_title.clone();
        undo_title_button.connect_clicked(move |_| {
            if title_undo_stage.get() == 0 {
                title_entry.set_text(&title_pre_session_text.borrow());
                title_undo_stage.set(1);
            } else {
                title_entry.set_text(&feed_default_title);
                title_undo_stage.set(2);
            }
            title_entry.select_region(0, -1);
        });
    }

    // Group, with a shortcut to Manage Groups for a group that doesn't exist
    // yet.
    let groups = {
        let conn = phodder_core::open_connection(&db_path);
        conn.ok().and_then(|c| phodder_core::groups::list_groups(&c).ok()).unwrap_or_default()
    };
    let group_names: Vec<&str> = groups.iter().map(|g| g.name.as_str()).collect();
    let current_group_index = groups.iter().position(|g| g.id == sub.group_id).unwrap_or(0) as u32;
    let group_row = libadwaita::ComboRow::builder().title(gettext("Group")).model(&gtk4::StringList::new(&group_names)).selected(current_group_index).build();
    // Gear suffix on the Group row.
    let manage_groups_button = gtk4::Button::from_icon_name("emblem-system-symbolic");
    manage_groups_button.set_valign(gtk4::Align::Center);
    manage_groups_button.set_tooltip_text(Some(&gettext("Manage Groups")));
    manage_groups_button.add_css_class("flat");
    group_row.add_suffix(&manage_groups_button);

    let auto_download_row = libadwaita::SwitchRow::builder().title(gettext("Auto-download new episodes")).active(sub.auto_download_enabled).build();
    let notify_row = libadwaita::SwitchRow::builder().title(gettext("Notify on new episodes")).active(sub.notify_on_new_episodes).build();

    // Required per-subscription expiry (no global fallback; NOT NULL DEFAULT
    // 10080), from the same choices as Settings. A value set before these
    // choices existed stays selectable. Applies whatever the played status
    // (see the delete_after_minutes_evicts_played_episodes_too test).
    let mut expiry_choices: Vec<i64> = EXPIRY_CHOICES_MINUTES.to_vec();
    if !expiry_choices.contains(&sub.delete_after_minutes) {
        expiry_choices.push(sub.delete_after_minutes);
        expiry_choices.sort_unstable();
    }
    let expiry_labels: Vec<String> = expiry_choices.iter().map(|m| duration_label(*m)).collect();
    let expiry_row = libadwaita::ComboRow::builder()
        .title(gettext("Delete episodes after"))
        .subtitle(gettext("Based on release date"))
        .model(&gtk4::StringList::new(&expiry_labels.iter().map(String::as_str).collect::<Vec<_>>()))
        .selected(expiry_choices.iter().position(|m| *m == sub.delete_after_minutes).unwrap_or(0) as u32)
        .build();

    let rule_rows: std::rc::Rc<std::cell::RefCell<Vec<FilterRuleRow>>> = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let settings_toast_overlay = libadwaita::ToastOverlay::new();
    // Shared by the save button and Enter.
    {
        let db_path = db_path.clone();
        let command_tx = command_tx.clone();
        let toast_overlay = settings_toast_overlay.clone();
        let title_entry = title_entry.clone();
        let title_label = title_label.clone();
        let title_display_row = title_display_row.clone();
        let title_edit_row = title_edit_row.clone();
        let undo_title_button = undo_title_button.clone();
        let title_pre_session_text = title_pre_session_text.clone();
        let title_undo_stage = title_undo_stage.clone();
        edit_title_button.connect_toggled(move |button| {
            if button.is_active() {
                *title_pre_session_text.borrow_mut() = title_label.label().to_string();
                title_undo_stage.set(0);
                title_entry.set_text(&title_pre_session_text.borrow());
                title_display_row.set_visible(false);
                title_edit_row.set_visible(true);
                undo_title_button.set_visible(true);
                title_entry.set_visible(true);
                title_entry.grab_focus();
                // Selected so typing replaces it.
                title_entry.select_region(0, -1);
                return;
            }

            // Toggled off: saves. Blank input is discarded instead of
            // re-enabling the toggle, which would desync it from the entry.
            let new_title = title_entry.text().trim().to_string();
            if !new_title.is_empty() && new_title != title_label.label() {
                match phodder_core::open_connection(&db_path)
                    .and_then(|conn| phodder_core::subscriptions::update_subscription_title(&conn, subscription_id, &new_title))
                {
                    Ok(()) => {
                        title_label.set_label(&new_title);
                        // The title appears in other tabs too.
                        if let Err(e) = command_tx.send(UiCommand::NotifyDataChanged) {
                            tracing::error!("failed to send NotifyDataChanged after rename: {e}");
                        }
                    }
                    Err(e) => {
                        tracing::error!("failed to rename subscription {subscription_id}: {e}");
                        toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't save the new title")));
                    }
                }
            }
            title_edit_row.set_visible(false);
            undo_title_button.set_visible(false);
            title_display_row.set_visible(true);
        });
    }
    {
        // Enter toggles the button off instead of saving directly, so button
        // and edit mode can't diverge.
        let edit_title_button = edit_title_button.clone();
        title_entry.connect_activate(move |_| edit_title_button.set_active(false));
    }

    // A new subscription shows Cancel instead of Delete; it also removes the
    // row Add Feed inserted.
    let delete_button = gtk4::Button::builder().label(if is_new_subscription { "Cancel" } else { "Delete" }).css_classes(["destructive-action"]).build();

    // Saves on every change; each field's handler calls this.
    let save_now: std::rc::Rc<dyn Fn()> = {
        let db_path = db_path.clone();
        let command_tx = command_tx.clone();
        let auto_download_row = auto_download_row.clone();
        let notify_row = notify_row.clone();
        let rule_rows = rule_rows.clone();
        let expiry_row = expiry_row.clone();
        let expiry_choices = expiry_choices.clone();
        let group_row = group_row.clone();
        let groups = groups.clone();
        let toast_overlay = settings_toast_overlay.clone();
        std::rc::Rc::new(move || {
            let rules: Vec<phodder_core::filter::FilterRule> = rule_rows.borrow().iter().filter_map(FilterRuleRow::rule).collect();
            let Some(&delete_after_minutes) = expiry_choices.get(expiry_row.selected() as usize) else { return };

            // Read first so the re-evaluations below run only when their
            // field changed.
            let previous_state = phodder_core::open_connection(&db_path)
                .ok()
                .and_then(|conn| phodder_core::subscriptions::get_subscription(&conn, subscription_id).ok().flatten());

            if let Err(e) = save_subscription_settings(
                &db_path,
                subscription_id,
                auto_download_row.is_active(),
                notify_row.is_active(),
                &rules,
                delete_after_minutes,
            ) {
                tracing::error!("failed to auto-save subscription {subscription_id} settings: {e}");
                toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't save changes")));
                return;
            }

            // Re-applies retention now when expiry changes.
            if previous_state.as_ref().is_some_and(|s| s.delete_after_minutes != delete_after_minutes) {
                if let Err(e) = command_tx.send(UiCommand::ReevaluateSubscriptionRetention { subscription_id }) {
                    tracing::error!("failed to send ReevaluateSubscriptionRetention: {e}");
                }
            }
            // Dequeues episodes a new block rule matches; manual downloads
            // are exempt (see queued_non_manual_episodes_for_subscription).
            if previous_state.as_ref().is_some_and(|s| s.download_filter_rules != rules) {
                if let Err(e) = command_tx.send(UiCommand::ReevaluateQueuedEpisodesAgainstFilters { subscription_id }) {
                    tracing::error!("failed to send ReevaluateQueuedEpisodesAgainstFilters: {e}");
                }
            }
            if let Some(selected_group) = groups.get(group_row.selected() as usize) {
                let group_changed_result = phodder_core::open_connection(&db_path).and_then(|conn| {
                    let previous_group_id = phodder_core::subscriptions::get_subscription(&conn, subscription_id).ok().flatten().map(|s| s.group_id);
                    phodder_core::subscriptions::update_subscription_group(&conn, subscription_id, selected_group.id)?;
                    Ok(previous_group_id)
                });
                match group_changed_result {
                    Ok(previous_group_id) => {
                        // Queue order and labels follow the group, but
                        // nothing rebuilt the tabs; sent only when the group
                        // changed.
                        if previous_group_id != Some(selected_group.id) {
                            if let Err(e) = command_tx.send(UiCommand::NotifyDataChanged) {
                                tracing::error!("failed to send NotifyDataChanged after group change: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("failed to update group for subscription {subscription_id}: {e}");
                    }
                }
            }
        })
    };

    {
        let save_now = save_now.clone();
        group_row.connect_selected_notify(move |_| save_now());
    }
    {
        let save_now = save_now.clone();
        auto_download_row.connect_active_notify(move |_| save_now());
    }
    {
        let save_now = save_now.clone();
        notify_row.connect_active_notify(move |_| save_now());
    }
    {
        let save_now = save_now.clone();
        expiry_row.connect_selected_notify(move |_| save_now());
    }


    // Structured rows matching phodder_core::filter::FilterRule, then Add
    // Rule as the list's last row.
    let rules_container = gtk4::ListBox::builder().selection_mode(gtk4::SelectionMode::None).css_classes(["boxed-list"]).build();
    let add_rule_row = libadwaita::ButtonRow::builder().title(gettext("Add Rule")).start_icon_name("list-add-symbolic").build();
    for rule in &sub.download_filter_rules {
        build_filter_rule_row(Some(rule), &rules_container, &rule_rows, &save_now, &add_rule_row);
    }
    resync_filter_rule_visual_order(&rules_container, &rule_rows.borrow(), &add_rule_row);
    {
        let rules_container = rules_container.clone();
        let rule_rows = rule_rows.clone();
        let save_now = save_now.clone();
        add_rule_row.connect_activated(move |add_rule_row| {
            let rule_row = build_filter_rule_row(None, &rules_container, &rule_rows, &save_now, add_rule_row);
            resync_filter_rule_visual_order(&rules_container, &rule_rows.borrow(), add_rule_row);
            rule_row.pattern_row.grab_focus();
        });
    }

    let general_group = libadwaita::PreferencesGroup::new();
    general_group.add(&group_row);
    general_group.add(&auto_download_row);
    general_group.add(&notify_row);
    general_group.add(&expiry_row);
    let filters_group = libadwaita::PreferencesGroup::builder()
        .title(gettext("Download Filter Rules"))
        .description(gettext("Checked top to bottom; the first matching rule wins."))
        .build();
    filters_group.add(&rules_container);
    let settings_page = libadwaita::PreferencesPage::new();
    settings_page.add(&general_group);
    settings_page.add(&filters_group);
    settings_page.add(&build_chapter_skip_section(&db_path, subscription_id, &sub.chapter_skip_rules));
    settings_toast_overlay.set_child(Some(&settings_page));

    // Fetched the first time the Episodes tab is shown; the full feed, not
    // just stored episodes.
    let episodes_slot = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).vexpand(true).build();
    let episodes_toast_overlay = libadwaita::ToastOverlay::new();
    let episodes_loading_status = libadwaita::StatusPage::builder().title(gettext("Loading Episodes…")).icon_name("view-list-symbolic").vexpand(true).build();
    let episodes_loading_spinner = gtk4::Spinner::builder().spinning(true).halign(gtk4::Align::Center).width_request(32).height_request(32).build();
    set_accessible_label(&episodes_loading_spinner, &gettext("Loading episodes"));
    episodes_loading_status.set_child(Some(&episodes_loading_spinner));
    replace_slot_content(&episodes_slot, episodes_loading_status.upcast());
    episodes_toast_overlay.set_child(Some(&episodes_slot));

    let details_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(12)
        .margin_start(16)
        .margin_end(16)
        .margin_top(16)
        .margin_bottom(16)
        .build();
    fill_podcast_info(&details_box, &subscription_thumbnail, &title_toggle_row, sub.description.as_deref(), &sub.feed_url);
    let details_scrolled = gtk4::ScrolledWindow::builder().child(&details_box).hscrollbar_policy(gtk4::PolicyType::Never).vexpand(true).build();

    let view_stack = libadwaita::ViewStack::builder().vexpand(true).build();
    view_stack.add_titled(&details_scrolled, Some("details"), "Details");
    view_stack.add_titled(&episodes_toast_overlay, Some("episodes"), "Episodes");
    view_stack.add_titled(&settings_toast_overlay, Some("settings"), "Settings");
    view_stack.set_visible_child_name(start_tab);

    // The tabs are the header's title. AdwInlineViewSwitcher, labels only:
    // AdwViewSwitcher always reserves an icon and the tabs were cut off.
    let view_switcher = libadwaita::InlineViewSwitcher::builder().stack(&view_stack).css_classes(["flat"]).build();

    let dialog_content = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).build();
    dialog_content.append(&view_stack);

    // Delete left. A new subscription has Cancel there and Done, which
    // confirms it, on the right; everything else saves as it changes and
    // closes from the header.
    let footer_separator = gtk4::Separator::new(gtk4::Orientation::Horizontal);
    dialog_content.append(&footer_separator);
    let action_row = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(8).margin_start(12).margin_end(12).margin_top(8).margin_bottom(8).build();
    action_row.append(&delete_button);
    let done_button = gtk4::Button::builder()
        .label(gettext("Done"))
        .css_classes(["suggested-action"])
        .hexpand(true)
        .halign(gtk4::Align::End)
        .visible(is_new_subscription)
        .build();
    action_row.append(&done_button);
    dialog_content.append(&action_row);

    // Delete/Cancel hides on Episodes, and with it the footer when Delete is
    // all it holds. A separate handler from the lazy fetch, which returns
    // early after the first run.
    {
        let delete_button = delete_button.clone();
        let action_row = action_row.clone();
        let footer_separator = footer_separator.clone();
        let update = move |stack: &libadwaita::ViewStack| {
            let on_episodes = stack.visible_child_name().as_deref() == Some("episodes");
            delete_button.set_visible(!on_episodes);
            let footer = is_new_subscription || !on_episodes;
            action_row.set_visible(footer);
            footer_separator.set_visible(footer);
        };
        update(&view_stack);
        view_stack.connect_visible_child_name_notify(update);
    }

    // At most once per dialog.
    let episode_list_already_fetched = std::rc::Rc::new(std::cell::Cell::new(false));
    {
        let episode_list_already_fetched = episode_list_already_fetched.clone();
        let episodes_slot = episodes_slot.clone();
        let episodes_toast_overlay = episodes_toast_overlay.clone();
        let command_tx = command_tx.clone();
        let feed_url = sub.feed_url.clone();
        let subscription_id = sub.id;
        let sub_artwork_local_path = sub.artwork_local_path.clone();
        let db_path = db_path.clone();
        view_stack.connect_visible_child_name_notify(move |stack| {
            let visible_name = stack.visible_child_name();
            if visible_name.as_deref() != Some("episodes") {
                return;
            }
            if episode_list_already_fetched.get() {
                return;
            }
            episode_list_already_fetched.set(true);

            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            if let Err(e) = command_tx.send(UiCommand::FetchFullEpisodeList { feed_url: feed_url.clone(), reply_tx }) {
                tracing::error!("failed to send FetchFullEpisodeList command: {e}");
                replace_slot_content(&episodes_slot, placeholder_page(&gettext("Couldn't Load Episodes"), "dialog-error-symbolic").upcast());
                return;
            }

            let episodes_slot = episodes_slot.clone();
            let episodes_toast_overlay = episodes_toast_overlay.clone();
            let command_tx = command_tx.clone();
            let db_path = db_path.clone();
            let sub_artwork_local_path = sub_artwork_local_path.clone();
            gtk4::glib::spawn_future_local(async move {
                let outcome = match reply_rx.await {
                    Ok(inner) => inner,
                    Err(_) => Err(gettext("the backend didn't respond")),
                };
                match outcome {
                    Ok(parsed_feed) => {
                        let list_widget = build_back_catalog_episode_list(
                            std::rc::Rc::new(parsed_feed.episodes),
                            subscription_id,
                            std::rc::Rc::new(sub_artwork_local_path.clone()),
                            db_path.clone(),
                            command_tx.clone(),
                            episodes_toast_overlay.clone(),
                            episodes_slot.clone(),
                            0,
                        );
                        replace_slot_content(&episodes_slot, list_widget);
                    }
                    Err(e) => {
                        tracing::warn!("failed to fetch full episode list for subscription {subscription_id}: {e}");
                        replace_slot_content(&episodes_slot, placeholder_page(&format!("Couldn't load episodes: {e}"), "dialog-error-symbolic").upcast());
                    }
                }
            });
        });
    }

    // A new subscription can't be dismissed by clicking away; nothing is
    // configured yet.
    let presentation =
        present_dialog_with_header(
            &dialog_content,
            &gettext("Subscription"),
            HeaderTitle::Widget(view_switcher.upcast_ref()),
            420,
            500,
            parent,
            dialog_sheet,
            !is_new_subscription,
        );

    // Manage Groups replaces this dialog, which is rebuilt when it closes so
    // group_row picks up changes. Owned clones because the callback is
    // 'static.
    {
        let presentation = presentation.clone();
        let parent = parent.clone();
        let db_path = db_path.clone();
        let subscriptions_slot = subscriptions_slot.clone();
        let tokio_handle = tokio_handle.clone();
        let is_narrow = is_narrow.clone();
        let command_tx = command_tx.clone();
        let dialog_sheet = dialog_sheet.clone();
        manage_groups_button.connect_clicked(move |_| {
            presentation.close();
            let db_path = db_path.clone();
            let subscriptions_slot = subscriptions_slot.clone();
            let tokio_handle = tokio_handle.clone();
            let is_narrow = is_narrow.clone();
            let command_tx = command_tx.clone();
            let command_tx_for_manage_groups = command_tx.clone();
            let on_close_parent = parent.clone();
            let on_close_dialog_sheet = dialog_sheet.clone();
            show_manage_groups_dialog(
                &parent,
                db_path.clone(),
                &dialog_sheet,
                Some(std::rc::Rc::new(move || {
                    show_subscription_detail(
                        &on_close_parent,
                        db_path.clone(),
                        subscription_id,
                        subscriptions_slot.clone(),
                        tokio_handle.clone(),
                        is_narrow.clone(),
                        command_tx.clone(),
                        is_new_subscription,
                        &on_close_dialog_sheet,
                        "settings",
                    );
                })),
                command_tx_for_manage_groups,
            );
        });
    }

    {
        let presentation = presentation.clone();
        let command_tx = command_tx.clone();
        done_button.connect_clicked(move |_| {
            // For a new subscription Done also starts the deferred first
            // refresh; everything else is already saved.
            if is_new_subscription {
                if let Err(e) = command_tx.send(UiCommand::RefreshSubscription(subscription_id)) {
                    tracing::error!("failed to send RefreshSubscription command: {e}");
                }
            }
            presentation.close();
        });
    }

    let delete_confirm_armed = std::rc::Rc::new(std::cell::Cell::new(false));
    {
        let confirm_armed = delete_confirm_armed.clone();
        let presentation = presentation.clone();
        let db_path = db_path.clone();
        let tokio_handle = tokio_handle.clone();
        let dialog_sheet = dialog_sheet.clone();
        let toast_overlay = settings_toast_overlay.clone();
        delete_button.connect_clicked(move |button| {
            // No confirm step for Cancel: nothing has downloaded yet.
            if !is_new_subscription && !confirm_armed.get() {
                confirm_armed.set(true);
                button.set_label(&gettext("Click again to confirm"));
                announce(button, &gettext("Press again to delete this podcast"));
                // One clone per closure.
                let is_armed_confirm_armed = confirm_armed.clone();
                let clear_armed_confirm_armed = confirm_armed.clone();
                let button_for_expiry = button.clone();
                auto_expire_confirm_arm(
                    move || is_armed_confirm_armed.get(),
                    move || clear_armed_confirm_armed.set(false),
                    move || button_for_expiry.set_label(&if is_new_subscription { gettext("Cancel") } else { gettext("Delete") }),
                );
                return;
            }
            match delete_subscription_and_files(&db_path, subscription_id) {
                Ok(()) => {
                    build_subscriptions_page(&tokio_handle, &db_path, &subscriptions_slot, &is_narrow, &command_tx, &dialog_sheet);
                    presentation.close();
                }
                Err(e) => {
                    tracing::error!("failed to delete subscription {subscription_id}: {e}");
                    toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't delete subscription")));
                    confirm_armed.set(false);
                    button.set_label(&if is_new_subscription { gettext("Cancel") } else { gettext("Delete") });
                }
            }
        });
    }
}

pub(super) fn save_subscription_settings(
    db_path: &std::path::Path,
    subscription_id: i64,
    auto_download_enabled: bool,
    notify_on_new_episodes: bool,
    rules: &[phodder_core::filter::FilterRule],
    delete_after_minutes: i64,
) -> rusqlite::Result<()> {
    let conn = phodder_core::open_connection(db_path)?;
    phodder_core::subscriptions::update_subscription_settings(
        &conn,
        subscription_id,
        auto_download_enabled,
        notify_on_new_episodes,
        rules,
        delete_after_minutes,
    )
}

pub(super) fn load_subscription(
    db_path: &std::path::Path,
    subscription_id: i64,
) -> rusqlite::Result<Option<phodder_core::models::Subscription>> {
    let conn = phodder_core::open_connection(db_path)?;
    phodder_core::subscriptions::get_subscription(&conn, subscription_id)
}

/// Deletes the subscription's downloaded files; a missing file is fine.
/// Shared by the tombstoning and hard delete paths.
pub(super) fn delete_subscription_episode_files(conn: &rusqlite::Connection, subscription_id: i64) -> rusqlite::Result<()> {
    let file_paths = phodder_core::subscriptions::episode_file_paths_for_subscription(conn, subscription_id)?;
    for path in file_paths {
        if let Err(e) = std::fs::remove_file(&path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("failed to delete file {path} while removing subscription {subscription_id}: {e}");
            }
        }
    }
    Ok(())
}

/// Deletes a subscription and its files, tombstoning the row when a sync
/// account is configured.
pub(crate) fn delete_subscription_and_files(db_path: &std::path::Path, subscription_id: i64) -> rusqlite::Result<()> {
    let conn = phodder_core::open_connection(db_path)?;
    stop_playback_if_current(phodder_core::subscriptions::episode_ids_for_subscription(&conn, subscription_id)?);
    // Downloads are cancelled first: episodes.id has no AUTOINCREMENT, so a
    // finished stale download could write to a reused id belonging to another
    // episode. Covers every episode, since one mid-download has no file path
    // yet.
    for episode_id in phodder_core::subscriptions::episode_ids_for_subscription(&conn, subscription_id)? {
        cancel_download(episode_id);
    }
    delete_subscription_episode_files(&conn, subscription_id)?;
    // Tombstoned only when a sync account needs to hear about the removal.
    match phodder_core::sync_accounts::get_enabled_sync_account(&conn)? {
        Some(_) => phodder_core::subscriptions::tombstone_subscription(&conn, subscription_id),
        None => phodder_core::subscriptions::delete_subscription(&conn, subscription_id),
    }
}

/// Always a hard delete, for removals that came from the sync server and need
/// no reporting.
pub(crate) fn delete_subscription_and_files_hard(db_path: &std::path::Path, subscription_id: i64) -> rusqlite::Result<()> {
    let conn = phodder_core::open_connection(db_path)?;
    // Cancels downloads first; see delete_subscription_and_files. Called from
    // sync in the service, where the downloader runs.
    for episode_id in phodder_core::subscriptions::episode_ids_for_subscription(&conn, subscription_id)? {
        crate::downloader::cancel_download(episode_id);
    }
    delete_subscription_episode_files(&conn, subscription_id)?;
    phodder_core::subscriptions::delete_subscription(&conn, subscription_id)
}

/// "Skip Chapters": chapter-title patterns, matched like download filter
/// patterns. Saved on every change, separately from the other settings.
fn build_chapter_skip_section(db_path: &std::path::Path, subscription_id: i64, rules: &[phodder_core::filter::FilterRule]) -> libadwaita::PreferencesGroup {
    let group = libadwaita::PreferencesGroup::builder()
        .title(gettext("Skip Chapters"))
        .description(gettext("Chapters whose titles match are skipped during playback. Seeking into one plays it."))
        .build();
    let list = gtk4::ListBox::builder().selection_mode(gtk4::SelectionMode::None).css_classes(["boxed-list"]).build();
    group.add(&list);

    let patterns: std::rc::Rc<std::cell::RefCell<Vec<String>>> = std::rc::Rc::new(std::cell::RefCell::new(rules.iter().map(|r| r.pattern.clone()).collect()));
    let save: std::rc::Rc<dyn Fn()> = {
        let db_path = db_path.to_path_buf();
        let patterns = patterns.clone();
        std::rc::Rc::new(move || {
            let rules: Vec<phodder_core::filter::FilterRule> = patterns
                .borrow()
                .iter()
                .map(|pattern| phodder_core::filter::FilterRule {
                    pattern: pattern.clone(),
                    target: phodder_core::filter::MatchTarget::Title,
                    action: phodder_core::filter::Action::Block,
                })
                .collect();
            let result = phodder_core::open_connection(&db_path).and_then(|conn| phodder_core::subscriptions::set_chapter_skip_rules(&conn, subscription_id, &rules));
            if let Err(e) = result {
                tracing::error!("failed to save chapter skip rules for subscription {subscription_id}: {e}");
            }
        })
    };

    // Adding is the list's last row.
    let add_row = libadwaita::EntryRow::builder().title(gettext("Add a chapter title, e.g. sponsor")).show_apply_button(true).build();

    // Rows go before the add row; each removes its own pattern.
    let append_pattern_row: std::rc::Rc<dyn Fn(String)> = {
        let list = list.clone();
        let add_row = add_row.clone();
        let patterns = patterns.clone();
        let save = save.clone();
        std::rc::Rc::new(move |pattern: String| {
            let row = libadwaita::ActionRow::builder().title(&pattern).use_markup(false).build();
            let remove = gtk4::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text(gettext_f("Remove “{pattern}”", &[("pattern", &pattern)]))
                .css_classes(["flat"])
                .valign(gtk4::Align::Center)
                .build();
            row.add_suffix(&remove);
            list.insert(&row, add_row.index());
            let list = list.clone();
            let patterns = patterns.clone();
            let save = save.clone();
            remove.connect_clicked(move |_| {
                patterns.borrow_mut().retain(|p| *p != pattern);
                list.remove(&row);
                save();
            });
        })
    };
    list.append(&add_row);
    for pattern in patterns.borrow().iter() {
        append_pattern_row(pattern.clone());
    }
    add_row.connect_apply(move |add_row| {
        let pattern = add_row.text().trim().to_string();
        if pattern.is_empty() || patterns.borrow().contains(&pattern) {
            return;
        }
        patterns.borrow_mut().push(pattern.clone());
        append_pattern_row(pattern);
        add_row.set_text("");
        save();
    });
    group
}

/// A podcast's Details content, shared with Add Podcast's preview: artwork,
/// the title widget, the description and the feed URL with a copy button.
pub(super) fn fill_podcast_info(
    container: &gtk4::Box,
    artwork: &gtk4::Image,
    title: &impl IsA<gtk4::Widget>,
    description_html: Option<&str>,
    feed_url: &str,
) -> gtk4::Label {
    container.append(artwork);
    container.append(title);
    // Not selectable, like show notes: touch drags would select instead of
    // scrolling.
    let description_label = gtk4::Label::builder().halign(gtk4::Align::Start).xalign(0.0).wrap(true).build();
    set_podcast_description(&description_label, description_html);
    container.append(&description_label);
    let feed_url_row = libadwaita::ActionRow::builder().title(gettext("Feed URL")).subtitle(&escape_markup(feed_url)).build();
    let copy_feed_url_button = gtk4::Button::builder()
        .icon_name("edit-copy-symbolic")
        .tooltip_text(gettext("Copy Feed URL"))
        .css_classes(["flat"])
        .valign(gtk4::Align::Center)
        .build();
    {
        let feed_url = feed_url.to_string();
        copy_feed_url_button.connect_clicked(move |button| button.clipboard().set_text(&feed_url));
    }
    feed_url_row.add_suffix(&copy_feed_url_button);
    let feed_rows = gtk4::ListBox::builder().selection_mode(gtk4::SelectionMode::None).css_classes(["boxed-list"]).build();
    feed_rows.append(&feed_url_row);
    container.append(&feed_rows);
    description_label
}

pub(super) fn set_podcast_description(label: &gtk4::Label, description_html: Option<&str>) {
    label.remove_css_class("dim-label");
    match description_html {
        Some(html) if !html.is_empty() => match html2pango::markup_html(html) {
            Ok(markup) => label.set_markup(&markup),
            // Malformed feed HTML falls back to raw text.
            Err(e) => {
                tracing::warn!("failed to convert podcast description HTML to Pango markup: {e}");
                label.set_label(html);
            }
        },
        _ => {
            label.set_label(&gettext("No description available."));
            label.add_css_class("dim-label");
        }
    }
}
