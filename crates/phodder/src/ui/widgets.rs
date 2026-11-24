use super::*;

/// "1:23:45", or "23:45" under an hour.
pub(super) fn format_duration_ms(duration_ms: i64) -> String {
    let total_seconds = duration_ms / 1000;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// "Jan 05, 2026"; the raw string if it doesn't parse.
pub(super) fn format_publish_date(rfc3339: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(rfc3339).map(|dt| dt.format("%b %d, %Y").to_string()).unwrap_or_else(|_| rfc3339.to_string())
}

/// "queued" shows as "Pending"; the rest are title-cased to match.
pub(super) fn display_download_status(status: &str) -> Option<String> {
    match status {
        "queued" => Some(gettext("Pending")),
        "downloading" => Some(gettext("Downloading")),
        "downloaded" => Some(gettext("Downloaded")),
        "failed" => Some(gettext("Failed")),
        // 'none' (deleted or never queued) shows nothing.
        "none" => None,
        other => Some(other.to_string()),
    }
}

/// Joins the present, non-blank parts with " · ". Callers pass `None` for
/// missing values; per-field "·" placeholders produced "5:17 ·  ·  · none".
pub(super) fn join_non_empty(parts: &[Option<&str>]) -> String {
    parts.iter().filter_map(|p| *p).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ")
}

thread_local! {
    static THUMBNAIL_CACHE: std::cell::RefCell<std::collections::HashMap<String, gtk4::gdk::Texture>> = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Un-arms a confirm button after 4 seconds without a second click. An
/// ancestor GestureClick that disarmed on any other click was rejected: it
/// fires on press, before the button's own release-time "clicked", so every
/// confirm click disarmed and then re-armed the button.
pub(super) fn auto_expire_confirm_arm(
    is_armed: impl Fn() -> bool + 'static,
    clear_armed: impl Fn() + 'static,
    on_expire: impl Fn() + 'static,
) {
    gtk4::glib::source::timeout_add_local_once(std::time::Duration::from_secs(4), move || {
        if is_armed() {
            clear_armed();
            on_expire();
        }
    });
}

/// Loads the first candidate that decodes, falling through on missing or
/// unreadable files, then the fallback icon. Decodes with Pixbuf to detect
/// failures, since `Image::from_file` reports none, and builds the Image
/// from a Texture because `Image::from_pixbuf` is deprecated since GTK 4.12.
pub(super) fn load_thumbnail_image(candidate_paths: &[Option<&str>], fallback_icon: &str, size: i32) -> gtk4::Image {
    for path in candidate_paths.iter().flatten() {
        if let Some(texture) = THUMBNAIL_CACHE.with(|cache| cache.borrow().get(*path).cloned()) {
            let image = decorative_image();
            image.set_paintable(Some(&texture));
            image.set_pixel_size(size);
            return image;
        }
        match gtk4::gdk_pixbuf::Pixbuf::from_file(path) {
            Ok(pixbuf) => {
                let texture = gtk4::gdk::Texture::for_pixbuf(&pixbuf);
                THUMBNAIL_CACHE.with(|cache| cache.borrow_mut().insert(path.to_string(), texture.clone()));
                let image = decorative_image();
                image.set_paintable(Some(&texture));
                image.set_pixel_size(size);
                return image;
            }
            Err(e) => {
                tracing::warn!("failed to load thumbnail image at {path}: {e}");
            }
        }
    }
    let image = decorative_image();
    image.set_icon_name(Some(fallback_icon));
    image.set_pixel_size(size);
    image
}

/// Hidden from screen readers. For artwork and icons next to text that says
/// the same thing.
pub(super) fn decorative_image() -> gtk4::Image {
    gtk4::Image::builder().accessible_role(gtk4::AccessibleRole::Presentation).build()
}

/// The name screen readers announce. Icon-only buttons get it from their
/// tooltip; this is for widgets without one, or whose content would read
/// badly.
pub(super) fn set_accessible_label(widget: &impl IsA<gtk4::Accessible>, label: &str) {
    widget.update_property(&[gtk4::accessible::Property::Label(label)]);
}

/// Gives a popover menu's items their text as their accessible names. GTK
/// 4.20 labels each item by a label it also hides from screen readers, so
/// the name came out empty and screen readers said only "menu item". The
/// relation outranks the label property, so it's removed.
pub(super) fn name_menu_items(widget: &gtk4::Widget) {
    let mut child = widget.first_child();
    while let Some(current) = child {
        if current.type_().name() == "GtkModelButton" {
            if let Some(text) = current.property_value("text").get::<Option<String>>().ok().flatten() {
                current.reset_relation(gtk4::AccessibleRelation::LabelledBy);
                set_accessible_label(&current, &text.replace('_', ""));
            }
        }
        name_menu_items(&current);
        child = current.next_sibling();
    }
}

/// After an episode pager replaces its page, focuses the new page's button
/// of the same name ("pager-previous" or "pager-next", or the other one at
/// the first or last page) and announces the page. The pressed button was
/// destroyed with the old page, which dropped keyboard focus.
pub(super) fn refocus_pager(slot: &gtk4::Box, button_name: &'static str, page_label: String) {
    let slot = slot.clone();
    gtk4::glib::idle_add_local_once(move || {
        let find = |name: &str| find_descendant_by_name(slot.upcast_ref(), name);
        let other = if button_name == "pager-next" { "pager-previous" } else { "pager-next" };
        if let Some(button) = find(button_name).filter(|b| b.is_sensitive()).or_else(|| find(other)) {
            button.grab_focus();
            announce(&button, &page_label);
        }
    });
}

fn find_descendant_by_name(widget: &gtk4::Widget, name: &str) -> Option<gtk4::Widget> {
    let mut child = widget.first_child();
    while let Some(current) = child {
        if current.widget_name() == name {
            return Some(current);
        }
        if let Some(found) = find_descendant_by_name(&current, name) {
            return Some(found);
        }
        child = current.next_sibling();
    }
    None
}

/// "Page 2 of 9", for the episode pagers.
pub(super) fn page_of(page: usize, total: usize) -> String {
    gettext_f("Page {page} of {total}", &[("page", &page.to_string()), ("total", &total.to_string())])
}

/// Spoken by the screen reader without moving focus.
pub(super) fn announce(widget: &impl IsA<gtk4::Accessible>, message: &str) {
    widget.announce(message, gtk4::AccessibleAnnouncementPriority::Medium);
}

/// Like load_thumbnail_image, for a thumbnail already on screen.
pub(super) fn swap_thumbnail_image(image: &gtk4::Image, path: &str) {
    match gtk4::gdk_pixbuf::Pixbuf::from_file(path) {
        Ok(pixbuf) => {
            let texture = gtk4::gdk::Texture::for_pixbuf(&pixbuf);
            image.set_paintable(Some(&texture));
        }
        Err(e) => {
            tracing::warn!("failed to load fetched episode artwork at {path}: {e}");
        }
    }
}

/// Closes either presentation: `AdwDialog::close()` or
/// `AdwBottomSheet::set_open(false)`.
#[derive(Clone)]
pub(super) enum AdaptivePresentation {
    Floating(libadwaita::Dialog),
    BottomSheet(libadwaita::BottomSheet),
}

impl AdaptivePresentation {
    pub(super) fn close(&self) {
        match self {
            AdaptivePresentation::Floating(dialog) => {
                dialog.close();
            }
            AdaptivePresentation::BottomSheet(sheet) => sheet.set_open(false),
        }
    }

    /// Runs `callback` once when this presentation closes, however it closes.
    /// `dialog_sheet` is shared by every dialog, so its handler disconnects
    /// itself after firing; each AdwDialog is new and needs no cleanup.
    pub(super) fn on_closed(&self, callback: impl Fn() + 'static) {
        match self {
            AdaptivePresentation::Floating(dialog) => {
                dialog.connect_closed(move |_| callback());
            }
            AdaptivePresentation::BottomSheet(sheet) => {
                let sheet_for_disconnect = sheet.clone();
                let handler_id: std::rc::Rc<std::cell::RefCell<Option<gtk4::glib::SignalHandlerId>>> =
                    std::rc::Rc::new(std::cell::RefCell::new(None));
                let handler_id_for_closure = handler_id.clone();
                let id = sheet.connect_open_notify(move |sheet| {
                    if !sheet.is_open() {
                        callback();
                        if let Some(id) = handler_id_for_closure.borrow_mut().take() {
                            sheet_for_disconnect.disconnect(id);
                        }
                    }
                });
                *handler_id.borrow_mut() = Some(id);
            }
        }
    }
}

/// Whether present_dialog_adaptively will use the bottom sheet at the
/// window's current width, for callers that build content differently. Uses
/// NARROW_WIDTH_THRESHOLD, shared with the tab layout switch.
pub(super) fn will_use_bottom_sheet(parent: &libadwaita::ApplicationWindow) -> bool {
    parent.width() <= NARROW_WIDTH_THRESHOLD
}

/// What a dialog's header bar shows in the middle.
pub(super) enum HeaderTitle<'a> {
    /// The dialog's title.
    Text,
    /// Nothing, for content that shows its own title (episode details).
    Empty,
    /// Tabs, say.
    Widget(&'a gtk4::Widget),
}

/// An AdwDialog on wide windows, the window's shared AdwBottomSheet on
/// narrow ones. AdwDialog's own bottom-sheet mode was rejected: it stayed
/// narrow whatever `content-width` was set to. Both have a header bar with
/// the title and a close button, and a Back button when the sheet replaces
/// another one.
pub(super) fn present_dialog_adaptively(
    content: &impl IsA<gtk4::Widget>,
    title: &str,
    content_width: i32,
    content_height: i32,
    parent: &libadwaita::ApplicationWindow,
    dialog_sheet: &libadwaita::BottomSheet,
    can_close: bool,
) -> AdaptivePresentation {
    present_dialog(content, title, HeaderTitle::Text, content_width, content_height, parent, dialog_sheet, can_close, false).0
}

/// Like present_dialog_adaptively, with something else in the header's
/// middle.
#[allow(clippy::too_many_arguments)]
pub(super) fn present_dialog_with_header(
    content: &impl IsA<gtk4::Widget>,
    title: &str,
    header_title: HeaderTitle,
    content_width: i32,
    content_height: i32,
    parent: &libadwaita::ApplicationWindow,
    dialog_sheet: &libadwaita::BottomSheet,
    can_close: bool,
) -> AdaptivePresentation {
    present_dialog(content, title, header_title, content_width, content_height, parent, dialog_sheet, can_close, false).0
}

/// For a page reached from another (a directory result from Add Podcast):
/// the header always has Back, which returns to what was showing before
/// (the previous sheet when nested, otherwise closing). The closure does
/// the same.
#[allow(clippy::too_many_arguments)]
pub(super) fn present_dialog_with_back(
    content: &impl IsA<gtk4::Widget>,
    title: &str,
    header_title: HeaderTitle,
    content_width: i32,
    content_height: i32,
    parent: &libadwaita::ApplicationWindow,
    dialog_sheet: &libadwaita::BottomSheet,
) -> (AdaptivePresentation, std::rc::Rc<dyn Fn()>) {
    present_dialog(content, title, header_title, content_width, content_height, parent, dialog_sheet, true, true)
}

#[allow(clippy::too_many_arguments)]
fn present_dialog(
    content: &impl IsA<gtk4::Widget>,
    title: &str,
    header_title: HeaderTitle,
    content_width: i32,
    content_height: i32,
    parent: &libadwaita::ApplicationWindow,
    dialog_sheet: &libadwaita::BottomSheet,
    can_close: bool,
    always_back: bool,
) -> (AdaptivePresentation, std::rc::Rc<dyn Fn()>) {
    let header = libadwaita::HeaderBar::new();
    match header_title {
        HeaderTitle::Text => header.set_title_widget(Some(&libadwaita::WindowTitle::new(title, ""))),
        HeaderTitle::Empty => header.set_title_widget(Some(&gtk4::Box::new(gtk4::Orientation::Horizontal, 0))),
        HeaderTitle::Widget(widget) => header.set_title_widget(Some(widget)),
    }
    let back_button = || gtk4::Button::builder().icon_name("go-previous-symbolic").tooltip_text(gettext("Back")).build();
    let toolbar = libadwaita::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(content));

    // The larger of the requested height and 80% of the window.
    // AdwBottomSheet sizes the sheet to its content, so the minimum goes on
    // the content.
    let effective_height = content_height.max((parent.height() as f64 * 0.8) as i32);
    if will_use_bottom_sheet(parent) {
        // Opened from inside another sheet (an episode from a subscription's
        // Episodes tab): Back restores that sheet, which the shared
        // dialog_sheet would otherwise lose. Floating dialogs stack natively.
        let previous = dialog_sheet.is_open().then(|| dialog_sheet.sheet()).flatten().map(|sheet| (sheet, dialog_sheet.can_close()));
        let go_back: std::rc::Rc<dyn Fn()> = {
            let dialog_sheet = dialog_sheet.clone();
            let previous = previous.clone();
            std::rc::Rc::new(move || match &previous {
                Some((previous_sheet, previous_can_close)) => {
                    dialog_sheet.set_can_close(*previous_can_close);
                    dialog_sheet.set_sheet(Some(previous_sheet));
                }
                None => dialog_sheet.set_open(false),
            })
        };
        // The window's own buttons don't belong on a sheet, and the drag
        // handle needs room above the title.
        header.set_show_start_title_buttons(false);
        header.set_show_end_title_buttons(false);
        header.set_margin_top(12);
        if previous.is_some() || always_back {
            let back = back_button();
            let go_back = go_back.clone();
            back.connect_clicked(move |_| go_back());
            header.pack_start(&back);
        }
        if can_close {
            let close = gtk4::Button::builder().icon_name("window-close-symbolic").tooltip_text(gettext("Close")).build();
            let dialog_sheet = dialog_sheet.clone();
            close.connect_clicked(move |_| dialog_sheet.set_open(false));
            header.pack_end(&close);
        }
        toolbar.set_size_request(-1, effective_height);
        // can-close false only blocks swipe and click-outside; set_open still
        // works. Reset on every presentation, since the sheet is shared
        // between dialogs.
        dialog_sheet.set_can_close(can_close);
        dialog_sheet.set_sheet(Some(&toolbar));
        dialog_sheet.set_open(true);
        (AdaptivePresentation::BottomSheet(dialog_sheet.clone()), go_back)
    } else {
        let dialog = libadwaita::Dialog::builder()
            .title(title)
            .content_width(content_width)
            .content_height(effective_height)
            .child(&toolbar)
            .can_close(can_close)
            .build();
        // Floating dialogs stack, so closing returns to the one beneath.
        let go_back: std::rc::Rc<dyn Fn()> = {
            let dialog = dialog.clone();
            std::rc::Rc::new(move || {
                dialog.close();
            })
        };
        if always_back {
            let back = back_button();
            let go_back = go_back.clone();
            back.connect_clicked(move |_| go_back());
            header.pack_start(&back);
        }
        dialog.present(Some(parent));
        (AdaptivePresentation::Floating(dialog), go_back)
    }
}

/// Stops scrolling over a SpinButton from changing its value, with a
/// capture-phase scroll controller on the button that claims every event.
/// Unverified: the same controller on a parent was reported not to work
/// (GNOME Discourse), so check this first if scrolling still changes values.
pub(super) fn disable_spin_button_scroll(spin_button: &gtk4::SpinButton) {
    let controller = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::BOTH_AXES);
    controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
    controller.connect_scroll(|_, _, _| gtk4::glib::Propagation::Stop);
    spin_button.add_controller(controller);
}

