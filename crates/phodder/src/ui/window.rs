use super::*;

/// Quiet period after the last scrub-bar movement before seeking.
pub(super) const SCRUB_SEEK_DELAY: std::time::Duration = std::time::Duration::from_millis(200);

/// The one width threshold for the tab layout (`apply_width_layout`) and
/// dialog presentation (`will_use_bottom_sheet`).
pub(super) const NARROW_WIDTH_THRESHOLD: i32 = 850;

/// Manage Subscription's hero thumbnail size, scaling linearly with window
/// width from 96px to 265px at NARROW_WIDTH_THRESHOLD + 400. Follows the
/// window, not the dialog, which stays 420px when floating.
pub(super) fn hero_thumbnail_size_for_width(window_width: i32) -> i32 {
    const MIN_SIZE: f64 = 96.0;
    const MAX_SIZE: f64 = 265.0;
    // Below this width the thumbnail stays at MIN_SIZE.
    const NARROWEST_PRACTICAL_WIDTH: i32 = 280;

    if window_width <= NARROWEST_PRACTICAL_WIDTH {
        return MIN_SIZE as i32;
    }
    if window_width >= NARROW_WIDTH_THRESHOLD {
        return MAX_SIZE as i32;
    }

    let progress = (window_width - NARROWEST_PRACTICAL_WIDTH) as f64 / (NARROW_WIDTH_THRESHOLD - NARROWEST_PRACTICAL_WIDTH) as f64;
    (MIN_SIZE + progress * (MAX_SIZE - MIN_SIZE)) as i32
}

