use super::*;

pub(super) const PLAYBACK_RATES: [f64; 15] = [0.5, 0.75, 1.0, 1.25, 1.5, 1.75, 2.0, 2.25, 2.5, 2.75, 3.0, 3.25, 3.5, 3.75, 4.0];

/// Check-for-new-episodes choices, in minutes.
const REFRESH_INTERVALS_MINUTES: [i64; 8] = [15, 30, 60, 120, 240, 480, 720, 1440];

/// New podcasts' delete-after choices, in minutes.
pub(super) const EXPIRY_CHOICES_MINUTES: [i64; 8] = [720, 1440, 2 * 1440, 3 * 1440, 10080, 2 * 10080, 4 * 10080, 8 * 10080];

pub(super) fn duration_label(minutes: i64) -> String {
    if minutes % 10080 == 0 {
        let weeks = minutes / 10080;
        ngettext_f("{weeks} week", "{weeks} weeks", weeks as u32, &[("weeks", &weeks.to_string())])
    } else if minutes % 1440 == 0 {
        let days = minutes / 1440;
        ngettext_f("{days} day", "{days} days", days as u32, &[("days", &days.to_string())])
    } else if minutes % 60 == 0 {
        let hours = minutes / 60;
        ngettext_f("{hours} hour", "{hours} hours", hours as u32, &[("hours", &hours.to_string())])
    } else {
        ngettext_f("{minutes} minute", "{minutes} minutes", minutes as u32, &[("minutes", &minutes.to_string())])
    }
}

fn refresh_interval_label(minutes: i64) -> String {
    if minutes % 60 == 0 {
        let hours = minutes / 60;
        if hours == 24 {
            return gettext("Every day");
        }
        return ngettext_f("Every hour", "Every {hours} hours", hours as u32, &[("hours", &hours.to_string())]);
    }
    ngettext_f("Every minute", "Every {minutes} minutes", minutes as u32, &[("minutes", &minutes.to_string())])
}

