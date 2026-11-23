use super::*;

/// Like `delete_episode_file`, looked up by (subscription_id, guid):
/// back-catalog rows come from the feed and have no episode id.
pub(super) fn delete_back_catalog_episode_file(db_path: &std::path::Path, subscription_id: i64, guid: &str) -> rusqlite::Result<()> {
    let conn = phodder_core::open_connection(db_path)?;
    let episode_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM episodes WHERE subscription_id = ?1 AND guid = ?2",
            rusqlite::params![subscription_id, guid],
            |row| row.get(0),
        )
        .optional()?;
    match episode_id {
        Some(id) => {
            drop(conn);
            delete_episode_file(db_path, id)
        }
        None => {
            tracing::warn!("tried to delete back-catalog episode {guid} for subscription {subscription_id}, but it has no DB row");
            Ok(())
        }
    }
}

/// An Episodes-tab row's single action: the first of Download, Retry, "+",
/// Mark Played and Delete that applies. Pending downloads get none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackCatalogAction {
    Download,
    Retry,
    Pending,
    Enqueue,
    MarkPlayed,
    Delete,
}

fn back_catalog_action(entry: Option<&phodder_core::subscriptions::BackCatalogEntry>) -> BackCatalogAction {
    use phodder_core::subscriptions::BackCatalogStatus;
    let Some(entry) = entry else { return BackCatalogAction::Download };
    let played = entry.status == BackCatalogStatus::Played;
    if entry.pending {
        BackCatalogAction::Pending
    } else if entry.failed {
        BackCatalogAction::Retry
    } else if !entry.downloaded {
        BackCatalogAction::Download
    } else if played {
        BackCatalogAction::Delete
    } else if !entry.is_queued {
        BackCatalogAction::Enqueue
    } else {
        BackCatalogAction::MarkPlayed
    }
}

/// What every row on an Episodes page shares.
struct BackCatalogContext {
    subscription_id: i64,
    db_path: std::path::PathBuf,
    command_tx: UnboundedSender<UiCommand>,
    toast_overlay: libadwaita::ToastOverlay,
}

/// One row's widgets and last applied state, so a refresh only touches rows
/// whose state changed. Weak refs: the refresh hook outlives the page.
struct BackCatalogRowHandle {
    row: gtk4::glib::WeakRef<libadwaita::ActionRow>,
    action_slot: gtk4::glib::WeakRef<gtk4::Box>,
    episode: phodder_core::feed::ParsedEpisode,
    applied: std::cell::Cell<Option<(BackCatalogAction, Option<i64>)>>,
}

fn back_catalog_statuses_now(db_path: &std::path::Path, subscription_id: i64) -> std::collections::HashMap<String, phodder_core::subscriptions::BackCatalogEntry> {
    phodder_core::open_connection(db_path)
        .ok()
        .and_then(|conn| phodder_core::subscriptions::back_catalog_statuses(&conn, subscription_id).ok())
        .unwrap_or_default()
}