/// Builds the main window and wires it to the backend; main.rs presents it.
/// Tabs read the database through their own connections (safe under WAL) and
/// rebuild on `UiUpdate::DataChanged`.
pub fn build_window(
    app: &libadwaita::Application,
    command_tx: UnboundedSender<UiCommand>,
    update_rx: async_channel::Receiver<UiUpdate>,
    db_path: &std::path::Path,
    tokio_handle: tokio::runtime::Handle,
) -> libadwaita::ApplicationWindow {
    // The Default group takes the system accent color on a fresh database
    // (color unset; see schema.sql). StyleManager needs GTK initialized, so
    // this can't happen in main.rs.
    if let Ok(conn) = phodder_core::open_connection(db_path) {
        if let Ok(groups) = phodder_core::groups::list_groups(&conn) {
            if let Some(default_group) = groups.iter().find(|g| g.id == phodder_core::groups::DEFAULT_GROUP_ID) {
                if default_group.color.is_none() {
                    let accent = libadwaita::StyleManager::default().accent_color_rgba();
                    if let Err(e) =
                        phodder_core::groups::set_group_color(&conn, phodder_core::groups::DEFAULT_GROUP_ID, Some(&accent.to_str()))
                    {
                        tracing::error!("failed to set Default group's own initial accent color: {e}");
                    }
                }
            }
        }
    }

    // Separate widget sets for the bottom bar and the sheet, since a widget
    // has one parent; both update on every PlaybackState.
    let title_label = gtk4::Label::builder()
        .label(gettext("Nothing playing"))
        .halign(gtk4::Align::Start)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    let subtitle_label = gtk4::Label::builder()
        .label("")
        .halign(gtk4::Align::Start)
        .css_classes(["caption", "dim-label"])
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .build();
    let play_pause_button = gtk4::Button::from_icon_name("media-playback-start-symbolic");
    play_pause_button.set_tooltip_text(Some(&gettext("Play")));
    play_pause_button.set_valign(gtk4::Align::Center);
    play_pause_button.add_css_class("circular");
    play_pause_button.add_css_class("flat");
    // Starts as the fallback icon; swapped once playback state carries
    // artwork.
    let mini_thumbnail = load_thumbnail_image(&[None], "audio-x-generic-symbolic", 40);
    mini_thumbnail.set_valign(gtk4::Align::Center);
    // Display only; seeking is in the sheet.
    let mini_progress_bar = gtk4::ProgressBar::builder().show_text(false).valign(gtk4::Align::End).build();
    set_accessible_label(&mini_progress_bar, &gettext("Playback position"));
    mini_progress_bar.set_size_request(-1, 3);

    let sheet_title_label = gtk4::Label::builder()
        .label(gettext("Nothing playing"))
        .halign(gtk4::Align::Center)
        .justify(gtk4::Justification::Center)
        .wrap(true)
        .css_classes(["title-1"])
        .build();
    let sheet_subtitle_label = gtk4::Label::builder()
        .label("")
        .halign(gtk4::Align::Center)
        .css_classes(["dim-label"])
        .build();
    let sheet_chapter_label = gtk4::Label::builder()
        .halign(gtk4::Align::Center)
        .css_classes(["caption-heading", "accent"])
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .visible(false)
        .build();
    let sheet_artwork = load_thumbnail_image(&[None], "audio-x-generic-symbolic", 240);
    sheet_artwork.set_halign(gtk4::Align::Center);
    sheet_artwork.set_margin_bottom(8);

    let scrub_bar = gtk4::Scale::builder().orientation(gtk4::Orientation::Horizontal).draw_value(false).build();
    scrub_bar.set_range(0.0, 1.0);
    set_accessible_label(&scrub_bar, &gettext("Position"));
    // The times flank the scrub bar, saving a row for the transcript
    // caption. Fixed widths with tabular digits keep the bar from shifting
    // as they tick.
    let time_label = |xalign: f32| {
        gtk4::Label::builder().label("0:00").xalign(xalign).width_chars(7).css_classes(["caption", "dim-label", "numeric"]).build()
    };
    let position_label = time_label(1.0);
    let duration_label = time_label(0.0);
    scrub_bar.set_hexpand(true);
    let scrub_labels_row = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(4).build();
    scrub_labels_row.append(&position_label);
    scrub_labels_row.append(&scrub_bar);
    scrub_labels_row.append(&duration_label);

    let current_duration_us: std::rc::Rc<std::cell::Cell<i64>> = std::rc::Rc::new(std::cell::Cell::new(0));

    // `change-value` fires only for user input (drag, keys, scroll), so the
    // position ticker's set_value never triggers a seek. Input is debounced
    // into one seek, and the ticker leaves the slider alone while a seek is
    // pending so it doesn't jump under the pointer. A separate GestureClick
    // on the Scale for press/release detection stopped drags from seeking.
    let pending_scrub_seek: std::rc::Rc<std::cell::RefCell<Option<gtk4::glib::SourceId>>> = Default::default();
    {
        let command_tx = command_tx.clone();
        let current_duration_us = current_duration_us.clone();
        let pending_scrub_seek = pending_scrub_seek.clone();
        let position_label = position_label.clone();
        let scrub_bar_for_text = scrub_bar.clone();
        scrub_bar.connect_change_value(move |_, _, value| {
            let duration_us = current_duration_us.get();
            if duration_us <= 0 {
                return gtk4::glib::Propagation::Stop;
            }
            let target_us = (value.clamp(0.0, 1.0) * duration_us as f64) as i64;
            position_label.set_label(&format_duration_ms(target_us / 1000));
            set_scrub_value_text(scrub_bar_for_text.upcast_ref(), target_us, duration_us);
            if let Some(previous) = pending_scrub_seek.borrow_mut().take() {
                previous.remove();
            }
            let command_tx = command_tx.clone();
            let pending = pending_scrub_seek.clone();
            let source = gtk4::glib::timeout_add_local_once(SCRUB_SEEK_DELAY, move || {
                // Dropped without remove(): the source has already fired.
                pending.borrow_mut().take();
                if let Err(e) = command_tx.send(UiCommand::SeekTo(target_us)) {
                    tracing::error!("failed to send SeekTo command: {e}");
                }
            });
            pending_scrub_seek.borrow_mut().replace(source);
            gtk4::glib::Propagation::Proceed
        });
    }

    // Tooltips follow the configured amounts.
    let (initial_skip_backward_ms, initial_skip_forward_ms) = {
        let conn = phodder_core::open_connection(db_path).ok();
        let backward_ms = conn.as_ref().and_then(|c| phodder_core::settings::get_setting_i64(c, "skip_backward_ms")).unwrap_or(10_000);
        let forward_ms = conn.as_ref().and_then(|c| phodder_core::settings::get_setting_i64(c, "skip_forward_ms")).unwrap_or(30_000);
        (backward_ms, forward_ms)
    };
    let skip_backward_button = big_icon_button(skip_icon_name(initial_skip_backward_ms, false));
    skip_backward_button.set_tooltip_text(Some(&skip_tooltip_text(initial_skip_backward_ms, false)));
    let skip_forward_button = big_icon_button(skip_icon_name(initial_skip_forward_ms, true));
    skip_forward_button.set_tooltip_text(Some(&skip_tooltip_text(initial_skip_forward_ms, true)));
    {
        let command_tx = command_tx.clone();
        skip_backward_button.connect_clicked(move |_| {
            if let Err(e) = command_tx.send(UiCommand::SkipBackward) {
                tracing::error!("failed to send SkipBackward command: {e}");
            }
        });
    }
    {
        let command_tx = command_tx.clone();
        skip_forward_button.connect_clicked(move |_| {
            if let Err(e) = command_tx.send(UiCommand::SkipForward) {
                tracing::error!("failed to send SkipForward command: {e}");
            }
        });
    }

    let sheet_play_pause_icon = decorative_image();
    sheet_play_pause_icon.set_icon_name(Some("media-playback-start-symbolic"));
    sheet_play_pause_icon.set_pixel_size(32);
    let sheet_play_pause_button = gtk4::Button::builder().child(&sheet_play_pause_icon).tooltip_text(gettext("Play")).build();
    sheet_play_pause_button.set_valign(gtk4::Align::Center);
    sheet_play_pause_button.set_css_classes(&["circular", "suggested-action"]);
    sheet_play_pause_button.set_size_request(68, 68);

    let transport_button = |icon: &str, tooltip: &str, command: fn() -> UiCommand| {
        let button = big_icon_button(icon);
        button.set_tooltip_text(Some(tooltip));
        let command_tx = command_tx.clone();
        button.connect_clicked(move |_| {
            if let Err(e) = command_tx.send(command()) {
                tracing::error!("failed to send transport command: {e}");
            }
        });
        button
    };
    let restart_button = transport_button("media-skip-backward-symbolic", &gettext("Restart Episode"), || UiCommand::RestartEpisode);
    let previous_chapter_button = transport_button("media-seek-backward-symbolic", &gettext("Previous Chapter"), || UiCommand::PreviousChapter);
    let next_chapter_button = transport_button("media-seek-forward-symbolic", &gettext("Next Chapter"), || UiCommand::NextChapter);
    let next_episode_button = transport_button("media-skip-forward-symbolic", &gettext("Next Episode"), || UiCommand::NextEpisode);
    // Seven controls must fit a 360px phone inside the sheet's margins.
    let transport_row = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(0).halign(gtk4::Align::Center).build();
    transport_row.append(&restart_button);
    transport_row.append(&previous_chapter_button);
    transport_row.append(&skip_backward_button);
    transport_row.append(&sheet_play_pause_button);
    transport_row.append(&skip_forward_button);
    transport_row.append(&next_chapter_button);
    transport_row.append(&next_episode_button);

    // Speed, details and sleep timer, below the transport row.
    let speed_button = gtk4::MenuButton::builder().label("1.0×").css_classes(["flat"]).valign(gtk4::Align::Center).build();
    set_accessible_label(&speed_button, &gettext_f("Playback speed {rate}×", &[("rate", "1.00")]));
    let speed_popover = gtk4::Popover::new();
    let speed_list = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(2)
        .margin_start(6)
        .margin_end(6)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    for rate in [0.5_f64, 0.75, 1.0, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 3.5, 4.0] {
        let rate_button = gtk4::Button::builder().label(format!("{rate:.2}×")).css_classes(["flat"]).build();
        {
            let command_tx = command_tx.clone();
            let speed_popover = speed_popover.clone();
            rate_button.connect_clicked(move |_| {
                if let Err(e) = command_tx.send(UiCommand::SetPlaybackRate(rate)) {
                    tracing::error!("failed to send SetPlaybackRate command: {e}");
                }
                speed_popover.popdown();
            });
        }
        speed_list.append(&rate_button);
    }
    speed_popover.set_child(Some(&speed_list));
    speed_button.set_popover(Some(&speed_popover));

    // The countdown beside the icon shows the timer is running.
    let sleep_timer_button =
        gtk4::MenuButton::builder().icon_name("sleep-timer-symbolic").css_classes(["flat"]).valign(gtk4::Align::Center).build();
    sleep_timer_button.set_tooltip_text(Some(&gettext("Sleep Timer")));
    // Set explicitly: while the popover is open, the name would otherwise
    // come from its contents.
    set_accessible_label(&sleep_timer_button, &gettext("Sleep timer: off"));
    let sleep_timer_popover = gtk4::Popover::new();
    let sleep_timer_list = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(2)
        .margin_start(6)
        .margin_end(6)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    {
        let end_button = gtk4::Button::builder().label(gettext("End of episode")).css_classes(["flat"]).build();
        let command_tx = command_tx.clone();
        let sleep_timer_popover = sleep_timer_popover.clone();
        end_button.connect_clicked(move |_| {
            if let Err(e) = command_tx.send(UiCommand::SleepAtEpisodeEnd) {
                tracing::error!("failed to send SleepAtEpisodeEnd command: {e}");
            }
            sleep_timer_popover.popdown();
        });
        sleep_timer_list.append(&end_button);
    }
    for minutes in [5u64, 10, 15, 30, 45, 60] {
        let label = ngettext_f("{n} minute", "{n} minutes", minutes as u32, &[("n", &minutes.to_string())]);
        let option_button = gtk4::Button::builder().label(label).css_classes(["flat"]).build();
        {
            let command_tx = command_tx.clone();
            let sleep_timer_popover = sleep_timer_popover.clone();
            option_button.connect_clicked(move |_| {
                let duration = std::time::Duration::from_secs(minutes * 60);
                if let Err(e) = command_tx.send(UiCommand::SetSleepTimer(Some(duration))) {
                    tracing::error!("failed to send SetSleepTimer command: {e}");
                }
                sleep_timer_popover.popdown();
            });
        }
        sleep_timer_list.append(&option_button);
    }
    sleep_timer_list.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
    let off_button = gtk4::Button::builder().label(gettext("Off")).css_classes(["flat"]).build();
    {
        let command_tx = command_tx.clone();
        let sleep_timer_popover = sleep_timer_popover.clone();
        off_button.connect_clicked(move |_| {
            if let Err(e) = command_tx.send(UiCommand::SetSleepTimer(None)) {
                tracing::error!("failed to send SetSleepTimer command: {e}");
            }
            sleep_timer_popover.popdown();
        });
    }
    sleep_timer_list.append(&off_button);
    sleep_timer_popover.set_child(Some(&sleep_timer_list));
    sleep_timer_button.set_popover(Some(&sleep_timer_popover));

    let sleep_timer_countdown_label = gtk4::Label::builder().css_classes(["dim-label", "numeric"]).valign(gtk4::Align::Center).visible(false).build();

    let episode_details_button = gtk4::Button::builder()
        .icon_name("help-about-symbolic")
        .tooltip_text(gettext("Episode Details"))
        .css_classes(["flat"])
        .valign(gtk4::Align::Center)
        .build();
    let details_and_sleep = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(6).build();
    details_and_sleep.append(&episode_details_button);
    details_and_sleep.append(&sleep_timer_button);
    details_and_sleep.append(&sleep_timer_countdown_label);
    let speed_and_sleep_row = gtk4::CenterBox::new();
    speed_and_sleep_row.set_start_widget(Some(&details_and_sleep));
    speed_and_sleep_row.set_end_widget(Some(&speed_button));

    for button in [&play_pause_button, &sheet_play_pause_button] {
        let command_tx = command_tx.clone();
        button.connect_clicked(move |_| {
            // A plain synchronous send; only receiving needs GLib
            // integration.
            if let Err(e) = command_tx.send(UiCommand::PlayPause) {
                tracing::error!("failed to send PlayPause command to backend: {e}");
            }
        });
    }

    let text_box = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .valign(gtk4::Align::Center)
        .hexpand(true)
        .build();
    text_box.append(&title_label);
    text_box.append(&subtitle_label);

    let expand_icon = gtk4::Image::from_icon_name("go-up-symbolic");
    expand_icon.add_css_class("dim-label");
    let bottom_bar_row = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Horizontal)
        .spacing(10)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(14)
        .margin_end(10)
        .build();
    bottom_bar_row.append(&expand_icon);
    bottom_bar_row.append(&mini_thumbnail);
    bottom_bar_row.append(&text_box);
    bottom_bar_row.append(&play_pause_button);
    let bottom_bar_content = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).build();
    bottom_bar_content.append(&bottom_bar_row);
    bottom_bar_content.append(&mini_progress_bar);
    {
        // The bottom bar is announced as one button; its name would
        // otherwise run every label and the progress bar together.
        let update_name = {
            let bottom_bar_content = bottom_bar_content.clone();
            let title_label = title_label.clone();
            let subtitle_label = subtitle_label.clone();
            move || {
                let subtitle = subtitle_label.label();
                let name = if subtitle.is_empty() {
                    gettext_f("Player: {title}", &[("title", &title_label.label())])
                } else {
                    gettext_f("Player: {title}, {subtitle}", &[("title", &title_label.label()), ("subtitle", &subtitle)])
                };
                // AdwBottomSheet wraps the bar in its own button, which
                // isn't exposed; it's the nearest button ancestor.
                let mut ancestor = bottom_bar_content.parent();
                while let Some(widget) = ancestor {
                    if widget.accessible_role() == gtk4::AccessibleRole::Button {
                        set_accessible_label(&widget, &name);
                        break;
                    }
                    ancestor = widget.parent();
                }
            }
        };
        let update_name = std::rc::Rc::new(update_name);
        {
            // The ancestor exists once the sheet has been built around it.
            let update_name = update_name.clone();
            bottom_bar_content.connect_realize(move |_| update_name());
        }
        for label in [&title_label, &subtitle_label] {
            let update_name = update_name.clone();
            label.connect_label_notify(move |_| update_name());
        }
    }

    let caption = TranscriptCaption::new();
    // The expanded sheet. The top margin clears the drag handle.
    let sheet_content = gtk4::Box::builder()
        .orientation(gtk4::Orientation::Vertical)
        .spacing(16)
        .margin_start(16)
        .margin_end(16)
        .margin_top(32)
        .margin_bottom(24)
        .valign(gtk4::Align::Center)
        .vexpand(true)
        .build();
    sheet_content.append(&sheet_artwork);
    sheet_content.append(&sheet_title_label);
    sheet_content.append(&sheet_subtitle_label);
    sheet_content.append(&sheet_chapter_label);
    sheet_content.append(&caption.button);
    sheet_content.append(&scrub_labels_row);
    sheet_content.append(&transport_row);
    sheet_content.append(&speed_and_sleep_row);

    // The details button slides to the episode's details; Back returns.
    let sheet_stack = gtk4::Stack::builder()
        .vexpand(true)
        .transition_type(gtk4::StackTransitionType::SlideLeftRight)
        .build();
    sheet_stack.add_named(&sheet_content, Some("player"));
    // Details scroll themselves (see episode_detail::render).
    let sheet_details_box = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).vexpand(true).build();
    let details_back_button = gtk4::Button::builder()
        .icon_name("go-previous-symbolic")
        .tooltip_text(gettext("Back to Player"))
        .css_classes(["flat"])
        .build();
    let details_header = gtk4::CenterBox::builder().margin_start(12).margin_end(12).margin_top(28).build();
    details_header.set_start_widget(Some(&details_back_button));
    details_header.set_center_widget(Some(&gtk4::Label::builder().label(gettext("Episode Details")).css_classes(["heading"]).build()));
    let sheet_details_page = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).build();
    sheet_details_page.append(&details_header);
    sheet_details_page.append(&sheet_details_box);
    sheet_stack.add_named(&sheet_details_page, Some("details"));
    let sheet_root = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).build();
    sheet_root.append(&sheet_stack);
    {
        let sheet_stack = sheet_stack.clone();
        episode_details_button.connect_clicked(move |_| sheet_stack.set_visible_child_name("details"));
    }
    {
        let sheet_stack = sheet_stack.clone();
        details_back_button.connect_clicked(move |_| sheet_stack.set_visible_child_name("player"));
    }
    {
        let sheet_stack = sheet_stack.clone();
        caption.button.connect_clicked(move |_| {
            prefer_transcript_tab();
            sheet_stack.set_visible_child_name("details");
        });
    }

    // Each tab lives in a slot Box so DataChanged can swap its content
    // without touching the ViewStack. vexpand, or short content such as a
    // placeholder gets only its natural height.
    let queue_slot = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).vexpand(true).build();
    let library_slot = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).vexpand(true).build();
    let subscriptions_slot = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).vexpand(true).build();
    let history_slot = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).vexpand(true).build();
    // Created early for the tabs' toasts; its child is set once bottom_sheet
    // exists.
    let main_toast_overlay = libadwaita::ToastOverlay::new();
    // Created early for the same reason.
    let dialog_sheet = libadwaita::BottomSheet::builder().content(&main_toast_overlay).modal(true).build();
    let progress_registry: DownloadProgressRegistry =
        std::rc::Rc::new(std::cell::RefCell::new(std::collections::HashMap::new()));
    // Narrow/wide state, written by the width tracking below and read by
    // Subscriptions for thumbnail size. Starts narrow.
    let is_narrow: std::rc::Rc<std::cell::Cell<bool>> = std::rc::Rc::new(std::cell::Cell::new(true));
    build_queue_page(&tokio_handle, &queue_slot, db_path, &command_tx);
    build_library_page(&tokio_handle, &library_slot, db_path, &progress_registry, &command_tx, &main_toast_overlay);
    build_subscriptions_page(&tokio_handle, db_path, &subscriptions_slot, &is_narrow, &command_tx, &dialog_sheet);
    build_history_page(&tokio_handle, &history_slot, db_path, &command_tx, &main_toast_overlay);

    // This ViewStack has no `add_titled_with_icon`; the icon is set on the
    // returned page.
    let view_stack = libadwaita::ViewStack::new();
    // vexpand so the tabs, not just content_area, take the extra height.
    view_stack.set_vexpand(true);
    let queue_stack_page = view_stack.add_titled(&queue_slot, Some("queue"), &gettext("Queue"));
    queue_stack_page.set_icon_name(Some("view-list-symbolic"));
    let library_stack_page = view_stack.add_titled(&library_slot, Some("library"), &gettext("Library"));
    library_stack_page.set_icon_name(Some("library-symbolic"));
    let subscriptions_stack_page =
        view_stack.add_titled(&subscriptions_slot, Some("subscriptions"), &gettext("Subscriptions"));
    subscriptions_stack_page.set_icon_name(Some("starred-symbolic"));
    let history_stack_page = view_stack.add_titled(&history_slot, Some("history"), &gettext("History"));
    history_stack_page.set_icon_name(Some("media-playlist-consecutive-symbolic"));

    let view_switcher = libadwaita::ViewSwitcher::builder()
        .stack(&view_stack)
        .build();
    // Policy is set by apply_width_layout.

    let header_bar = libadwaita::HeaderBar::builder().title_widget(&view_switcher).build();

    // Refresh is app-wide, so it lives in the header bar; Add Feed is on the
    // Subscriptions tab.
    let refresh_button = gtk4::Button::from_icon_name("view-refresh-symbolic");
    refresh_button.set_tooltip_text(Some(&gettext("Refresh All")));
    {
        // The icon turns while a refresh runs, as in Owlet.
        let css = gtk4::CssProvider::new();
        css.load_from_string(
            "@keyframes phodder-spin { from { -gtk-icon-transform: rotate(0deg); } to { -gtk-icon-transform: rotate(360deg); } }
             .phodder-spinning image { animation: phodder-spin 1s linear infinite; }",
        );
        if let Some(display) = gtk4::gdk::Display::default() {
            gtk4::style_context_add_provider_for_display(&display, &css, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);
        }
    }
    header_bar.pack_start(&refresh_button);
    {
        let command_tx = command_tx.clone();
        refresh_button.connect_clicked(move |_| {
            if let Err(e) = command_tx.send(UiCommand::RefreshAll) {
                tracing::error!("failed to send RefreshAll command: {e}");
            }
        });
    }

    // The bottom tab bar. Swapped by manual width tracking, not
    // AdwBreakpoint, which hasn't been tried with the current feature flags.
    let view_switcher_bar = libadwaita::ViewSwitcherBar::builder().stack(&view_stack).build();

    // One banner for offline state, failing feeds and background errors; its
    // Details button lists them (see problems.rs).
    let banner = libadwaita::Banner::builder().revealed(false).build();

    // Everything except the player. vexpand fills the window; without it a
    // large gap appeared below the tabs.
    let content_area = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).vexpand(true).build();
    content_area.append(&header_bar);
    content_area.append(&banner);
    content_area.append(&view_stack);

    // The mini player is the bottom bar; the full player is the sheet.
    let bottom_sheet = libadwaita::BottomSheet::builder()
        .content(&content_area)
        .bottom_bar(&bottom_bar_content)
        .sheet(&sheet_root)
        .build();
    {
        // A tab switch shows that tab, so an expanded player gives way.
        let bottom_sheet = bottom_sheet.clone();
        view_stack.connect_visible_child_name_notify(move |_| bottom_sheet.set_open(false));
    }
    {
        // Reopening the player starts on the player page. Artwork fills what
        // the controls and the transcript caption leave free, sized when the
        // sheet opens and when the caption appears or goes.
        let sheet_stack = sheet_stack.clone();
        let size_artwork = {
            let sheet_artwork = sheet_artwork.clone();
            let caption_button = caption.button.clone();
            let bottom_sheet = bottom_sheet.clone();
            move || {
                let Some(window) = bottom_sheet.root().and_downcast::<gtk4::Window>() else { return };
                let caption_height = if caption_button.is_visible() { 130 } else { 0 };
                let size = (window.width() - 64).min(window.height() - 400 - caption_height).clamp(120, 400);
                sheet_artwork.set_pixel_size(size);
            }
        };
        let size_artwork = std::rc::Rc::new(size_artwork);
        {
            let size_artwork = size_artwork.clone();
            caption.button.connect_visible_notify(move |_| size_artwork());
        }
        bottom_sheet.connect_open_notify(move |sheet| {
            if sheet.is_open() {
                size_artwork();
            } else {
                sheet_stack.set_transition_type(gtk4::StackTransitionType::None);
                sheet_stack.set_visible_child_name("player");
                sheet_stack.set_transition_type(gtk4::StackTransitionType::SlideLeftRight);
            }
        });
    }

    // `bottom-bar-height` was wrong while the bar was hidden, which left a
    // large gap below the tabs at startup, so the margin follows it only
    // while revealed. The mini player is never hidden; the cell stays for the
    // margin sync.
    let bottom_bar_revealed: std::rc::Rc<std::cell::Cell<bool>> = std::rc::Rc::new(std::cell::Cell::new(true));
    fn sync_content_area_bottom_margin(
        bottom_sheet: &libadwaita::BottomSheet,
        content_area: &gtk4::Box,
        revealed: bool,
    ) {
        content_area.set_margin_bottom(if revealed { bottom_sheet.bottom_bar_height() } else { 0 });
    }
    {
        let content_area_for_notify = content_area.clone();
        let bottom_bar_revealed_for_notify = bottom_bar_revealed.clone();
        bottom_sheet.connect_bottom_bar_height_notify(move |bs| {
            sync_content_area_bottom_margin(bs, &content_area_for_notify, bottom_bar_revealed_for_notify.get());
        });
    }

    // The main window's toast overlay. The tab bar sits outside the bottom
    // sheet so the mini player stays above it and the tabs keep a fixed
    // position at the bottom edge.
    let sheet_and_tabs = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).build();
    sheet_and_tabs.append(&bottom_sheet);
    sheet_and_tabs.append(&view_switcher_bar);
    main_toast_overlay.set_child(Some(&sheet_and_tabs));

    // A gio::Menu model with `win.*` actions, which resolve at activation, so
    // the model can be built before the window exists. Buttons in a Popover
    // rendered bold and weren't a real menu.
    let main_menu_model = gtk4::gio::Menu::new();
    main_menu_model.append(Some(&gettext("Manage Groups")), Some("win.manage-groups"));
    main_menu_model.append(Some(&gettext("Settings")), Some("win.show-settings"));
    let opml_section = gtk4::gio::Menu::new();
    opml_section.append(Some(&gettext("Import OPML…")), Some("win.import-opml"));
    opml_section.append(Some(&gettext("Export OPML…")), Some("win.export-opml"));
    main_menu_model.append_section(None, &opml_section);
    let quit_section = gtk4::gio::Menu::new();
    quit_section.append(Some(&gettext("Keyboard Shortcuts")), Some("win.show-shortcuts"));
    quit_section.append(Some(&gettext("About Phodder")), Some("win.about"));
    quit_section.append(Some(&gettext("Quit")), Some("win.quit"));
    main_menu_model.append_section(None, &quit_section);

    let menu_button = gtk4::MenuButton::builder().icon_name("open-menu-symbolic").tooltip_text(gettext("Menu")).menu_model(&main_menu_model).build();
    if let Some(popover) = menu_button.popover() {
        popover.connect_map(|popover| name_menu_items(popover.upcast_ref()));
    }
    header_bar.pack_end(&menu_button);

    let window = libadwaita::ApplicationWindow::builder()
        .application(app)
        .title("Phodder")
        .default_width(400)
        .default_height(700)
        .content(&dialog_sheet)
        .build();

    {
        let db_path = db_path.to_path_buf();
        let window = window.clone();
        let dialog_sheet = dialog_sheet.clone();
        let command_tx = command_tx.clone();
        let manage_groups_action = gtk4::gio::SimpleAction::new("manage-groups", None);
        window.add_action(&manage_groups_action);
        manage_groups_action.connect_activate(move |_, _| {
            show_manage_groups_dialog(&window, db_path.clone(), &dialog_sheet, None, command_tx.clone());
        });
    }
    {
        let window = window.clone();
        let command_tx = command_tx.clone();
        let main_toast_overlay = main_toast_overlay.clone();
        let import_opml_action = gtk4::gio::SimpleAction::new("import-opml", None);
        window.add_action(&import_opml_action);
        import_opml_action.connect_activate(move |_, _| {
            let window = window.clone();
            let command_tx = command_tx.clone();
            let main_toast_overlay = main_toast_overlay.clone();
            gtk4::glib::spawn_future_local(async move {
                let file_dialog = gtk4::FileDialog::builder().title(gettext("Import OPML")).build();
                let file = match file_dialog.open_future(Some(&window)).await {
                    Ok(file) => file,
                    // Errors include the user cancelling; no toast either
                    // way.
                    Err(_) => return,
                };
                let Some(path) = file.path() else {
                    main_toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't determine the selected file's path")));
                    return;
                };
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if let Err(e) = command_tx.send(UiCommand::ImportOpml { path, reply_tx }) {
                    tracing::error!("failed to send ImportOpml command: {e}");
                    main_toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't reach the backend to import")));
                    return;
                }
                match reply_rx.await {
                    Ok(Ok(summary)) => {
                        let imported = ngettext_f(
                            "Imported {n} feed",
                            "Imported {n} feeds",
                            summary.imported as u32,
                            &[("n", &summary.imported.to_string())],
                        );
                        let message = gettext_f(
                            "{imported} · {already} already subscribed · {failed} failed",
                            &[
                                ("imported", &imported),
                                ("already", &summary.already_subscribed.to_string()),
                                ("failed", &summary.failed.to_string()),
                            ],
                        );
                        main_toast_overlay.add_toast(libadwaita::Toast::new(&message));
                    }
                    Ok(Err(e)) => {
                        tracing::error!("OPML import failed: {e}");
                        main_toast_overlay.add_toast(libadwaita::Toast::new(&gettext_f("Import failed: {error}", &[("error", &e.to_string())])));
                    }
                    Err(_) => {
                        main_toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Import failed: the backend didn't respond")));
                    }
                }
            });
        });
    }
    {
        let window = window.clone();
        let command_tx = command_tx.clone();
        let main_toast_overlay = main_toast_overlay.clone();
        let export_opml_action = gtk4::gio::SimpleAction::new("export-opml", None);
        window.add_action(&export_opml_action);
        export_opml_action.connect_activate(move |_, _| {
            let window = window.clone();
            let command_tx = command_tx.clone();
            let main_toast_overlay = main_toast_overlay.clone();
            gtk4::glib::spawn_future_local(async move {
                let file_dialog =
                    gtk4::FileDialog::builder().title(gettext("Export OPML")).initial_name("phodder-subscriptions.opml").build();
                let file = match file_dialog.save_future(Some(&window)).await {
                    Ok(file) => file,
                    Err(_) => return,
                };
                let Some(path) = file.path() else {
                    main_toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't determine the selected file's path")));
                    return;
                };
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if let Err(e) = command_tx.send(UiCommand::ExportOpml { path, reply_tx }) {
                    tracing::error!("failed to send ExportOpml command: {e}");
                    main_toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't reach the backend to export")));
                    return;
                }
                match reply_rx.await {
                    Ok(Ok(())) => {
                        main_toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Subscriptions exported")));
                    }
                    Ok(Err(e)) => {
                        tracing::error!("OPML export failed: {e}");
                        main_toast_overlay.add_toast(libadwaita::Toast::new(&gettext_f("Export failed: {error}", &[("error", &e.to_string())])));
                    }
                    Err(_) => {
                        main_toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Export failed: the backend didn't respond")));
                    }
                }
            });
        });
    }
    {
        let db_path = db_path.to_path_buf();
        let window = window.clone();
        let dialog_sheet = dialog_sheet.clone();
        let skip_backward_button = skip_backward_button.clone();
        let skip_forward_button = skip_forward_button.clone();
        let command_tx = command_tx.clone();
        let settings_action = gtk4::gio::SimpleAction::new("show-settings", None);
        window.add_action(&settings_action);
        settings_action.connect_activate(move |_, _| {
            show_settings_dialog(&window, db_path.clone(), &dialog_sheet, &skip_backward_button, &skip_forward_button, &command_tx);
        });
    }
    {
        let about_action = gtk4::gio::SimpleAction::new("about", None);
        let window_for_about = window.clone();
        about_action.connect_activate(move |_, _| show_about_dialog(&window_for_about));
        window.add_action(&about_action);
    }
    {
        let app = app.clone();
        let tokio_handle = tokio_handle.clone();
        let quit_action = gtk4::gio::SimpleAction::new("quit", None);
        // Closing the window leaves the service to Run in Background; Quit
        // stops both.
        quit_action.connect_activate(move |_, _| {
            super::client::quit_service(&tokio_handle);
            app.quit();
        });
        window.add_action(&quit_action);
    }
    install_keyboard_shortcuts(app, &window, &view_stack, &refresh_button, &command_tx);
    {
        // An app action, so notifications can open a podcast's Episodes tab.
        let show_podcast_action = gtk4::gio::SimpleAction::new("show-podcast", Some(gtk4::glib::VariantTy::INT64));
        let window_for_action = window.clone();
        let view_stack = view_stack.clone();
        let db_path = db_path.to_path_buf();
        let tokio_handle = tokio_handle.clone();
        let command_tx = command_tx.clone();
        let subscriptions_slot = subscriptions_slot.clone();
        let is_narrow = is_narrow.clone();
        let dialog_sheet = dialog_sheet.clone();
        show_podcast_action.connect_activate(move |_, parameter| {
            let Some(subscription_id) = parameter.and_then(|p| p.get::<i64>()) else { return };
            window_for_action.present();
            view_stack.set_visible_child_name("subscriptions");
            show_subscription_detail(
                &window_for_action,
                db_path.clone(),
                subscription_id,
                subscriptions_slot.clone(),
                tokio_handle.clone(),
                is_narrow.clone(),
                command_tx.clone(),
                false,
                &dialog_sheet,
                "episodes",
            );
        });
        app.add_action(&show_podcast_action);
    }
    {
        let add_podcast_action = gtk4::gio::SimpleAction::new("add-podcast", None);
        let window_for_action = window.clone();
        let db_path = db_path.to_path_buf();
        let tokio_handle = tokio_handle.clone();
        let command_tx = command_tx.clone();
        let subscriptions_slot = subscriptions_slot.clone();
        let is_narrow = is_narrow.clone();
        let dialog_sheet = dialog_sheet.clone();
        add_podcast_action.connect_activate(move |_, _| {
            show_discover_page(
                &window_for_action,
                db_path.clone(),
                tokio_handle.clone(),
                command_tx.clone(),
                subscriptions_slot.clone(),
                is_narrow.clone(),
                &dialog_sheet,
            );
        });
        window.add_action(&add_podcast_action);
    }

    // Narrow/wide swap between the header ViewSwitcher and the bottom
    // ViewSwitcherBar, setting the switcher's policy explicitly since leaving
    // it unset rendered Narrow at every width. The live width is polled:
    // `default-width` notify fired unreliably during interactive resizes. The
    // notify handler stays to cut latency.
    fn apply_width_layout(
        width: i32,
        view_switcher: &libadwaita::ViewSwitcher,
        view_switcher_bar: &libadwaita::ViewSwitcherBar,
        subscriptions_slot: &gtk4::Box,
        tokio_handle: &tokio::runtime::Handle,
        db_path: &std::path::Path,
        is_narrow: &std::rc::Rc<std::cell::Cell<bool>>,
        command_tx: &UnboundedSender<UiCommand>,
        dialog_sheet: &libadwaita::BottomSheet,
    ) {
        let narrow = width < NARROW_WIDTH_THRESHOLD;
        view_switcher.set_visible(!narrow);
        view_switcher.set_policy(if narrow {
            libadwaita::ViewSwitcherPolicy::Narrow
        } else {
            libadwaita::ViewSwitcherPolicy::Wide
        });
        view_switcher_bar.set_reveal(narrow);

        // Subscriptions is rebuilt only when the state flips; this is called
        // on every tick.
        if narrow != is_narrow.get() {
            is_narrow.set(narrow);
            build_subscriptions_page(tokio_handle, db_path, subscriptions_slot, is_narrow, command_tx, dialog_sheet);
        }
    }

    // Deferred with idle_add_local: changing layout from inside map or notify
    // logged GTK allocation warnings. Moving the call to map alone didn't
    // stop them.
    {
        let view_switcher = view_switcher.clone();
        let view_switcher_bar = view_switcher_bar.clone();
        let bottom_sheet_for_map = bottom_sheet.clone();
        let content_area_for_map = content_area.clone();
        let subscriptions_slot_for_map = subscriptions_slot.clone();
        let tokio_handle_for_map = tokio_handle.clone();
        let db_path_for_map = db_path.to_path_buf();
        let is_narrow_for_map = is_narrow.clone();
        let command_tx_for_map = command_tx.clone();
        let dialog_sheet_for_map = dialog_sheet.clone();
        let bottom_bar_revealed_for_map = bottom_bar_revealed.clone();
        window.connect_map(move |win| {
            let width = win.width();
            let view_switcher = view_switcher.clone();
            let view_switcher_bar = view_switcher_bar.clone();
            let bottom_sheet = bottom_sheet_for_map.clone();
            let content_area = content_area_for_map.clone();
            let bottom_bar_revealed_for_map = bottom_bar_revealed_for_map.clone();
            let subscriptions_slot = subscriptions_slot_for_map.clone();
            let tokio_handle = tokio_handle_for_map.clone();
            let db_path = db_path_for_map.clone();
            let is_narrow = is_narrow_for_map.clone();
            let command_tx = command_tx_for_map.clone();
            let dialog_sheet = dialog_sheet_for_map.clone();
            gtk4::glib::source::idle_add_local(move || {
                apply_width_layout(width, &view_switcher, &view_switcher_bar, &subscriptions_slot, &tokio_handle, &db_path, &is_narrow, &command_tx, &dialog_sheet);

                // Applied here, after the first measure/allocate cycle:
                // mutating the sheet's layout state at construction caused
                // GTK allocation warnings. Uses the last known state because
                // a PlaybackState update (restored episode) can arrive before
                // the window maps, and map runs again whenever the window is
                // re-shown.
                let revealed = bottom_bar_revealed_for_map.get();
                bottom_sheet.set_reveal_bottom_bar(revealed);
                sync_content_area_bottom_margin(&bottom_sheet, &content_area, revealed);

                gtk4::glib::ControlFlow::Break
            });
        });
    }

    {
        let view_switcher = view_switcher.clone();
        let view_switcher_bar = view_switcher_bar.clone();
        let subscriptions_slot_for_notify = subscriptions_slot.clone();
        let tokio_handle_for_notify = tokio_handle.clone();
        let db_path_for_notify = db_path.to_path_buf();
        let is_narrow_for_notify = is_narrow.clone();
        let dialog_sheet_for_notify = dialog_sheet.clone();
        let command_tx_for_notify = command_tx.clone();
        window.connect_default_width_notify(move |win| {
            let width = win.width();
            let view_switcher = view_switcher.clone();
            let view_switcher_bar = view_switcher_bar.clone();
            let subscriptions_slot = subscriptions_slot_for_notify.clone();
            let tokio_handle = tokio_handle_for_notify.clone();
            let db_path = db_path_for_notify.clone();
            let is_narrow = is_narrow_for_notify.clone();
            let command_tx = command_tx_for_notify.clone();
            let dialog_sheet = dialog_sheet_for_notify.clone();
            gtk4::glib::source::idle_add_local(move || {
                apply_width_layout(width, &view_switcher, &view_switcher_bar, &subscriptions_slot, &tokio_handle, &db_path, &is_narrow, &command_tx, &dialog_sheet);
                gtk4::glib::ControlFlow::Break
            });
        });
    }
    {
        let window = window.clone();
        let view_switcher = view_switcher.clone();
        let view_switcher_bar = view_switcher_bar.clone();
        let subscriptions_slot = subscriptions_slot.clone();
        let tokio_handle = tokio_handle.clone();
        let db_path = db_path.to_path_buf();
        let is_narrow = is_narrow.clone();
        let command_tx = command_tx.clone();
        let dialog_sheet = dialog_sheet.clone();
        let last_narrow = std::rc::Rc::new(std::cell::Cell::new(true));
        gtk4::glib::source::timeout_add_local(std::time::Duration::from_millis(300), move || {
            let width = window.width();
            let narrow = width < NARROW_WIDTH_THRESHOLD;
            if narrow != last_narrow.get() {
                last_narrow.set(narrow);
                apply_width_layout(width, &view_switcher, &view_switcher_bar, &subscriptions_slot, &tokio_handle, &db_path, &is_narrow, &command_tx, &dialog_sheet);
            }
            gtk4::glib::ControlFlow::Continue
        });
    }

    // Receives UiUpdates on the GTK main context, so widgets can be touched
    // directly.
    let tokio_handle = tokio_handle.clone();
    let data_changed_rebuild_pending = std::rc::Rc::new(std::cell::Cell::new(false));
    let data_changed_rebuild_dirty = std::rc::Rc::new(std::cell::Cell::new(false));
    let rebuild_all_tabs: std::rc::Rc<dyn Fn()> = {
        let tokio_handle = tokio_handle.clone();
        let queue_slot = queue_slot.clone();
        let library_slot = library_slot.clone();
        let subscriptions_slot = subscriptions_slot.clone();
        let history_slot = history_slot.clone();
        let db_path = db_path.to_path_buf();
        let progress_registry = progress_registry.clone();
        let command_tx = command_tx.clone();
        let main_toast_overlay = main_toast_overlay.clone();
        let is_narrow = is_narrow.clone();
        let dialog_sheet = dialog_sheet.clone();
        std::rc::Rc::new(move || {
            build_queue_page(&tokio_handle, &queue_slot, &db_path, &command_tx);
            build_library_page(&tokio_handle, &library_slot, &db_path, &progress_registry, &command_tx, &main_toast_overlay);
            build_subscriptions_page(&tokio_handle, &db_path, &subscriptions_slot, &is_narrow, &command_tx, &dialog_sheet);
            build_history_page(&tokio_handle, &history_slot, &db_path, &command_tx, &main_toast_overlay);
            refresh_back_catalog_pages();
            refresh_open_details();
        })
    };
    let speed_button = speed_button.clone();
    let caption = caption.clone();
    let command_tx_for_caption = command_tx.clone();
    let sleep_timer_countdown_label = sleep_timer_countdown_label.clone();
    let current_episode_id: std::rc::Rc<std::cell::Cell<Option<i64>>> = Default::default();
    {
        let window = window.clone();
        let dialog_sheet = dialog_sheet.clone();
        let command_tx = command_tx.clone();
        banner.connect_button_clicked(move |banner| problems::show_details(banner, &window, &dialog_sheet, &command_tx));
    }
    set_details_context(DetailsContext {
        tokio_handle: tokio_handle.clone(),
        db_path: db_path.to_path_buf(),
        command_tx: command_tx.clone(),
        window: window.clone(),
        dialog_sheet: dialog_sheet.clone(),
        toast_overlay: main_toast_overlay.clone(),
        current_episode: current_episode_id.clone(),
    });
    {
        let current_episode_id = current_episode_id.clone();
        let sheet_details_box = sheet_details_box.clone();
        sheet_stack.connect_visible_child_name_notify(move |stack| {
            if let (Some("details"), Some(id)) = (stack.visible_child_name().as_deref(), current_episode_id.get()) {
                fill_episode_details(&sheet_details_box, id, DetailsMode::Player);
            }
        });
    }
    // Shown while nothing is loaded. Play starts the queue, so it's only
    // offered when something is queued.
    let refresh_idle_mini_player = {
        let tokio_handle = tokio_handle.clone();
        let db_path = db_path.to_path_buf();
        let title_label = title_label.clone();
        let subtitle_label = subtitle_label.clone();
        let play_pause_button = play_pause_button.clone();
        let current_episode_id = current_episode_id.clone();
        move || {
            let title_label = title_label.clone();
            let subtitle_label = subtitle_label.clone();
            let play_pause_button = play_pause_button.clone();
            let current_episode_id = current_episode_id.clone();
            run_db_op(
                &tokio_handle,
                db_path.clone(),
                |conn| {
                    Ok(conn.query_row(
                        "SELECT COUNT(*) FROM episodes WHERE group_queue_position IS NOT NULL AND local_file_path IS NOT NULL",
                        [],
                        |row| row.get::<_, i64>(0),
                    )?)
                },
                move |result| {
                    if current_episode_id.get().is_some() {
                        return;
                    }
                    let queued = result.unwrap_or(0);
                    if queued == 0 {
                        title_label.set_label(&gettext("No episodes queued"));
                        subtitle_label.set_label(&gettext("Queue episodes from Library or History"));
                    } else {
                        title_label.set_label(&gettext("Nothing playing"));
                        subtitle_label.set_label(&ngettext_f("{n} episode queued", "{n} episodes queued", queued as u32, &[("n", &queued.to_string())]));
                    }
                    play_pause_button.set_sensitive(queued > 0);
                },
            );
        }
    };
    refresh_idle_mini_player();
    let scrub_marks_key: std::rc::Rc<std::cell::RefCell<Option<(Option<i64>, Option<i64>, std::sync::Arc<Vec<u64>>)>>> = Default::default();
    let last_artwork_path: std::rc::Rc<std::cell::RefCell<Option<Option<String>>>> = Default::default();
    let sleep_deadline: std::rc::Rc<std::cell::Cell<Option<std::time::Instant>>> = Default::default();
    let sleep_at_end: std::rc::Rc<std::cell::Cell<bool>> = Default::default();
    let refresh_sleep_label = {
        let sleep_deadline = sleep_deadline.clone();
        let sleep_at_end = sleep_at_end.clone();
        let label = sleep_timer_countdown_label.clone();
        let sleep_timer_button = sleep_timer_button.clone();
        move || {
            let name = match (sleep_at_end.get(), sleep_deadline.get()) {
                (true, _) => {
                    label.set_label(&gettext("Episode end"));
                    label.set_visible(true);
                    gettext("Sleep timer: end of episode")
                }
                (false, Some(deadline)) => {
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    label.set_label(&format_duration_ms(remaining.as_millis() as i64));
                    label.set_visible(true);
                    let minutes = remaining.as_secs().div_ceil(60);
                    ngettext_f("Sleep timer: {n} minute left", "Sleep timer: {n} minutes left", minutes as u32, &[("n", &minutes.to_string())])
                }
                (false, None) => {
                    label.set_visible(false);
                    gettext("Sleep timer: off")
                }
            };
            set_accessible_label(&sleep_timer_button, &name);
        }
    };
    {
        let refresh_sleep_label = refresh_sleep_label.clone();
        gtk4::glib::timeout_add_seconds_local(1, move || {
            refresh_sleep_label();
            gtk4::glib::ControlFlow::Continue
        });
    }
    let notice_overlay = main_toast_overlay.clone();
    let refresh_button_for_updates = refresh_button.clone();
    gtk4::glib::spawn_future_local(async move {
        while let Ok(update) = update_rx.recv().await {
            match update {
                UiUpdate::PlaybackState {
                    status,
                    title,
                    podcast_title,
                    artwork_local_path,
                    position_us,
                    duration_us,
                    playback_rate,
                    sleep_timer_remaining_secs,
                    sleep_at_episode_end,
                    episode_id,
                    has_chapters,
                    chapter_starts_ms,
                    chapter_title,
                } => {
                    let marks_key = (episode_id, duration_us, chapter_starts_ms.clone());
                    if scrub_marks_key.borrow().as_ref() != Some(&marks_key) {
                        scrub_bar.clear_marks();
                        if let Some(duration_us) = duration_us.filter(|d| *d > 0) {
                            for start_ms in chapter_starts_ms.iter().filter(|s| **s > 0) {
                                let fraction = (*start_ms as f64 * 1000.0 / duration_us as f64).min(1.0);
                                scrub_bar.add_mark(fraction, gtk4::PositionType::Bottom, None);
                            }
                        }
                        scrub_marks_key.replace(Some(marks_key));
                    }
                    speed_button.set_label(&format!("{playback_rate:.2}×"));
                    set_accessible_label(&speed_button, &gettext_f("Playback speed {rate}×", &[("rate", &format!("{playback_rate:.2}"))]));
                    if let Some(d) = duration_us.filter(|d| *d > 0) {
                        // Arrow keys move 10 seconds, Page Up and Down a minute.
                        let d = d as f64;
                        scrub_bar.set_increments((10_000_000.0 / d).min(1.0), (60_000_000.0 / d).min(1.0));
                    }
                    // Updates arrive only while playing, so the label counts
                    // down locally from the deadline.
                    sleep_deadline.set(sleep_timer_remaining_secs.map(|secs| std::time::Instant::now() + std::time::Duration::from_secs(secs)));
                    sleep_at_end.set(sleep_at_episode_end);
                    refresh_sleep_label();
                    set_queue_play_state(episode_id, status == "Playing");
                    let position_ms = (position_us / 1000).max(0) as u64;
                    crate::ui::transcript::update_position(episode_id, position_ms);
                    caption.update(episode_id, position_ms, &command_tx_for_caption);
                    let nothing_playing = gettext("Nothing playing");
                    let display_title = if title.is_empty() { &nothing_playing } else { &title };
                    title_label.set_label(display_title);
                    subtitle_label.set_label(&podcast_title);
                    sheet_title_label.set_label(display_title);
                    sheet_subtitle_label.set_label(&podcast_title);
                    let icon = if status == "Playing" {
                        "media-playback-pause-symbolic"
                    } else {
                        "media-playback-start-symbolic"
                    };
                    play_pause_button.set_icon_name(icon);
                    sheet_play_pause_icon.set_icon_name(Some(icon));
                    let play_pause_name = if status == "Playing" { "Pause" } else { "Play" };
                    play_pause_button.set_tooltip_text(Some(play_pause_name));
                    sheet_play_pause_button.set_tooltip_text(Some(play_pause_name));
                    if status != "Stopped" {
                        play_pause_button.set_sensitive(true);
                    }

                    // Decoding the file on every update was wasted work.
                    let artwork_changed = last_artwork_path.borrow().as_ref() != Some(&artwork_local_path);
                    if artwork_changed {
                        last_artwork_path.replace(Some(artwork_local_path.clone()));
                    }
                    match artwork_local_path.as_deref().filter(|_| artwork_changed) {
                        None if !artwork_changed => {}
                        Some(path) => {
                            swap_thumbnail_image(&mini_thumbnail, path);
                            swap_thumbnail_image(&sheet_artwork, path);
                        }
                        None => {
                            mini_thumbnail.set_icon_name(Some("audio-x-generic-symbolic"));
                            sheet_artwork.set_icon_name(Some("audio-x-generic-symbolic"));
                        }
                    }

                    current_duration_us.set(duration_us.unwrap_or(0));
                    let fraction = match duration_us {
                        Some(d) if d > 0 => (position_us as f64 / d as f64).clamp(0.0, 1.0),
                        _ => 0.0,
                    };
                    mini_progress_bar.set_fraction(fraction);
                    if pending_scrub_seek.borrow().is_none() {
                        scrub_bar.set_value(fraction);
                        position_label.set_label(&format_duration_ms(position_us / 1000));
                        set_scrub_value_text(scrub_bar.upcast_ref(), position_us, duration_us.unwrap_or(0));
                    }
                    duration_label.set_label(&duration_us.map(|d| format_duration_ms(d / 1000)).unwrap_or_else(|| "·:·".to_string()));

                    let loaded = status != "Stopped";
                    for button in [&restart_button, &next_episode_button, &skip_backward_button, &skip_forward_button, &episode_details_button] {
                        button.set_sensitive(loaded);
                    }
                    for button in [&previous_chapter_button, &next_chapter_button] {
                        button.set_sensitive(loaded && has_chapters);
                    }
                    scrub_bar.set_sensitive(loaded);
                    sheet_chapter_label.set_label(chapter_title.as_deref().unwrap_or(""));
                    sheet_chapter_label.set_visible(chapter_title.is_some());
                    if current_episode_id.replace(episode_id) != episode_id {
                        match episode_id {
                            Some(id) => fill_episode_details(&sheet_details_box, id, DetailsMode::Player),
                            None => {
                                while let Some(child) = sheet_details_box.first_child() {
                                    sheet_details_box.remove(&child);
                                }
                                sheet_stack.set_visible_child_name("player");
                            }
                        }
                    }
                    bottom_sheet.set_can_open(loaded);
                    if !loaded {
                        bottom_sheet.set_open(false);
                        refresh_idle_mini_player();
                    }
                }
                UiUpdate::DataChanged => {
                    if current_episode_id.get().is_none() {
                        refresh_idle_mini_player();
                    }
                    // Rebuilds at once, then batches further changes for a
                    // second: a fixed one-second delay made every delete and
                    // reorder lag.
                    if data_changed_rebuild_pending.get() {
                        data_changed_rebuild_dirty.set(true);
                    } else {
                        rebuild_all_tabs();
                        data_changed_rebuild_pending.set(true);
                        let data_changed_rebuild_pending = data_changed_rebuild_pending.clone();
                        let data_changed_rebuild_dirty = data_changed_rebuild_dirty.clone();
                        let rebuild_all_tabs = rebuild_all_tabs.clone();
                        gtk4::glib::source::timeout_add_local(std::time::Duration::from_millis(1000), move || {
                            if data_changed_rebuild_dirty.replace(false) {
                                rebuild_all_tabs();
                                return gtk4::glib::ControlFlow::Continue;
                            }
                            data_changed_rebuild_pending.set(false);
                            gtk4::glib::ControlFlow::Break
                        });
                    }
                }
                // The service handles these itself and doesn't forward them.
                UiUpdate::EpisodesRemoved(_) | UiUpdate::NewEpisodes { .. } => {}
                UiUpdate::DownloadProgress { episode_id, percent } => {
                    // None when the row isn't in the displayed list; nothing
                    // to update.
                    if let Some((provider, episode_row, group_color)) = progress_registry.borrow().get(&episode_id) {
                        let css_class = download_progress_css_class(episode_id);
                        provider.load_from_string(&download_progress_css(&css_class, percent, group_color.as_deref()));
                        episode_row.set_subtitle(&gettext_f("Downloading ({percent}%)", &[("percent", &percent.to_string())]));
                    }
                }
                UiUpdate::Notice(message) => {
                    // Wrapped: a toast's own title is one line, which cut
                    // off the message after a long episode title.
                    let label = gtk4::Label::builder().label(&message).wrap(true).max_width_chars(48).justify(gtk4::Justification::Center).build();
                    let toast = libadwaita::Toast::builder().custom_title(&label).timeout(8).build();
                    notice_overlay.add_toast(toast);
                    announce(&notice_overlay, &message);
                }
                UiUpdate::Error(message) => {
                    problems::push_error(message);
                    problems::update_banner(&banner);
                }
                UiUpdate::RefreshStatus { offline, feed_problems } => {
                    problems::set_refresh_status(offline, feed_problems);
                    problems::update_banner(&banner);
                }
                UiUpdate::SyncStatus(problem) => {
                    problems::set_sync_status(problem);
                    problems::update_banner(&banner);
                }
                UiUpdate::Refreshing(running) => {
                    let button = &refresh_button_for_updates;
                    if running {
                        button.add_css_class("phodder-spinning");
                        button.update_property(&[gtk4::accessible::Property::Label(&gettext("Refresh All, refreshing"))]);
                    } else {
                        button.remove_css_class("phodder-spinning");
                        button.update_property(&[gtk4::accessible::Property::Label(&gettext("Refresh All"))]);
                    }
                }
            }
        }
    });

    window
}

