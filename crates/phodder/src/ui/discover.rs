use super::*;

/// Wait after the last keystroke before searching.
const SEARCH_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

/// What a result row needs to open Add Feed or an existing subscription.
#[derive(Clone)]
struct DiscoverContext {
    parent: libadwaita::ApplicationWindow,
    db_path: std::path::PathBuf,
    tokio_handle: tokio::runtime::Handle,
    command_tx: UnboundedSender<UiCommand>,
    subscriptions_slot: gtk4::Box,
    is_narrow: std::rc::Rc<std::cell::Cell<bool>>,
    dialog_sheet: libadwaita::BottomSheet,
}

/// Add Podcast: a search bar, the enabled directories' popular podcasts
/// until something is searched, and "Or Add by Feed URL" at the bottom.
/// Picking a result opens its preview (show_podcast_preview).
pub(super) fn show_discover_page(
    parent: &libadwaita::ApplicationWindow,
    db_path: std::path::PathBuf,
    tokio_handle: tokio::runtime::Handle,
    command_tx: UnboundedSender<UiCommand>,
    subscriptions_slot: gtk4::Box,
    is_narrow: std::rc::Rc<std::cell::Cell<bool>>,
    dialog_sheet: &libadwaita::BottomSheet,
) {
    let context = DiscoverContext {
        parent: parent.clone(),
        db_path,
        tokio_handle,
        command_tx,
        subscriptions_slot,
        is_narrow,
        dialog_sheet: dialog_sheet.clone(),
    };

    let search_entry = gtk4::SearchEntry::builder().placeholder_text(gettext("Search podcasts")).hexpand(true).build();
    set_accessible_label(&search_entry, &gettext("Search podcasts"));
    let heading = gtk4::Label::builder()
        .label(gettext("Popular"))
        .halign(gtk4::Align::Start)
        .accessible_role(gtk4::AccessibleRole::Heading)
        .css_classes(["heading"])
        .build();
    // Names a directory that didn't answer while others did.
    let partial_note = gtk4::Label::builder().halign(gtk4::Align::Start).wrap(true).visible(false).css_classes(["dim-label", "caption"]).build();
    let results_slot = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).vexpand(true).build();
    let scrolled = gtk4::ScrolledWindow::builder().child(&results_slot).vexpand(true).hscrollbar_policy(gtk4::PolicyType::Never).build();

    let by_url_row = libadwaita::ButtonRow::builder().title(gettext("Or Add by Feed URL")).start_icon_name("list-add-symbolic").build();
    let by_url_list = gtk4::ListBox::builder().selection_mode(gtk4::SelectionMode::None).css_classes(["boxed-list"]).build();
    by_url_list.append(&by_url_row);
    {
        let context = context.clone();
        by_url_row.connect_activated(move |_| {
            show_add_feed_dialog(
                &context.parent,
                context.db_path.clone(),
                context.tokio_handle.clone(),
                context.command_tx.clone(),
                context.subscriptions_slot.clone(),
                context.is_narrow.clone(),
                &context.dialog_sheet,
            );
        });
    }

    let content = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(12)
        .margin_start(16)
        .margin_end(16)
        .margin_top(12)
        .margin_bottom(16)
        .build();
    content.append(&search_entry);
    content.append(&heading);
    content.append(&partial_note);
    content.append(&scrolled);
    content.append(&by_url_list);

    // Only the newest request's reply is shown; slower earlier ones are
    // dropped.
    let request_serial = std::rc::Rc::new(std::cell::Cell::new(0u64));
    let load = {
        let context = context.clone();
        let results_slot = results_slot.clone();
        let heading = heading.clone();
        let partial_note = partial_note.clone();
        let request_serial = request_serial.clone();
        std::rc::Rc::new(move |query: Option<String>| {
            let serial = request_serial.get() + 1;
            request_serial.set(serial);
            heading.set_label(&if query.is_some() { gettext("Results") } else { gettext("Popular") });
            partial_note.set_visible(false);
            let spinner = gtk4::Spinner::builder().spinning(true).halign(gtk4::Align::Center).valign(gtk4::Align::Center).vexpand(true).width_request(32).height_request(32).build();
            set_accessible_label(&spinner, &gettext("Loading"));
            replace_slot_content(&results_slot, spinner.upcast());

            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            if context.command_tx.send(UiCommand::Discover { query: query.clone(), reply_tx }).is_err() {
                replace_slot_content(&results_slot, discover_status(&gettext("Couldn't reach the backend"), None));
                return;
            }
            let context = context.clone();
            let results_slot = results_slot.clone();
            let request_serial = request_serial.clone();
            let partial_note = partial_note.clone();
            gtk4::glib::spawn_future_local(async move {
                let reply = reply_rx.await.unwrap_or_else(|_| Err(gettext("the backend didn't respond")));
                if request_serial.get() != serial {
                    return;
                }
                let sources_enabled = phodder_core::open_connection(&context.db_path)
                    .map(|conn| !crate::discovery::enabled_sources(&conn).is_empty())
                    .unwrap_or(true);
                let widget = match reply {
                    _ if !sources_enabled => discover_status(&gettext("No Directories Enabled"), Some("Turn on Apple Podcasts or gpodder.net, or add a Podcast Index key, in Settings.")),
                    Ok(results) => {
                        let mut notes = Vec::new();
                        if results.key_rejected {
                            notes.push(gettext("Podcast Index rejected the key and secret in Settings."));
                        }
                        let unanswered: Vec<&str> = results
                            .failed
                            .iter()
                            .filter(|s| !(results.key_rejected && **s == crate::discovery::Source::PodcastIndex))
                            .map(|s| s.label())
                            .collect();
                        if !unanswered.is_empty() {
                            // Apple answers too many requests with 403 for a minute or so.
                            notes.push(ngettext_f(
                                "{sources} didn't answer; try again in a minute.",
                                "{sources} didn't answer; try again in a minute.",
                                unanswered.len() as u32,
                                // Directory names, such as "Apple Podcasts, gpodder.net".
                                &[("sources", &unanswered.join(", "))],
                            ));
                        }
                        partial_note.set_label(&notes.join(" "));
                        partial_note.set_visible(!notes.is_empty());
                        // Results replace the list silently; typing in the
                        // search field keeps focus there.
                        if query.is_some() {
                            let count = results.podcasts.len();
                            let found = if count == 0 {
                                gettext("No podcasts found")
                            } else {
                                ngettext_f("{n} podcast found", "{n} podcasts found", count as u32, &[("n", &count.to_string())])
                            };
                            let message = if notes.is_empty() { found } else { format!("{found}. {}", notes.join(" ")) };
                            announce(&results_slot, &message);
                        }
                        if results.podcasts.is_empty() {
                            discover_status(&if query.is_some() { gettext("No Podcasts Found") } else { gettext("Nothing to Show") }, None)
                        } else {
                            build_discover_results(&results.podcasts, &context)
                        }
                    }
                    Err(e) => discover_status(&gettext("Couldn't Reach the Directories"), Some(&e)),
                };
                replace_slot_content(&results_slot, widget);
            });
        })
    };

    let pending_search: std::rc::Rc<std::cell::RefCell<Option<gtk4::glib::SourceId>>> = Default::default();
    {
        let load = load.clone();
        let pending_search = pending_search.clone();
        search_entry.connect_search_changed(move |entry| {
            if let Some(previous) = pending_search.borrow_mut().take() {
                previous.remove();
            }
            let text = entry.text().trim().to_string();
            let load = load.clone();
            let pending = pending_search.clone();
            let source = gtk4::glib::timeout_add_local_once(SEARCH_DELAY, move || {
                // Dropped without remove(): the source has already fired.
                pending.borrow_mut().take();
                load((!text.is_empty()).then_some(text));
            });
            pending_search.borrow_mut().replace(source);
        });
    }
    {
        let load = load.clone();
        search_entry.connect_activate(move |entry| {
            if let Some(previous) = pending_search.borrow_mut().take() {
                previous.remove();
            }
            let text = entry.text().trim().to_string();
            load((!text.is_empty()).then_some(text));
        });
    }

    present_dialog_adaptively(&content, &gettext("Add Podcast"), 440, 600, parent, dialog_sheet, true);
    load(None);
    search_entry.grab_focus();
}

