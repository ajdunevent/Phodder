use super::*;

/// Rebuilds the Manage Groups list. A free function so each row's buttons can
/// trigger a rebuild after renaming, deleting or reordering.
pub(super) fn rebuild_groups_list(list_box: &gtk4::ListBox, db_path: &std::path::Path, toast_overlay: &libadwaita::ToastOverlay, command_tx: &UnboundedSender<UiCommand>) {
    while let Some(child) = list_box.first_child() {
        list_box.remove(&child);
    }

    let conn = match phodder_core::open_connection(db_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("failed to open db to list groups: {e}");
            return;
        }
    };
    let groups = match phodder_core::groups::list_groups(&conn) {
        Ok(g) => g,
        Err(e) => {
            tracing::error!("failed to list groups: {e}");
            return;
        }
    };
    drop(conn);

    let group_count = groups.len();
    for (index, group) in groups.iter().enumerate() {
        // The name is edited in place; Enter or the row's apply button saves it.
        let row = libadwaita::EntryRow::builder().title(gettext("Name")).text(&group.name).show_apply_button(true).build();
        {
            let db_path = db_path.to_path_buf();
            let list_box = list_box.clone();
            let toast_overlay = toast_overlay.clone();
            let command_tx = command_tx.clone();
            let group_id = group.id;
            row.connect_apply(move |row| {
                let new_name = row.text().trim().to_string();
                if new_name.is_empty() {
                    return;
                }
                match phodder_core::open_connection(&db_path).and_then(|conn| phodder_core::groups::rename_group(&conn, group_id, &new_name)) {
                    Ok(()) => {
                        rebuild_groups_list(&list_box, &db_path, &toast_overlay, &command_tx);
                        if let Err(e) = command_tx.send(UiCommand::NotifyDataChanged) {
                            tracing::error!("failed to send NotifyDataChanged after group rename: {e}");
                        }
                    }
                    Err(e) => {
                        tracing::error!("failed to rename group {group_id}: {e}");
                        toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't rename group")));
                    }
                }
            });
        }
        let initial_rgba = group
            .color
            .as_deref()
            .and_then(|c| gtk4::gdk::RGBA::parse(c).ok())
            // Gray for a group with no color; transparent looked like a
            // rendering bug.
            .unwrap_or(gtk4::gdk::RGBA::new(0.6, 0.6, 0.6, 1.0));
        let color_button = color_button(&initial_rgba, &gettext("Group color"));
        {
            let db_path = db_path.to_path_buf();
            let toast_overlay = toast_overlay.clone();
            let command_tx = command_tx.clone();
            let group_id = group.id;
            color_button.connect_rgba_notify(move |button| {
                let color_str = button.rgba().to_str();
                match phodder_core::open_connection(&db_path)
                    .and_then(|conn| phodder_core::groups::set_group_color(&conn, group_id, Some(&color_str)))
                {
                    Ok(()) => {
                        if let Err(e) = command_tx.send(UiCommand::NotifyDataChanged) {
                            tracing::error!("failed to send NotifyDataChanged after group color change: {e}");
                        }
                    }
                    Err(e) => {
                        tracing::error!("failed to set color for group {group_id}: {e}");
                        toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't set group color")));
                    }
                }
                // No rebuild here: it would destroy this button and its open
                // color popover mid-interaction.
            });
        }
        row.add_prefix(&color_button);

        let icon_button = |icon: &str, tooltip: &str, sensitive: bool| {
            let button = gtk4::Button::builder().icon_name(icon).tooltip_text(tooltip).valign(gtk4::Align::Center).sensitive(sensitive).build();
            button.add_css_class("flat");
            button
        };
        let move_up_button = icon_button("go-up-symbolic", &gettext("Move up"), index != 0);
        let move_down_button = icon_button("go-down-symbolic", &gettext("Move down"), index != group_count - 1);
        for (button, direction) in [(&move_up_button, -1i32), (&move_down_button, 1i32)] {
            let db_path = db_path.to_path_buf();
            let list_box = list_box.clone();
            let toast_overlay = toast_overlay.clone();
            let command_tx = command_tx.clone();
            let group_id = group.id;
            button.connect_clicked(move |_| {
                match phodder_core::open_connection(&db_path).and_then(|conn| phodder_core::groups::move_group(&conn, group_id, direction)) {
                    Ok(()) => {
                        rebuild_groups_list(&list_box, &db_path, &toast_overlay, &command_tx);
                        if let Err(e) = command_tx.send(UiCommand::NotifyDataChanged) {
                            tracing::error!("failed to send NotifyDataChanged after group reorder: {e}");
                        }
                    }
                    Err(e) => {
                        tracing::error!("failed to reorder group {group_id}: {e}");
                        toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't reorder group")));
                    }
                }
            });
        }
        row.add_suffix(&move_up_button);
        row.add_suffix(&move_down_button);

        // Default can't be deleted; disabled rather than hidden so the
        // action's absence is visible.
        let delete_button = icon_button("user-trash-symbolic", &gettext("Delete group"), group.id != phodder_core::groups::DEFAULT_GROUP_ID);
        {
            let db_path = db_path.to_path_buf();
            let list_box = list_box.clone();
            let toast_overlay = toast_overlay.clone();
            let command_tx = command_tx.clone();
            let group_id = group.id;
            delete_button.connect_clicked(move |_| {
                let result = phodder_core::open_connection(&db_path)
                    .map_err(phodder_core::groups::DeleteGroupError::Sqlite)
                    .and_then(|conn| phodder_core::groups::delete_group(&conn, group_id));
                match result {
                    Ok(()) => {
                        rebuild_groups_list(&list_box, &db_path, &toast_overlay, &command_tx);
                        if let Err(e) = command_tx.send(UiCommand::NotifyDataChanged) {
                            tracing::error!("failed to send NotifyDataChanged after group delete: {e}");
                        }
                    }
                    Err(e) => {
                        tracing::error!("failed to delete group {group_id}: {e:?}");
                        toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't delete group")));
                    }
                }
            });
        }
        row.add_suffix(&delete_button);
        list_box.append(&row);
    }

    // Adding is the list's last row.
    let new_row = libadwaita::EntryRow::builder().title(gettext("New group")).show_apply_button(true).build();
    let new_color_button = color_button(&gtk4::gdk::RGBA::new(0.6, 0.6, 0.6, 1.0), &gettext("New group's color"));
    new_row.add_prefix(&new_color_button);
    {
        let db_path = db_path.to_path_buf();
        let list_box = list_box.clone();
        let toast_overlay = toast_overlay.clone();
        let command_tx = command_tx.clone();
        new_row.connect_apply(move |row| {
            let name = row.text().trim().to_string();
            if name.is_empty() {
                return;
            }
            let color_str = new_color_button.rgba().to_str();
            match phodder_core::open_connection(&db_path)
                .and_then(|conn| phodder_core::groups::create_group(&conn, &name).and_then(|id| phodder_core::groups::set_group_color(&conn, id, Some(&color_str))))
            {
                Ok(()) => {
                    rebuild_groups_list(&list_box, &db_path, &toast_overlay, &command_tx);
                    if let Err(e) = command_tx.send(UiCommand::NotifyDataChanged) {
                        tracing::error!("failed to send NotifyDataChanged after group creation: {e}");
                    }
                }
                Err(e) => {
                    tracing::error!("failed to create group: {e}");
                    toast_overlay.add_toast(libadwaita::Toast::new(&gettext("Couldn't create group")));
                }
            }
        });
    }
    list_box.append(&new_row);
}