/// The Add Feed dialog, an AdwDialog. As a separate gtk4::Window it had
/// broken touch and input-method behavior on mobile.

/// Replaces a slot's single child.
pub(super) fn replace_slot_content(slot: &gtk4::Box, new_content: gtk4::Widget) {
    let old_scroll_position = slot.first_child().and_then(|child| child.downcast::<gtk4::ScrolledWindow>().ok()).map(|sw| sw.vadjustment().value());

    while let Some(child) = slot.first_child() {
        slot.remove(&child);
    }
    slot.append(&new_content);

    if let Some(position) = old_scroll_position {
        if let Ok(new_scrolled) = new_content.clone().downcast::<gtk4::ScrolledWindow>() {
            gtk4::glib::source::idle_add_local(move || {
                new_scrolled.vadjustment().set_value(position);
                gtk4::glib::ControlFlow::Break
            });
        }
    }
}

/// What a screen reader says for the scrub bar's value, which is a fraction.
fn set_scrub_value_text(scrub_bar: &gtk4::Range, position_us: i64, duration_us: i64) {
    let text = if duration_us > 0 {
        gettext_f(
            "{position} of {duration}",
            &[("position", &format_duration_ms(position_us / 1000)), ("duration", &format_duration_ms(duration_us / 1000))],
        )
    } else {
        format_duration_ms(position_us / 1000)
    };
    scrub_bar.update_property(&[gtk4::accessible::Property::ValueText(&text)]);
}