/// Icon for a skip button's amount and direction, shared by the player and
/// Settings.
pub(super) fn skip_icon_name(ms: i64, is_forward: bool) -> &'static str {
    let is_30 = ms >= 30_000;
    match (is_forward, is_30) {
        (false, false) => "skip-backwards-10-symbolic",
        (false, true) => "skip-backwards-30-symbolic",
        (true, false) => "skip-forward-10-symbolic",
        (true, true) => "skip-forward-30-symbolic",
    }
}

pub(super) fn skip_tooltip_text(ms: i64, is_forward: bool) -> String {
    let seconds = ms / 1000;
    let seconds_text = seconds.to_string();
    let args = [("seconds", seconds_text.as_str())];
    if is_forward {
        ngettext_f("Forward {seconds} second", "Forward {seconds} seconds", seconds as u32, &args)
    } else {
        ngettext_f("Back {seconds} second", "Back {seconds} seconds", seconds as u32, &args)
    }
}

/// Runs `on_activate` from a capture-phase GestureClick that claims the
/// press, so a suffix button's click doesn't also activate the row
/// containing it. `clicked` covers the keyboard and screen readers, which
/// the gesture doesn't see; the `clicked` that a pointer release also
/// emits is skipped.
pub(super) fn wire_suffix_button_action(button: &gtk4::Button, on_activate: impl Fn() + 'static) {
    let on_activate = std::rc::Rc::new(on_activate);
    let pointer_handled = std::rc::Rc::new(std::cell::Cell::new(false));
    let gesture = gtk4::GestureClick::new();
    gesture.set_propagation_phase(gtk4::PropagationPhase::Capture);
    gesture.connect_pressed(|gesture, _, _, _| {
        gesture.set_state(gtk4::EventSequenceState::Claimed);
    });
    {
        let on_activate = on_activate.clone();
        let pointer_handled = pointer_handled.clone();
        gesture.connect_released(move |_, _, _, _| {
            on_activate();
            // `clicked` follows in the same event if it comes at all; the
            // flag is cleared afterwards so a missing one can't swallow the
            // next keyboard activation.
            pointer_handled.set(true);
            let pointer_handled = pointer_handled.clone();
            gtk4::glib::idle_add_local_once(move || pointer_handled.set(false));
        });
    }
    button.add_controller(gesture);
    button.connect_clicked(move |_| {
        if !pointer_handled.replace(false) {
            on_activate();
        }
    });
}

