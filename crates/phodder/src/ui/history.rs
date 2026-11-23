use super::*;

/// The History tab: `list_history_episodes` (played on this device, newest
/// `played_order` first). Each row has one action (Delete, then Clear);
/// tapping a row opens details in History mode, which adds Clear.
pub(super) fn build_history_page_from_data(
    episodes: Vec<phodder_core::subscriptions::EpisodeSummary>,
    db_path: &std::path::Path,
    command_tx: &UnboundedSender<UiCommand>,
    toast_overlay: &libadwaita::ToastOverlay,
) -> gtk4::Widget {
    let selection = selection::scope("history");
    if episodes.is_empty() {
        selection.set_active(false);
        return placeholder_page(&gettext("History"), "media-playlist-consecutive-symbolic").upcast();
    }
    selection.begin_page(db_path, command_tx, toast_overlay);

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
            let selection = selection.clone();
            episode_row.connect_activated(move |_| {
                if !selection.handle_activate(&selection::Item::Episode(episode_id)) {
                    open_episode_details_in(episode_id, DetailsMode::History);
                }
            });
        }
        let thumbnail = load_thumbnail_image(
            &[episode.artwork_local_path.as_deref(), episode.subscription_artwork_local_path.as_deref()],
            "audio-x-generic-symbolic",
            64,
        );
        episode_row.add_prefix(&thumbnail);
        selection.attach_row(&episode_row, selection::Item::Episode(episode.id));

        // One action: Delete while the file exists, then Clear.
        let has_file = episode.download_status == "downloaded";
        let (icon, tooltip) = if has_file { ("user-trash-symbolic", "Delete Download") } else { ("edit-clear-symbolic", "Clear from History") };
        let action_button = gtk4::Button::builder().icon_name(icon).tooltip_text(tooltip).css_classes(["flat"]).valign(gtk4::Align::Center).build();
        episode_row.add_suffix(&action_button);
        {
            let db_path = db_path.to_path_buf();
            let command_tx = command_tx.clone();
            let toast_overlay = toast_overlay.clone();
            let episode_id = episode.id;
            wire_suffix_button_action(&action_button, move || {
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

    selection.wrap(&scrolled, None, false).upcast()
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
    let db_path_owned = db_path.to_path_buf();
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
                    &db_path_owned,
                    &command_tx,
                    &toast_overlay,
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
