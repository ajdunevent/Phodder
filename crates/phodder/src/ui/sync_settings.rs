use super::*;

const GPODDER_NET_URL: &str = "https://gpodder.net";

/// Settings' Sync group: connecting to Nextcloud's GPodder Sync or
/// gpodder.net, then Sync Now and Disconnect. Everything acts on click:
/// signing in opens a browser, talks to the server and stores a credential,
/// none of which waits for or is undone by closing Settings.
#[derive(Clone)]
struct SyncRows {
    group: libadwaita::PreferencesGroup,
    /// "Not connected", sign-in progress, or "Connected as …".
    status: libadwaita::ActionRow,
    service: libadwaita::ComboRow,
    nextcloud_server: libadwaita::EntryRow,
    nextcloud_connect: libadwaita::ButtonRow,
    username: libadwaita::EntryRow,
    password: libadwaita::PasswordEntryRow,
    gpodder_server: libadwaita::EntryRow,
    gpodder_connect: libadwaita::ButtonRow,
    cancel: libadwaita::ButtonRow,
    sync_now: libadwaita::ButtonRow,
    disconnect: libadwaita::ButtonRow,
}

impl SyncRows {
    /// Progress happens away from the focused control, so it's announced.
    fn set_status(&self, text: &str) {
        self.status.set_title(text);
        if self.status.is_mapped() {
            announce(&self.status, text);
        }
    }

    fn gpodder_net_selected(&self) -> bool {
        self.service.selected() == 1
    }

    fn show_disconnected(&self) {
        self.set_status(&gettext("Not connected"));
        self.service.set_visible(true);
        for widget in [self.nextcloud_server.upcast_ref::<gtk4::Widget>(), self.nextcloud_connect.upcast_ref(), self.username.upcast_ref(), self.password.upcast_ref(), self.gpodder_server.upcast_ref(), self.gpodder_connect.upcast_ref()] {
            widget.set_sensitive(true);
        }
        self.show_service_fields();
        self.cancel.set_visible(false);
        self.sync_now.set_visible(false);
        self.disconnect.set_visible(false);
        self.disconnect.set_sensitive(true);
    }

    fn show_service_fields(&self) {
        let gpodder = self.gpodder_net_selected();
        self.nextcloud_server.set_visible(!gpodder);
        self.nextcloud_connect.set_visible(!gpodder);
        self.username.set_visible(gpodder);
        self.password.set_visible(gpodder);
        self.gpodder_server.set_visible(gpodder);
        self.gpodder_connect.set_visible(gpodder);
        self.group.set_description(Some(&if gpodder {
            gettext("Phodder joins your other gpodder.net devices, so they share subscriptions. Change the server only for a self-hosted copy.")
        } else {
            gettext("Uses the GPodder Sync app on your Nextcloud server.")
        }));
    }

    fn show_connected(&self, user: &str, server: Option<&str>) {
        self.set_status(&connected_as(user, server));
        for widget in [
            self.service.upcast_ref::<gtk4::Widget>(),
            self.nextcloud_server.upcast_ref(),
            self.nextcloud_connect.upcast_ref(),
            self.username.upcast_ref(),
            self.password.upcast_ref(),
            self.gpodder_server.upcast_ref(),
            self.gpodder_connect.upcast_ref(),
            self.cancel.upcast_ref(),
        ] {
            widget.set_visible(false);
        }
        self.group.set_description(Some(&gettext("Subscriptions and listening progress are shared with your other devices.")));
        self.sync_now.set_visible(true);
        self.sync_now.set_sensitive(true);
        self.disconnect.set_visible(true);
        self.disconnect.set_sensitive(true);
    }
}

/// Added after construction: the builder's css-classes replaces every class,
/// including the one that styles a button row, which then collapsed to a
/// sliver.
fn styled_button_row(title: &str, class: &str) -> libadwaita::ButtonRow {
    let row = libadwaita::ButtonRow::builder().title(title).build();
    row.add_css_class(class);
    row
}