/// Runs a blocking database operation off the GTK thread and `on_complete`
/// back on it; the synchronous version froze the UI. The connection opens
/// inside the blocking task.
pub(super) fn run_db_op<T: Send + 'static>(
    tokio_handle: &tokio::runtime::Handle,
    db_path: std::path::PathBuf,
    op: impl FnOnce(&rusqlite::Connection) -> anyhow::Result<T> + Send + 'static,
    on_complete: impl FnOnce(anyhow::Result<T>) + 'static,
) {
    let tokio_handle = tokio_handle.clone();
    gtk4::glib::spawn_future_local(async move {
        let result = tokio_handle
            .spawn_blocking(move || phodder_core::open_connection(&db_path).map_err(anyhow::Error::from).and_then(|conn| op(&conn)))
            .await
            .unwrap_or_else(|e| Err(anyhow::anyhow!("blocking DB task panicked: {e}")));
        on_complete(result);
    });
}

/// History's "+" and download-only, by episode id; History episodes always
/// have a row.
pub(super) fn trigger_manual_download(command_tx: &UnboundedSender<UiCommand>, episode_id: i64, skip_enqueue: bool) {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    if let Err(e) = command_tx.send(UiCommand::ManuallyDownloadEpisode { episode_id, skip_enqueue, reply_tx }) {
        tracing::error!("failed to send ManuallyDownloadEpisode command: {e}");
        return;
    }
    gtk4::glib::spawn_future_local(async move {
        match reply_rx.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::error!("failed to manually download history episode: {e}");
            }
            Err(_) => {
                tracing::error!("failed to manually download history episode: the backend didn't respond");
            }
        }
    });
}