fn discover_status(title: &str, description: Option<&str>) -> gtk4::Widget {
    libadwaita::StatusPage::builder().title(title).description(description.unwrap_or("")).vexpand(true).build().upcast()
}

fn build_discover_results(podcasts: &[crate::discovery::DiscoveredPodcast], context: &DiscoverContext) -> gtk4::Widget {
    // Already-subscribed feeds open their subscription instead of Add Feed.
    let subscribed: std::collections::HashMap<String, i64> = phodder_core::open_connection(&context.db_path)
        .ok()
        .and_then(|conn| phodder_core::subscriptions::list_subscriptions(&conn).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|s| (crate::discovery::normalize_feed_url(&s.feed_url), s.id))
        .collect();

    let list = gtk4::ListBox::builder().selection_mode(gtk4::SelectionMode::None).css_classes(["boxed-list"]).valign(gtk4::Align::Start).build();
    for podcast in podcasts {
        let subscription_id = subscribed.get(&crate::discovery::normalize_feed_url(&podcast.feed_url)).copied();
        let subscribed_text = gettext("Subscribed");
        let subtitle = join_non_empty(&[podcast.author.as_deref(), Some(podcast.source.label()), subscription_id.map(|_| subscribed_text.as_str())]);
        let row = libadwaita::ActionRow::builder()
            .activatable(true)
            .title(&escape_markup(&podcast.title))
            .subtitle(&escape_markup(&subtitle))
            .title_lines(2)
            .subtitle_lines(1)
            .build();
        let thumbnail = load_thumbnail_image(&[None], "image-x-generic-symbolic", 56);
        row.add_prefix(&thumbnail);
        if subscription_id.is_some() {
            let subscribed = gtk4::Image::from_icon_name("object-select-symbolic");
            set_accessible_label(&subscribed, &gettext("Subscribed"));
            row.add_suffix(&subscribed);
        }
        if let Some(artwork_url) = &podcast.artwork_url {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            if context.command_tx.send(UiCommand::FetchDiscoverArtwork { artwork_url: artwork_url.clone(), reply_tx }).is_ok() {
                let thumbnail = thumbnail.clone();
                gtk4::glib::spawn_future_local(async move {
                    if let Ok(Some(local_path)) = reply_rx.await {
                        swap_thumbnail_image(&thumbnail, &local_path);
                    }
                });
            }
        }
        {
            let context = context.clone();
            let podcast = podcast.clone();
            row.connect_activated(move |_| match subscription_id {
                Some(id) => show_subscription_detail(
                    &context.parent,
                    context.db_path.clone(),
                    id,
                    context.subscriptions_slot.clone(),
                    context.tokio_handle.clone(),
                    context.is_narrow.clone(),
                    context.command_tx.clone(),
                    false,
                    &context.dialog_sheet,
                    "details",
                ),
                None => show_podcast_preview(&podcast, &context),
            });
        }
        list.append(&row);
    }
    list.upcast()
}

