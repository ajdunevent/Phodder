use super::*;

/// Summarizes an OPML import in one toast instead of one per feed.
#[derive(Debug)]
pub struct OpmlImportSummary {
    pub imported: usize,
    pub already_subscribed: usize,
    pub failed: usize,
}

pub(super) fn show_add_feed_dialog(
    parent: &libadwaita::ApplicationWindow,
    db_path: std::path::PathBuf,
    tokio_handle: tokio::runtime::Handle,
    command_tx: UnboundedSender<UiCommand>,
    subscriptions_slot: gtk4::Box,
    is_narrow: std::rc::Rc<std::cell::Cell<bool>>,
    dialog_sheet: &libadwaita::BottomSheet,
) {
    // A plain entry. input-purpose tuning and a custom long-press gesture for
    // touch paste were reverted: the field should behave like any platform
    // entry, and broken long-press selection is a GTK/Phosh issue to report
    // upstream.
    let entry = gtk4::Entry::builder()
        .hexpand(true)
        .placeholder_text("https://example.com/feed.xml")
        .build();
    set_accessible_label(&entry, &gettext("Feed URL"));

    // Paste button inside the entry, since long-press paste isn't reachable
    // on the target device. Replaces the whole contents: the field is almost
    // always empty.
    entry.set_icon_from_icon_name(gtk4::EntryIconPosition::Secondary, Some("edit-paste-symbolic"));
    entry.set_icon_tooltip_text(gtk4::EntryIconPosition::Secondary, Some(&gettext("Paste")));
    entry.set_icon_activatable(gtk4::EntryIconPosition::Secondary, true);
    {
        let entry = entry.clone();
        entry.connect_icon_press(move |entry, icon_pos| {
            if icon_pos != gtk4::EntryIconPosition::Secondary {
                return;
            }
            let entry = entry.clone();
            let clipboard = entry.display().clipboard();
            gtk4::glib::spawn_future_local(async move {
                match clipboard.read_text_future().await {
                    Ok(Some(text)) => entry.set_text(&text),
                    Ok(None) => tracing::debug!("clipboard has no text to paste"),
                    Err(e) => tracing::warn!("failed to read clipboard for paste: {e}"),
                }
            });
        });
    }

    let add_button =
        gtk4::Button::builder().label(gettext("Add")).css_classes(["suggested-action"]).build();
    // Shown during the preview fetch, so a bad URL or unparseable feed fails
    // before a subscription exists.
    let loading_spinner = gtk4::Spinner::builder().visible(false).build();
    set_accessible_label(&loading_spinner, &gettext("Loading"));

    let button_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk4::Align::End)
        .build();
    button_box.append(&loading_spinner);
    button_box.append(&add_button);

    let content_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(12)
        .margin_start(16)
        .margin_end(16)
        .margin_top(16)
        .margin_bottom(16)
        .build();
    content_box.append(&entry);
    content_box.append(&button_box);

    let toast_overlay = libadwaita::ToastOverlay::new();
    toast_overlay.set_child(Some(&content_box));

    let presentation = present_dialog_adaptively(&toast_overlay, &gettext("Add Feed URL"), 400, 300, parent, dialog_sheet, true);

    {
        let presentation = presentation.clone();
        let toast_overlay = toast_overlay.clone();
        let parent = parent.clone();
        let subscriptions_slot = subscriptions_slot.clone();
        let tokio_handle = tokio_handle.clone();
        let is_narrow = is_narrow.clone();
        let entry = entry.clone();
        let loading_spinner = loading_spinner.clone();
        let dialog_sheet = dialog_sheet.clone();
        add_button.connect_clicked(move |button| {
            let url = entry.text().to_string();
            let url = url.trim().to_string();
            if url.is_empty() {
                toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Enter a feed URL first")));
                return;
            }

            button.set_sensitive(false);
            entry.set_sensitive(false);
            loading_spinner.set_visible(true);
            loading_spinner.start();

            // A one-off reply channel for this dialog, not the shared
            // UiUpdate stream.
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            if let Err(e) = command_tx.send(UiCommand::FetchFeedPreview { url: url.clone(), directory: false, reply_tx }) {
                tracing::error!("failed to send FetchFeedPreview command: {e}");
                toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't reach the backend to fetch the feed")));
                button.set_sensitive(true);
                entry.set_sensitive(true);
                loading_spinner.set_visible(false);
                loading_spinner.stop();
                return;
            }

            let presentation = presentation.clone();
            let toast_overlay = toast_overlay.clone();
            let parent = parent.clone();
            let db_path = db_path.clone();
            let command_tx = command_tx.clone();
            let subscriptions_slot = subscriptions_slot.clone();
            let tokio_handle = tokio_handle.clone();
            let is_narrow = is_narrow.clone();
            let button = button.clone();
            let entry = entry.clone();
            let loading_spinner = loading_spinner.clone();
            let dialog_sheet = dialog_sheet.clone();
            gtk4::glib::spawn_future_local(async move {
                let outcome = match reply_rx.await {
                    Ok(inner) => inner,
                    // The backend task dropped the channel, for example by
                    // panicking.
                    Err(_) => Err(gettext("the backend didn't respond")),
                };

                match outcome {
                    Ok(preview) => match add_subscription_with_preview(&db_path, &url, &preview) {
                        Ok(new_id) => {
                            presentation.close();
                            // Rebuilt at once: dismissing Manage Subscription
                            // any way but Save left the new subscription
                            // inserted but invisible.
                            build_subscriptions_page(&tokio_handle, &db_path, &subscriptions_slot, &is_narrow, &command_tx, &dialog_sheet);
                            // Opens Manage Subscription with the real title
                            // so settings can be set before anything
                            // downloads; the first refresh, which queues
                            // episodes, waits for Save (is_new_subscription).
                            show_subscription_detail(
                                &parent,
                                db_path.clone(),
                                new_id,
                                subscriptions_slot.clone(),
                                tokio_handle.clone(),
                                is_narrow.clone(),
                                command_tx.clone(),
                                true,
                                &dialog_sheet,
                                "settings",
                            );
                        }
                        Err(e) => {
                            // A duplicate feed URL opens the existing
                            // subscription instead of ending on a constraint
                            // error.
                            let is_unique_violation = matches!(
                                &e,
                                rusqlite::Error::SqliteFailure(sqlite_err, _)
                                    if sqlite_err.code == rusqlite::ErrorCode::ConstraintViolation
                            );
                            let existing = is_unique_violation
                                .then(|| phodder_core::open_connection(&db_path).ok())
                                .flatten()
                                .and_then(|conn| {
                                    phodder_core::subscriptions::get_subscription_by_feed_url(&conn, &url).ok().flatten()
                                });
                            match existing {
                                Some(existing) => {
                                    presentation.close();
                                    show_subscription_detail(
                                        &parent,
                                        db_path.clone(),
                                        existing.id,
                                        subscriptions_slot.clone(),
                                        tokio_handle.clone(),
                                        is_narrow.clone(),
                                        command_tx.clone(),
                                        false,
                                        &dialog_sheet,
                                        "details",
                                    );
                                }
                                None => {
                                    tracing::error!("failed to save subscription after preview fetch: {e}");
                                    toast_overlay.add_toast(libadwaita::Toast::new(&gettext_f("Couldn't add feed: {error}", &[("error", &e.to_string())])));
                                }
                            }
                        }
                    },
                    Err(e) => {
                        tracing::warn!("feed preview fetch failed for {url}: {e}");
                        toast_overlay.add_toast(libadwaita::Toast::new(&gettext_f("Couldn't fetch feed: {error}", &[("error", &e.to_string())])));
                    }
                }

                // Reached only on failure: makes the dialog retryable.
                button.set_sensitive(true);
                entry.set_sensitive(true);
                loading_spinner.set_visible(false);
                loading_spinner.stop();
            });
        });
    }
}

