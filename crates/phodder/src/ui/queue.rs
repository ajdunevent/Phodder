use super::*;

/// The Queue tab, with group headers, since groups determine play order.
pub(super) fn build_queue_page_from_data(
    queue: Vec<phodder_core::queue::QueuedEpisodeSummary>,
    db_path: &std::path::Path,
    command_tx: &UnboundedSender<UiCommand>,
    toast_overlay: &libadwaita::ToastOverlay,
) -> gtk4::Widget {
    QUEUE_PLAY_BUTTONS.with(|buttons| buttons.borrow_mut().clear());
    let selection = selection::scope("queue");
    if queue.is_empty() {
        selection.set_active(false);
        return placeholder_page(&gettext("Queue"), "view-list-symbolic").upcast();
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

    // Replaces GTK's whole-row :drop(active) highlight, which read as the
    // target's border, with a line at the bottom edge: a drop always lands
    // right after the hovered row or header.
    list_box.set_widget_name("queue-list-box");
    let drop_indicator_css = gtk4::CssProvider::new();
    drop_indicator_css.load_from_string(
        "#queue-list-box row:drop(active) {
            background: none;
            border: none;
            outline: none;
            box-shadow: none;
            border-bottom: 3px solid var(--accent-bg-color);
        }",
    );
    if let Some(display) = gtk4::gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(&display, &drop_indicator_css, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }

    // Rows play from a button; tapping opens details.

    // Each episode's neighbors within its group, for keyboard reordering.
    let order: std::rc::Rc<Vec<(i64, i64)>> = std::rc::Rc::new(queue.iter().map(|e| (e.episode_id, e.group_id)).collect());
    let pending_focus = QUEUE_PENDING_FOCUS.with(|pending| pending.take());

    let mut last_group_id: Option<i64> = None;
    for episode in queue.iter() {
        if last_group_id != Some(episode.group_id) {
            let header_label = gtk4::Label::builder()
                .label(&episode.group_name)
                .halign(gtk4::Align::Start)
                .accessible_role(gtk4::AccessibleRole::Heading)
                .css_classes(["heading"])
                .margin_start(6)
                .margin_end(6)
                .margin_top(0)
                .margin_bottom(2)
                .build();
            let header_row = gtk4::ListBoxRow::builder().child(&header_label).selectable(false).activatable(false).build();
            set_accessible_label(&header_row, &episode.group_name);
            if let Some((r, g, b)) = episode.group_color.as_deref().and_then(parse_rgb_components) {
                // A low-opacity tint of the group color.
                let widget_name = format!("queue-group-header-{}", episode.group_id);
                header_row.set_widget_name(&widget_name);
                let css = gtk4::CssProvider::new();
                css.load_from_string(&format!("#{widget_name} {{ background-color: rgba({r},{g},{b},0.15); }}"));
                if let Some(display) = gtk4::gdk::Display::default() {
                    gtk4::style_context_add_provider_for_display(&display, &css, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);
                }
            }
            list_box.append(&header_row);
            last_group_id = Some(episode.group_id);

            // Headers accept drops so an episode can move to the front of its
            // group; rows only place after themselves.
            let header_drop_target = gtk4::DropTarget::new(i64::static_type(), gtk4::gdk::DragAction::MOVE);
            {
                let db_path = db_path.to_path_buf();
                let command_tx = command_tx.clone();
                let target_group_id = episode.group_id;
                header_drop_target.connect_drop(move |_, value, _, _| {
                    let Ok(dragged_episode_id) = value.get::<i64>() else {
                        return false;
                    };
                    let result = phodder_core::open_connection(&db_path)
                        .map_err(phodder_core::queue::ReorderError::Sqlite)
                        .and_then(|conn| phodder_core::queue::move_episode_to_front_of_group_if_member(&conn, dragged_episode_id, target_group_id));
                    match result {
                        Ok(()) => {
                            if let Err(e) = command_tx.send(UiCommand::NotifyDataChanged) {
                                tracing::error!("failed to send NotifyDataChanged after move-to-front: {e}");
                            }
                            true
                        }
                        Err(e) => {
                            // Cross-group drops are refused by design; GTK's
                            // refusal animation is enough feedback.
                            tracing::debug!("move-to-front refused for episode {dragged_episode_id}: {e:?}");
                            false
                        }
                    }
                });
            }
            header_row.add_controller(header_drop_target);
        }

        let duration_display = episode.duration_ms.map(format_duration_ms);
        let size_display = display_file_size(episode.file_size_bytes);
        let second_line = join_non_empty(&[duration_display.as_deref(), size_display.as_deref()]);
        let subtitle_text =
            format!("{} · {}\n{}", escape_markup(&episode.podcast_title), format_publish_date(&episode.publish_date), second_line);
        // The wrapper row below is the one focused and announced.
        let episode_row = libadwaita::ActionRow::builder()
            .activatable(true)
            .focusable(false)
            .accessible_role(gtk4::AccessibleRole::Presentation)
            .title(&escape_markup(&episode.title))
            .subtitle(&subtitle_text)
            .title_lines(1)
            .subtitle_lines(1)
            .build();

        // The drag source, left of the row as a sibling in a plain Box so it
        // sits flush without negative margins fighting row padding.
        let grab_handle = decorative_image();
        grab_handle.set_icon_name(Some("list-drag-handle-symbolic"));
        grab_handle.set_tooltip_text(Some(&gettext("Drag to reorder within this group")));
        grab_handle.set_size_request(20, -1);
        grab_handle.set_vexpand(true);
        grab_handle.set_valign(gtk4::Align::Fill);
        if let Some((r, g, b)) = episode.group_color.as_deref().and_then(parse_rgb_components) {
            // Tinted like the group header.
            let widget_name = format!("queue-grab-handle-{}", episode.episode_id);
            grab_handle.set_widget_name(&widget_name);
            let css = gtk4::CssProvider::new();
            css.load_from_string(&format!("#{widget_name} {{ background-color: rgba({r},{g},{b},0.25); }}"));
            if let Some(display) = gtk4::gdk::Display::default() {
                gtk4::style_context_add_provider_for_display(&display, &css, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);
            }
        }

        let thumbnail = load_thumbnail_image(
            &[episode.artwork_local_path.as_deref(), episode.subscription_artwork_local_path.as_deref()],
            "audio-x-generic-symbolic",
            64,
        );
        episode_row.add_prefix(&thumbnail);
        selection.attach_row(&episode_row, selection::Item::Episode(episode.episode_id));

        // The one action: Play, or Pause for the playing episode. Removing
        // from the queue is in the episode's details.
        let play_button = gtk4::Button::from_icon_name("media-playback-start-symbolic");
        play_button.set_valign(gtk4::Align::Center);
        play_button.add_css_class("flat");
        episode_row.add_suffix(&play_button);
        {
            let command_tx = command_tx.clone();
            let episode_id = episode.episode_id;
            wire_suffix_button_action(&play_button, move || {
                let is_current = QUEUE_PLAY_STATE.with(|state| state.get().0 == Some(episode_id));
                let command = if is_current { UiCommand::PlayPause } else { UiCommand::PlayEpisode(episode_id) };
                if let Err(e) = command_tx.send(command) {
                    tracing::error!("failed to send queue play command: {e}");
                }
            });
        }
        QUEUE_PLAY_BUTTONS.with(|buttons| buttons.borrow_mut().push((episode.episode_id, play_button.downgrade())));

        let drag_source = gtk4::DragSource::new();
        drag_source.set_actions(gtk4::gdk::DragAction::MOVE);
        {
            let episode_id = episode.episode_id;
            let content = gtk4::gdk::ContentProvider::for_value(&episode_id.to_value());
            drag_source.set_content(Some(&content));
        }
        {
            // The drag icon is a snapshot of the whole row, hotspot at the
            // handle; the default icon was just the handle. Measured at
            // drag-begin, when the row has a height.
            let episode_row_for_icon = episode_row.clone();
            drag_source.connect_drag_begin(move |source, _| {
                let paintable = gtk4::WidgetPaintable::new(Some(&episode_row_for_icon));
                source.set_icon(Some(&paintable), 10, episode_row_for_icon.height() / 2);
            });
        }
        grab_handle.add_controller(drag_source);

        let drop_target = gtk4::DropTarget::new(i64::static_type(), gtk4::gdk::DragAction::MOVE);
        {
            let db_path = db_path.to_path_buf();
            let command_tx = command_tx.clone();
            let target_episode_id = episode.episode_id;
            drop_target.connect_drop(move |_, value, _, _| {
                let Ok(dragged_episode_id) = value.get::<i64>() else {
                    return false;
                };
                let result = phodder_core::open_connection(&db_path)
                    .map_err(phodder_core::queue::ReorderError::Sqlite)
                    .and_then(|conn| phodder_core::queue::reorder_queued_episode(&conn, dragged_episode_id, target_episode_id));
                match result {
                    Ok(()) => {
                        if let Err(e) = command_tx.send(UiCommand::NotifyDataChanged) {
                            tracing::error!("failed to send NotifyDataChanged after reorder: {e}");
                        }
                        true
                    }
                    Err(e) => {
                        // Cross-group drops are refused by design; GTK's
                        // refusal animation is enough feedback.
                        tracing::debug!("reorder refused for episode {dragged_episode_id}: {e:?}");
                        false
                    }
                }
            });
        }
        // The drop target goes on list_row (below), covering the handle too.



        episode_row.set_hexpand(true);
        let row_with_handle = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(0).hexpand(true).build();
        row_with_handle.append(&grab_handle);
        row_with_handle.append(&episode_row);

        // An explicit ListBoxRow so its padding can be zeroed by id selector,
        // beating boxed-list's class rule.
        let list_row = gtk4::ListBoxRow::builder().child(&row_with_handle).build();
        list_row.add_controller(drop_target);
        set_accessible_label(&list_row, &episode.title);
        list_row.update_property(&[gtk4::accessible::Property::Description(&gettext_f(
            "{details}. Alt+Up or Alt+Down moves it within {group}.",
            &[
                ("details", &format!("{} · {} · {}", episode.podcast_title, format_publish_date(&episode.publish_date), second_line)),
                ("group", &episode.group_name),
            ],
        ))]);
        {
            let key_controller = gtk4::EventControllerKey::new();
            let db_path = db_path.to_path_buf();
            let command_tx = command_tx.clone();
            let order = order.clone();
            let episode_id = episode.episode_id;
            let group_name = episode.group_name.clone();
            key_controller.connect_key_pressed(move |controller, key, _, modifiers| {
                let up = match key {
                    gtk4::gdk::Key::Up => true,
                    gtk4::gdk::Key::Down => false,
                    _ => return gtk4::glib::Propagation::Proceed,
                };
                if !modifiers.contains(gtk4::gdk::ModifierType::ALT_MASK) {
                    return gtk4::glib::Propagation::Proceed;
                }
                let row = controller.widget();
                match move_within_group(&db_path, &order, episode_id, up) {
                    Ok(true) => {
                        QUEUE_PENDING_FOCUS.with(|pending| pending.set(Some((episode_id, up))));
                        if let Err(e) = command_tx.send(UiCommand::NotifyDataChanged) {
                            tracing::error!("failed to send NotifyDataChanged after keyboard reorder: {e}");
                        }
                    }
                    Ok(false) => {
                        if let Some(row) = row {
                            let message = if up {
                                gettext_f("Already first in {group}", &[("group", &group_name)])
                            } else {
                                gettext_f("Already last in {group}", &[("group", &group_name)])
                            };
                            announce(&row, &message);
                        }
                    }
                    Err(e) => tracing::error!("keyboard reorder failed for episode {episode_id}: {e:?}"),
                }
                gtk4::glib::Propagation::Stop
            });
            list_row.add_controller(key_controller);
        }
        if let Some((focus_id, moved_up)) = pending_focus.filter(|(id, _)| *id == episode.episode_id) {
            let list_row = list_row.clone();
            // After the page is in the window, or grab_focus does nothing.
            gtk4::glib::idle_add_local_once(move || {
                list_row.grab_focus();
                announce(&list_row, &if moved_up { gettext("Moved up") } else { gettext("Moved down") });
                tracing::debug!("refocused queue row {focus_id} after keyboard reorder");
            });
        }
        let list_row_widget_name = format!("queue-row-{}", episode.episode_id);
        list_row.set_widget_name(&list_row_widget_name);
        let row_css = gtk4::CssProvider::new();
        row_css.load_from_string(&format!("#{list_row_widget_name} {{ padding: 0; margin: 0; }}"));
        if let Some(display) = gtk4::gdk::Display::default() {
            gtk4::style_context_add_provider_for_display(&display, &row_css, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);
        }
        list_box.append(&list_row);
    }

    // Episode rows are nested in a wrapper row (for the grab handle), so
    // activation arrives here rather than on the inner row.
    {
        let selection = selection.clone();
        list_box.connect_row_activated(move |_, row| {
            if let Some(id) = row.widget_name().strip_prefix("queue-row-").and_then(|id| id.parse().ok()) {
                if !selection.handle_activate(&selection::Item::Episode(id)) {
                    open_episode_details(id);
                }
            }
        });
    }

    // As in Library: see build_library_page.
    let bottom_divider_row = gtk4::ListBoxRow::builder()
        .activatable(false)
        .selectable(false)
        .focusable(false)
        .accessible_role(gtk4::AccessibleRole::Presentation)
        .child(&gtk4::Box::new(gtk4::Orientation::Horizontal, 0))
        .build();
    list_box.append(&bottom_divider_row);

    let scrolled = gtk4::ScrolledWindow::builder().child(&list_box).vexpand(true).build();

    // Auto-scrolls near the edges while dragging, so queues longer than the
    // view can be reordered. Attached to the ScrolledWindow so coordinates
    // are viewport-relative.
    const EDGE_ZONE_PX: f64 = 48.0;
    const SCROLL_STEP_PX: f64 = 14.0;
    let scroll_direction: std::rc::Rc<std::cell::Cell<f64>> = std::rc::Rc::new(std::cell::Cell::new(0.0));
    let auto_scroll_timer_active = std::rc::Rc::new(std::cell::Cell::new(false));
    let edge_drop_target = gtk4::DropTarget::new(i64::static_type(), gtk4::gdk::DragAction::MOVE);
    {
        let scrolled_for_motion = scrolled.clone();
        let scroll_direction = scroll_direction.clone();
        let timer_active = auto_scroll_timer_active.clone();
        edge_drop_target.connect_motion(move |_, _, y| {
            let height = scrolled_for_motion.height() as f64;
            let new_direction =
                if y < EDGE_ZONE_PX { -SCROLL_STEP_PX } else if height > 0.0 && y > height - EDGE_ZONE_PX { SCROLL_STEP_PX } else { 0.0 };
            scroll_direction.set(new_direction);

            // One timer, started on demand; it stops itself when the
            // direction returns to 0.
            if new_direction != 0.0 && !timer_active.get() {
                timer_active.set(true);
                let scrolled_for_tick = scrolled_for_motion.clone();
                let scroll_direction_for_tick = scroll_direction.clone();
                let timer_active_for_tick = timer_active.clone();
                gtk4::glib::source::timeout_add_local(std::time::Duration::from_millis(40), move || {
                    let direction = scroll_direction_for_tick.get();
                    if direction == 0.0 {
                        timer_active_for_tick.set(false);
                        return gtk4::glib::ControlFlow::Break;
                    }
                    let adjustment = scrolled_for_tick.vadjustment();
                    adjustment.set_value(adjustment.value() + direction);
                    gtk4::glib::ControlFlow::Continue
                });
            }

            gtk4::gdk::DragAction::MOVE
        });
    }
    {
        let scroll_direction = scroll_direction.clone();
        edge_drop_target.connect_leave(move |_| {
            // Dragging off the view stops motion events, so nothing else
            // would stop the scroll.
            scroll_direction.set(0.0);
        });
    }
    scrolled.add_controller(edge_drop_target);

    refresh_queue_play_buttons();
    selection.wrap(&scrolled, None, false).upcast()
}

