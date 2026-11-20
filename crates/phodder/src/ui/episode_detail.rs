//! Episode details: shown as the player sheet's Details tab for the
//! current episode, and as a dialog for any other episode.

use super::*;
use phodder_core::chapters::Chapter;
use phodder_core::subscriptions::EpisodeDetail;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// Delay before a details view reloads after an action, so the backend's
/// write has landed.
const RELOAD_AFTER_ACTION: std::time::Duration = std::time::Duration::from_millis(150);

pub(super) struct DetailsContext {
    pub tokio_handle: tokio::runtime::Handle,
    pub db_path: std::path::PathBuf,
    pub command_tx: UnboundedSender<UiCommand>,
    pub window: libadwaita::ApplicationWindow,
    pub dialog_sheet: libadwaita::BottomSheet,
    pub toast_overlay: libadwaita::ToastOverlay,
    pub current_episode: Rc<Cell<Option<i64>>>,
}

thread_local! {
    // Set once by build_window. Rows on every page open details, and this
    // avoids threading the same handles through each page builder.
    static CONTEXT: RefCell<Option<Rc<DetailsContext>>> = const { RefCell::new(None) };
    // The player's details tab, kept when the episode changes so a
    // transcript reader stays on the Transcript tab.
    static PLAYER_TAB: Cell<&'static str> = const { Cell::new("notes") };
    // The action button that had focus before a reload, by position, so
    // keyboard and screen reader users stay on it.
    static ACTION_FOCUS_INDEX: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    // Every rendered details view, so a data change elsewhere (a finished
    // download, sync, a command landing) refreshes what it shows.
    static OPEN: RefCell<Vec<OpenDetails>> = const { RefCell::new(Vec::new()) };
}

struct OpenDetails {
    container: gtk4::glib::WeakRef<gtk4::Box>,
    /// Header and action buttons; replaced alone so the notes and
    /// transcript keep their scroll position.
    top: gtk4::glib::WeakRef<gtk4::Box>,
    episode_id: i64,
    mode: DetailsMode,
    /// The rest of the view depends on these, so a change re-renders it all.
    download_status: String,
    has_transcript: bool,
}

/// Re-reads every open details view, called on each data change.
pub(super) fn refresh_open_details() {
    let Some(ctx) = context() else { return };
    let open: Vec<_> = OPEN.with(|open| {
        let mut open = open.borrow_mut();
        // A closed dialog's widgets can outlive it, outside any window.
        open.retain(|e| e.container.upgrade().is_some_and(|c| c.root().is_some()));
        open.iter()
            .filter_map(|e| Some((e.container.upgrade()?, e.top.upgrade()?, e.episode_id, e.mode, e.download_status.clone(), e.has_transcript)))
            .collect()
    });
    for (container, top, episode_id, mode, download_status, has_transcript) in open {
        let db_path = ctx.db_path.clone();
        run_db_op(&ctx.tokio_handle, ctx.db_path.clone(), move |_| load(&db_path, episode_id), move |result| match result {
            Ok(Some(loaded)) if loaded.detail.download_status == download_status && loaded.has_transcript == has_transcript => {
                let Some(ctx) = context() else { return };
                ACTION_FOCUS_INDEX.with(|cell| cell.set(focused_action_index(&top)));
                clear(&top);
                fill_top(&top, &container, episode_id, &loaded.detail, mode, &ctx);
            }
            Ok(Some(loaded)) => render(&container, episode_id, loaded, mode),
            Ok(None) => {}
            Err(e) => tracing::error!("failed to refresh episode {episode_id} details: {e:#}"),
        });
    }
}

/// The action button with keyboard focus inside `within`, by position.
fn focused_action_index(within: &gtk4::Box) -> Option<usize> {
    let mut widget = within.root().and_then(|root| gtk4::prelude::RootExt::focus(&root))?;
    if !widget.is_ancestor(within) {
        return None;
    }
    loop {
        if let Some(index) = widget.widget_name().strip_prefix(ACTION_NAME_PREFIX).and_then(|i| i.parse().ok()) {
            return Some(index);
        }
        widget = widget.parent()?;
    }
}

const ACTION_NAME_PREFIX: &str = "episode-action-";