/// A flat circular player button with a 24px icon.
fn big_icon_button(icon: &str) -> gtk4::Button {
    let image = decorative_image();
    image.set_icon_name(Some(icon));
    image.set_pixel_size(24);
    gtk4::Button::builder()
        .child(&image)
        .css_classes(["circular", "flat"])
        .valign(gtk4::Align::Center)
        .width_request(40)
        .height_request(44)
        .build()
}

/// The shortcuts, as shown in the Keyboard Shortcuts dialog.
const PLAYBACK_SHORTCUTS: &[(&str, &str)] = &[
    (N_("Play or Pause"), "space"),
    (N_("Skip Backward"), "<Control>Left"),
    (N_("Skip Forward"), "<Control>Right"),
    (N_("Previous Chapter"), "<Control><Shift>Left"),
    (N_("Next Chapter"), "<Control><Shift>Right"),
];
const GENERAL_SHORTCUTS: &[(&str, &str, &str)] = &[
    (N_("Queue"), "win.show-tab::queue", "<Alt>1"),
    (N_("Library"), "win.show-tab::library", "<Alt>2"),
    (N_("Subscriptions"), "win.show-tab::subscriptions", "<Alt>3"),
    (N_("History"), "win.show-tab::history", "<Alt>4"),
    (N_("Refresh Feeds"), "win.refresh", "<Control>r"),
    (N_("Add Podcast"), "win.add-podcast", "<Control>n"),
    (N_("Settings"), "win.show-settings", "<Control>comma"),
    (N_("Keyboard Shortcuts"), "win.show-shortcuts", "<Control>question"),
    (N_("Close Window"), "window.close", "<Control>w"),
    (N_("Quit"), "win.quit", "<Control>q"),
];
/// Handled by the Queue's rows (see queue.rs); listed for discovery.
const QUEUE_SHORTCUTS: &[(&str, &str)] = &[(N_("Move Episode Up"), "<Alt>Up"), (N_("Move Episode Down"), "<Alt>Down")];