/// Escapes feed text for markup-enabled properties such as row titles; an
/// unescaped `&` ("Emily & Matt") makes GTK drop the title with a warning.
pub(super) fn escape_markup(text: &str) -> String {
    gtk4::glib::markup_escape_text(text).to_string()
}

/// Parses gdk::RGBA::to_str output ("rgb(r,g,b)" or "rgba(r,g,b,a)",
/// components 0-255) to build CSS tints.
pub(super) fn parse_rgb_components(color: &str) -> Option<(u8, u8, u8)> {
    let inner = color.strip_prefix("rgba(").or_else(|| color.strip_prefix("rgb("))?.strip_suffix(')')?;
    let mut parts = inner.split(',');
    let r = parts.next()?.trim().parse().ok()?;
    let g = parts.next()?.trim().parse().ok()?;
    let b = parts.next()?.trim().parse().ok()?;
    Some((r, g, b))
}

pub(super) fn placeholder_page(title: &str, icon_name: &str) -> libadwaita::StatusPage {
    // Without vexpand the StatusPage takes its natural height, leaving Add
    // Feed high above a large gap.
    libadwaita::StatusPage::builder()
        .title(title)
        .icon_name(icon_name)
        .vexpand(true)
        .build()
}

thread_local! {
    static SLOT_FINGERPRINTS: std::cell::RefCell<std::collections::HashMap<usize, u64>> = Default::default();
}

