//! Selecting several episodes in a list (Queue, Library, History, a
//! podcast's Episodes tab) to act on them together.
//!
//! Each list has a named scope that outlives its page, since pages are
//! rebuilt whenever data changes. A page registers its rows as it builds
//! them; selection state and the bar at the top (count with Select All/None,
//! then the actions) live in the scope. Every action is shown; one that
//! doesn't apply to every selected episode is dimmed.

use super::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

/// One selectable row. Rows on a podcast's Episodes tab may have no
/// database row yet, so they're keyed by guid and carry the feed's episode.
#[derive(Clone)]
pub(super) enum Item {
    Episode(i64),
    Feed { subscription_id: i64, episode: phodder_core::feed::ParsedEpisode },
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Key {
    Episode(i64),
    Feed(i64, String),
}

impl Item {
    fn key(&self) -> Key {
        match self {
            Item::Episode(id) => Key::Episode(*id),
            Item::Feed { subscription_id, episode } => Key::Feed(*subscription_id, episode.guid.clone()),
        }
    }
}

/// What an action needs to know about one selected episode, read when the
/// selection changes.
#[derive(Clone, Copy, Default)]
struct Facts {
    episode_id: Option<i64>,
    downloaded: bool,
    pending: bool,
    failed: bool,
    queued: bool,
    played: bool,
    in_history: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Action {
    Download,
    AddToQueue,
    RemoveFromQueue,
    MarkPlayed,
    MarkUnplayed,
    DeleteFile,
    ClearFromHistory,
}

/// Paired actions share a slot: at most one of a pair can apply to every
/// selected episode, and the bar has to fit a 360 px phone.
const SLOTS: [&[Action]; 5] = [
    &[Action::Download],
    &[Action::AddToQueue, Action::RemoveFromQueue],
    &[Action::MarkPlayed, Action::MarkUnplayed],
    &[Action::DeleteFile],
    &[Action::ClearFromHistory],
];

impl Action {
    fn applies(self, facts: &Facts) -> bool {
        match self {
            Action::Download => !facts.downloaded && !facts.pending,
            Action::AddToQueue => facts.downloaded && !facts.queued,
            Action::RemoveFromQueue => facts.queued,
            Action::MarkPlayed => facts.episode_id.is_some() && !facts.played,
            Action::MarkUnplayed => facts.episode_id.is_some() && facts.played,
            Action::DeleteFile => facts.downloaded,
            Action::ClearFromHistory => facts.in_history,
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Action::Download => "folder-download-symbolic",
            Action::AddToQueue => "list-add-symbolic",
            Action::RemoveFromQueue => "list-remove-symbolic",
            Action::MarkPlayed => "object-select-symbolic",
            Action::MarkUnplayed => "edit-undo-symbolic",
            Action::DeleteFile => "user-trash-symbolic",
            Action::ClearFromHistory => "edit-clear-symbolic",
        }
    }

    fn label(self) -> String {
        match self {
            Action::Download => gettext("Download"),
            Action::AddToQueue => gettext("Add to Queue"),
            Action::RemoveFromQueue => gettext("Remove from Queue"),
            Action::MarkPlayed => gettext("Mark Played"),
            Action::MarkUnplayed => gettext("Mark Unplayed"),
            Action::DeleteFile => gettext("Delete Files"),
            Action::ClearFromHistory => gettext("Clear from History"),
        }
    }