pub(super) fn build_sync_group(
    parent: &libadwaita::ApplicationWindow,
    db_path: &std::path::Path,
    command_tx: &UnboundedSender<UiCommand>,
    toast_overlay: &libadwaita::ToastOverlay,
) -> libadwaita::PreferencesGroup {
    let group = libadwaita::PreferencesGroup::builder().title(gettext("Sync")).build();
    // Service names aren't translated.
    let rows = SyncRows {
        group: group.clone(),
        status: libadwaita::ActionRow::builder().title(gettext("Not connected")).build(),
        service: libadwaita::ComboRow::builder().title(gettext("Service")).model(&gtk4::StringList::new(&["Nextcloud", "gpodder.net"])).build(),
        nextcloud_server: libadwaita::EntryRow::builder().title(gettext("Nextcloud server address")).input_purpose(gtk4::InputPurpose::Url).build(),
        nextcloud_connect: styled_button_row(&gettext("Connect to Nextcloud"), "suggested-action"),
        username: libadwaita::EntryRow::builder().title(gettext("Username")).build(),
        password: libadwaita::PasswordEntryRow::builder().title(gettext("Password")).build(),
        gpodder_server: libadwaita::EntryRow::builder().title(gettext("Server")).text(GPODDER_NET_URL).input_purpose(gtk4::InputPurpose::Url).build(),
        gpodder_connect: styled_button_row(&gettext("Connect to gpodder.net"), "suggested-action"),
        cancel: libadwaita::ButtonRow::builder().title(gettext("Cancel")).build(),
        sync_now: libadwaita::ButtonRow::builder().title(gettext("Sync Now")).build(),
        disconnect: styled_button_row(&gettext("Disconnect"), "destructive-action"),
    };
    for widget in [
        rows.status.upcast_ref::<gtk4::Widget>(),
        rows.service.upcast_ref(),
        rows.nextcloud_server.upcast_ref(),
        rows.nextcloud_connect.upcast_ref(),
        rows.username.upcast_ref(),
        rows.password.upcast_ref(),
        rows.gpodder_server.upcast_ref(),
        rows.gpodder_connect.upcast_ref(),
        rows.cancel.upcast_ref(),
        rows.sync_now.upcast_ref(),
        rows.disconnect.upcast_ref(),
    ] {
        group.add(widget);
    }
    match current_account(db_path) {
        Some(account) => rows.show_connected(&account.login_name, account.server_url.as_deref()),
        None => rows.show_disconnected(),
    }
    {
        let rows = rows.clone();
        rows.service.clone().connect_selected_notify(move |_| rows.show_service_fields());
    }

    wire_nextcloud(&rows, parent, command_tx, toast_overlay);
    wire_gpodder_net(&rows, command_tx, toast_overlay);
    wire_disconnect(&rows, db_path, command_tx, toast_overlay);
    wire_sync_now(&rows, db_path, command_tx, toast_overlay);
    group
}