/// "Ctrl+Shift+Left Arrow" for "<Control><Shift>Left": the dialog shows
/// symbols such as ␣ and ←, which screen readers read as their symbol names.
fn spoken_accelerator(accelerator: &str) -> String {
    let mut parts = Vec::new();
    let mut rest = accelerator;
    while let Some(stripped) = rest.strip_prefix('<') {
        let Some((modifier, after)) = stripped.split_once('>') else { break };
        parts.push(match modifier {
            // Key names as printed on keyboards.
            "Control" => gettext("Ctrl"),
            "Shift" => gettext("Shift"),
            "Alt" => gettext("Alt"),
            other => other.to_string(),
        });
        rest = after;
    }
    parts.push(match rest {
        "space" => gettext("Space"),
        "Left" => gettext("Left Arrow"),
        "Right" => gettext("Right Arrow"),
        "Up" => gettext("Up Arrow"),
        "Down" => gettext("Down Arrow"),
        "comma" => gettext("Comma"),
        "question" => gettext("Question Mark"),
        other => other.to_uppercase(),
    });
    parts.join("+")
}

/// Names each row of the shortcuts dialog "Title, keys", and its search
/// field: libadwaita leaves them unnamed.
fn name_shortcut_rows(widget: &gtk4::Widget, spoken: &std::collections::HashMap<String, String>) {
    let mut child = widget.first_child();
    while let Some(current) = child {
        if current.is::<gtk4::SearchEntry>() {
            set_accessible_label(&current, &gettext("Search shortcuts"));
        }
        if current.accessible_role() == gtk4::AccessibleRole::ListItem {
            if let Some(title) = first_label_text(&current) {
                if let Some(keys) = spoken.get(&title) {
                    set_accessible_label(&current, &gettext_f("{action}, {keys}", &[("action", &title), ("keys", keys)]));
                }
            }
        }
        name_shortcut_rows(&current, spoken);
        child = current.next_sibling();
    }
}