/// Loads the queue off the GTK thread and swaps in the tab's widget.
pub(super) fn build_queue_page(
    tokio_handle: &tokio::runtime::Handle,
    queue_slot: &gtk4::Box,
    db_path: &std::path::Path,
    command_tx: &UnboundedSender<UiCommand>,
    toast_overlay: &libadwaita::ToastOverlay,
) {
    let queue_slot = queue_slot.clone();
    let db_path_owned = db_path.to_path_buf();
    let command_tx = command_tx.clone();
    let toast_overlay = toast_overlay.clone();
    run_db_op(
        tokio_handle,
        db_path.to_path_buf(),
        |conn| phodder_core::queue::list_queue(conn).map_err(anyhow::Error::from),
        move |result| match result {
            Ok(queue) => {
                if slot_data_unchanged(&queue_slot, &queue) {
                    return;
                }
                let widget = build_queue_page_from_data(queue, &db_path_owned, &command_tx, &toast_overlay);
                replace_slot_content(&queue_slot, widget);
            }
            Err(e) => tracing::error!("failed to load queue: {e}"),
        },
    );
}

/// Download-progress widgets for episodes currently downloading, with the
/// group color captured at build time. Repopulated with every Library
/// rebuild, since rebuilds drop the old rows.
pub(super) type DownloadProgressRegistry = std::rc::Rc<std::cell::RefCell<std::collections::HashMap<i64, (gtk4::CssProvider, libadwaita::ActionRow, Option<String>)>>>;