/// True when `data` matches what `slot` was last built from, so a rebuild can
/// be skipped; records the new fingerprint otherwise. Rebuilding an unchanged
/// tab reset its scroll position and flickered on every refresh.
pub(super) fn slot_data_unchanged(slot: &gtk4::Box, data: &impl std::fmt::Debug) -> bool {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    format!("{data:?}").hash(&mut hasher);
    let fingerprint = hasher.finish();
    let key = slot.as_ptr() as usize;
    let previous = SLOT_FINGERPRINTS.with(|f| f.borrow_mut().insert(key, fingerprint));
    slot.first_child().is_some() && previous == Some(fingerprint)
}

/// Changes a button's icon, keeping a custom-sized Image child if it has one;
/// `set_icon_name` would replace it with a default-size icon.
pub(super) fn set_button_icon(button: &gtk4::Button, icon: &str) {
    match button.child().and_downcast::<gtk4::Image>() {
        Some(image) => image.set_icon_name(Some(icon)),
        None => button.set_icon_name(icon),
    }
}

/// "2.5 MB", or None for a missing or placeholder size: feeds sometimes give
/// 0 or 1 byte, which displayed as "0.0 MB".
pub(super) fn display_file_size(bytes: Option<i64>) -> Option<String> {
    bytes.filter(|b| *b >= 50_000).map(|b| gettext_f("{size} MB", &[("size", &format!("{:.1}", b as f64 / 1_000_000.0))]))
}