fn wire_nextcloud(rows: &SyncRows, parent: &libadwaita::ApplicationWindow, command_tx: &UnboundedSender<UiCommand>, toast_overlay: &libadwaita::ToastOverlay) {
    let rows = rows.clone();
    let parent = parent.clone();
    let command_tx = command_tx.clone();
    let toast_overlay = toast_overlay.clone();
    rows.nextcloud_connect.clone().connect_activated(move |_| {
        let server_url = rows.nextcloud_server.text().trim().to_string();
        if server_url.is_empty() {
            toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Enter a server URL first")));
            return;
        }
        rows.nextcloud_connect.set_sensitive(false);
        rows.nextcloud_server.set_sensitive(false);
        rows.service.set_sensitive(false);
        rows.set_status(&gettext("Starting sign-in…"));
        rows.cancel.set_visible(true);

        // Arc<AtomicBool>: it crosses into the Tokio task polling the login.
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cancel_handler = {
            let cancelled = cancelled.clone();
            rows.cancel.connect_activated(move |_| cancelled.store(true, std::sync::atomic::Ordering::Relaxed))
        };

        let rows = rows.clone();
        let parent = parent.clone();
        let command_tx = command_tx.clone();
        let toast_overlay = toast_overlay.clone();
        gtk4::glib::spawn_future_local(async move {
            let fail = |message: Option<String>| {
                if let Some(message) = message {
                    toast_overlay.add_toast(libadwaita::Toast::new(&message));
                }
                rows.service.set_sensitive(true);
                rows.show_disconnected();
            };
            let result = async {
                // Steps 1, 3 and 4 run on the Tokio runtime via
                // run_command_loop: reqwest and oo7 need it, and calling them
                // on the GLib loop panicked with "no reactor running".
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                command_tx
                    .send(UiCommand::StartNextcloudLogin { server_url: server_url.clone(), reply_tx })
                    .map_err(|_| Some(gettext("Couldn't reach the backend to start sign-in")))?;
                let flow = match reply_rx.await {
                    Ok(Ok(flow)) => flow,
                    Ok(Err(e)) => return Err(Some(gettext_f("Couldn't start sign-in: {error}", &[("error", &e.to_string())]))),
                    Err(_) => return Err(Some(gettext("Sign-in failed: the backend didn't respond"))),
                };

                // Step 2 stays on the GTK thread: UriLauncher is GTK-only.
                // If the browser doesn't open, polling continues and the
                // user can open the address themselves.
                if let Err(e) = gtk4::UriLauncher::new(&flow.login_url).launch_future(Some(&parent)).await {
                    tracing::warn!("failed to open browser for Nextcloud sign-in: {e}");
                    toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't open your browser automatically for sign-in")));
                }
                rows.set_status(&gettext("Waiting for you to finish signing in in your browser…"));

                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                command_tx
                    .send(UiCommand::PollNextcloudLogin { flow, cancelled: cancelled.clone(), reply_tx })
                    .map_err(|_| Some(gettext("Couldn't reach the backend to finish sign-in")))?;
                let login_result = match reply_rx.await {
                    Ok(Ok(result)) => result,
                    Ok(Err(crate::nextcloud_login::LoginFlowError::Cancelled)) => return Err(None),
                    Ok(Err(e)) => return Err(Some(gettext_f("Sign-in failed: {error}", &[("error", &e.to_string())]))),
                    Err(_) => return Err(Some(gettext("Sign-in failed: the backend didn't respond"))),
                };

                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                command_tx
                    .send(UiCommand::CompleteNextcloudConnection { login_result: login_result.clone(), reply_tx })
                    .map_err(|_| Some(gettext("Couldn't reach the backend to finish connecting")))?;
                match reply_rx.await {
                    Ok(Ok(())) => Ok(login_result),
                    Ok(Err(e)) => {
                        tracing::error!("failed to save sync account after successful sign-in: {e}");
                        Err(Some(gettext_f("Signed in, but couldn't finish connecting: {error}", &[("error", &e.to_string())])))
                    }
                    Err(_) => Err(Some(gettext("Signed in, but the backend didn't respond"))),
                }
            }
            .await;
            rows.cancel.disconnect(cancel_handler);
            match result {
                Ok(login_result) => {
                    rows.service.set_sensitive(true);
                    rows.show_connected(&login_result.login_name, Some(&login_result.server));
                    toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Connected to Nextcloud")));
                }
                Err(message) => fail(message),
            }
        });
    });
}

/// gpodder.net's sign-in: server (for self-hosted copies), username and
/// password. Connecting registers this device and links it to the
/// account's other devices.
fn wire_gpodder_net(rows: &SyncRows, command_tx: &UnboundedSender<UiCommand>, toast_overlay: &libadwaita::ToastOverlay) {
    let submit = {
        let rows = rows.clone();
        let command_tx = command_tx.clone();
        let toast_overlay = toast_overlay.clone();
        std::rc::Rc::new(move || {
            let username = rows.username.text().trim().to_string();
            let password = zeroize::Zeroizing::new(rows.password.text().to_string());
            let server = rows.gpodder_server.text().trim().trim_end_matches('/').to_string();
            if username.is_empty() || password.is_empty() || server.is_empty() {
                toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Enter your username and password")));
                return;
            }
            let inputs = [
                rows.service.upcast_ref::<gtk4::Widget>(),
                rows.username.upcast_ref(),
                rows.password.upcast_ref(),
                rows.gpodder_server.upcast_ref(),
                rows.gpodder_connect.upcast_ref(),
            ];
            for widget in inputs {
                widget.set_sensitive(false);
            }
            rows.set_status(&gettext("Connecting…"));
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            if command_tx.send(UiCommand::ConnectGpodderNet { server: server.clone(), username: username.clone(), password, reply_tx }).is_err() {
                toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't reach the backend")));
                rows.show_disconnected();
                return;
            }
            let rows = rows.clone();
            let toast_overlay = toast_overlay.clone();
            gtk4::glib::spawn_future_local(async move {
                let result = reply_rx.await.unwrap_or_else(|_| Err(gettext("the backend didn't respond")));
                rows.service.set_sensitive(true);
                match result {
                    Ok(()) => {
                        rows.password.set_text("");
                        rows.show_connected(&username, Some(&server));
                        toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Connected to gpodder.net")));
                    }
                    Err(message) => {
                        rows.show_disconnected();
                        toast_overlay.add_toast(libadwaita::Toast::new(&message));
                    }
                }
            });
        })
    };
    {
        let submit = submit.clone();
        rows.gpodder_connect.connect_activated(move |_| submit());
    }
    rows.password.connect_entry_activated(move |_| submit());
}