/// A directory result's info page, laid out like an existing podcast's:
/// Details and Episodes tabs (read-only), Back in the header and Add at the
/// bottom.
/// The feed itself is fetched for the description and episodes; Add stays
/// disabled until it arrives, since adding uses that fetch.
fn show_podcast_preview(podcast: &crate::discovery::DiscoveredPodcast, context: &DiscoverContext) {
    let content = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(12).margin_start(16).margin_end(16).margin_top(16).margin_bottom(16).build();
    let artwork = load_thumbnail_image(&[None], "image-x-generic-symbolic", 200);
    artwork.set_halign(gtk4::Align::Center);
    let title_box = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(2).build();
    title_box.append(&gtk4::Label::builder().label(&podcast.title).wrap(true).justify(gtk4::Justification::Center).css_classes(["title-2"]).build());
    if let Some(author) = &podcast.author {
        title_box.append(&gtk4::Label::builder().label(author).wrap(true).justify(gtk4::Justification::Center).css_classes(["dim-label"]).build());
    }
    let description_label = fill_podcast_info(&content, &artwork, &title_box, None, &podcast.feed_url);
    description_label.set_label(&gettext("Loading…"));
    let scrolled = gtk4::ScrolledWindow::builder().child(&content).vexpand(true).hscrollbar_policy(gtk4::PolicyType::Never).build();

    let episodes_slot = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).vexpand(true).build();
    let loading = libadwaita::StatusPage::builder().title(gettext("Loading Episodes…")).vexpand(true).build();
    let spinner = gtk4::Spinner::builder().spinning(true).halign(gtk4::Align::Center).width_request(32).height_request(32).build();
    set_accessible_label(&spinner, &gettext("Loading"));
    loading.set_child(Some(&spinner));
    replace_slot_content(&episodes_slot, loading.upcast());

    let view_stack = libadwaita::ViewStack::builder().vexpand(true).build();
    view_stack.add_titled(&scrolled, Some("details"), "Details");
    view_stack.add_titled(&episodes_slot, Some("episodes"), "Episodes");
    // The tabs are the header's title; Back is in the header too.
    let view_switcher = libadwaita::InlineViewSwitcher::builder().stack(&view_stack).css_classes(["flat"]).build();

    let add_button = gtk4::Button::builder().label(gettext("Add")).css_classes(["suggested-action"]).sensitive(false).build();
    let bottom = gtk4::CenterBox::builder().margin_start(16).margin_end(16).margin_top(8).margin_bottom(12).build();
    bottom.set_end_widget(Some(&add_button));
    let toast_overlay = libadwaita::ToastOverlay::new();
    let page = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).build();
    page.append(&view_stack);
    page.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
    page.append(&bottom);
    toast_overlay.set_child(Some(&page));

    let (presentation, _go_back) = present_dialog_with_back(
        &toast_overlay,
        &podcast.title,
        HeaderTitle::Widget(view_switcher.upcast_ref()),
        440,
        600,
        &context.parent,
        &context.dialog_sheet,
    );

    // The directory's artwork shows at once from the results' cache.
    if let Some(artwork_url) = &podcast.artwork_url {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if context.command_tx.send(UiCommand::FetchDiscoverArtwork { artwork_url: artwork_url.clone(), reply_tx }).is_ok() {
            let artwork = artwork.clone();
            gtk4::glib::spawn_future_local(async move {
                if let Ok(Some(local_path)) = reply_rx.await {
                    swap_thumbnail_image(&artwork, &local_path);
                }
            });
        }
    }

    let preview: std::rc::Rc<std::cell::RefCell<Option<crate::downloader::FeedPreview>>> = Default::default();
    {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let _ = context.command_tx.send(UiCommand::FetchFeedPreview { url: podcast.feed_url.clone(), directory: true, reply_tx });
        let preview = preview.clone();
        let add_button = add_button.clone();
        let podcast_title = podcast.title.clone();
        let command_tx = context.command_tx.clone();
        gtk4::glib::spawn_future_local(async move {
            match reply_rx.await.unwrap_or_else(|_| Err(gettext("the backend didn't respond"))) {
                Ok(fetched) => {
                    set_podcast_description(&description_label, fetched.parsed.description.as_deref());
                    let mut episodes = fetched.parsed.episodes.clone();
                    episodes.sort_by(|a, b| b.publish_date.cmp(&a.publish_date));
                    let episodes = std::rc::Rc::new(PreviewEpisodes { episodes, podcast_title, artwork: fetched.artwork_local_path.clone(), command_tx });
                    replace_slot_content(&episodes_slot, build_preview_episode_page(&episodes, &episodes_slot, 0));
                    preview.replace(Some(fetched));
                    add_button.set_sensitive(true);
                }
                Err(e) => {
                    description_label.set_label(&gettext_f("Couldn't fetch this feed: {error}", &[("error", &e.to_string())]));
                    description_label.add_css_class("dim-label");
                    replace_slot_content(&episodes_slot, discover_status(&gettext("Couldn't Load Episodes"), Some(&e)));
                }
            }
        });
    }

    // As Add Feed: insert, then open the new podcast's Settings so it can be
    // set up before anything downloads.
    let context = context.clone();
    let feed_url = podcast.feed_url.clone();
    add_button.connect_clicked(move |_| {
        let result = match preview.borrow_mut().as_mut() {
            Some(preview) => {
                preview.artwork_local_path = preview.artwork_local_path.take().and_then(|path| crate::downloader::keep_discover_artwork(&path));
                add_subscription_with_preview(&context.db_path, &feed_url, preview)
            }
            None => return,
        };
        match result {
            Ok(new_id) => {
                presentation.close();
                build_subscriptions_page(&context.tokio_handle, &context.db_path, &context.subscriptions_slot, &context.is_narrow, &context.command_tx, &context.dialog_sheet);
                show_subscription_detail(
                    &context.parent,
                    context.db_path.clone(),
                    new_id,
                    context.subscriptions_slot.clone(),
                    context.tokio_handle.clone(),
                    context.is_narrow.clone(),
                    context.command_tx.clone(),
                    true,
                    &context.dialog_sheet,
                    "settings",
                );
            }
            Err(e) => {
                tracing::error!("failed to add {feed_url}: {e}");
                toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't add this podcast")));
            }
        }
    });
}

