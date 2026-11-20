//! The Transcript tab of episode details, and the shared position updates
//! that drive it and the player's caption.
//!
//! While the episode plays, the view follows the current line until the
//! user scrolls by any means; the floating button resumes following.
//! Activating a line plays from there, except a tap that only stops a
//! fling.

use super::*;
use phodder_core::transcript::Transcript;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

/// A tap this soon after pointer scrolling (including a fling's
/// deceleration) stops the scroll instead of seeking.
const TAP_AFTER_SCROLL_GUARD: Duration = Duration::from_millis(400);
/// Value changes this soon after a scroll or drag event are a fling's
/// deceleration, not keyboard focus moving.
const FLING_WINDOW: Duration = Duration::from_millis(1500);

thread_local! {
    static VIEWS: RefCell<Vec<Weak<View>>> = const { RefCell::new(Vec::new()) };
    // The loaded episode and its position, for views built while playing.
    static LAST_POSITION: Cell<Option<(i64, u64)>> = const { Cell::new(None) };
}

struct View {
    episode_id: i64,
    transcript: Arc<Transcript>,
    scrolled: gtk4::ScrolledWindow,
    list: gtk4::ListBox,
    rows: Vec<gtk4::ListBoxRow>,
    current: Cell<Option<usize>>,
    follow: Cell<bool>,
    recenter: gtk4::Button,
    last_pointer_scroll: Cell<Option<Instant>>,
    animation: RefCell<Option<libadwaita::TimedAnimation>>,
}

/// Called with every playback update. `episode_id` is the loaded episode.
pub(super) fn update_position(episode_id: Option<i64>, position_ms: u64) {
    LAST_POSITION.with(|p| p.set(episode_id.map(|id| (id, position_ms))));
    let views: Vec<Rc<View>> = VIEWS.with(|views| {
        let mut views = views.borrow_mut();
        views.retain(|v| v.strong_count() > 0);
        views.iter().filter_map(Weak::upgrade).collect()
    });
    for view in views {
        let playing_here = episode_id == Some(view.episode_id);
        view.recenter.set_visible(playing_here && !view.follow.get());
        if playing_here {
            view.set_position(position_ms);
        }
    }
}

/// The current position of `episode_id`, if it's the loaded episode.
fn position_of(episode_id: i64) -> Option<u64> {
    LAST_POSITION.with(|p| p.get()).filter(|(id, _)| *id == episode_id).map(|(_, pos)| pos)
}

/// "Speaker 1" for diarization labels such as "SPEAKER_00"; None for
/// "UNKNOWN".
pub(super) fn display_speaker(raw: &str) -> Option<String> {
    if raw.eq_ignore_ascii_case("unknown") {
        return None;
    }
    match raw.strip_prefix("SPEAKER_").and_then(|n| n.parse::<u32>().ok()) {
        Some(n) => Some(gettext_f("Speaker {number}", &[("number", &(n + 1).to_string())])),
        None => Some(raw.to_string()),
    }
}

/// Whether to show speakers at all: one voice needs no labels.
pub(super) fn has_several_speakers(transcript: &Transcript) -> bool {
    let mut speakers = transcript.lines.iter().filter_map(|l| l.speaker.as_deref());
    let Some(first) = speakers.next() else { return false };
    speakers.any(|s| s != first)
}

/// The Transcript tab's content: a spinner until the transcript loads.
pub(super) fn build_transcript_page(episode_id: i64, command_tx: &UnboundedSender<UiCommand>) -> gtk4::Widget {
    let slot = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).vexpand(true).build();
    let spinner = gtk4::Spinner::builder().spinning(true).halign(gtk4::Align::Center).valign(gtk4::Align::Center).vexpand(true).width_request(32).height_request(32).build();
    set_accessible_label(&spinner, &gettext("Loading transcript"));
    slot.append(&spinner);
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    if command_tx.send(UiCommand::LoadTranscript { episode_id, reply_tx }).is_err() {
        return slot.upcast();
    }
    let slot_for_reply = slot.clone();
    gtk4::glib::spawn_future_local(async move {
        let content: gtk4::Widget = match reply_rx.await.ok().flatten() {
            Some(transcript) => build_view(episode_id, transcript),
            None => libadwaita::StatusPage::builder()
                .title(gettext("Couldn't Load the Transcript"))
                .description(gettext("It may be offline or unavailable from the podcast."))
                .vexpand(true)
                .build()
                .upcast(),
        };
        while let Some(child) = slot_for_reply.first_child() {
            slot_for_reply.remove(&child);
        }
        slot_for_reply.append(&content);
    });
    slot.upcast()
}