/// One definition of the class name for the row builder and the progress
/// updater.
pub(super) fn download_progress_css_class(episode_id: i64) -> String {
    format!("download-progress-{episode_id}")
}

/// A hard-edged fill from the leading edge, tinted with the group color, or
/// `alpha(@accent_bg_color, 0.2)` when the group has none.
pub(super) fn download_progress_css(css_class: &str, percent: u8, group_color: Option<&str>) -> String {
    let percent = percent.min(100);
    let fill_color = group_color
        .and_then(parse_rgb_components)
        .map(|(r, g, b)| format!("rgba({r},{g},{b},0.2)"))
        .unwrap_or_else(|| "alpha(@accent_bg_color, 0.2)".to_string());
    format!("row.{css_class} {{ background-image: linear-gradient(to right, {fill_color} {percent}%, transparent {percent}%); }}")
}

thread_local! {
    // The loaded episode and whether it's playing, from PlaybackState.
    static QUEUE_PLAY_STATE: std::cell::Cell<(Option<i64>, bool)> = const { std::cell::Cell::new((None, false)) };
    // Each Queue row's play button, rebuilt with the Queue.
    static QUEUE_PLAY_BUTTONS: std::cell::RefCell<Vec<(i64, gtk4::glib::WeakRef<gtk4::Button>)>> = Default::default();
    // The row a keyboard move left focus on, refocused and announced when
    // the Queue is rebuilt.
    // The bool is whether it moved up.
    static QUEUE_PENDING_FOCUS: std::cell::Cell<Option<(i64, bool)>> = const { std::cell::Cell::new(None) };
}