struct PreviewEpisodes {
    /// Newest first.
    episodes: Vec<phodder_core::feed::ParsedEpisode>,
    podcast_title: String,
    artwork: Option<String>,
    command_tx: UnboundedSender<UiCommand>,
}

thread_local! {
    /// The audio URL of the preview last started here, and whether the
    /// player is playing it; drives the rows' Play/Pause buttons.
    static PREVIEWING: std::cell::RefCell<(Option<String>, bool)> = const { std::cell::RefCell::new((None, false)) };
    static PREVIEW_BUTTONS: std::cell::RefCell<Vec<(String, gtk4::glib::WeakRef<gtk4::Button>)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Called with every playback state. A preview ending or something else
/// loading clears it.
pub(super) fn set_preview_play_state(preview: bool, playing: bool) {
    PREVIEWING.with(|p| {
        let mut p = p.borrow_mut();
        if !preview {
            p.0 = None;
        }
        p.1 = playing;
    });
    update_preview_buttons();
}

fn update_preview_buttons() {
    let (url, playing) = PREVIEWING.with(|p| p.borrow().clone());
    PREVIEW_BUTTONS.with(|buttons| {
        buttons.borrow_mut().retain(|(button_url, button)| {
            let Some(button) = button.upgrade() else { return false };
            let active = url.as_deref() == Some(button_url.as_str()) && playing;
            set_button_icon(&button, if active { "media-playback-pause-symbolic" } else { "media-playback-start-symbolic" });
            let label = if active { gettext("Pause Preview") } else { gettext("Play Preview") };
            button.set_tooltip_text(Some(&label));
            set_accessible_label(&button, &label);
            true
        });
    });
}

/// One 50-episode page of the preview's Episodes tab: like a subscription's,
/// but read-only, since nothing can be downloaded before subscribing.
/// Tapping an episode shows its notes.
fn build_preview_episode_page(episodes: &std::rc::Rc<PreviewEpisodes>, slot: &gtk4::Box, page_index: usize) -> gtk4::Widget {
    const PAGE_SIZE: usize = 50;
    if episodes.episodes.is_empty() {
        return discover_status(&gettext("No Episodes"), None);
    }
    let total_pages = episodes.episodes.len().div_ceil(PAGE_SIZE);
    let list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["boxed-list"])
        .valign(gtk4::Align::Start)
        .margin_start(12)
        .margin_end(12)
        .margin_top(12)
        .margin_bottom(12)
        .build();
    for episode in episodes.episodes.iter().skip(page_index * PAGE_SIZE).take(PAGE_SIZE) {
        let date = episode.publish_date.format("%b %d, %Y").to_string();
        let duration = episode.duration_ms.map(format_duration_ms);
        let size = display_file_size(episode.file_size_bytes);
        let row = libadwaita::ActionRow::builder()
            .activatable(true)
            .title(&escape_markup(&episode.title))
            .subtitle(&join_non_empty(&[Some(&date), duration.as_deref(), size.as_deref()]))
            .title_lines(1)
            .subtitle_lines(1)
            .build();
        row.add_prefix(&load_thumbnail_image(&[episodes.artwork.as_deref()], "audio-x-generic-symbolic", 48));
        let play = gtk4::Button::builder().icon_name("media-playback-start-symbolic").css_classes(["flat"]).valign(gtk4::Align::Center).build();
        row.add_suffix(&play);
        PREVIEW_BUTTONS.with(|b| b.borrow_mut().push((episode.audio_url.clone(), play.downgrade())));
        {
            let episode = episode.clone();
            let episodes = episodes.clone();
            play.connect_clicked(move |_| {
                let current = PREVIEWING.with(|p| p.borrow().0.as_deref() == Some(episode.audio_url.as_str()));
                let command = if current {
                    UiCommand::PlayPause
                } else {
                    PREVIEWING.with(|p| *p.borrow_mut() = (Some(episode.audio_url.clone()), true));
                    UiCommand::PlayPreview {
                        url: episode.audio_url.clone(),
                        title: episode.title.clone(),
                        podcast_title: gettext_f("Preview · {podcast}", &[("podcast", &episodes.podcast_title)]),
                        artwork_local_path: episodes.artwork.clone(),
                    }
                };
                let _ = episodes.command_tx.send(command);
            });
        }
        {
            let episode = episode.clone();
            let episodes = episodes.clone();
            row.connect_activated(move |_| {
                open_episode_preview(
                    &episode.title,
                    &episodes.podcast_title,
                    episodes.artwork.as_deref(),
                    &episode.publish_date.to_rfc3339(),
                    episode.duration_ms,
                    episode.show_notes_html.as_deref(),
                )
            });
        }
        list.append(&row);
    }
    update_preview_buttons();
    let scrolled = gtk4::ScrolledWindow::builder().child(&list).vexpand(true).hscrollbar_policy(gtk4::PolicyType::Never).build();
    let page = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).build();
    page.append(&scrolled);
    if total_pages > 1 {
        let nav = gtk4::Box::builder().spacing(8).halign(gtk4::Align::Center).margin_top(6).margin_bottom(6).build();
        let previous = gtk4::Button::builder().label(gettext("Previous")).name("pager-previous").sensitive(page_index > 0).build();
        let next = gtk4::Button::builder().label(gettext("Next")).name("pager-next").sensitive(page_index + 1 < total_pages).build();
        nav.append(&previous);
        nav.append(&gtk4::Label::new(Some(&page_of(page_index + 1, total_pages))));
        nav.append(&next);
        page.append(&nav);
        for (button, target) in [(previous, page_index.wrapping_sub(1)), (next, page_index + 1)] {
            let episodes = episodes.clone();
            let slot = slot.clone();
            button.connect_clicked(move |button| {
                let name = if button.widget_name() == "pager-next" { "pager-next" } else { "pager-previous" };
                replace_slot_content(&slot, build_preview_episode_page(&episodes, &slot, target));
                refocus_pager(&slot, name, page_of(target + 1, total_pages));
            });
        }
    }
    page.upcast()
}