fn build_view(episode_id: i64, transcript: Arc<Transcript>) -> gtk4::Widget {
    add_display_css(
        ".transcript-list { background: none; }
         .transcript-line { padding: 6px 12px; border-radius: 8px; }
         .transcript-current { background-color: alpha(var(--accent-bg-color), 0.15); }",
    );
    let list = gtk4::ListBox::builder()
        .selection_mode(gtk4::SelectionMode::None)
        .css_classes(["transcript-list"])
        .margin_start(6)
        .margin_end(6)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    let show_speakers = has_several_speakers(&transcript);
    let mut rows = Vec::with_capacity(transcript.lines.len());
    let mut previous_speaker: Option<&str> = None;
    for line in &transcript.lines {
        let row_box = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(2).build();
        let speaker = line.speaker.as_deref().filter(|_| show_speakers).and_then(display_speaker);
        let speaker_changed = show_speakers && line.speaker.as_deref() != previous_speaker;
        previous_speaker = line.speaker.as_deref();
        if speaker_changed {
            if let Some(name) = &speaker {
                let heading = if transcript.timed { format!("{name} · {}", format_duration_ms(line.start_ms as i64)) } else { name.clone() };
                row_box.append(&gtk4::Label::builder().label(heading).xalign(0.0).css_classes(["caption-heading", "dim-label"]).build());
            }
        }
        row_box.append(&gtk4::Label::builder().label(&line.text).wrap(true).xalign(0.0).build());
        let row = gtk4::ListBoxRow::builder().child(&row_box).activatable(transcript.timed).css_classes(["transcript-line"]).build();
        let spoken = match (&speaker, transcript.timed) {
            (Some(name), true) => format!("{}, {name}: {}", format_duration_ms(line.start_ms as i64), line.text),
            (None, true) => format!("{}, {}", format_duration_ms(line.start_ms as i64), line.text),
            (Some(name), false) => format!("{name}: {}", line.text),
            (None, false) => line.text.clone(),
        };
        set_accessible_label(&row, &spoken);
        list.append(&row);
        rows.push(row);
    }

    let scrolled = gtk4::ScrolledWindow::builder().child(&list).hscrollbar_policy(gtk4::PolicyType::Never).vexpand(true).build();
    let recenter = gtk4::Button::builder()
        .icon_name("find-location-symbolic")
        .tooltip_text(gettext("Follow Along"))
        .css_classes(["circular", "osd"])
        .halign(gtk4::Align::End)
        .valign(gtk4::Align::End)
        .margin_end(18)
        .margin_bottom(18)
        .width_request(48)
        .height_request(48)
        .visible(false)
        .build();
    let overlay = gtk4::Overlay::builder().child(&scrolled).build();
    overlay.add_overlay(&recenter);

    let view = Rc::new(View {
        episode_id,
        transcript: transcript.clone(),
        scrolled: scrolled.clone(),
        list: list.clone(),
        rows,
        current: Cell::new(None),
        follow: Cell::new(true),
        recenter: recenter.clone(),
        last_pointer_scroll: Cell::new(None),
        animation: RefCell::new(None),
    });
    if transcript.timed {
        view.wire_user_scrolling();
        let weak = Rc::downgrade(&view);
        recenter.connect_clicked(move |_| {
            if let Some(view) = weak.upgrade() {
                view.set_follow(true);
                view.scroll_to_current(true);
            }
        });
        let weak = Rc::downgrade(&view);
        list.connect_row_activated(move |_, row| {
            let Some(view) = weak.upgrade() else { return };
            if view.last_pointer_scroll.get().is_some_and(|t| t.elapsed() < TAP_AFTER_SCROLL_GUARD) {
                return;
            }
            let Some(index) = view.rows.iter().position(|r| r == row) else { return };
            // Playing from a line means following from there.
            view.set_follow(true);
            seek_or_play(view.episode_id, view.transcript.lines[index].start_ms);
        });
        if let Some(position) = position_of(episode_id) {
            view.set_position_without_scrolling(position);
            // Once the rows have sizes.
            let weak = Rc::downgrade(&view);
            list.connect_map(move |_| {
                if let Some(view) = weak.upgrade() {
                    view.scroll_to_current_when_laid_out(0);
                }
            });
        }
        // One Tab stop, arrows inside: GTK's list otherwise tabs through
        // every line. Focus moves on with the list made unfocusable for the
        // moment, so traversal skips it.
        let tab_out = gtk4::EventControllerKey::new();
        tab_out.set_propagation_phase(gtk4::PropagationPhase::Capture);
        {
            let list = list.clone();
            tab_out.connect_key_pressed(move |_, key, _, modifiers| {
                let direction = match key {
                    gtk4::gdk::Key::Tab if !modifiers.contains(gtk4::gdk::ModifierType::SHIFT_MASK) => gtk4::DirectionType::TabForward,
                    gtk4::gdk::Key::Tab | gtk4::gdk::Key::ISO_Left_Tab => gtk4::DirectionType::TabBackward,
                    _ => return gtk4::glib::Propagation::Proceed,
                };
                let Some(root) = list.root() else { return gtk4::glib::Propagation::Proceed };
                list.set_can_focus(false);
                root.child_focus(direction);
                list.set_can_focus(true);
                gtk4::glib::Propagation::Stop
            });
        }
        list.add_controller(tab_out);
        // Tabbing in lands on the current line, not the first.
        let focus = gtk4::EventControllerFocus::new();
        let weak = Rc::downgrade(&view);
        focus.connect_enter(move |_| {
            let Some(view) = weak.upgrade() else { return };
            if let Some(row) = view.current.get().and_then(|i| view.rows.get(i)).cloned() {
                // After GTK finishes moving focus to the first row.
                gtk4::glib::idle_add_local_once(move || {
                    row.grab_focus();
                });
            }
        });
        list.add_controller(focus);
        // Keeps the view alive for position updates; it ends with the widget.
        VIEWS.with(|views| views.borrow_mut().push(Rc::downgrade(&view)));
        let keep_alive = RefCell::new(Some(view));
        overlay.connect_destroy(move |_| {
            keep_alive.borrow_mut().take();
        });
    }
    overlay.upcast()
}