/// Sets a row's subtitle and action button for `entry`, skipping rows whose
/// state is unchanged.
fn apply_back_catalog_state(
    handle: &BackCatalogRowHandle,
    entry: Option<&phodder_core::subscriptions::BackCatalogEntry>,
    context: &std::rc::Rc<BackCatalogContext>,
) {
    use phodder_core::subscriptions::BackCatalogStatus;
    let (Some(row), Some(action_slot)) = (handle.row.upgrade(), handle.action_slot.upgrade()) else { return };
    let action = back_catalog_action(entry);
    let key = (action, entry.and_then(|e| e.duration_ms));
    if handle.applied.get() == Some(key) {
        return;
    }
    handle.applied.set(Some(key));

    let episode = &handle.episode;
    let publish_date_display = episode.publish_date.format("%b %d, %Y").to_string();
    // The probed duration beats the feed's, which can be wrong.
    let duration_display = entry.and_then(|e| e.duration_ms).or(episode.duration_ms).map(format_duration_ms);
    let size_display = display_file_size(episode.file_size_bytes);
    let state_display = match (action, entry.map(|e| e.status)) {
        (BackCatalogAction::Pending, _) => Some(gettext("Downloading")),
        (BackCatalogAction::Retry, _) => Some(gettext("Failed")),
        (_, Some(BackCatalogStatus::Played)) => Some(gettext("Played")),
        _ => None,
    };
    row.set_subtitle(&join_non_empty(&[Some(&publish_date_display), duration_display.as_deref(), size_display.as_deref(), state_display.as_deref()]));

    while let Some(child) = action_slot.first_child() {
        action_slot.remove(&child);
    }
    let (icon, tooltip) = match action {
        BackCatalogAction::Download => ("folder-download-symbolic", "Download"),
        BackCatalogAction::Retry => ("view-refresh-symbolic", "Retry Download"),
        BackCatalogAction::Pending => return,
        BackCatalogAction::Enqueue => ("list-add-symbolic", "Add to Queue"),
        BackCatalogAction::MarkPlayed => ("object-select-symbolic", "Mark Played"),
        BackCatalogAction::Delete => ("user-trash-symbolic", "Delete Download"),
    };
    let button = gtk4::Button::builder().icon_name(icon).tooltip_text(tooltip).css_classes(["flat"]).valign(gtk4::Align::Center).build();
    action_slot.append(&button);
    let episode_id = entry.map(|e| e.episode_id);
    let context = context.clone();
    let episode = episode.clone();
    wire_suffix_button_action(&button, move || {
        let send = |command: UiCommand| {
            if let Err(e) = context.command_tx.send(command) {
                tracing::error!("failed to send Episodes-tab command: {e}");
            }
        };
        match (action, episode_id) {
            (BackCatalogAction::Download, _) => queue_back_catalog_download(&context, episode.clone()),
            (BackCatalogAction::Retry, Some(id)) => {
                let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
                send(UiCommand::RequeueEpisode { episode_id: id, reply_tx });
            }
            (BackCatalogAction::Enqueue, Some(id)) => send(UiCommand::EnqueueEpisode(id)),
            (BackCatalogAction::MarkPlayed, Some(id)) => send(UiCommand::SetPlayed { episode_id: id, played: true }),
            (BackCatalogAction::Delete, _) => {
                if let Err(e) = delete_back_catalog_episode_file(&context.db_path, context.subscription_id, &episode.guid) {
                    tracing::error!("failed to delete back-catalog episode {}: {e}", episode.guid);
                    context.toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't delete episode")));
                    return;
                }
                send(UiCommand::NotifyDataChanged);
            }
            _ => {}
        }
    });
}

/// Download only (skip_enqueue). Sets manually_downloaded, exempting the
/// episode from age and size retention but not delete-after-play.
fn queue_back_catalog_download(context: &BackCatalogContext, episode: phodder_core::feed::ParsedEpisode) {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    if let Err(e) = context.command_tx.send(UiCommand::QueueBackCatalogEpisode {
        subscription_id: context.subscription_id,
        episode,
        skip_enqueue: true,
        reply_tx,
    }) {
        tracing::error!("failed to send QueueBackCatalogEpisode command: {e}");
        context.toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't reach the backend to download this episode")));
        return;
    }
    let toast_overlay = context.toast_overlay.clone();
    let command_tx = context.command_tx.clone();
    gtk4::glib::spawn_future_local(async move {
        match reply_rx.await {
            // Shows the row as downloading.
            Ok(Ok(())) => {
                let _ = command_tx.send(UiCommand::NotifyDataChanged);
            }
            Ok(Err(e)) => {
                tracing::error!("failed to queue back-catalog episode: {e}");
                toast_overlay.add_toast(libadwaita::Toast::new(&gettext_f("Couldn't download episode: {error}", &[("error", &e.to_string())])));
            }
            Err(_) => {
                toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't download episode: the backend didn't respond")));
            }
        }
    });
}

thread_local! {
    // Episodes pages currently built; each refreshes its rows on DataChanged
    // and is dropped once its list is gone.
    static BACK_CATALOG_REFRESHERS: std::cell::RefCell<Vec<Box<dyn Fn() -> bool>>> = Default::default();
}

/// Re-reads download and played state for open Episodes pages. Called with
/// every tab rebuild; that dialog isn't one of the tabs.
pub(super) fn refresh_back_catalog_pages() {
    let refreshers = BACK_CATALOG_REFRESHERS.with(|r| std::mem::take(&mut *r.borrow_mut()));
    let alive: Vec<_> = refreshers.into_iter().filter(|refresh| refresh()).collect();
    BACK_CATALOG_REFRESHERS.with(|r| r.borrow_mut().extend(alive));
}