/// Imports every OPML feed through Add Feed's fetch-then-insert path, without
/// the preview step (see UiCommand::ImportOpml). A failing feed is counted
/// and skipped.
pub(super) async fn import_opml(db_path: &std::path::Path, opml_path: &std::path::Path) -> anyhow::Result<OpmlImportSummary> {
    let xml = std::fs::read_to_string(opml_path)?;
    let feeds = phodder_core::opml::parse_opml(&xml)?;

    let mut imported = 0;
    let mut already_subscribed = 0;
    let mut failed = 0;

    for feed in feeds {
        let already_exists = {
            let conn = phodder_core::open_connection(db_path)?;
            let count: i64 =
                conn.query_row("SELECT COUNT(*) FROM subscriptions WHERE feed_url = ?1", rusqlite::params![feed.feed_url], |row| {
                    row.get(0)
                })?;
            count > 0
        };
        if already_exists {
            already_subscribed += 1;
            continue;
        }

        match crate::downloader::fetch_feed_preview(&feed.feed_url).await {
            Ok(preview) => match add_subscription_with_preview(db_path, &feed.feed_url, &preview) {
                Ok(_) => imported += 1,
                Err(e) => {
                    tracing::warn!("failed to insert imported subscription {}: {e}", feed.feed_url);
                    failed += 1;
                }
            },
            Err(e) => {
                tracing::warn!("failed to fetch preview for imported feed {}: {e}", feed.feed_url);
                failed += 1;
            }
        }
    }

    Ok(OpmlImportSummary { imported, already_subscribed, failed })
}