fn first_label_text(widget: &gtk4::Widget) -> Option<String> {
    let mut child = widget.first_child();
    while let Some(current) = child {
        if let Some(label) = current.downcast_ref::<gtk4::Label>() {
            return Some(label.label().to_string());
        }
        if let Some(text) = first_label_text(&current) {
            return Some(text);
        }
        child = current.next_sibling();
    }
    None
}

/// Playback keys are handled in the capture phase, ahead of the focused
/// widget, so Ctrl+arrows work from anywhere. They're skipped while typing,
/// where Space and Ctrl+arrows edit text, and Space is left to a focused
/// button, switch, checkbox or row, which it presses.
fn install_keyboard_shortcuts(
    app: &libadwaita::Application,
    window: &libadwaita::ApplicationWindow,
    view_stack: &libadwaita::ViewStack,
    refresh_button: &gtk4::Button,
    command_tx: &UnboundedSender<UiCommand>,
) {
    let key_controller = gtk4::EventControllerKey::new();
    key_controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
    {
        let window = window.clone();
        let command_tx = command_tx.clone();
        key_controller.connect_key_pressed(move |_, key, _, modifiers| {
            let focus = gtk4::prelude::GtkWindowExt::focus(&window);
            let typing = focus.as_ref().is_some_and(|w| w.is::<gtk4::Text>() || w.is::<gtk4::TextView>() || w.ancestor(gtk4::Editable::static_type()).is_some());
            if typing {
                return gtk4::glib::Propagation::Proceed;
            }
            // Space presses a focused button, switch, checkbox or row, as
            // keyboard and screen reader users expect.
            let space_activates_focus = focus.as_ref().is_some_and(|w| {
                w.is::<gtk4::Button>() || w.is::<gtk4::Switch>() || w.is::<gtk4::CheckButton>() || w.is::<gtk4::ListBoxRow>()
            });
            let control = modifiers.contains(gtk4::gdk::ModifierType::CONTROL_MASK);
            let shift = modifiers.contains(gtk4::gdk::ModifierType::SHIFT_MASK);
            let alt = modifiers.contains(gtk4::gdk::ModifierType::ALT_MASK);
            let command = match key {
                gtk4::gdk::Key::space if !control && !shift && !alt && !space_activates_focus => UiCommand::PlayPause,
                gtk4::gdk::Key::Left if control && !alt => if shift { UiCommand::PreviousChapter } else { UiCommand::SkipBackward },
                gtk4::gdk::Key::Right if control && !alt => if shift { UiCommand::NextChapter } else { UiCommand::SkipForward },
                _ => return gtk4::glib::Propagation::Proceed,
            };
            if let Err(e) = command_tx.send(command) {
                tracing::error!("failed to send a keyboard shortcut's command: {e}");
            }
            gtk4::glib::Propagation::Stop
        });
    }
    window.add_controller(key_controller);

    let show_tab = gtk4::gio::SimpleAction::new("show-tab", Some(gtk4::glib::VariantTy::STRING));
    {
        let view_stack = view_stack.clone();
        show_tab.connect_activate(move |_, parameter| {
            if let Some(name) = parameter.and_then(|p| p.get::<String>()) {
                view_stack.set_visible_child_name(&name);
            }
        });
    }
    window.add_action(&show_tab);

    let refresh = gtk4::gio::SimpleAction::new("refresh", None);
    {
        let refresh_button = refresh_button.clone();
        refresh.connect_activate(move |_, _| refresh_button.emit_clicked());
    }
    window.add_action(&refresh);

    let show_shortcuts = gtk4::gio::SimpleAction::new("show-shortcuts", None);
    {
        let window = window.clone();
        show_shortcuts.connect_activate(move |_, _| {
            let dialog = libadwaita::ShortcutsDialog::new();
            let playback = libadwaita::ShortcutsSection::new(Some(&gettext("Playback")));
            for (title, accelerator) in PLAYBACK_SHORTCUTS {
                playback.add(libadwaita::ShortcutsItem::new(&gettext(*title), accelerator));
            }
            dialog.add(playback);
            let general = libadwaita::ShortcutsSection::new(Some(&gettext("General")));
            for (title, _, accelerator) in GENERAL_SHORTCUTS {
                general.add(libadwaita::ShortcutsItem::new(&gettext(*title), accelerator));
            }
            dialog.add(general);
            let queue = libadwaita::ShortcutsSection::new(Some(&gettext("Queue")));
            for (title, accelerator) in QUEUE_SHORTCUTS {
                queue.add(libadwaita::ShortcutsItem::new(&gettext(*title), accelerator));
            }
            dialog.add(queue);
            // Keyed by the translated title, which is what the rows show.
            let spoken: std::collections::HashMap<String, String> = PLAYBACK_SHORTCUTS
                .iter()
                .chain(QUEUE_SHORTCUTS)
                .copied()
                .chain(GENERAL_SHORTCUTS.iter().map(|(title, _, accelerator)| (*title, *accelerator)))
                .map(|(title, accelerator)| (gettext(title), spoken_accelerator(accelerator)))
                .collect();
            dialog.connect_map(move |dialog| name_shortcut_rows(dialog.upcast_ref(), &spoken));
            dialog.present(Some(&window));
        });
    }
    window.add_action(&show_shortcuts);

    for (_, action, accelerator) in GENERAL_SHORTCUTS {
        app.set_accels_for_action(action, &[accelerator]);
    }
}