    fn done_message(self, n: usize) -> String {
        let count = n.to_string();
        let args = [("n", count.as_str())];
        match self {
            Action::Download => ngettext_f("Downloading {n} episode", "Downloading {n} episodes", n as u32, &args),
            Action::AddToQueue => ngettext_f("Added {n} episode to the queue", "Added {n} episodes to the queue", n as u32, &args),
            Action::RemoveFromQueue => ngettext_f("Removed {n} episode from the queue", "Removed {n} episodes from the queue", n as u32, &args),
            Action::MarkPlayed => ngettext_f("Marked {n} episode played", "Marked {n} episodes played", n as u32, &args),
            Action::MarkUnplayed => ngettext_f("Marked {n} episode unplayed", "Marked {n} episodes unplayed", n as u32, &args),
            Action::DeleteFile => ngettext_f("Deleted {n} file", "Deleted {n} files", n as u32, &args),
            Action::ClearFromHistory => ngettext_f("Cleared {n} episode from History", "Cleared {n} episodes from History", n as u32, &args),
        }
    }
}

struct Row {
    item: Item,
    check: gtk4::glib::WeakRef<gtk4::CheckButton>,
}

/// The current page's bars; replaced on each rebuild.
struct Bars {
    top: gtk4::glib::WeakRef<gtk4::Revealer>,
    count: gtk4::glib::WeakRef<gtk4::MenuButton>,
    actions: gtk4::glib::WeakRef<gtk4::Box>,
    /// The page's own bar (the podcast tab's Select bar), hidden while
    /// selecting.
    idle_bars: Vec<gtk4::glib::WeakRef<gtk4::Widget>>,
}

struct Inner {
    active: Cell<bool>,
    selected: RefCell<Vec<Key>>,
    rows: RefCell<HashMap<Key, Row>>,
    /// A paged list's every item, including other pages' (a podcast's
    /// Episodes tab). Empty for lists that show everything.
    all_items: RefCell<Vec<Item>>,
    name: String,
    bars: RefCell<Option<Bars>>,
    context: RefCell<Option<(std::path::PathBuf, UnboundedSender<UiCommand>, libadwaita::ToastOverlay)>>,
    refresh_pending: Cell<bool>,
    on_mode_changed: RefCell<Vec<Box<dyn Fn(bool)>>>,
}

#[derive(Clone)]
pub(super) struct Selection(Rc<Inner>);

thread_local! {
    static SCOPES: RefCell<HashMap<String, Selection>> = RefCell::default();
}

/// The scope for a list: "queue", "library", "history", or
/// "podcast:<subscription id>".
pub(super) fn scope(name: &str) -> Selection {
    SCOPES.with(|s| {
        s.borrow_mut()
            .entry(name.to_string())
            .or_insert_with(|| {
                Selection(Rc::new(Inner {
                    active: Cell::new(false),
                    selected: RefCell::default(),
                    rows: RefCell::default(),
                    all_items: RefCell::default(),
                    name: name.to_string(),
                    bars: RefCell::default(),
                    context: RefCell::default(),
                    refresh_pending: Cell::new(false),
                    on_mode_changed: RefCell::default(),
                }))
            })
            .clone()
    })
}

/// Leaves selection in every list, for Escape and tab switches.
pub(super) fn exit_all() {
    let scopes: Vec<Selection> = SCOPES.with(|s| s.borrow().values().cloned().collect());
    for scope in scopes {
        scope.set_active(false);
    }
}

impl Selection {
    pub(super) fn is_active(&self) -> bool {
        self.0.active.get()
    }

    pub(super) fn connect_mode_changed(&self, f: impl Fn(bool) + 'static) {
        self.0.on_mode_changed.borrow_mut().push(Box::new(f));
    }

    pub(super) fn set_active(&self, on: bool) {
        if on && self.0.rows.borrow().is_empty() {
            for f in self.0.on_mode_changed.borrow().iter() {
                f(false);
            }
            return;
        }
        if self.0.active.replace(on) == on {
            return;
        }
        if !on {
            self.0.selected.borrow_mut().clear();
        }
        for row in self.0.rows.borrow().values() {
            if let Some(check) = row.check.upgrade() {
                check.set_visible(on);
                check.set_active(false);
            }
        }
        for f in self.0.on_mode_changed.borrow().iter() {
            f(on);
        }
        self.refresh_now();
    }

    /// Starts a page build: forgets the previous page's rows and records
    /// what actions need.
    pub(super) fn begin_page(&self, db_path: &std::path::Path, command_tx: &UnboundedSender<UiCommand>, toast_overlay: &libadwaita::ToastOverlay) {
        self.0.rows.borrow_mut().clear();
        self.0.all_items.borrow_mut().clear();
        *self.0.context.borrow_mut() = Some((db_path.to_path_buf(), command_tx.clone(), toast_overlay.clone()));
    }

    /// For a list of more than one page, after begin_page: every item on
    /// every page, so selections on other pages count and Select All can
    /// cover them.
    pub(super) fn set_all_items(&self, items: Vec<Item>) {
        *self.0.all_items.borrow_mut() = items;
    }