/// Global defaults: updates and downloads, sync, playback, the storage
/// defaults for podcasts without their own, podcast directories, and
/// running in the background. Every change is saved at once. `at_sync`
/// scrolls to the Sync group.
pub(super) fn show_settings_dialog(
    parent: &libadwaita::ApplicationWindow,
    db_path: std::path::PathBuf,
    dialog_sheet: &libadwaita::BottomSheet,
    skip_backward_button: &gtk4::Button,
    skip_forward_button: &gtk4::Button,
    command_tx: &UnboundedSender<UiCommand>,
    at_sync: bool,
) {
    let conn = match phodder_core::open_connection(&db_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("failed to open db for settings dialog: {e}");
            return;
        }
    };
    let get_i64 = |key: &str| phodder_core::settings::get_setting_i64(&conn, key);
    let get_flag = |key: &str, default: bool| phodder_core::settings::get_setting(&conn, key).map(|v| v != "0").unwrap_or(default);

    let toast_overlay = libadwaita::ToastOverlay::new();
    // Writes one setting; None removes it.
    let save: std::rc::Rc<dyn Fn(&str, Option<String>)> = {
        let db_path = db_path.clone();
        let toast_overlay = toast_overlay.clone();
        std::rc::Rc::new(move |key: &str, value: Option<String>| {
            let result = phodder_core::open_connection(&db_path).and_then(|conn| match &value {
                Some(value) => phodder_core::settings::set_setting(&conn, key, value),
                None => conn.execute("DELETE FROM settings WHERE key = ?1", [key]).map(drop),
            });
            if let Err(e) = result {
                tracing::error!("failed to save setting {key}: {e}");
                toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't save changes")));
            }
        })
    };
    let flag = |on: bool| Some(if on { "1" } else { "0" }.to_string());

    let page = libadwaita::PreferencesPage::new();

    // General
    let general = libadwaita::PreferencesGroup::builder().title(gettext("General")).build();
    let background_row = libadwaita::SwitchRow::builder()
        .title(gettext("Run in background"))
        .subtitle(gettext("Keep playing and checking for episodes after the window closes"))
        .active(crate::run_in_background())
        .build();
    let autostart_row = libadwaita::SwitchRow::builder()
        .title(gettext("Launch on startup"))
        .subtitle(gettext("Requires running in the background"))
        .active(crate::is_autostart_enabled())
        .sensitive(background_row.is_active())
        .build();
    general.add(&background_row);
    general.add(&autostart_row);
    page.add(&general);

    // Updates and downloads
    let updates = libadwaita::PreferencesGroup::builder().title(gettext("Updates")).build();
    let current_refresh_minutes = (get_i64("refresh_interval_secs").unwrap_or(3600) / 60).max(1);
    let mut interval_choices: Vec<i64> = REFRESH_INTERVALS_MINUTES.to_vec();
    // A value set before these choices existed stays selectable.
    if !interval_choices.contains(&current_refresh_minutes) {
        interval_choices.push(current_refresh_minutes);
        interval_choices.sort_unstable();
    }
    let interval_labels: Vec<String> = interval_choices.iter().map(|m| refresh_interval_label(*m)).collect();
    let interval_row = libadwaita::ComboRow::builder()
        .title(gettext("Check for new episodes"))
        .model(&gtk4::StringList::new(&interval_labels.iter().map(String::as_str).collect::<Vec<_>>()))
        .selected(interval_choices.iter().position(|m| *m == current_refresh_minutes).unwrap_or(2) as u32)
        .build();
    {
        let save = save.clone();
        interval_row.connect_selected_notify(move |row| {
            if let Some(minutes) = interval_choices.get(row.selected() as usize) {
                save("refresh_interval_secs", Some((minutes * 60).to_string()));
            }
        });
    }
    let refresh_metered_row = libadwaita::SwitchRow::builder()
        .title(gettext("Update feeds on metered connections"))
        .active(get_flag(crate::downloader::REFRESH_ON_METERED_SETTING, true))
        .build();
    let download_metered_row = libadwaita::SwitchRow::builder()
        .title(gettext("Download episodes on metered connections"))
        .subtitle(gettext("Otherwise new episodes wait for an unmetered connection"))
        .active(get_flag(crate::downloader::DOWNLOAD_ON_METERED_SETTING, false))
        .build();
    for (row, key) in [
        (&refresh_metered_row, crate::downloader::REFRESH_ON_METERED_SETTING),
        (&download_metered_row, crate::downloader::DOWNLOAD_ON_METERED_SETTING),
    ] {
        let save = save.clone();
        row.connect_active_notify(move |row| save(key, flag(row.is_active())));
    }
    let (backfill_row, backfill_spin) =
        spin_row(&gettext("Episodes to download when subscribing"), 0.0, 100.0, get_i64("initial_backfill_limit").unwrap_or(3) as f64);
    {
        let save = save.clone();
        backfill_spin.connect_value_changed(move |spin| save("initial_backfill_limit", Some((spin.value() as i64).to_string())));
    }
    updates.add(&interval_row);
    updates.add(&refresh_metered_row);
    updates.add(&download_metered_row);
    updates.add(&backfill_row);
    page.add(&updates);
    let sync_group = build_sync_group(parent, &db_path, command_tx, &toast_overlay);
    page.add(&sync_group);
    if at_sync {
        // Positions are only known once the dialog has been laid out.
        sync_group.connect_map(|group| {
            let group = group.clone();
            gtk4::glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
                let Some(scrolled) = group.ancestor(gtk4::ScrolledWindow::static_type()).and_downcast::<gtk4::ScrolledWindow>() else { return };
                let Some(child) = scrolled.child() else { return };
                if let Some(point) = group.compute_point(&child, &gtk4::graphene::Point::new(0.0, 0.0)) {
                    scrolled.vadjustment().set_value(f64::from(point.y()) - 12.0);
                }
            });
        });
    }

    // Playback
    let playback = libadwaita::PreferencesGroup::builder().title(gettext("Playback")).build();
    let skip_labels = [gettext("10 seconds"), gettext("30 seconds")];
    let skip_model = gtk4::StringList::new(&skip_labels.iter().map(String::as_str).collect::<Vec<_>>());
    for (title, key, default_ms, forward, button) in [
        (gettext("Skip back"), "skip_backward_ms", 10_000, false, skip_backward_button),
        (gettext("Skip forward"), "skip_forward_ms", 30_000, true, skip_forward_button),
    ] {
        let current = get_i64(key).unwrap_or(default_ms);
        let row = libadwaita::ComboRow::builder().title(title).model(&skip_model).selected(if current >= 30_000 { 1 } else { 0 }).build();
        let save = save.clone();
        let button = button.clone();
        row.connect_selected_notify(move |row| {
            let ms = if row.selected() == 1 { 30_000 } else { 10_000 };
            save(key, Some(ms.to_string()));
            set_button_icon(&button, skip_icon_name(ms, forward));
            button.set_tooltip_text(Some(&skip_tooltip_text(ms, forward)));
        });
        playback.add(&row);
    }
    let (played_threshold_row, played_threshold_spin) = spin_row(
        &gettext("Mark played near the end"),
        0.0,
        9999.0,
        phodder_core::settings::get_setting_f64(&conn, "default_played_threshold_seconds").unwrap_or(0.0),
    );
    played_threshold_row.set_subtitle(&gettext("Seconds left when an episode counts as played"));
    {
        let save = save.clone();
        played_threshold_spin.connect_value_changed(move |spin| save("default_played_threshold_seconds", Some(spin.value().to_string())));
    }
    playback.add(&played_threshold_row);
    let trim_row = libadwaita::SwitchRow::builder()
        .title(gettext("Trim silence"))
        .subtitle(gettext("Shortens long pauses. Podcasts can set their own."))
        .active(get_flag(phodder_core::settings::TRIM_SILENCE, false))
        .build();
    {
        let save = save.clone();
        let command_tx = command_tx.clone();
        trim_row.connect_active_notify(move |row| {
            save(phodder_core::settings::TRIM_SILENCE, flag(row.is_active()));
            let _ = command_tx.send(UiCommand::ApplyPodcastPlayback(None));
        });
    }
    playback.add(&trim_row);
    page.add(&playback);

    // Storage defaults
    let storage = libadwaita::PreferencesGroup::builder()
        .title(gettext("Storage"))
        .description(gettext("Defaults for podcasts that don't set their own"))
        .build();
    let delete_after_play_row = libadwaita::SwitchRow::builder()
        .title(gettext("Delete episodes after playing"))
        .active(phodder_core::settings::get_setting_bool(&conn, "default_delete_after_play").unwrap_or(false))
        .build();
    {
        let save = save.clone();
        delete_after_play_row.connect_active_notify(move |row| {
            save("default_delete_after_play", Some(if row.is_active() { "true" } else { "false" }.to_string()));
        });
    }
    storage.add(&delete_after_play_row);
    let size_limit_bytes = get_i64("default_size_limit_bytes");
    storage.add(&limit_row(
        &gettext("Limit total download size"),
        &gettext("Megabytes"),
        size_limit_bytes.map(|b| b as f64 / 1_000_000.0),
        500.0,
        999_999.0,
        {
            let save = save.clone();
            move |megabytes| save("default_size_limit_bytes", megabytes.map(|mb| ((mb * 1_000_000.0) as i64).to_string()))
        },
    ));
    storage.add(&limit_row(
        &gettext("Limit downloaded episodes"),
        &gettext("Episodes"),
        get_i64("default_episode_count_limit").map(|n| n as f64),
        50.0,
        9999.0,
        {
            let save = save.clone();
            move |count| save("default_episode_count_limit", count.map(|n| (n as i64).to_string()))
        },
    ));
    // Applies when a podcast is added; each podcast then has its own.
    let current_expiry = get_i64("default_new_subscription_expiry_minutes").unwrap_or(10080);
    let mut expiry_choices: Vec<i64> = EXPIRY_CHOICES_MINUTES.to_vec();
    if !expiry_choices.contains(&current_expiry) {
        expiry_choices.push(current_expiry);
        expiry_choices.sort_unstable();
    }
    let expiry_labels: Vec<String> = expiry_choices.iter().map(|m| duration_label(*m)).collect();
    let expiry_row = libadwaita::ComboRow::builder()
        .title(gettext("Delete episodes after"))
        .subtitle(gettext("Starting value for new podcasts"))
        .model(&gtk4::StringList::new(&expiry_labels.iter().map(String::as_str).collect::<Vec<_>>()))
        .selected(expiry_choices.iter().position(|m| *m == current_expiry).unwrap_or(0) as u32)
        .build();
    {
        let save = save.clone();
        expiry_row.connect_selected_notify(move |row| {
            if let Some(minutes) = expiry_choices.get(row.selected() as usize) {
                save("default_new_subscription_expiry_minutes", Some(minutes.to_string()));
            }
        });
    }
    storage.add(&expiry_row);
    page.add(&storage);

    // Podcast directories
    let directories = libadwaita::PreferencesGroup::builder()
        .title(gettext("Podcast Directories"))
        .description(gettext("Searched when adding a podcast"))
        .build();
    // Directory names aren't translated.
    for (label, key) in [("Apple Podcasts", crate::discovery::APPLE_SETTING), ("gpodder.net", crate::discovery::GPODDER_SETTING)] {
        let row = libadwaita::SwitchRow::builder().title(label).active(get_flag(key, true)).build();
        let save = save.clone();
        row.connect_active_notify(move |row| save(key, flag(row.is_active())));
        directories.add(&row);
    }
    directories.add(&build_podcast_index_row(&db_path, command_tx, &toast_overlay, get_flag(crate::discovery::PODCAST_INDEX_SETTING, true), save.clone()));
    page.add(&directories);

    wire_background_switches(&background_row, &autostart_row, command_tx, &toast_overlay);

    toast_overlay.set_child(Some(&page));
    present_dialog_adaptively(&toast_overlay, &gettext("Settings"), 480, 640, parent, dialog_sheet, true);
}