/// The player's three-line transcript caption: the previous line dimmed
/// above, the current line, the next line dimmed below. Hidden unless the
/// loaded episode has a timed transcript.
#[derive(Clone)]
struct TranscriptCaption {
    button: gtk4::Button,
    previous: gtk4::Label,
    current: gtk4::Label,
    next: gtk4::Label,
    // The episode it was loaded for, and its transcript once loaded.
    loaded: std::rc::Rc<std::cell::RefCell<Option<(i64, Option<Arc<phodder_core::transcript::Transcript>>)>>>,
    shown: std::rc::Rc<std::cell::Cell<Option<Option<usize>>>>,
    // Updates arrive only while playing; a transcript that loads while
    // paused is drawn at this position.
    last_position_ms: std::rc::Rc<std::cell::Cell<u64>>,
}

impl TranscriptCaption {
    fn new() -> Self {
        add_display_css(
            ".transcript-caption-faded { opacity: 0.4; font-weight: normal; }
             .transcript-caption-current { min-height: 4.2em; }",
        );
        let faded = |ellipsize| {
            gtk4::Label::builder()
                .ellipsize(ellipsize)
                .single_line_mode(true)
                .justify(gtk4::Justification::Center)
                .css_classes(["transcript-caption-faded"])
                .build()
        };
        let previous = faded(gtk4::pango::EllipsizeMode::Start);
        let next = faded(gtk4::pango::EllipsizeMode::End);
        let current = gtk4::Label::builder()
            .wrap(true)
            .lines(3)
            .ellipsize(gtk4::pango::EllipsizeMode::End)
            .justify(gtk4::Justification::Center)
            .valign(gtk4::Align::Center)
            .css_classes(["transcript-caption-current"])
            .build();
        let lines = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(4).build();
        lines.append(&previous);
        lines.append(&current);
        lines.append(&next);
        let button = gtk4::Button::builder().child(&lines).css_classes(["flat"]).tooltip_text(gettext("Show Transcript")).visible(false).build();
        Self { button, previous, current, next, loaded: Default::default(), shown: Default::default(), last_position_ms: Default::default() }
    }