    /// Adds a checkbox to the start of `row`, and long-press and right-click
    /// to start selecting. Call after adding the row's other prefixes:
    /// add_prefix puts each new one first.
    pub(super) fn attach_row(&self, row: &libadwaita::ActionRow, item: Item) {
        let key = item.key();
        let check = gtk4::CheckButton::builder()
            .valign(gtk4::Align::Center)
            .visible(self.is_active())
            .active(self.0.selected.borrow().contains(&key))
            .build();
        check.add_css_class("selection-mode");
        set_accessible_label(&check, &gettext("Select"));
        row.add_prefix(&check);
        {
            let scope = self.clone();
            let key = key.clone();
            check.connect_toggled(move |check| scope.set_checked(&key, check.is_active()));
        }
        // Touch only: a mouse gets right-click instead.
        let long_press = gtk4::GestureLongPress::builder().touch_only(true).build();
        {
            let scope = self.clone();
            let key = key.clone();
            long_press.connect_pressed(move |gesture, _, _| {
                gesture.set_state(gtk4::EventSequenceState::Claimed);
                scope.start_with(&key);
            });
        }
        row.add_controller(long_press);
        let right_click = gtk4::GestureClick::builder().button(gtk4::gdk::BUTTON_SECONDARY).build();
        {
            let scope = self.clone();
            let key = key.clone();
            right_click.connect_pressed(move |gesture, _, _, _| {
                gesture.set_state(gtk4::EventSequenceState::Claimed);
                scope.start_with(&key);
            });
        }
        row.add_controller(right_click);
        self.0.rows.borrow_mut().insert(key, Row { item, check: check.downgrade() });
        if self.is_active() {
            self.schedule_refresh();
        }
    }

    /// For a row's activation: while selecting, a tap toggles the row
    /// instead of opening it. Returns true when it did.
    pub(super) fn handle_activate(&self, item: &Item) -> bool {
        if !self.is_active() {
            return false;
        }
        let key = item.key();
        let check = self.0.rows.borrow().get(&key).and_then(|r| r.check.upgrade());
        if let Some(check) = check {
            check.set_active(!check.is_active());
        }
        true
    }

    fn start_with(&self, key: &Key) {
        let was_active = self.is_active();
        self.set_active(true);
        let check = self.0.rows.borrow().get(key).and_then(|r| r.check.upgrade());
        if let Some(check) = check {
            check.set_active(!(was_active && check.is_active()));
        }
    }

    fn set_checked(&self, key: &Key, on: bool) {
        {
            let mut selected = self.0.selected.borrow_mut();
            let present = selected.contains(key);
            if on && !present {
                selected.push(key.clone());
            } else if !on && present {
                selected.retain(|k| k != key);
            } else {
                return;
            }
        }
        self.schedule_refresh();
    }

    /// Rows hidden by a filter are left alone.
    fn set_all(&self, on: bool) {
        let checks: Vec<gtk4::CheckButton> =
            self.0.rows.borrow().values().filter_map(|r| r.check.upgrade()).filter(|check| row_shown(check)).collect();
        for check in checks {
            check.set_active(on);
        }
    }

    /// Every page of a paged list.
    fn select_everything(&self) {
        {
            let mut selected = self.0.selected.borrow_mut();
            for item in self.0.all_items.borrow().iter() {
                let key = item.key();
                if !selected.contains(&key) {
                    selected.push(key);
                }
            }
        }
        self.set_all(true);
        self.schedule_refresh();
    }

    /// Including rows on other pages and ones a filter hides.
    fn select_none(&self) {
        self.0.selected.borrow_mut().clear();
        self.set_all(false);
        self.schedule_refresh();
    }

    fn is_paged(&self) -> bool {
        !self.0.all_items.borrow().is_empty()
    }

    /// Wraps a page's content with the selection bars. `idle_top` is the
    /// page's own bar, shown only while not selecting.
    /// `exit_toggle` adds a pressed Select button that leaves selection, for
    /// pages without the header's.
    pub(super) fn wrap(&self, content: &impl IsA<gtk4::Widget>, idle_top: Option<&gtk4::Widget>, exit_toggle: bool) -> gtk4::Box {
        let page = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).vexpand(true).build();