/// A row with a number. Not AdwSpinRow: screen readers couldn't reach it
/// (missing from the accessibility tree, GTK 4.20 / libadwaita 1.8).
/// Scrolling past it doesn't change the value.
fn spin_row(title: &str, min: f64, max: f64, value: f64) -> (libadwaita::ActionRow, gtk4::SpinButton) {
    let spin_button = gtk4::SpinButton::with_range(min, max, 1.0);
    spin_button.set_value(value);
    spin_button.set_valign(gtk4::Align::Center);
    disable_spin_button_scroll(&spin_button);
    set_accessible_label(&spin_button, title);
    let row = libadwaita::ActionRow::builder().title(title).activatable_widget(&spin_button).build();
    row.add_suffix(&spin_button);
    (row, spin_button)
}

/// Names an expander's on/off switch by its title alone. libadwaita labels
/// it by the expanded rows too, so it read "Limit total download size
/// Megabytes". The relation outranks the label property, so it's removed.
fn name_enable_switch(expander: &libadwaita::ExpanderRow, title: &str) {
    fn first_switch(widget: &gtk4::Widget) -> Option<gtk4::Switch> {
        let mut child = widget.first_child();
        while let Some(current) = child {
            if let Ok(switch) = current.clone().downcast::<gtk4::Switch>() {
                return Some(switch);
            }
            if let Some(found) = first_switch(&current) {
                return Some(found);
            }
            child = current.next_sibling();
        }
        None
    }
    if let Some(switch) = first_switch(expander.upcast_ref()) {
        switch.reset_relation(gtk4::AccessibleRelation::LabelledBy);
        set_accessible_label(&switch, title);
    }
}

