use super::*;

thread_local! {
    static DELETE_ALL_HISTORY_CONFIRM_ARMED: std::cell::Cell<bool> = std::cell::Cell::new(false);
    static CLEAR_ALL_HISTORY_CONFIRM_ARMED: std::cell::Cell<bool> = std::cell::Cell::new(false);
}

/// The History tab: `list_history_episodes` (played on this device, newest
/// `played_order` first). Each row has one action (Delete, then Clear);
/// tapping a row opens details in History mode, which adds Clear. A sticky
/// row below the list holds Delete All Files and Clear All.
pub(super) fn build_history_page_from_data(
    episodes: Vec<phodder_core::subscriptions::EpisodeSummary>,
    tokio_handle: &tokio::runtime::Handle,
    db_path: &std::path::Path,
    command_tx: &UnboundedSender<UiCommand>,
    toast_overlay: &libadwaita::ToastOverlay,
    history_slot: &gtk4::Box,
) -> gtk4::Widget {
    if episodes.is_empty() {
        return placeholder_page(&gettext("History"), "media-playlist-consecutive-symbolic").upcast();
    }
    let any_files = episodes.iter().any(|e| e.download_status == "downloaded");

    let list_box = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .margin_start(12)
        .margin_end(12)
        .margin_top(12)
        .margin_bottom(12)
        .build();


    for episode in episodes {
        let duration_display = episode.duration_ms.map(format_duration_ms);
        let size_display = display_file_size(episode.file_size_bytes);
        let second_line = join_non_empty(&[
            duration_display.as_deref(),
            size_display.as_deref(),
            display_download_status(&episode.download_status).as_deref(),
        ]);
        let subtitle_text =
            format!("{} · {}\n{}", escape_markup(&episode.podcast_title), format_publish_date(&episode.publish_date), second_line);

        let episode_row = libadwaita::ActionRow::builder()
            .activatable(true)
            .title(&escape_markup(&episode.title))
            .subtitle(&subtitle_text)
            .title_lines(1)
            .subtitle_lines(1)
            .build();
        {
            let episode_id = episode.id;
            episode_row.connect_activated(move |_| open_episode_details_in(episode_id, DetailsMode::History));
        }
        let thumbnail = load_thumbnail_image(
            &[episode.artwork_local_path.as_deref(), episode.subscription_artwork_local_path.as_deref()],
            "audio-x-generic-symbolic",
            64,
        );
        episode_row.add_prefix(&thumbnail);

        // One action: Delete while the file exists, then Clear, which takes
        // a second tap.
        let has_file = episode.download_status == "downloaded";
        let (icon, tooltip) = if has_file { ("user-trash-symbolic", "Delete Download") } else { ("edit-clear-symbolic", "Clear from History") };
        let action_button = gtk4::Button::builder().icon_name(icon).tooltip_text(tooltip).css_classes(["flat"]).valign(gtk4::Align::Center).build();
        episode_row.add_suffix(&action_button);
        {
            let db_path = db_path.to_path_buf();
            let command_tx = command_tx.clone();
            let toast_overlay = toast_overlay.clone();
            let episode_id = episode.id;
            let armed = std::rc::Rc::new(std::cell::Cell::new(false));
            let button = action_button.clone();
            wire_suffix_button_action(&action_button, move || {
                if !has_file && !armed.get() {
                    armed.set(true);
                    button.set_icon_name("object-select-symbolic");
                    button.set_tooltip_text(Some(&gettext("Tap again to clear")));
                    announce(&button, &gettext("Press again to clear"));
                    let armed_check = armed.clone();
                    let armed_clear = armed.clone();
                    let button = button.clone();
                    auto_expire_confirm_arm(
                        move || armed_check.get(),
                        move || armed_clear.set(false),
                        move || {
                            button.set_icon_name("edit-clear-symbolic");
                            button.set_tooltip_text(Some(&gettext("Clear from History")));
                        },
                    );
                    return;
                }
                let result = if has_file { delete_episode_file(&db_path, episode_id) } else { clear_history_episode(&db_path, episode_id) };
                match result {
                    // Deleting can dequeue, so every tab refreshes.
                    Ok(()) => {
                        if let Err(e) = command_tx.send(UiCommand::NotifyDataChanged) {
                            tracing::error!("failed to send NotifyDataChanged after History action: {e}");
                        }
                    }
                    Err(e) => {
                        tracing::error!("History action failed for episode {episode_id}: {e}");
                        toast_overlay.add_toast(libadwaita::Toast::new(&if has_file { gettext("Couldn't delete episode") } else { gettext("Couldn't clear episode") }));
                    }
                }
            });
        }

        list_box.append(&episode_row);
    }

    let bottom_divider_row = gtk4::ListBoxRow::builder()
        .activatable(false)
        .selectable(false)
        .focusable(false)
        .accessible_role(gtk4::AccessibleRole::Presentation)
        .child(&gtk4::Box::new(gtk4::Orientation::Horizontal, 0))
        .build();
    list_box.append(&bottom_divider_row);

    let scrolled = gtk4::ScrolledWindow::builder().child(&list_box).vexpand(true).build();

    let bulk_action_row = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk4::Align::Center)
        .margin_top(8)
        .margin_bottom(8)
        .build();
    let delete_all_button = gtk4::Button::builder().label(gettext("Delete All Files")).css_classes(["destructive-action"]).build();
    let clear_all_button = gtk4::Button::builder().css_classes(["destructive-action"]).build();
    let clear_all_label = gtk4::Label::new(Some(&gettext("Clear All")));
    {
        let content = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(6).build();
        let icon = decorative_image();
        icon.set_icon_name(Some("edit-clear-all-symbolic"));
        content.append(&icon);
        content.append(&clear_all_label);
        clear_all_button.set_child(Some(&content));
    }
    delete_all_button.set_sensitive(any_files);
    if !any_files {
        delete_all_button.set_tooltip_text(Some(&gettext("No downloaded files in History")));
        DELETE_ALL_HISTORY_CONFIRM_ARMED.with(|c| c.set(false));
    }
    if DELETE_ALL_HISTORY_CONFIRM_ARMED.with(|c| c.get()) {
        delete_all_button.set_label(&gettext("Click again to confirm"));
    }
    if CLEAR_ALL_HISTORY_CONFIRM_ARMED.with(|c| c.get()) {
        clear_all_label.set_label(&gettext("Click again to confirm"));
    }
    bulk_action_row.append(&delete_all_button);
    bulk_action_row.append(&clear_all_button);

    {
        let tokio_handle = tokio_handle.clone();
        let db_path = db_path.to_path_buf();
        let toast_overlay = toast_overlay.clone();
        let history_slot = history_slot.clone();
        let command_tx = command_tx.clone();
        delete_all_button.connect_clicked(move |button| {
            if !DELETE_ALL_HISTORY_CONFIRM_ARMED.with(|c| c.get()) {
                DELETE_ALL_HISTORY_CONFIRM_ARMED.with(|c| c.set(true));
                button.set_label(&gettext("Click again to confirm"));
                announce(button, &gettext("Press again to delete all files"));
                let button_for_expiry = button.clone();
                auto_expire_confirm_arm(
                    || DELETE_ALL_HISTORY_CONFIRM_ARMED.with(|c| c.get()),
                    || DELETE_ALL_HISTORY_CONFIRM_ARMED.with(|c| c.set(false)),
                    move || button_for_expiry.set_label(&gettext("Delete All Files")),
                );
                return;
            }
            DELETE_ALL_HISTORY_CONFIRM_ARMED.with(|c| c.set(false));
            button.set_label(&gettext("Delete All Files"));
            match delete_all_history_files(&db_path) {
                Ok(deleted) => {
                    let count = deleted.to_string();
                    toast_overlay.add_toast(libadwaita::Toast::new(&crate::i18n::ngettext_f(
                        "Deleted {count} file",
                        "Deleted {count} files",
                        deleted as u32,
                        &[("count", &count)],
                    )));
                    // Deleting can dequeue, so every tab refreshes.
                    if let Err(e) = command_tx.send(UiCommand::NotifyDataChanged) {
                        tracing::error!("failed to send NotifyDataChanged after deleting History files: {e}");
                    }
                    build_history_page(&tokio_handle, &history_slot, &db_path, &command_tx, &toast_overlay);
                }
                Err(e) => {
                    tracing::error!("failed to delete all history files: {e}");
                    toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't delete files")));
                }
            }
        });
    }
    {
        let tokio_handle = tokio_handle.clone();
        let db_path = db_path.to_path_buf();
        let toast_overlay = toast_overlay.clone();
        let history_slot = history_slot.clone();
        let command_tx = command_tx.clone();
        let clear_all_label = clear_all_label.clone();
        clear_all_button.connect_clicked(move |button| {
            if !CLEAR_ALL_HISTORY_CONFIRM_ARMED.with(|c| c.get()) {
                CLEAR_ALL_HISTORY_CONFIRM_ARMED.with(|c| c.set(true));
                clear_all_label.set_label(&gettext("Click again to confirm"));
                announce(button, &gettext("Press again to clear all history"));
                let clear_all_label_for_expiry = clear_all_label.clone();
                auto_expire_confirm_arm(
                    || CLEAR_ALL_HISTORY_CONFIRM_ARMED.with(|c| c.get()),
                    || CLEAR_ALL_HISTORY_CONFIRM_ARMED.with(|c| c.set(false)),
                    move || clear_all_label_for_expiry.set_label(&gettext("Clear All")),
                );
                return;
            }
            CLEAR_ALL_HISTORY_CONFIRM_ARMED.with(|c| c.set(false));
            clear_all_label.set_label(&gettext("Clear All"));
            match clear_all_history(&db_path) {
                Ok(()) => {
                    if let Err(e) = command_tx.send(UiCommand::NotifyDataChanged) {
                        tracing::error!("failed to send NotifyDataChanged after clearing History: {e}");
                    }
                    build_history_page(&tokio_handle, &history_slot, &db_path, &command_tx, &toast_overlay);
                }
                Err(e) => {
                    tracing::error!("failed to clear all history: {e}");
                    toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't clear History")));
                }
            }
        });
    }

    let outer = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).build();
    outer.append(&scrolled);
    outer.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
    outer.append(&bulk_action_row);
    outer.upcast()
}