/// Adds a stylesheet for the whole display, for per-widget colors set by id.
pub(super) fn add_display_css(css: &str) {
    let provider = gtk4::CssProvider::new();
    provider.load_from_string(css);
    if let Some(display) = gtk4::gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(&display, &provider, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
}

/// A speed slider from 0.5× to 4× in 0.25× steps: short ticks at .25 and
/// .75, medium at .50, and labelled full ones at whole numbers.
pub(super) fn speed_slider() -> gtk4::Scale {
    let scale = gtk4::Scale::with_range(gtk4::Orientation::Horizontal, PLAYBACK_RATES[0], PLAYBACK_RATES[PLAYBACK_RATES.len() - 1], 0.25);
    scale.set_value(1.0);
    scale.set_increments(0.25, 0.5);
    scale.set_digits(2);
    scale.set_draw_value(true);
    scale.set_value_pos(gtk4::PositionType::Top);
    scale.set_format_value_func(|_, value| format!("{value:.2}×"));
    scale.add_css_class("phodder-speed");
    set_accessible_label(&scale, &gettext("Playback speed"));
    for rate in PLAYBACK_RATES {
        let label = (rate.fract() == 0.0).then(|| format!("{rate:.0}×"));
        scale.add_mark(rate, gtk4::PositionType::Bottom, label.as_deref());
    }
    thread_local! {
        static STYLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    if !STYLED.with(|s| s.replace(true)) {
        // Ticks follow the marks' order: whole numbers are 3rd, 7th, …;
        // halves 1st, 5th, ….
        let css = gtk4::CssProvider::new();
        css.load_from_string(
            ".phodder-speed marks.bottom mark indicator { min-height: 4px; }
             .phodder-speed marks.bottom mark:nth-child(4n+1) indicator { min-height: 8px; }
             .phodder-speed marks.bottom mark:nth-child(4n+3) indicator { min-height: 12px; }",
        );
        if let Some(display) = gtk4::gdk::Display::default() {
            gtk4::style_context_add_provider_for_display(&display, &css, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);
        }
    }
    scale.connect_change_value(|scale, _, value| {
        scale.set_value((value * 4.0).round() / 4.0);
        gtk4::glib::Propagation::Stop
    });
    scale
}

/// Calls `f` with the speed once the slider stops moving; each change is
/// saved and applied with a seek.
pub(super) fn connect_speed_settled(scale: &gtk4::Scale, f: impl Fn(f64) + 'static) {
    let f = std::rc::Rc::new(f);
    let pending: std::rc::Rc<std::cell::RefCell<Option<gtk4::glib::SourceId>>> = std::rc::Rc::default();
    scale.connect_value_changed(move |scale| {
        if let Some(source) = pending.borrow_mut().take() {
            source.remove();
        }
        let rate = scale.value();
        let f = f.clone();
        let pending_done = pending.clone();
        let source = gtk4::glib::timeout_add_local_once(std::time::Duration::from_millis(300), move || {
            pending_done.borrow_mut().take();
            f(rate);
        });
        *pending.borrow_mut() = Some(source);
    });
}