pub(super) fn export_opml(db_path: &std::path::Path, output_path: &std::path::Path) -> anyhow::Result<()> {
    let conn = phodder_core::open_connection(db_path)?;
    let mut stmt = conn.prepare("SELECT title, feed_url FROM subscriptions ORDER BY title")?;
    let rows: Vec<(String, String)> = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?.collect::<rusqlite::Result<_>>()?;

    let feeds: Vec<phodder_core::opml::ExportedFeed> =
        rows.iter().map(|(title, feed_url)| phodder_core::opml::ExportedFeed { title, feed_url }).collect();
    let xml = phodder_core::opml::generate_opml(&feeds)?;
    std::fs::write(output_path, xml)?;
    Ok(())
}

/// Inserts a subscription with the title, description and artwork from a
/// preview fetch, so Manage Subscription opens with real artwork.
pub(crate) fn add_subscription_with_preview(
    db_path: &std::path::Path,
    feed_url: &str,
    preview: &crate::downloader::FeedPreview,
) -> rusqlite::Result<i64> {
    let conn = phodder_core::open_connection(db_path)?;
    // Group 1 is the seeded Default group.
    let new_id = phodder_core::subscriptions::insert_subscription(&conn, feed_url, &preview.parsed.title, 1)?;
    phodder_core::subscriptions::update_subscription_metadata(
        &conn,
        new_id,
        &preview.parsed.title,
        preview.parsed.artwork_url.as_deref(),
        preview.parsed.description.as_deref(),
        preview.artwork_local_path.as_deref(),
    )?;

    // New subscriptions start with the Settings expiry when one is set,
    // instead of the schema default. A one-time value; there's no ongoing
    // fallback. A direct UPDATE avoids rewriting the other settings.
    if let Some(default_minutes) =
        phodder_core::settings::get_setting_i64(&conn, "default_new_subscription_expiry_minutes")
    {
        conn.execute(
            "UPDATE subscriptions SET delete_after_minutes = ?1 WHERE id = ?2",
            rusqlite::params![default_minutes, new_id],
        )?;
    }

    Ok(new_id)
}