/// Builds one 50-episode page of a subscription's Episodes list. Returns the
/// empty list at once and fills it in batches from one spawn_future_local
/// loop, yielding between batches. Building hundreds of rows in one loop
/// froze the window ("not responding"), and chunking alone wasn't enough for
/// huge catalogs, hence pagination.
pub(super) fn build_back_catalog_episode_list(
    parsed_episodes: std::rc::Rc<Vec<phodder_core::feed::ParsedEpisode>>,
    subscription_id: i64,
    subscription_artwork_local_path: std::rc::Rc<Option<String>>,
    db_path: std::path::PathBuf,
    command_tx: UnboundedSender<UiCommand>,
    toast_overlay: libadwaita::ToastOverlay,
    episodes_slot: gtk4::Box,
    page_index: usize,
) -> gtk4::Widget {
    if parsed_episodes.is_empty() {
        return placeholder_page(&gettext("No Episodes Found"), "view-list-symbolic").upcast();
    }

    const PAGE_SIZE: usize = 50;
    const BATCH_SIZE: usize = 20;
    let selection = selection::scope(&format!("podcast:{subscription_id}"));
    selection.begin_page(&db_path, &command_tx, &toast_overlay);
    let total_pages = (parsed_episodes.len() + PAGE_SIZE - 1) / PAGE_SIZE;
    let page_start = page_index * PAGE_SIZE;
    let page_end = (page_start + PAGE_SIZE).min(parsed_episodes.len());

    let list_box = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .margin_start(12)
        .margin_end(12)
        .margin_top(12)
        .margin_bottom(12)
        .build();

    let context = std::rc::Rc::new(BackCatalogContext {
        subscription_id,
        db_path: db_path.clone(),
        command_tx: command_tx.clone(),
        toast_overlay: toast_overlay.clone(),
    });
    let handles: std::rc::Rc<std::cell::RefCell<Vec<BackCatalogRowHandle>>> = Default::default();
    {
        let list_box = list_box.downgrade();
        let handles = std::rc::Rc::downgrade(&handles);
        let context = std::rc::Rc::downgrade(&context);
        BACK_CATALOG_REFRESHERS.with(|r| {
            r.borrow_mut().push(Box::new(move || {
                let (Some(_list_box), Some(handles), Some(context)) = (list_box.upgrade(), handles.upgrade(), context.upgrade()) else {
                    return false;
                };
                let statuses = back_catalog_statuses_now(&context.db_path, context.subscription_id);
                for handle in handles.borrow().iter() {
                    apply_back_catalog_state(handle, statuses.get(&handle.episode.guid), &context);
                }
                true
            }))
        });
    }
    // Fetched per page, so paging never shows the state from when the tab
    // first opened.
    let statuses = std::rc::Rc::new(back_catalog_statuses_now(&db_path, subscription_id));

    // Thumbnails by guid, for artwork that arrives after the row is built.
    let thumbnails: std::rc::Rc<std::cell::RefCell<std::collections::HashMap<String, gtk4::Image>>> =
        std::rc::Rc::new(std::cell::RefCell::new(std::collections::HashMap::new()));

    {
        let list_box = list_box.clone();
        let parsed_episodes = parsed_episodes.clone();
        let subscription_artwork_local_path = subscription_artwork_local_path.clone();
        let db_path = db_path.clone();
        let command_tx = command_tx.clone();
        let thumbnails = thumbnails.clone();
        let selection = selection.clone();
        // Held strongly here so the page's state lives as long as its rows.
        let page_state = (context.clone(), handles.clone());
        list_box.connect_destroy(move |_| {
            let _ = &page_state;
        });
        let context = context.clone();
        let handles = handles.clone();
        gtk4::glib::spawn_future_local(async move {
            let mut batch_start = page_start;
            while batch_start < page_end {
                let batch_end = (batch_start + BATCH_SIZE).min(page_end);

                // Each row's artwork fetch is its own task; awaiting a
                // batch's fetches together made images appear in clumps.
                for episode in &parsed_episodes[batch_start..batch_end] {
                    let entry = statuses.get(&episode.guid);
                    let row = libadwaita::ActionRow::builder()
                        .activatable(true)
                        .title(&escape_markup(&episode.title))
                        .title_lines(1)
                        .subtitle_lines(1)
                        .build();
                    // Starts with the show's artwork; episode artwork replaces
                    // it once fetched.
                    let thumbnail = load_thumbnail_image(&[subscription_artwork_local_path.as_deref()], "audio-x-generic-symbolic", 48);
                    row.add_prefix(&thumbnail);
                    let item = selection::Item::Feed { subscription_id, episode: episode.clone() };
                    selection.attach_row(&row, item.clone());
                    let action_slot = gtk4::Box::builder().valign(gtk4::Align::Center).build();
                    row.add_suffix(&action_slot);
                    {
                        // Looked up at tap time: a downloaded episode gains an id.
                        let episode = episode.clone();
                        let db_path = db_path.clone();
                        let artwork = subscription_artwork_local_path.clone();
                        let selection = selection.clone();
                        row.connect_activated(move |_| {
                            if selection.handle_activate(&item) {
                                return;
                            }
                            match back_catalog_statuses_now(&db_path, subscription_id).get(&episode.guid).map(|e| e.episode_id) {
                                Some(id) => open_episode_details(id),
                                None => open_episode_preview(
                                    &episode.title,
                                    "",
                                    artwork.as_deref(),
                                    &episode.publish_date.to_rfc3339(),
                                    episode.duration_ms,
                                    episode.show_notes_html.as_deref(),
                                ),
                            }
                        });
                    }
                    let handle = BackCatalogRowHandle {
                        row: row.downgrade(),
                        action_slot: action_slot.downgrade(),
                        episode: episode.clone(),
                        applied: std::cell::Cell::new(None),
                    };
                    apply_back_catalog_state(&handle, entry, &context);
                    handles.borrow_mut().push(handle);
                    thumbnails.borrow_mut().insert(episode.guid.clone(), thumbnail);
                    list_box.append(&row);

                    // Most episodes have no artwork of their own.
                    if let Some(artwork_url) = &episode.artwork_url {
                        if Some(artwork_url.as_str()) != subscription_artwork_local_path.as_deref() {
                            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                            if command_tx
                                .send(UiCommand::FetchEpisodeArtwork {
                                    guid: episode.guid.clone(),
                                    artwork_url: artwork_url.clone(),
                                    reply_tx,
                                })
                                .is_ok()
                            {
                                let guid = episode.guid.clone();
                                let thumbnails = thumbnails.clone();
                                gtk4::glib::spawn_future_local(async move {
                                    let Ok(Some(local_path)) = reply_rx.await else { return };
                                    let Some(image) = thumbnails.borrow().get(&guid).cloned() else { return };
                                    swap_thumbnail_image(&image, &local_path);
                                });
                            }
                        }
                    }
                }
                batch_start = batch_end;

                // The only await point left in the loop; without it the page
                // builds synchronously and freezes the window again.
                gtk4::glib::timeout_future_seconds(0).await;
            }

            // See build_library_page_from_data; added once per page.
            let bottom_divider_row = gtk4::ListBoxRow::builder()
                .activatable(false)
                .selectable(false)
                .focusable(false)
                .accessible_role(gtk4::AccessibleRole::Presentation)
                .child(&gtk4::Box::new(gtk4::Orientation::Horizontal, 0))
                .build();
            list_box.append(&bottom_divider_row);
        });
    }

    let scrolled = gtk4::ScrolledWindow::builder().child(&list_box).vexpand(true).build();

    let outer = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).build();
    outer.append(&scrolled);

    // Pager, shown only for more than one page.
    if total_pages > 1 {
        let nav_row = gtk4::Box::builder()
            .orientation(gtk4::Orientation::Horizontal)
            .spacing(8)
            .halign(gtk4::Align::Center)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        let prev_button = gtk4::Button::builder().label(gettext("Previous")).name("pager-previous").sensitive(page_index > 0).build();
        let page_label = gtk4::Label::builder().label(page_of(page_index + 1, total_pages)).build();
        let next_button = gtk4::Button::builder().label(gettext("Next")).name("pager-next").sensitive(page_index + 1 < total_pages).build();
        nav_row.append(&prev_button);
        nav_row.append(&page_label);
        nav_row.append(&next_button);
        outer.append(&nav_row);

        for (button, target_page) in [(&prev_button, page_index.wrapping_sub(1)), (&next_button, page_index + 1)] {
            let episodes_slot = episodes_slot.clone();
            let parsed_episodes = parsed_episodes.clone();
            let subscription_artwork_local_path = subscription_artwork_local_path.clone();
            let db_path = db_path.clone();
            let command_tx = command_tx.clone();
            let toast_overlay = toast_overlay.clone();
            button.connect_clicked(move |button| {
                let name = if button.widget_name() == "pager-next" { "pager-next" } else { "pager-previous" };
                let new_widget = build_back_catalog_episode_list(
                    parsed_episodes.clone(),
                    subscription_id,
                    subscription_artwork_local_path.clone(),
                    db_path.clone(),
                    command_tx.clone(),
                    toast_overlay.clone(),
                    episodes_slot.clone(),
                    target_page,
                );
                replace_slot_content(&episodes_slot, new_widget);
                refocus_pager(&episodes_slot, name, page_of(target_page + 1, total_pages));
            });
        }
    }

    // The podcast dialog's header holds its tabs, so Select lives here.
    let count = parsed_episodes.len();
    let select_bar = gtk4::Box::builder().spacing(6).margin_start(18).margin_end(6).margin_top(6).margin_bottom(6).build();
    select_bar.append(
        &gtk4::Label::builder()
            .label(ngettext_f("{n} episode", "{n} episodes", count as u32, &[("n", &count.to_string())]))
            .halign(gtk4::Align::Start)
            .hexpand(true)
            .css_classes(["dim-label"])
            .build(),
    );
    let select_button = gtk4::Button::builder().icon_name("selection-mode-symbolic").tooltip_text(gettext("Select Episodes")).css_classes(["flat"]).build();
    {
        let selection = selection.clone();
        select_button.connect_clicked(move |_| selection.set_active(true));
    }
    select_bar.append(&select_button);
    selection.wrap(&outer, Some(select_bar.upcast_ref()), true).upcast()
}