impl View {
    fn set_follow(&self, follow: bool) {
        self.follow.set(follow);
        self.recenter.set_visible(!follow && position_of(self.episode_id).is_some());
        if !follow {
            if let Some(animation) = self.animation.borrow_mut().take() {
                animation.pause();
            }
        }
    }

    fn wire_user_scrolling(self: &Rc<Self>) {
        let stop_following = {
            let weak = Rc::downgrade(self);
            move |pointer: bool| {
                if let Some(view) = weak.upgrade() {
                    if pointer {
                        view.last_pointer_scroll.set(Some(Instant::now()));
                    }
                    if view.follow.get() {
                        view.set_follow(false);
                    }
                }
            }
        };
        // Wheel and touchpad.
        let scroll = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::BOTH_AXES);
        scroll.set_propagation_phase(gtk4::PropagationPhase::Capture);
        {
            let stop_following = stop_following.clone();
            scroll.connect_scroll(move |_, _, _| {
                stop_following(true);
                gtk4::glib::Propagation::Proceed
            });
        }
        self.scrolled.add_controller(scroll);
        // Touch drags and the scrollbar. Never claims the sequence, so the
        // scrolled window still pans and flings.
        let drag = gtk4::GestureDrag::new();
        drag.set_propagation_phase(gtk4::PropagationPhase::Capture);
        {
            let stop_following = stop_following.clone();
            drag.connect_drag_update(move |_, _, dy| {
                if dy.abs() > 4.0 {
                    stop_following(true);
                }
            });
        }
        self.scrolled.add_controller(drag);
        // Keyboard scrolling and moving between lines.
        let keys = gtk4::EventControllerKey::new();
        keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
        {
            let stop_following = stop_following.clone();
            keys.connect_key_pressed(move |_, key, _, _| {
                use gtk4::gdk::Key;
                if matches!(key, Key::Up | Key::Down | Key::Page_Up | Key::Page_Down | Key::Home | Key::End | Key::KP_Up | Key::KP_Down) {
                    stop_following(false);
                }
                gtk4::glib::Propagation::Proceed
            });
        }
        self.scrolled.add_controller(keys);
        // A fling keeps moving after the last input event.
        let weak = Rc::downgrade(self);
        self.scrolled.vadjustment().connect_value_changed(move |_| {
            if let Some(view) = weak.upgrade() {
                if view.last_pointer_scroll.get().is_some_and(|t| t.elapsed() < FLING_WINDOW) {
                    view.last_pointer_scroll.set(Some(Instant::now()));
                }
            }
        });
    }

    fn set_position_without_scrolling(&self, position_ms: u64) {
        let index = self.transcript.line_at(position_ms);
        if index == self.current.get() {
            return;
        }
        if let Some(old) = self.current.get().and_then(|i| self.rows.get(i)) {
            old.remove_css_class("transcript-current");
        }
        if let Some(new) = index.and_then(|i| self.rows.get(i)) {
            new.add_css_class("transcript-current");
        }
        self.current.set(index);
    }

    fn set_position(&self, position_ms: u64) {
        let before = self.current.get();
        self.set_position_without_scrolling(position_ms);
        if self.follow.get() && self.current.get() != before {
            self.scroll_to_current(true);
        }
    }

    /// Centers the current line, if it has been laid out.
    fn scroll_to_current(&self, animate: bool) -> bool {
        let Some(row) = self.current.get().and_then(|i| self.rows.get(i)) else { return true };
        let Some(point) = row.compute_point(&self.list, &gtk4::graphene::Point::new(0.0, 0.0)) else { return false };
        if row.height() == 0 {
            return false;
        }
        let adjustment = self.scrolled.vadjustment();
        let target = (point.y() as f64 + row.height() as f64 / 2.0 - adjustment.page_size() / 2.0)
            .clamp(adjustment.lower(), (adjustment.upper() - adjustment.page_size()).max(adjustment.lower()));
        if let Some(animation) = self.animation.borrow_mut().take() {
            animation.pause();
        }
        if animate && self.scrolled.is_mapped() {
            let target_property = libadwaita::PropertyAnimationTarget::new(&adjustment, "value");
            let animation = libadwaita::TimedAnimation::new(&self.scrolled, adjustment.value(), target, 300, target_property);
            animation.set_easing(libadwaita::Easing::EaseOutCubic);
            animation.play();
            self.animation.replace(Some(animation));
        } else {
            adjustment.set_value(target);
        }
        true
    }

    /// Retries until GTK has sized the rows, which happens after mapping.
    fn scroll_to_current_when_laid_out(self: Rc<Self>, attempt: u32) {
        if self.scroll_to_current(false) || attempt >= 20 {
            return;
        }
        gtk4::glib::timeout_add_local_once(Duration::from_millis(30), move || self.scroll_to_current_when_laid_out(attempt + 1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diarization_labels_read_as_speaker_numbers() {
        assert_eq!(display_speaker("SPEAKER_00").as_deref(), Some("Speaker 1"));
        assert_eq!(display_speaker("Alice").as_deref(), Some("Alice"));
        assert_eq!(display_speaker("UNKNOWN"), None);
    }
}