/// An optional limit: the expander's switch turns it on, and its value row
/// sets it. `on_change` gets `None` when it's off.
fn limit_row(
    title: &str,
    value_title: &str,
    current: Option<f64>,
    default: f64,
    max: f64,
    on_change: impl Fn(Option<f64>) + 'static,
) -> libadwaita::ExpanderRow {
    let expander = libadwaita::ExpanderRow::builder()
        .title(title)
        .show_enable_switch(true)
        .enable_expansion(current.is_some())
        .expanded(current.is_some())
        .build();
    name_enable_switch(&expander, title);
    let (value_row, value_spin) = spin_row(value_title, 1.0, max, current.unwrap_or(default));
    expander.add_row(&value_row);
    let on_change = std::rc::Rc::new(on_change);
    {
        let on_change = on_change.clone();
        let value_spin = value_spin.clone();
        expander.connect_enable_expansion_notify(move |expander| {
            let enabled = expander.enables_expansion();
            if enabled {
                expander.set_expanded(true);
            }
            on_change(enabled.then(|| value_spin.value()));
        });
    }
    {
        let expander = expander.clone();
        value_spin.connect_value_changed(move |spin| {
            if expander.enables_expansion() {
                on_change(Some(spin.value()));
            }
        });
    }
    expander
}