fn wire_disconnect(rows: &SyncRows, db_path: &std::path::Path, command_tx: &UnboundedSender<UiCommand>, toast_overlay: &libadwaita::ToastOverlay) {
    let rows = rows.clone();
    let db_path = db_path.to_path_buf();
    let command_tx = command_tx.clone();
    let toast_overlay = toast_overlay.clone();
    rows.disconnect.clone().connect_activated(move |row| {
        let Some(account) = current_account(&db_path) else { return };
        row.set_sensitive(false);
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if command_tx.send(UiCommand::DisconnectSyncAccount { account, reply_tx }).is_err() {
            toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't reach the backend to disconnect")));
            row.set_sensitive(true);
            return;
        }
        let rows = rows.clone();
        let toast_overlay = toast_overlay.clone();
        gtk4::glib::spawn_future_local(async move {
            match reply_rx.await {
                Ok(Ok(())) => {
                    rows.show_disconnected();
                    toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Disconnected")));
                }
                Ok(Err(e)) => {
                    tracing::error!("failed to remove sync account: {e}");
                    toast_overlay.add_toast(libadwaita::Toast::new(&gettext_f("Couldn't remove the account: {error}", &[("error", &e.to_string())])));
                    rows.disconnect.set_sensitive(true);
                }
                Err(_) => {
                    toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't remove the account: the backend didn't respond")));
                    rows.disconnect.set_sensitive(true);
                }
            }
        });
    });
}

fn wire_sync_now(rows: &SyncRows, db_path: &std::path::Path, command_tx: &UnboundedSender<UiCommand>, toast_overlay: &libadwaita::ToastOverlay) {
    let rows = rows.clone();
    let db_path = db_path.to_path_buf();
    let command_tx = command_tx.clone();
    let toast_overlay = toast_overlay.clone();
    rows.sync_now.clone().connect_activated(move |row| {
        row.set_sensitive(false);
        // A first sync fetches a preview for every remotely added feed and
        // can take a while; without this, Sync Now looked like it did
        // nothing.
        rows.set_status(&gettext("Syncing…"));
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let sent = command_tx.send(UiCommand::SyncSubscriptionsNow { reply_tx }).is_ok();
        let rows = rows.clone();
        let db_path = db_path.clone();
        let command_tx = command_tx.clone();
        let toast_overlay = toast_overlay.clone();
        gtk4::glib::spawn_future_local(async move {
            let message = if !sent {
                gettext("Couldn't reach the backend to sync")
            } else {
                match reply_rx.await {
                    Ok(Ok(())) => {
                        // Sync can add or remove subscriptions.
                        let _ = command_tx.send(UiCommand::NotifyDataChanged);
                        gettext("Subscriptions synced")
                    }
                    Ok(Err(e)) => {
                        tracing::error!("subscription sync failed: {e}");
                        gettext_f("Sync failed: {error}", &[("error", &e.to_string())])
                    }
                    Err(_) => gettext("Sync failed: the backend didn't respond"),
                }
            };
            toast_overlay.add_toast(libadwaita::Toast::new(&message));
            match current_account(&db_path) {
                Some(account) => rows.show_connected(&account.login_name, account.server_url.as_deref()),
                None => rows.show_disconnected(),
            }
        });
    });
}

/// The connected account, read when it's needed: Settings can connect or
/// disconnect one after it opens.
fn current_account(db_path: &std::path::Path) -> Option<phodder_core::sync_accounts::SyncAccount> {
    phodder_core::open_connection(db_path).ok().and_then(|conn| phodder_core::sync_accounts::get_enabled_sync_account(&conn).ok().flatten())
}

/// "Connected as {user} on {server}".
fn connected_as(user: &str, server: Option<&str>) -> String {
    let unknown = gettext("(unknown server)");
    gettext_f("Connected as {user} on {server}", &[("user", user), ("server", server.unwrap_or(&unknown))])
}
