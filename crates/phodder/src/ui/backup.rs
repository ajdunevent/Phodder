//! Back Up Library and Restore Library, from the main menu.

use super::*;

pub(super) fn install_backup_actions(
    window: &libadwaita::ApplicationWindow,
    db_path: &std::path::Path,
    command_tx: &UnboundedSender<UiCommand>,
    toast_overlay: &libadwaita::ToastOverlay,
) {
    {
        let back_up = gtk4::gio::SimpleAction::new("back-up", None);
        window.add_action(&back_up);
        let window = window.clone();
        let db_path = db_path.to_path_buf();
        let toast_overlay = toast_overlay.clone();
        back_up.connect_activate(move |_, _| {
            let (window, db_path, toast_overlay) = (window.clone(), db_path.clone(), toast_overlay.clone());
            gtk4::glib::spawn_future_local(async move {
                let name = format!("phodder-backup-{}.json", chrono::Local::now().format("%Y-%m-%d"));
                let dialog = gtk4::FileDialog::builder().title(gettext("Back Up Library")).initial_name(name).build();
                let Ok(file) = dialog.save_future(Some(&window)).await else { return };
                let Some(path) = file.path() else {
                    toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't determine the selected file's path")));
                    return;
                };
                let result = gtk4::gio::spawn_blocking(move || -> anyhow::Result<()> {
                    let conn = phodder_core::open_connection(&db_path)?;
                    let json = phodder_core::backup::export(&conn, env!("CARGO_PKG_VERSION"))?;
                    std::fs::write(&path, json)?;
                    Ok(())
                })
                .await;
                match result {
                    Ok(Ok(())) => toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Library backed up"))),
                    Ok(Err(e)) => {
                        tracing::error!("backup failed: {e:#}");
                        toast_overlay.add_toast(libadwaita::Toast::new(&gettext_f("Backup failed: {error}", &[("error", &e.to_string())])));
                    }
                    Err(_) => toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Backup failed"))),
                }
            });
        });
    }
    {
        let restore = gtk4::gio::SimpleAction::new("restore", None);
        window.add_action(&restore);
        let window = window.clone();
        let command_tx = command_tx.clone();
        let toast_overlay = toast_overlay.clone();
        restore.connect_activate(move |_, _| {
            let (window, command_tx, toast_overlay) = (window.clone(), command_tx.clone(), toast_overlay.clone());
            gtk4::glib::spawn_future_local(async move {
                let filter = gtk4::FileFilter::new();
                filter.set_name(Some(&gettext("Phodder Backups")));
                filter.add_suffix("json");
                let filters = gtk4::gio::ListStore::new::<gtk4::FileFilter>();
                filters.append(&filter);
                let dialog = gtk4::FileDialog::builder().title(gettext("Restore Library")).filters(&filters).build();
                let Ok(file) = dialog.open_future(Some(&window)).await else { return };
                let Some(path) = file.path() else {
                    toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't determine the selected file's path")));
                    return;
                };
                let read = gtk4::gio::spawn_blocking(move || -> anyhow::Result<(String, usize, usize)> {
                    let json = std::fs::read_to_string(&path)?;
                    let (podcasts, episodes) = phodder_core::backup::inspect(&json)?;
                    Ok((json, podcasts, episodes))
                })
                .await;
                let (json, podcasts, episodes) = match read {
                    Ok(Ok(read)) => read,
                    Ok(Err(e)) => {
                        toast_overlay.add_toast(libadwaita::Toast::new(&gettext_f("Couldn't read the backup: {error}", &[("error", &e.to_string())])));
                        return;
                    }
                    Err(_) => return,
                };
                let podcasts = ngettext_f("{n} podcast", "{n} podcasts", podcasts as u32, &[("n", &podcasts.to_string())]);
                let episodes = ngettext_f("{n} episode", "{n} episodes", episodes as u32, &[("n", &episodes.to_string())]);
                let alert = libadwaita::AlertDialog::new(
                    Some(&gettext("Replace Everything?")),
                    Some(&gettext_f(
                        "Your podcasts, groups, queue, history, playback progress, and settings will be replaced by the backup's {podcasts} and {episodes}. This can't be undone.\n\nDownloads of episodes in the backup are kept; other downloads are deleted. Sync passwords aren't in backups and may need to be entered again.",
                        &[("podcasts", &podcasts), ("episodes", &episodes)],
                    )),
                );
                alert.add_responses(&[("cancel", &gettext("Cancel")), ("restore", &gettext("Replace Everything"))]);
                alert.set_response_appearance("restore", libadwaita::ResponseAppearance::Destructive);
                alert.set_default_response(Some("cancel"));
                alert.set_close_response("cancel");
                if alert.choose_future(Some(&window)).await != "restore" {
                    return;
                }
                // The service reads its own copy; a file chosen through the
                // portal may not be readable from its process.
                let pending = crate::data_dir().join("restore-pending.json");
                if let Err(e) = std::fs::write(&pending, json) {
                    toast_overlay.add_toast(libadwaita::Toast::new(&gettext_f("Restore failed: {error}", &[("error", &e.to_string())])));
                    return;
                }
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if command_tx.send(UiCommand::RestoreBackup { path: pending.to_string_lossy().into_owned(), reply_tx }).is_err() {
                    return;
                }
                let restoring = libadwaita::Toast::builder().title(gettext("Restoring…")).timeout(0).build();
                toast_overlay.add_toast(restoring.clone());
                let result = reply_rx.await;
                restoring.dismiss();
                match result {
                    Ok(Ok(())) => toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Library restored"))),
                    Ok(Err(e)) => toast_overlay.add_toast(libadwaita::Toast::new(&gettext_f("Restore failed: {error}", &[("error", &e)]))),
                    Err(_) => toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Restore failed"))),
                }
            });
        });
    }
}