        let select_menu = gtk4::gio::Menu::new();
        if self.is_paged() {
            let total = self.0.all_items.borrow().len();
            select_menu.append(Some(&gettext("Select _Page")), Some("selection.all"));
            select_menu.append(
                Some(&ngettext_f("Select All {n} Episode", "Select All {n} Episodes", total as u32, &[("n", &total.to_string())])),
                Some("selection.everything"),
            );
        } else {
            select_menu.append(Some(&gettext("Select _All")), Some("selection.all"));
        }
        select_menu.append(Some(&gettext("Select _None")), Some("selection.none"));
        let count = gtk4::MenuButton::builder().menu_model(&select_menu).css_classes(["flat"]).build();
        let actions = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        let end = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        end.append(&actions);
        if exit_toggle {
            let toggle = gtk4::ToggleButton::builder().icon_name("selection-mode-symbolic").tooltip_text(gettext("Select Episodes")).active(true).build();
            set_accessible_label(&toggle, &gettext("Select Episodes"));
            let scope = self.clone();
            toggle.connect_toggled(move |t| {
                if !t.is_active() {
                    scope.set_active(false);
                    t.set_active(true);
                }
            });
            end.append(&toggle);
        }
        let top_bar = gtk4::CenterBox::builder().margin_start(6).margin_end(6).margin_top(6).margin_bottom(6).build();
        top_bar.set_start_widget(Some(&count));
        top_bar.set_end_widget(Some(&end));
        let top = gtk4::Revealer::builder().child(&top_bar).reveal_child(self.is_active()).build();

        let group = gtk4::gio::SimpleActionGroup::new();
        let handlers: [(&str, fn(&Selection)); 3] =
            [("all", |s| s.set_all(true)), ("everything", Selection::select_everything), ("none", Selection::select_none)];
        for (name, handler) in handlers {
            let action = gtk4::gio::SimpleAction::new(name, None);
            let scope = self.clone();
            action.connect_activate(move |_, _| handler(&scope));
            group.add_action(&action);
        }
        page.insert_action_group("selection", Some(&group));
        let keys = gtk4::EventControllerKey::new();
        {
            let scope = self.clone();
            keys.connect_key_pressed(move |_, key, _, _| {
                if key == gtk4::gdk::Key::Escape && scope.is_active() {
                    scope.set_active(false);
                    return gtk4::glib::Propagation::Stop;
                }
                gtk4::glib::Propagation::Proceed
            });
        }
        page.add_controller(keys);

        if let Some(idle) = idle_top {
            page.append(idle);
        }
        page.append(&top);
        page.append(content);
        *self.0.bars.borrow_mut() = Some(Bars {
            top: top.downgrade(),
            count: count.downgrade(),
            actions: actions.downgrade(),
            idle_bars: idle_top.into_iter().map(|w| w.downgrade()).collect(),
        });
        // A closed dialog or a page that wasn't replaced by another leaves
        // selection; a rebuild has already pointed the bars at its new page.
        {
            let scope = self.clone();
            let top = top.downgrade();
            page.connect_destroy(move |_| {
                let current = scope.0.bars.borrow().as_ref().map(|b| b.top.upgrade());
                if matches!(current, Some(None)) || current.flatten().zip(top.upgrade()).is_some_and(|(a, b)| a == b) {
                    scope.set_active(false);
                }
            });
        }
        self.refresh_now();
        page
    }

    /// Coalesces refreshes while a page adds rows in batches.
    fn schedule_refresh(&self) {
        if self.0.refresh_pending.replace(true) {
            return;
        }
        let scope = self.clone();
        gtk4::glib::idle_add_local_once(move || {
            scope.0.refresh_pending.set(false);
            scope.refresh_now();
        });
    }

    /// Rows a filter hides stay checked but aren't counted or acted on.
    /// Selections on a paged list's other pages count.
    fn selected_items(&self) -> Vec<Item> {
        let rows = self.0.rows.borrow();
        let all_items = self.0.all_items.borrow();
        let elsewhere: HashMap<Key, &Item> = all_items.iter().map(|item| (item.key(), item)).collect();
        self.0
            .selected
            .borrow()
            .iter()
            .filter_map(|key| match rows.get(key) {
                Some(row) => row.check.upgrade().is_some_and(|check| row_shown(&check)).then(|| row.item.clone()),
                None => elsewhere.get(key).map(|item| (*item).clone()),
            })
            .collect()
    }