/// Loads history and swaps in the tab's widget, as build_queue_page does.
pub(super) fn build_history_page(
    tokio_handle: &tokio::runtime::Handle,
    history_slot: &gtk4::Box,
    db_path: &std::path::Path,
    command_tx: &UnboundedSender<UiCommand>,
    toast_overlay: &libadwaita::ToastOverlay,
) {
    let history_slot_for_content = history_slot.clone();
    let history_slot_for_build = history_slot.clone();
    let db_path_owned = db_path.to_path_buf();
    let tokio_handle_for_completion = tokio_handle.clone();
    let command_tx = command_tx.clone();
    let toast_overlay = toast_overlay.clone();
    run_db_op(
        tokio_handle,
        db_path.to_path_buf(),
        |conn| phodder_core::subscriptions::list_history_episodes(conn).map_err(anyhow::Error::from),
        move |result| match result {
            Ok(episodes) => {
                if slot_data_unchanged(&history_slot_for_content, &episodes) {
                    return;
                }
                let widget = build_history_page_from_data(
                    episodes,
                    &tokio_handle_for_completion,
                    &db_path_owned,
                    &command_tx,
                    &toast_overlay,
                    &history_slot_for_build,
                );
                replace_slot_content(&history_slot_for_content, widget);
            }
            Err(e) => tracing::error!("failed to load history list: {e}"),
        },
    );
}