/// Run in Background and Launch on Startup go through the background
/// portal, which can refuse; a refusal flips the switch back.
fn wire_background_switches(
    background_row: &libadwaita::SwitchRow,
    autostart_row: &libadwaita::SwitchRow,
    command_tx: &UnboundedSender<UiCommand>,
    toast_overlay: &libadwaita::ToastOverlay,
) {
    {
        let command_tx = command_tx.clone();
        let toast_overlay = toast_overlay.clone();
        let autostart_row = autostart_row.clone();
        // Set while the switch is flipped back after a refusal.
        let reverting = std::rc::Rc::new(std::cell::Cell::new(false));
        background_row.connect_active_notify(move |row| {
            if reverting.get() {
                return;
            }
            let enabled = row.is_active();
            if !enabled {
                // Launch on Startup only makes sense while running in the
                // background, so it's turned off first.
                autostart_row.set_active(false);
                autostart_row.set_sensitive(false);
            }
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            if command_tx.send(UiCommand::SetRunInBackground { enabled, reply_tx }).is_err() {
                return;
            }
            let row = row.clone();
            let toast_overlay = toast_overlay.clone();
            let reverting = reverting.clone();
            let autostart_row = autostart_row.clone();
            gtk4::glib::spawn_future_local(async move {
                let message = match reply_rx.await {
                    Ok(Ok(true)) => {
                        autostart_row.set_sensitive(enabled);
                        return;
                    }
                    Ok(Ok(false)) => gettext("Running in the background wasn't allowed"),
                    _ => gettext("Couldn't update Run in Background"),
                };
                toast_overlay.add_toast(libadwaita::Toast::new(&message));
                reverting.set(true);
                row.set_active(!enabled);
                reverting.set(false);
            });
        });
    }
    let command_tx = command_tx.clone();
    let toast_overlay = toast_overlay.clone();
    let reverting = std::rc::Rc::new(std::cell::Cell::new(false));
    autostart_row.connect_active_notify(move |row| {
        if reverting.get() {
            return;
        }
        let enabled = row.is_active();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if command_tx.send(UiCommand::SetAutostart { enabled, reply_tx }).is_err() {
            return;
        }
        let row = row.clone();
        let toast_overlay = toast_overlay.clone();
        let reverting = reverting.clone();
        gtk4::glib::spawn_future_local(async move {
            let message = match reply_rx.await {
                Ok(Ok(true)) => return,
                Ok(Ok(false)) => gettext("Launch on Startup wasn't allowed"),
                _ => gettext("Couldn't update Launch on Startup"),
            };
            toast_overlay.add_toast(libadwaita::Toast::new(&message));
            reverting.set(true);
            row.set_active(!enabled);
            reverting.set(false);
        });
    });
}