    /// After a page's filter changes which rows are shown.
    pub(super) fn filter_changed(&self) {
        self.schedule_refresh();
    }

    fn refresh_now(&self) {
        let Some((top, count, actions, idle_bars)) = self.0.bars.borrow().as_ref().and_then(|b| {
            Some((b.top.upgrade()?, b.count.upgrade()?, b.actions.upgrade()?, b.idle_bars.iter().filter_map(|w| w.upgrade()).collect::<Vec<_>>()))
        }) else {
            return;
        };
        let active = self.is_active();
        top.set_reveal_child(active);
        for idle in idle_bars {
            idle.set_visible(!active);
        }
        while let Some(child) = actions.first_child() {
            actions.remove(&child);
        }
        if !active {
            return;
        }
        let items = self.selected_items();
        let n = items.len();
        let label = ngettext_f("{n} Selected", "{n} Selected", n as u32, &[("n", &n.to_string())]);
        count.set_label(&label);
        announce(&count, &label);
        let Some((db_path, command_tx, toast_overlay)) = self.0.context.borrow().clone() else { return };
        let in_history = self.0.name == "history";
        let mut facts = if n == 0 { Vec::new() } else { read_facts(&db_path, &items) };
        for f in &mut facts {
            f.in_history = in_history;
        }
        for slot in SLOTS {
            if slot.contains(&Action::ClearFromHistory) && !in_history {
                continue;
            }
            let usable = slot.iter().copied().find(|a| n > 0 && facts.iter().all(|f| a.applies(f)));
            let action = usable.unwrap_or(slot[0]);
            let button = gtk4::Button::builder().icon_name(action.icon()).tooltip_text(action.label()).css_classes(["flat"]).sensitive(usable.is_some()).build();
            set_accessible_label(&button, &action.label());
            let scope = self.clone();
            let items = items.clone();
            let facts = facts.clone();
            let db_path = db_path.clone();
            let command_tx = command_tx.clone();
            let toast_overlay = toast_overlay.clone();
            button.connect_clicked(move |button| {
                let run = {
                    let (scope, items, facts, db_path, command_tx, toast_overlay) =
                        (scope.clone(), items.clone(), facts.clone(), db_path.clone(), command_tx.clone(), toast_overlay.clone());
                    move || {
                        let done = perform(action, &items, &facts, &db_path, &command_tx);
                        if done > 0 {
                            toast_overlay.add_toast(libadwaita::Toast::new(&action.done_message(done)));
                        }
                        let _ = command_tx.send(UiCommand::NotifyDataChanged);
                        scope.set_active(false);
                    }
                };
                if action != Action::Download || items.len() <= DOWNLOAD_CONFIRM_OVER {
                    run();
                    return;
                }
                let n = items.len();
                let size: Option<i64> = items
                    .iter()
                    .map(|item| match item {
                        Item::Feed { episode, .. } => episode.file_size_bytes.filter(|b| *b > 0),
                        Item::Episode(_) => None,
                    })
                    .sum();
                let body = match size.and_then(|bytes| u64::try_from(bytes).ok()) {
                    Some(bytes) => gettext_f("About {size} in all.", &[("size", &gtk4::glib::format_size(bytes))]),
                    None => String::new(),
                };
                let alert = libadwaita::AlertDialog::new(
                    Some(&ngettext_f("Download {n} Episode?", "Download {n} Episodes?", n as u32, &[("n", &n.to_string())])),
                    (!body.is_empty()).then_some(body.as_str()),
                );
                alert.add_responses(&[("cancel", &gettext("Cancel")), ("download", &gettext("Download"))]);
                alert.set_response_appearance("download", libadwaita::ResponseAppearance::Suggested);
                alert.set_default_response(Some("download"));
                alert.set_close_response("cancel");
                let button = button.clone();
                gtk4::glib::spawn_future_local(async move {
                    if alert.choose_future(Some(&button)).await == "download" {
                        run();
                    }
                });
            });
            actions.append(&button);
        }
    }
}

/// Downloading more than this many at once asks first.
const DOWNLOAD_CONFIRM_OVER: usize = 20;

fn row_shown(check: &gtk4::CheckButton) -> bool {
    check.ancestor(gtk4::ListBoxRow::static_type()).is_some_and(|row| row.is_visible())
}

fn read_facts(db_path: &std::path::Path, items: &[Item]) -> Vec<Facts> {
    let Ok(conn) = phodder_core::open_connection(db_path) else { return vec![Facts::default(); items.len()] };
    let mut podcast_statuses: HashMap<i64, HashMap<String, phodder_core::subscriptions::BackCatalogEntry>> = HashMap::new();
    items
        .iter()
        .map(|item| match item {
            Item::Episode(id) => conn
                .query_row(
                    "SELECT download_status, group_queue_position IS NOT NULL, played FROM episodes WHERE id = ?1",
                    [id],
                    |row| {
                        let status: String = row.get(0)?;
                        Ok(Facts {
                            episode_id: Some(*id),
                            downloaded: status == "downloaded",
                            pending: status == "queued" || status == "downloading",
                            failed: status == "failed",
                            queued: row.get(1)?,
                            played: row.get(2)?,
                            in_history: false,
                        })
                    },
                )
                .unwrap_or_default(),
            Item::Feed { subscription_id, episode } => {
                let statuses = podcast_statuses
                    .entry(*subscription_id)
                    .or_insert_with(|| phodder_core::subscriptions::back_catalog_statuses(&conn, *subscription_id).unwrap_or_default());
                match statuses.get(&episode.guid) {
                    Some(entry) => Facts {
                        episode_id: Some(entry.episode_id),
                        downloaded: entry.downloaded,
                        pending: entry.pending,
                        failed: entry.failed,
                        queued: entry.is_queued,
                        played: entry.status == phodder_core::subscriptions::BackCatalogStatus::Played,
                        in_history: false,
                    },
                    None => Facts::default(),
                }
            }
        })
        .collect()
}

/// Returns how many episodes the action was applied to.
fn perform(action: Action, items: &[Item], facts: &[Facts], db_path: &std::path::Path, command_tx: &UnboundedSender<UiCommand>) -> usize {
    let send = |command: UiCommand| {
        if let Err(e) = command_tx.send(command) {
            tracing::error!("failed to send a selection command: {e}");
        }
    };
    if action == Action::DeleteFile {
        send(UiCommand::StopIfCurrent(facts.iter().filter_map(|f| f.episode_id).collect()));
    }
    let mut done = 0;
    for (item, facts) in items.iter().zip(facts) {
        let ok = match (action, item, facts.episode_id) {
            (Action::Download, _, Some(id)) if facts.failed => {
                let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
                send(UiCommand::RequeueEpisode { episode_id: id, reply_tx });
                true
            }
            (Action::Download, Item::Feed { subscription_id, episode }, _) => {
                let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
                send(UiCommand::QueueBackCatalogEpisode { subscription_id: *subscription_id, episode: episode.clone(), skip_enqueue: true, reply_tx });
                true
            }
            (Action::Download, Item::Episode(id), _) => {
                trigger_manual_download(command_tx, *id, true);
                true
            }
            (Action::AddToQueue, _, Some(id)) => {
                send(UiCommand::EnqueueEpisode(id));
                true
            }
            (Action::RemoveFromQueue, _, Some(id)) => {
                send(UiCommand::DequeueEpisode(id));
                true
            }
            (Action::MarkPlayed | Action::MarkUnplayed, _, Some(id)) => {
                send(UiCommand::SetPlayed { episode_id: id, played: action == Action::MarkPlayed });
                true
            }
            (Action::DeleteFile, Item::Feed { subscription_id, episode }, _) => match delete_back_catalog_episode_file(db_path, *subscription_id, &episode.guid) {
                Ok(()) => true,
                Err(e) => {
                    tracing::error!("failed to delete {}: {e}", episode.guid);
                    false
                }
            },
            (Action::ClearFromHistory, _, Some(id)) => match clear_history_episode(db_path, id) {
                Ok(()) => true,
                Err(e) => {
                    tracing::error!("failed to clear episode {id} from History: {e}");
                    false
                }
            },
            (Action::DeleteFile, Item::Episode(id), _) => match delete_episode_file(db_path, *id) {
                Ok(()) => true,
                Err(e) => {
                    tracing::error!("failed to delete episode {id}: {e}");
                    false
                }
            },
            _ => false,
        };
        done += usize::from(ok);
    }
    done
}