pub(super) fn set_details_context(context: DetailsContext) {
    CONTEXT.with(|c| *c.borrow_mut() = Some(Rc::new(context)));
}

fn context() -> Option<Rc<DetailsContext>> {
    CONTEXT.with(|c| c.borrow().clone())
}

/// Unloads the player if it holds one of these episodes, before the user
/// deletes them. A no-op off the GTK thread, where no context is set.
pub(super) fn stop_playback_if_current(episode_ids: Vec<i64>) {
    let Some(context) = context() else { return };
    if context.current_episode.get().is_some_and(|id| episode_ids.contains(&id)) {
        if let Err(e) = context.command_tx.send(UiCommand::StopIfCurrent(episode_ids)) {
            tracing::error!("failed to send StopIfCurrent command: {e}");
        }
    }
}

/// Downloads run in the service, so the cancel goes there.
pub(super) fn cancel_download(episode_id: i64) {
    let Some(context) = context() else { return };
    if let Err(e) = context.command_tx.send(UiCommand::CancelDownload(episode_id)) {
        tracing::error!("failed to send CancelDownload command: {e}");
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum DetailsMode {
    Dialog,
    /// Also offers Clear, which removes the episode from History.
    History,
    /// The player's details page: no Play button, the transport controls are
    /// one page away.
    Player,
}

struct Loaded {
    detail: EpisodeDetail,
    chapters: Vec<Chapter>,
    has_transcript: bool,
}

fn load(db_path: &std::path::Path, episode_id: i64) -> anyhow::Result<Option<Loaded>> {
    let conn = phodder_core::open_connection(db_path)?;
    let Some(detail) = phodder_core::subscriptions::get_episode_detail(&conn, episode_id)? else {
        return Ok(None);
    };
    let mut chapters = phodder_core::chapters::list(&conn, episode_id)?;
    if chapters.is_empty() {
        if let Some(path) = &detail.local_file_path {
            chapters = phodder_core::chapters::from_id3_file(std::path::Path::new(path));
        }
    }
    let has_transcript = crate::transcripts::has_transcript(&conn, episode_id);
    Ok(Some(Loaded { detail, chapters, has_transcript }))
}

/// Opens an episode's details in a dialog (a bottom sheet on narrow windows).
pub(super) fn open_episode_details(episode_id: i64) {
    open_episode_details_in(episode_id, DetailsMode::Dialog);
}

pub(super) fn open_episode_details_in(episode_id: i64, mode: DetailsMode) {
    let Some(ctx) = context() else { return };
    let db_path = ctx.db_path.clone();
    let tokio_handle = ctx.tokio_handle.clone();
    run_db_op(
        &tokio_handle,
        ctx.db_path.clone(),
        move |_| load(&db_path, episode_id),
        move |result| match result {
            Ok(Some(loaded)) => {
                // render() adds its own scrolling: the Transcript tab scrolls
                // separately to follow along.
                let container = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).vexpand(true).build();
                let title = loaded.detail.title.clone();
                render(&container, episode_id, loaded, mode);
                // The content shows the title, so the header doesn't repeat it.
                present_dialog_with_header(&container, &title, HeaderTitle::Empty, 480, 640, &ctx.window, &ctx.dialog_sheet, true);
            }
            Ok(None) => tracing::warn!("episode {episode_id} no longer exists"),
            Err(e) => tracing::error!("failed to load episode {episode_id} details: {e:#}"),
        },
    );
}

/// Loads and renders an episode's details into `container`, replacing its children.
pub(super) fn fill_episode_details(container: &gtk4::Box, episode_id: i64, mode: DetailsMode) {
    let Some(ctx) = context() else { return };
    let container = container.clone();
    let db_path = ctx.db_path.clone();
    run_db_op(&ctx.tokio_handle, ctx.db_path.clone(), move |_| load(&db_path, episode_id), move |result| match result {
        Ok(Some(loaded)) => render(&container, episode_id, loaded, mode),
        Ok(None) => clear(&container),
        Err(e) => tracing::error!("failed to load episode {episode_id} details: {e:#}"),
    });
}

/// Details for a back-catalog episode that isn't in the database: header
/// and show notes only.
/// `artwork` is the podcast's cached artwork, if any.
pub(super) fn open_episode_preview(
    title: &str,
    podcast_title: &str,
    artwork: Option<&str>,
    publish_date: &str,
    duration_ms: Option<i64>,
    notes_html: Option<&str>,
) {
    let Some(ctx) = context() else { return };
    let container = content_box();
    container.append(&header(artwork, title, podcast_title, publish_date, duration_ms, None, None));
    container.append(&show_notes(notes_html, None));
    let scrolled = gtk4::ScrolledWindow::builder().child(&container).hscrollbar_policy(gtk4::PolicyType::Never).vexpand(true).build();
    present_dialog_with_header(&scrolled, title, HeaderTitle::Empty, 480, 640, &ctx.window, &ctx.dialog_sheet, true);
}

fn clear(container: &gtk4::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

fn content_box() -> gtk4::Box {
    gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(18)
        .margin_start(18)
        .margin_end(18)
        .margin_top(18)
        .margin_bottom(18)
        .build()
}

/// Opens the player's details on the Transcript tab, for the caption.
pub(super) fn prefer_transcript_tab() {
    PLAYER_TAB.with(|tab| tab.set("transcript"));
}

fn render(container: &gtk4::Box, episode_id: i64, loaded: Loaded, mode: DetailsMode) {
    clear(container);
    let Some(ctx) = context() else { return };
    let d = &loaded.detail;
    let body = content_box();
    let top = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(18).build();
    fill_top(&top, container, episode_id, d, mode, &ctx);
    body.append(&top);
    OPEN.with(|open| {
        let mut open = open.borrow_mut();
        open.retain(|e| e.container.upgrade().is_some_and(|c| c != *container));
        open.push(OpenDetails {
            container: container.downgrade(),
            top: top.downgrade(),
            episode_id,
            mode,
            download_status: d.download_status.clone(),
            has_transcript: loaded.has_transcript,
        });
    });

    let downloaded = d.download_status == "downloaded";
    if !loaded.chapters.is_empty() {
        body.append(&chapters(episode_id, &loaded.chapters, downloaded));
    }
    body.append(&show_notes(d.show_notes_html.as_deref(), downloaded.then_some(episode_id)));
    let notes = gtk4::ScrolledWindow::builder().child(&body).hscrollbar_policy(gtk4::PolicyType::Never).vexpand(true).build();
    if !loaded.has_transcript {
        container.append(&notes);
        return;
    }

    // Show Notes | Transcript. The transcript loads when its tab opens.
    let stack = libadwaita::ViewStack::builder().vexpand(true).build();
    stack.add_titled(&notes, Some("notes"), &gettext("Show Notes"));
    let transcript_slot = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).vexpand(true).build();
    stack.add_titled(&transcript_slot, Some("transcript"), &gettext("Transcript"));
    let switcher = libadwaita::InlineViewSwitcher::builder()
        .stack(&stack)
        .css_classes(["flat"])
        .margin_start(18)
        .margin_end(18)
        .margin_top(6)
        .build();
    body.set_margin_top(12);
    {
        let command_tx = ctx.command_tx.clone();
        let show_transcript = move |stack: &libadwaita::ViewStack| {
            let name = stack.visible_child_name();
            if mode == DetailsMode::Player {
                PLAYER_TAB.with(|tab| tab.set(if name.as_deref() == Some("transcript") { "transcript" } else { "notes" }));
            }
            if name.as_deref() == Some("transcript") && transcript_slot.first_child().is_none() {
                transcript_slot.append(&crate::ui::transcript::build_transcript_page(episode_id, &command_tx));
            }
        };
        stack.connect_visible_child_name_notify(show_transcript);
    }
    if mode == DetailsMode::Player {
        stack.set_visible_child_name(PLAYER_TAB.with(|tab| tab.get()));
    }
    container.append(&switcher);
    container.append(&stack);
}

/// The header and action buttons of a details view.
fn fill_top(top: &gtk4::Box, container: &gtk4::Box, episode_id: i64, d: &EpisodeDetail, mode: DetailsMode, ctx: &Rc<DetailsContext>) {
    let status = if d.played {
        Some(gettext("Played"))
    } else if d.download_status == "downloading" {
        Some(gettext("Downloading…"))
    } else if d.download_status != "downloaded" {
        Some(gettext("Not downloaded"))
    } else if d.playback_position_ms > 0 {
        d.duration_ms.map(|total| gettext_f("{time} left", &[("time", &format_duration_ms((total - d.playback_position_ms).max(0)))]))
    } else {
        None
    };
    top.append(&header(
        d.artwork_local_path.as_deref(),
        &d.title,
        &d.podcast_title,
        &d.publish_date,
        d.duration_ms,
        d.file_size_bytes,
        status.as_deref(),
    ));
    top.append(&actions(container, episode_id, d, mode, ctx));
}

fn header(
    artwork: Option<&str>,
    title: &str,
    podcast_title: &str,
    publish_date: &str,
    duration_ms: Option<i64>,
    size_bytes: Option<i64>,
    status: Option<&str>,
) -> gtk4::Box {
    let row = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(16).build();
    let art = load_thumbnail_image(&[artwork], "audio-x-generic-symbolic", 96);
    art.set_valign(gtk4::Align::Start);
    row.append(&art);

    let text = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(4).hexpand(true).valign(gtk4::Align::Center).build();
    text.append(&gtk4::Label::builder().label(title).wrap(true).xalign(0.0).css_classes(["title-3"]).build());
    if !podcast_title.is_empty() {
        text.append(&gtk4::Label::builder().label(podcast_title).wrap(true).xalign(0.0).css_classes(["dim-label"]).build());
    }
    let date = format_publish_date(publish_date);
    let duration = duration_ms.map(format_duration_ms);
    let size = display_file_size(size_bytes);
    let meta = join_non_empty(&[Some(date.as_str()), duration.as_deref(), size.as_deref()]);
    text.append(&gtk4::Label::builder().label(meta).xalign(0.0).css_classes(["caption", "dim-label"]).build());
    // Always present, blank when there's no status, so marking played or
    // unplayed doesn't move the buttons below.
    text.append(&gtk4::Label::builder().label(status.unwrap_or(" ")).xalign(0.0).css_classes(["caption-heading", "accent"]).build());
    row.append(&text);
    row
}

fn actions(container: &gtk4::Box, episode_id: i64, d: &EpisodeDetail, mode: DetailsMode, ctx: &Rc<DetailsContext>) -> libadwaita::WrapBox {
    let wrap = libadwaita::WrapBox::builder().child_spacing(6).line_spacing(6).build();
    let downloaded = d.download_status == "downloaded";
    let is_current = ctx.current_episode.get() == Some(episode_id);

    // Icon-only, with the label as tooltip, except Mark Played/Unplayed.
    let icon_button = |tooltip: &str, icon: &str, classes: &[&str]| {
        let button = gtk4::Button::builder().icon_name(icon).tooltip_text(tooltip).valign(gtk4::Align::Center).build();
        button.add_css_class("circular");
        for class in classes {
            button.add_css_class(class);
        }
        button
    };
    // Rebuilds this view after the action's write lands.
    let reload = {
        let container = container.clone();
        move || {
            ACTION_FOCUS_INDEX.with(|cell| cell.set(focused_action_index(&container)));
            let container = container.clone();
            gtk4::glib::timeout_add_local_once(RELOAD_AFTER_ACTION, move || fill_episode_details(&container, episode_id, mode));
        }
    };
    let send = {
        let command_tx = ctx.command_tx.clone();
        move |command: UiCommand| {
            if let Err(e) = command_tx.send(command) {
                tracing::error!("failed to send command from episode details: {e}");
            }
        }
    };

    // Closes whichever presentation holds these details.
    let close_details = {
        let dialog_sheet = ctx.dialog_sheet.clone();
        move |button: &gtk4::Button| {
            // On narrow windows the sheet covers the player.
            dialog_sheet.set_open(false);
            if let Some(dialog) = button.ancestor(libadwaita::Dialog::static_type()) {
                dialog.downcast_ref::<libadwaita::Dialog>().map(|d| d.close());
            }
        }
    };

    if mode != DetailsMode::Player && downloaded {
        let label = if is_current { "Playing" } else if d.playback_position_ms > 0 && !d.played { "Resume" } else { "Play" };
        let play = icon_button(label, "media-playback-start-symbolic", &["suggested-action"]);
        play.set_sensitive(!is_current);
        let send = send.clone();
        let close_details = close_details.clone();
        play.connect_clicked(move |button| {
            send(UiCommand::PlayEpisode(episode_id));
            close_details(button);
        });
        wrap.append(&play);
    }

    if downloaded {
        let (label, icon) = if d.queued { ("Remove from Queue", "list-remove-symbolic") } else { ("Add to Queue", "list-add-symbolic") };
        let queue = icon_button(label, icon, &[]);
        let queued = d.queued;
        let send = send.clone();
        let reload = reload.clone();
        queue.connect_clicked(move |_| {
            send(if queued { UiCommand::DequeueEpisode(episode_id) } else { UiCommand::EnqueueEpisode(episode_id) });
            reload();
        });
        wrap.append(&queue);
    } else if d.download_status != "downloading" {
        // "+" downloads and queues; Download only downloads.
        let add = icon_button(&gettext("Download and Add to Queue"), "list-add-symbolic", &[]);
        {
            let command_tx = ctx.command_tx.clone();
            let reload = reload.clone();
            add.connect_clicked(move |_| {
                trigger_manual_download(&command_tx, episode_id, false);
                reload();
            });
        }
        wrap.append(&add);
        let download = icon_button(&gettext("Download"), "folder-download-symbolic", &[]);
        let command_tx = ctx.command_tx.clone();
        let reload = reload.clone();
        download.connect_clicked(move |_| {
            trigger_manual_download(&command_tx, episode_id, true);
            reload();
        });
        wrap.append(&download);
    }

    // Both labels sit in a homogeneous Stack, so the button keeps the wider
    // one's width either way and toggling doesn't rewrap the buttons.
    let played_labels = gtk4::Stack::builder().hhomogeneous(true).build();
    // Plain boxes: AdwButtonContent names the button after its own label,
    // and with two of them the button was announced with both.
    let content = |label: &str, icon: &str| {
        let content = gtk4::Box::builder().spacing(6).halign(gtk4::Align::Center).build();
        let image = decorative_image();
        image.set_icon_name(Some(icon));
        content.append(&image);
        content.append(&gtk4::Label::new(Some(label)));
        content
    };
    played_labels.add_named(&content("Mark Played", "object-select-symbolic"), Some("played"));
    played_labels.add_named(&content("Mark Unplayed", "edit-undo-symbolic"), Some("unplayed"));
    played_labels.set_visible_child_name(if d.played { "unplayed" } else { "played" });
    let played_button = gtk4::Button::builder().child(&played_labels).css_classes(["pill"]).build();
    set_accessible_label(&played_button, &if d.played { gettext("Mark Unplayed") } else { gettext("Mark Played") });
    {
        let played = d.played;
        let send = send.clone();
        let reload = reload.clone();
        played_button.connect_clicked(move |_| {
            send(UiCommand::SetPlayed { episode_id, played: !played });
            reload();
        });
    }
    wrap.append(&played_button);

    // Deleting the loaded episode unloads it first (see delete_episode_file).
    if downloaded {
        let delete = icon_button(&gettext("Delete Download"), "user-trash-symbolic", &["destructive-action"]);
        let db_path = ctx.db_path.clone();
        let toast_overlay = ctx.toast_overlay.clone();
        let send = send.clone();
        let reload = reload.clone();
        delete.connect_clicked(move |_| match delete_episode_file(&db_path, episode_id) {
            Ok(()) => {
                send(UiCommand::NotifyDataChanged);
                reload();
            }
            Err(e) => {
                tracing::error!("failed to delete episode {episode_id}: {e}");
                toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't delete the download")));
            }
        });
        wrap.append(&delete);
    }
    if mode == DetailsMode::History {
        // Two taps: the first arms it for a few seconds.
        let clear = icon_button(&gettext("Clear from History"), "edit-clear-symbolic", &["destructive-action"]);
        let armed = Rc::new(Cell::new(false));
        let db_path = ctx.db_path.clone();
        let toast_overlay = ctx.toast_overlay.clone();
        let send = send.clone();
        clear.connect_clicked(move |button| {
            if !armed.get() {
                armed.set(true);
                button.set_icon_name("object-select-symbolic");
                button.set_tooltip_text(Some(&gettext("Tap again to clear")));
                announce(button, &gettext("Press again to clear"));
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
            armed.set(false);
            match clear_history_episode(&db_path, episode_id) {
                Ok(()) => {
                    // Clearing deletes the file, which can dequeue.
                    send(UiCommand::NotifyDataChanged);
                    close_details(button);
                }
                Err(e) => {
                    tracing::error!("failed to clear history episode {episode_id}: {e}");
                    toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't clear the episode")));
                }
            }
        });
        wrap.append(&clear);
    }
    let mut buttons = Vec::new();
    let mut child = wrap.first_child();
    while let Some(widget) = child {
        widget.set_widget_name(&format!("{ACTION_NAME_PREFIX}{}", buttons.len()));
        child = widget.next_sibling();
        buttons.push(widget);
    }
    if let Some(index) = ACTION_FOCUS_INDEX.with(|cell| cell.take()) {
        if let Some(button) = buttons.get(index).or(buttons.last()).cloned() {
            // After the page is in the window, or grab_focus does nothing.
            gtk4::glib::idle_add_local_once(move || {
                button.grab_focus();
            });
        }
    }
    wrap
}

/// Seeks within the current episode, or starts this one at `position_ms`.
pub(super) fn seek_or_play(episode_id: i64, position_ms: u64) {
    let Some(ctx) = context() else { return };
    let send = |command| {
        if let Err(e) = ctx.command_tx.send(command) {
            tracing::error!("failed to send seek from episode details: {e}");
        }
    };
    if ctx.current_episode.get() != Some(episode_id) {
        send(UiCommand::PlayEpisode(episode_id));
    }
    send(UiCommand::SeekTo(position_ms as i64 * 1000));
}

fn section_heading(text: &str) -> gtk4::Label {
    gtk4::Label::builder().label(text).xalign(0.0).css_classes(["heading"]).build()
}

fn chapters(episode_id: i64, chapters: &[Chapter], playable: bool) -> gtk4::Box {
    let section = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(8).build();
    section.append(&section_heading(&gettext("Chapters")));
    let list = gtk4::ListBox::builder().selection_mode(gtk4::SelectionMode::None).css_classes(["boxed-list"]).build();
    for (index, chapter) in chapters.iter().enumerate() {
        let title = if chapter.title.is_empty() { gettext_f("Chapter {number}", &[("number", &(index + 1).to_string())]) } else { escape_markup(&chapter.title) };
        let row = libadwaita::ActionRow::builder().title(&title).activatable(playable).build();
        let time = gtk4::Label::builder()
            .label(format_duration_ms(chapter.start_ms as i64))
            .css_classes(["numeric", "dim-label"])
            .width_chars(7)
            .xalign(0.0)
            .build();
        row.add_prefix(&time);
        let start_ms = chapter.start_ms;
        row.connect_activated(move |_| seek_or_play(episode_id, start_ms));
        list.append(&row);
    }
    section.append(&list);
    section
}

/// `seekable_episode` makes timestamps in the notes clickable.
fn show_notes(html: Option<&str>, seekable_episode: Option<i64>) -> gtk4::Box {
    let section = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(8).build();
    section.append(&section_heading(&gettext("Show Notes")));
    // Not selectable: on touch, a drag over selectable text selects it
    // instead of scrolling. Links still work.
    let label = gtk4::Label::builder().wrap(true).xalign(0.0).build();
    match html {
        Some(html) if !html.trim().is_empty() => {
            let html = if seekable_episode.is_some() { phodder_core::show_notes::linkify_timestamps(html) } else { html.to_string() };
            match html2pango::markup_html(&html) {
                Ok(markup) => label.set_markup(&markup),
                Err(e) => {
                    tracing::warn!("failed to convert show notes HTML to Pango markup: {e}");
                    label.set_label(&html);
                }
            }
        }
        _ => {
            label.set_label(&gettext("No show notes."));
            label.add_css_class("dim-label");
        }
    }
    label.connect_activate_link(move |_, uri| match phodder_core::show_notes::parse_seek_link(uri) {
        Some(ms) => {
            if let Some(episode_id) = seekable_episode {
                seek_or_play(episode_id, ms);
            }
            gtk4::glib::Propagation::Stop
        }
        None => gtk4::glib::Propagation::Proceed,
    });
    section.append(&label);
    section
}