/// Deletes a downloaded episode's file and resets it to 'none'. Mirrors
/// coordinator.rs's eviction sequence, including treating NotFound as
/// success.
pub(super) fn delete_episode_file(db_path: &std::path::Path, episode_id: i64) -> rusqlite::Result<()> {
    stop_playback_if_current(vec![episode_id]);
    // Cancels an in-progress download first; see
    // delete_subscription_and_files.
    cancel_download(episode_id);
    let conn = phodder_core::open_connection(db_path)?;
    if let Some(path) = phodder_core::retention::episode_local_file_path(&conn, episode_id)? {
        match std::fs::remove_file(&path) {
            Ok(()) => tracing::info!("deleted {path} (episode {episode_id})"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!("episode {episode_id}'s file {path} was already gone -- clearing DB state anyway");
            }
            Err(e) => {
                tracing::error!("failed to delete {path} for episode {episode_id}: {e}");
            }
        }
    }
    phodder_core::retention::mark_episode_deleted(&conn, episode_id)
}

/// The Library tab: episodes with any download status besides 'none', or the
/// shared placeholder. Fills `progress_registry` (after clearing it) for
/// downloading episodes, so progress updates touch one row instead of
/// rebuilding the list.
pub(super) fn build_library_page_from_data(
    episodes: Vec<phodder_core::subscriptions::EpisodeSummary>,
    db_path: &std::path::Path,
    progress_registry: &DownloadProgressRegistry,
    command_tx: &UnboundedSender<UiCommand>,
    toast_overlay: &libadwaita::ToastOverlay,
) -> gtk4::Widget {
    let selection = selection::scope("library");
    if episodes.is_empty() {
        selection.set_active(false);
        return placeholder_page(&gettext("Library"), "library-symbolic").upcast();
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
        let is_downloading = episode.download_status == "downloading";
        let subtitle_text = if is_downloading {
            // The progress handler replaces the subtitle while downloading.
            gettext_f("Downloading ({percent}%)", &[("percent", "0")])
        } else {
            let duration_display = episode.duration_ms.map(format_duration_ms);
            let size_display = display_file_size(episode.file_size_bytes);
            let second_line = join_non_empty(&[
                duration_display.as_deref(),
                size_display.as_deref(),
                display_download_status(&episode.download_status).as_deref(),
            ]);
            format!("{} · {}\n{}", escape_markup(&episode.podcast_title), format_publish_date(&episode.publish_date), second_line)
        };

        let episode_row = libadwaita::ActionRow::builder()
            .activatable(true)
            .title(&escape_markup(&episode.title))
            .subtitle(&subtitle_text)
            .title_lines(1)
            .subtitle_lines(1)
            .build();
        episode_row.add_css_class(&format!("{LIBRARY_STATUS_CLASS_PREFIX}{}", episode.download_status));
        {
            let episode_id = episode.id;
            let selection = selection.clone();
            episode_row.connect_activated(move |_| {
                if !selection.handle_activate(&selection::Item::Episode(episode_id)) {
                    open_episode_details(episode_id);
                }
            });
        }
        // Episode artwork, falling back to the show's, including when a file
        // fails to load.
        let thumbnail = load_thumbnail_image(
            &[episode.artwork_local_path.as_deref(), episode.subscription_artwork_local_path.as_deref()],
            "audio-x-generic-symbolic",
            64,
        );
        episode_row.add_prefix(&thumbnail);
        selection.attach_row(&episode_row, selection::Item::Episode(episode.id));

        // One action: Retry for a failed download, Delete for a downloaded
        // one. Pending downloads get none.
        let action_button = match episode.download_status.as_str() {
            "failed" => Some(("view-refresh-symbolic", "Retry Download")),
            "downloaded" => Some(("user-trash-symbolic", "Delete Download")),
            _ => None,
        }
        .map(|(icon, tooltip)| {
            let button = gtk4::Button::builder().icon_name(icon).tooltip_text(tooltip).css_classes(["flat"]).valign(gtk4::Align::Center).build();
            episode_row.add_suffix(&button);
            button
        });

        if is_downloading {
            // The progress fill is a CSS class on the row's own node, one
            // provider per row so each fill updates independently. An Overlay
            // was rejected: boxed-list styling needs rows to be direct
            // ListBox children.
            let css_class = download_progress_css_class(episode.id);
            episode_row.add_css_class(&css_class);
            let provider = gtk4::CssProvider::new();
            provider.load_from_string(&download_progress_css(&css_class, 0, episode.group_color.as_deref()));
            if let Some(display) = gtk4::gdk::Display::default() {
                gtk4::style_context_add_provider_for_display(&display, &provider, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);
            }
            progress_registry.borrow_mut().insert(episode.id, (provider, episode_row.clone(), episode.group_color.clone()));
        }

        if let Some(button) = &action_button {
            let db_path = db_path.to_path_buf();
            let episode_id = episode.id;
            let toast_overlay = toast_overlay.clone();
            let command_tx = command_tx.clone();
            let retry = episode.download_status == "failed";
            wire_suffix_button_action(button, move || {
                if retry {
                    // RequeueEpisode sends DataChanged, which redraws the row.
                    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                    if let Err(e) = command_tx.send(UiCommand::RequeueEpisode { episode_id, reply_tx }) {
                        tracing::error!("failed to send RequeueEpisode command: {e}");
                        return;
                    }
                    let toast_overlay = toast_overlay.clone();
                    gtk4::glib::spawn_future_local(async move {
                        if !matches!(reply_rx.await, Ok(Ok(()))) {
                            toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't retry the download")));
                        }
                    });
                    return;
                }
                match delete_episode_file(&db_path, episode_id) {
                    // Deleting dequeues, so every tab refreshes.
                    Ok(()) => {
                        if let Err(e) = command_tx.send(UiCommand::NotifyDataChanged) {
                            tracing::error!("failed to send NotifyDataChanged after Library delete: {e}");
                        }
                    }
                    Err(e) => {
                        tracing::error!("failed to delete episode {episode_id}: {e}");
                        toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't delete episode")));
                    }
                }
            });
        }

        list_box.append(&episode_row);
    }

    // Boxed-list draws dividers only between rows, so a short list in a tall
    // view had no visible end. An empty last row gets the same divider.
    let bottom_divider_row = gtk4::ListBoxRow::builder()
        .activatable(false)
        .selectable(false)
        .focusable(false)
        .accessible_role(gtk4::AccessibleRole::Presentation)
        .child(&gtk4::Box::new(gtk4::Orientation::Horizontal, 0))
        .build();
    list_box.append(&bottom_divider_row);

    let scroller = gtk4::ScrolledWindow::builder().child(&list_box).vexpand(true).build();
    let empty_page = libadwaita::StatusPage::builder().vexpand(true).visible(false).build();
    let filter_group = libadwaita::ToggleGroup::builder()
        .halign(gtk4::Align::Center)
        .margin_top(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    for (name, label) in LIBRARY_FILTERS {
        filter_group.add(libadwaita::Toggle::builder().name(*name).label(gettext(*label)).build());
    }
    let apply_filter = {
        let list_box = list_box.clone();
        let scroller = scroller.clone();
        let empty_page = empty_page.clone();
        let bottom_divider_row = bottom_divider_row.clone();
        move |filter: &str| {
            let filter = filter.to_string();
            let mut any_visible = false;
            let mut child = list_box.first_child();
            while let Some(row) = child {
                child = row.next_sibling();
                if row == bottom_divider_row.clone().upcast::<gtk4::Widget>() {
                    continue;
                }
                let visible = library_filter_matches(&filter, &row);
                any_visible |= visible;
                row.set_visible(visible);
            }
            scroller.set_visible(any_visible);
            empty_page.set_visible(!any_visible);
            // Whole sentences per filter, so each translates naturally.
            empty_page.set_title(&match filter.as_ref() {
                "pending" => gettext("No Pending Episodes"),
                "downloaded" => gettext("No Downloaded Episodes"),
                "failed" => gettext("No Failed Episodes"),
                _ => gettext("No Episodes"),
            });
        }
    };
    let initial_filter = LIBRARY_FILTER.with(|f| f.borrow().clone());
    filter_group.set_active_name(Some(initial_filter.as_str()));
    apply_filter(&initial_filter);
    filter_group.connect_active_name_notify(move |group| {
        let name = group.active_name().map(|n| n.to_string()).unwrap_or_else(|| "all".to_string());
        LIBRARY_FILTER.with(|f| *f.borrow_mut() = name.clone());
        apply_filter(&name);
    });

    let content = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).vexpand(true).build();
    content.append(&filter_group);
    content.append(&scroller);
    content.append(&empty_page);
    selection.wrap(&content, None, false).upcast()
}