/// Podcast Index needs the user's own free key and secret; it joins Add
/// Podcast's directories once both are saved. Save stores the secret in the
/// keyring; Remove deletes both. The expander's switch turns the directory
/// on and off and keeps the saved key.
fn build_podcast_index_row(
    db_path: &std::path::Path,
    command_tx: &UnboundedSender<UiCommand>,
    toast_overlay: &libadwaita::ToastOverlay,
    enabled: bool,
    save: std::rc::Rc<dyn Fn(&str, Option<String>)>,
) -> libadwaita::ExpanderRow {
    let saved_key = phodder_core::open_connection(db_path).ok().as_ref().and_then(crate::discovery::podcast_index_key);
    let expander = libadwaita::ExpanderRow::builder()
        .title("Podcast Index")
        .subtitle(gettext("Uses your own free key from api.podcastindex.org"))
        .show_enable_switch(true)
        .enable_expansion(enabled)
        .build();
    name_enable_switch(&expander, "Podcast Index");
    expander.connect_enable_expansion_notify(move |expander| {
        save(crate::discovery::PODCAST_INDEX_SETTING, Some(if expander.enables_expansion() { "1" } else { "0" }.to_string()));
    });

    let key_row = libadwaita::EntryRow::builder().title(gettext("API key")).text(saved_key.clone().unwrap_or_default()).build();
    // The saved secret stays in the keyring and isn't shown.
    let secret_title = |saved: bool| if saved { gettext("API secret (saved)") } else { gettext("API secret") };
    let secret_row = libadwaita::PasswordEntryRow::builder().title(secret_title(saved_key.is_some())).build();
    let save_button = gtk4::Button::builder().label(gettext("Save")).css_classes(["suggested-action"]).valign(gtk4::Align::Center).build();
    let remove_button =
        gtk4::Button::builder().label(gettext("Remove")).css_classes(["destructive-action"]).valign(gtk4::Align::Center).visible(saved_key.is_some()).build();
    let buttons_row = libadwaita::ActionRow::new();
    buttons_row.add_suffix(&remove_button);
    buttons_row.add_suffix(&save_button);
    expander.add_row(&key_row);
    expander.add_row(&secret_row);
    expander.add_row(&buttons_row);

    let send = {
        let command_tx = command_tx.clone();
        let toast_overlay = toast_overlay.clone();
        let remove_button = remove_button.clone();
        let secret_row = secret_row.clone();
        move |credentials: Option<(String, zeroize::Zeroizing<String>)>| {
            let saving = credentials.is_some();
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            if command_tx.send(UiCommand::SetPodcastIndexCredentials { credentials, reply_tx }).is_err() {
                toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't reach the backend")));
                return;
            }
            let toast_overlay = toast_overlay.clone();
            let remove_button = remove_button.clone();
            let secret_row = secret_row.clone();
            gtk4::glib::spawn_future_local(async move {
                match reply_rx.await {
                    Ok(Ok(())) => {
                        secret_row.set_text("");
                        secret_row.set_title(&secret_title(saving));
                        remove_button.set_visible(saving);
                        toast_overlay.add_toast(libadwaita::Toast::new(&if saving { gettext("Podcast Index key saved") } else { gettext("Podcast Index key removed") }));
                    }
                    Ok(Err(e)) => {
                        tracing::error!("failed to update Podcast Index credentials: {e}");
                        toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't save the key to the keyring")));
                    }
                    Err(_) => toast_overlay.add_toast(libadwaita::Toast::new(&gettext("The backend didn't respond"))),
                }
            });
        }
    };
    {
        let send = send.clone();
        let key_row = key_row.clone();
        let secret_row = secret_row.clone();
        let toast_overlay = toast_overlay.clone();
        save_button.connect_clicked(move |_| {
            let key = key_row.text().trim().to_string();
            let secret = zeroize::Zeroizing::new(secret_row.text().trim().to_string());
            if key.is_empty() || secret.is_empty() {
                toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Enter both the key and the secret")));
                return;
            }
            send(Some((key, secret)));
        });
    }
    remove_button.connect_clicked(move |_| {
        key_row.set_text("");
        send(None);
    });
    expander
}