fn color_button(rgba: &gtk4::gdk::RGBA, label: &str) -> gtk4::ColorDialogButton {
    let button = gtk4::ColorDialogButton::builder().dialog(&gtk4::ColorDialog::new()).rgba(rgba).valign(gtk4::Align::Center).tooltip_text(label).build();
    set_accessible_label(&button, label);
    button
}

/// `on_close` runs once when the dialog closes, however it closes. Manage
/// Subscription passes it to reopen itself with the updated groups; the main
/// menu passes `None`.

pub(super) fn show_manage_groups_dialog(
    parent: &libadwaita::ApplicationWindow,
    db_path: std::path::PathBuf,
    dialog_sheet: &libadwaita::BottomSheet,
    on_close: Option<std::rc::Rc<dyn Fn()>>,
    command_tx: UnboundedSender<UiCommand>,
) {
    let toast_overlay = libadwaita::ToastOverlay::new();
    let list_box = gtk4::ListBox::builder().selection_mode(gtk4::SelectionMode::None).css_classes(["boxed-list"]).build();
    rebuild_groups_list(&list_box, &db_path, &toast_overlay, &command_tx);
    let group = libadwaita::PreferencesGroup::builder().description(gettext("Episodes from higher groups play first")).build();
    group.add(&list_box);
    let page = libadwaita::PreferencesPage::new();
    page.add(&group);
    toast_overlay.set_child(Some(&page));

    let presentation = present_dialog_adaptively(&toast_overlay, &gettext("Manage Groups"), 440, 480, parent, dialog_sheet, true);
    if let Some(on_close) = on_close {
        presentation.on_closed(move || on_close());
    }
}