const LIBRARY_STATUS_CLASS_PREFIX: &str = "library-status-";
const LIBRARY_FILTERS: &[(&str, &str)] = &[("all", N_("All")), ("pending", N_("Pending")), ("downloaded", N_("Downloaded")), ("failed", N_("Failed"))];

thread_local! {
    // Kept across rebuilds, which happen on every data change.
    static LIBRARY_FILTER: std::cell::RefCell<String> = std::cell::RefCell::new("all".to_string());
}

/// Pending covers both 'queued' and 'downloading'.
fn library_filter_matches(filter: &str, row: &gtk4::Widget) -> bool {
    let has = |status: &str| row.has_css_class(&format!("{LIBRARY_STATUS_CLASS_PREFIX}{status}"));
    match filter {
        "pending" => has("queued") || has("downloading"),
        "downloaded" => has("downloaded"),
        "failed" => has("failed"),
        _ => true,
    }
}

/// Loads downloads and swaps in the Library tab's widget, as build_queue_page
/// does.
pub(super) fn build_library_page(
    tokio_handle: &tokio::runtime::Handle,
    library_slot: &gtk4::Box,
    db_path: &std::path::Path,
    progress_registry: &DownloadProgressRegistry,
    command_tx: &UnboundedSender<UiCommand>,
    toast_overlay: &libadwaita::ToastOverlay,
) {
    let library_slot = library_slot.clone();
    let db_path_owned = db_path.to_path_buf();
    let progress_registry = progress_registry.clone();
    let command_tx = command_tx.clone();
    let toast_overlay = toast_overlay.clone();
    run_db_op(
        tokio_handle,
        db_path.to_path_buf(),
        |conn| phodder_core::subscriptions::list_episodes_with_status(conn).map_err(anyhow::Error::from),
        move |result| match result {
            Ok(episodes) => {
                // Checked before clearing the progress registry, which still
                // points at the current rows.
                if slot_data_unchanged(&library_slot, &episodes) {
                    return;
                }
                // Providers are removed from the display, or they accumulate
                // with every rebuild.
                if let Some(display) = gtk4::gdk::Display::default() {
                    for (provider, _, _) in progress_registry.borrow().values() {
                        gtk4::style_context_remove_provider_for_display(&display, provider);
                    }
                }
                progress_registry.borrow_mut().clear();

                let widget =
                    build_library_page_from_data(episodes, &db_path_owned, &progress_registry, &command_tx, &toast_overlay);
                replace_slot_content(&library_slot, widget);
            }
            Err(e) => tracing::error!("failed to load downloads list: {e}"),
        },
    );
}