    fn update(&self, episode_id: Option<i64>, position_ms: u64, command_tx: &UnboundedSender<UiCommand>) {
        self.last_position_ms.set(position_ms);
        let Some(episode_id) = episode_id else {
            self.loaded.replace(None);
            self.button.set_visible(false);
            return;
        };
        if self.loaded.borrow().as_ref().map(|(id, _)| *id) != Some(episode_id) {
            self.loaded.replace(Some((episode_id, None)));
            self.shown.set(None);
            self.button.set_visible(false);
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            if command_tx.send(UiCommand::LoadTranscript { episode_id, reply_tx }).is_ok() {
                let caption = self.clone();
                gtk4::glib::spawn_future_local(async move {
                    let transcript = reply_rx.await.ok().flatten().filter(|t| t.timed);
                    {
                        let mut loaded = caption.loaded.borrow_mut();
                        match loaded.as_mut() {
                            Some((id, slot)) if *id == episode_id => *slot = transcript,
                            _ => return,
                        }
                    }
                    caption.draw(caption.last_position_ms.get());
                });
            }
            return;
        }
        self.draw(position_ms);
    }

    fn draw(&self, position_ms: u64) {
        let loaded = self.loaded.borrow();
        let Some((_, Some(transcript))) = loaded.as_ref() else { return };
        let index = transcript.line_at(position_ms);
        if self.shown.get() == Some(index) {
            return;
        }
        self.shown.set(Some(index));
        let text = |i: Option<usize>| i.and_then(|i| transcript.lines.get(i)).map(|l| l.text.clone()).unwrap_or_default();
        let (previous, next) = match index {
            Some(i) => (i.checked_sub(1), Some(i + 1)),
            None => (None, Some(0)),
        };
        // The speaker leads the current line when it changes.
        let mut current = text(index);
        if crate::ui::transcript::has_several_speakers(transcript) {
            if let Some(line) = index.and_then(|i| transcript.lines.get(i)) {
                let changed = previous.and_then(|p| transcript.lines.get(p)).map(|p| p.speaker != line.speaker).unwrap_or(true);
                if let Some(name) = line.speaker.as_deref().filter(|_| changed).and_then(crate::ui::transcript::display_speaker) {
                    current = gettext_f("{speaker}: {text}", &[("speaker", &name), ("text", &current)]);
                }
            }
        }
        self.previous.set_label(&text(previous));
        self.current.set_label(&current);
        self.next.set_label(&text(next));
        // Not announced as it changes, which would talk over the episode;
        // a screen reader user can move to it.
        set_accessible_label(&self.button, &gettext("Show Transcript"));
        self.button.update_property(&[gtk4::accessible::Property::Description(&current)]);
        self.button.set_visible(true);
    }
}

/// Name, developer, license, version, links and release notes come from the
/// bundled metainfo, so they're kept up to date in one place. The donation
/// link is added here because libadwaita doesn't read that kind of URL.
fn show_about_dialog(window: &libadwaita::ApplicationWindow) {
    let about = libadwaita::AboutDialog::from_appdata("/net/dunevent/phodder/metainfo.xml", Some(env!("CARGO_PKG_VERSION")));
    about.set_version(env!("CARGO_PKG_VERSION"));
    about.set_copyright("© 2026 Phodder contributors");
    about.add_link(&gettext("Donate"), "https://dunevent.net/donate");
    about.add_link(&gettext("Source Code"), "https://dev.dunevent.net/aj/Phodder");
    // Translators: replace with your names, one per line, to be credited in
    // the About dialog.
    let translators = gettext("translator-credits");
    if translators != "translator-credits" {
        about.set_translator_credits(&translators);
    }
    about.present(Some(window));
}