/// Deletes the file if present, then the row.
pub(super) fn clear_history_episode(db_path: &std::path::Path, episode_id: i64) -> rusqlite::Result<()> {
    delete_episode_file(db_path, episode_id)?;
    let conn = phodder_core::open_connection(db_path)?;
    phodder_core::subscriptions::clear_from_history(&conn, episode_id)
}

/// Deletes every History episode's file.
/// Deletes the file of every History episode that has one; returns how many.
pub(super) fn delete_all_history_files(db_path: &std::path::Path) -> rusqlite::Result<usize> {
    let conn = phodder_core::open_connection(db_path)?;
    let episode_ids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM episodes WHERE played_locally = 1 AND download_status = 'downloaded'")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        rows.collect::<rusqlite::Result<Vec<i64>>>()?
    };
    drop(conn);
    for &episode_id in &episode_ids {
        delete_episode_file(db_path, episode_id)?;
    }
    Ok(episode_ids.len())
}

/// Deletes every History episode's file, then every row.
pub(super) fn clear_all_history(db_path: &std::path::Path) -> rusqlite::Result<()> {
    let conn = phodder_core::open_connection(db_path)?;
    let episode_ids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM episodes WHERE played_locally = 1")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        rows.collect::<rusqlite::Result<Vec<i64>>>()?
    };
    drop(conn);
    for episode_id in episode_ids {
        clear_history_episode(db_path, episode_id)?;
    }
    Ok(())
}