/// Moves an episode one place within its group. Ok(false) when it's already
/// at that end. `order` is the Queue as displayed: (episode_id, group_id).
fn move_within_group(
    db_path: &std::path::Path,
    order: &[(i64, i64)],
    episode_id: i64,
    up: bool,
) -> Result<bool, phodder_core::queue::ReorderError> {
    let Some(&(_, group_id)) = order.iter().find(|(id, _)| *id == episode_id) else { return Ok(false) };
    let group: Vec<i64> = order.iter().filter(|(_, g)| *g == group_id).map(|(id, _)| *id).collect();
    let index = group.iter().position(|id| *id == episode_id).unwrap_or(0);
    let conn = phodder_core::open_connection(db_path).map_err(phodder_core::queue::ReorderError::Sqlite)?;
    // reorder_queued_episode places an episode after its target, so moving
    // up targets the episode two places above, or the group's front.
    match (up, index) {
        (true, 0) => Ok(false),
        (true, 1) => phodder_core::queue::move_episode_to_front_of_group_if_member(&conn, episode_id, group_id).map(|_| true),
        (true, i) => phodder_core::queue::reorder_queued_episode(&conn, episode_id, group[i - 2]).map(|_| true),
        (false, i) if i + 1 >= group.len() => Ok(false),
        (false, i) => phodder_core::queue::reorder_queued_episode(&conn, episode_id, group[i + 1]).map(|_| true),
    }
}

/// Called on every PlaybackState; only touches buttons when the loaded
/// episode or play state changes.
pub(super) fn set_queue_play_state(episode_id: Option<i64>, playing: bool) {
    if QUEUE_PLAY_STATE.with(|state| state.replace((episode_id, playing))) == (episode_id, playing) {
        return;
    }
    refresh_queue_play_buttons();
}

fn refresh_queue_play_buttons() {
    let (current, playing) = QUEUE_PLAY_STATE.with(|state| state.get());
    QUEUE_PLAY_BUTTONS.with(|buttons| {
        buttons.borrow_mut().retain(|(episode_id, button)| {
            let Some(button) = button.upgrade() else { return false };
            let pause = playing && current == Some(*episode_id);
            button.set_icon_name(if pause { "media-playback-pause-symbolic" } else { "media-playback-start-symbolic" });
            button.set_tooltip_text(Some(&if pause { gettext("Pause") } else { gettext("Play") }));
            true
        });
    });
}
