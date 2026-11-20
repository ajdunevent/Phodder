//! Group management: list, create, rename, delete, reorder and color.

use rusqlite::{params, Connection, OptionalExtension};

#[derive(Debug, Clone)]
pub struct Group {
    pub id: i64,
    pub name: String,
    pub sort_order: f64,
    /// A CSS color string; see schema.sql.
    pub color: Option<String>,
}

/// New subscriptions land here, and a deleted group's subscriptions move
/// here, so it can't be deleted.
pub const DEFAULT_GROUP_ID: i64 = 1;

pub fn list_groups(conn: &Connection) -> rusqlite::Result<Vec<Group>> {
    let mut stmt = conn.prepare("SELECT id, name, sort_order, color FROM groups ORDER BY sort_order ASC")?;
    let rows = stmt.query_map([], |row| {
        Ok(Group { id: row.get(0)?, name: row.get(1)?, sort_order: row.get(2)?, color: row.get(3)? })
    })?;
    rows.collect()
}

/// `None` clears the color.
pub fn set_group_color(conn: &Connection, group_id: i64, color: Option<&str>) -> rusqlite::Result<()> {
    conn.execute("UPDATE groups SET color = ?1 WHERE id = ?2", params![color, group_id])?;
    Ok(())
}

/// Appends after every existing group. 1.0 if the table is somehow empty.
pub fn create_group(conn: &Connection, name: &str) -> rusqlite::Result<i64> {
    let max_sort_order: Option<f64> = conn.query_row("SELECT MAX(sort_order) FROM groups", [], |row| row.get(0)).optional()?.flatten();
    let new_sort_order = max_sort_order.map(|m| m + 1.0).unwrap_or(1.0);
    conn.execute("INSERT INTO groups (name, sort_order) VALUES (?1, ?2)", params![name, new_sort_order])?;
    Ok(conn.last_insert_rowid())
}

pub fn rename_group(conn: &Connection, group_id: i64, new_name: &str) -> rusqlite::Result<()> {
    conn.execute("UPDATE groups SET name = ?1 WHERE id = ?2", params![new_name, group_id])?;
    Ok(())
}

#[derive(Debug)]
pub enum DeleteGroupError {
    CannotDeleteDefaultGroup,
    Sqlite(rusqlite::Error),
}

impl From<rusqlite::Error> for DeleteGroupError {
    fn from(e: rusqlite::Error) -> Self {
        DeleteGroupError::Sqlite(e)
    }
}

/// Reassigns the group's subscriptions to Default first. Refuses to delete
/// Default with an error rather than a silent no-op.
pub fn delete_group(conn: &Connection, group_id: i64) -> Result<(), DeleteGroupError> {
    if group_id == DEFAULT_GROUP_ID {
        return Err(DeleteGroupError::CannotDeleteDefaultGroup);
    }
    conn.execute("UPDATE subscriptions SET group_id = ?1 WHERE group_id = ?2", params![DEFAULT_GROUP_ID, group_id])?;
    conn.execute("DELETE FROM groups WHERE id = ?1", params![group_id])?;
    Ok(())
}

/// Moves a group up (-1) or down (1) past its neighbor by updating only this
/// row's fractional sort_order. A no-op at either edge or for an unknown
/// group.
pub fn move_group(conn: &Connection, group_id: i64, direction: i32) -> rusqlite::Result<()> {
    let groups = list_groups(conn)?;
    let Some(index) = groups.iter().position(|g| g.id == group_id) else { return Ok(()) };
    let new_index = index as i32 + direction;
    if new_index < 0 || new_index as usize >= groups.len() {
        return Ok(());
    }
    let new_index = new_index as usize;
    let neighbor = &groups[new_index];
    // The new value sits between the neighbor and the group beyond it.
    let far_neighbor_sort_order = if direction < 0 {
        new_index.checked_sub(1).map(|i| groups[i].sort_order)
    } else {
        groups.get(new_index + 1).map(|g| g.sort_order)
    };
    let new_sort_order = match far_neighbor_sort_order {
        Some(far) => (far + neighbor.sort_order) / 2.0,
        // The neighbor is at an edge: go one unit past it, as create_group
        // does.
        None => neighbor.sort_order + direction as f64,
    };
    conn.execute("UPDATE groups SET sort_order = ?1 WHERE id = ?2", params![new_sort_order, group_id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{migrate, open_connection};

    fn setup() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_connection(&dir.path().join("test.db")).unwrap();
        migrate(&conn).unwrap();
        (dir, conn)
    }

    #[test]
    fn list_groups_includes_the_seeded_default_group() {
        let (_dir, conn) = setup();
        let groups = list_groups(&conn).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].id, DEFAULT_GROUP_ID);
        assert_eq!(groups[0].name, "Default");
    }

    #[test]
    fn create_group_appends_after_existing_groups() {
        let (_dir, conn) = setup();
        let new_id = create_group(&conn, "Later").unwrap();
        let groups = list_groups(&conn).unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[1].id, new_id);
        assert_eq!(groups[1].name, "Later");
        assert!(groups[1].sort_order > groups[0].sort_order);
    }

    #[test]
    fn rename_group_updates_the_name_only() {
        let (_dir, conn) = setup();
        let new_id = create_group(&conn, "Later").unwrap();
        rename_group(&conn, new_id, "Someday").unwrap();
        let groups = list_groups(&conn).unwrap();
        assert_eq!(groups[1].name, "Someday");
    }

    #[test]
    fn a_new_group_starts_uncolored() {
        let (_dir, conn) = setup();
        let new_id = create_group(&conn, "Later").unwrap();
        let groups = list_groups(&conn).unwrap();
        assert_eq!(groups.iter().find(|g| g.id == new_id).unwrap().color, None);
    }

    #[test]
    fn set_group_color_sets_and_clears() {
        let (_dir, conn) = setup();
        let new_id = create_group(&conn, "Later").unwrap();

        set_group_color(&conn, new_id, Some("rgb(53,132,228)")).unwrap();
        let groups = list_groups(&conn).unwrap();
        assert_eq!(groups.iter().find(|g| g.id == new_id).unwrap().color.as_deref(), Some("rgb(53,132,228)"));

        set_group_color(&conn, new_id, None).unwrap();
        let groups = list_groups(&conn).unwrap();
        assert_eq!(groups.iter().find(|g| g.id == new_id).unwrap().color, None);
    }

    #[test]
    fn delete_group_reassigns_subscriptions_to_default_first() {
        let (_dir, conn) = setup();
        let group_id = create_group(&conn, "Later").unwrap();
        conn.execute(
            "INSERT INTO subscriptions (feed_url, title, group_id) VALUES ('https://example.com/feed.xml', 'Show', ?1)",
            params![group_id],
        )
        .unwrap();

        delete_group(&conn, group_id).unwrap();

        let groups = list_groups(&conn).unwrap();
        assert_eq!(groups.len(), 1, "the deleted group itself should be gone");
        let reassigned_group_id: i64 = conn
            .query_row("SELECT group_id FROM subscriptions WHERE feed_url = 'https://example.com/feed.xml'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(reassigned_group_id, DEFAULT_GROUP_ID);
    }

    #[test]
    fn delete_group_refuses_to_delete_the_default_group() {
        let (_dir, conn) = setup();
        let result = delete_group(&conn, DEFAULT_GROUP_ID);
        assert!(matches!(result, Err(DeleteGroupError::CannotDeleteDefaultGroup)));
        assert_eq!(list_groups(&conn).unwrap().len(), 1, "Default must still exist");
    }

    #[test]
    fn move_group_swaps_with_its_neighbor_without_touching_others() {
        let (_dir, conn) = setup();
        let second = create_group(&conn, "Second").unwrap();
        let third = create_group(&conn, "Third").unwrap();

        move_group(&conn, third, -1).unwrap();

        let groups = list_groups(&conn).unwrap();
        assert_eq!(groups.iter().map(|g| g.id).collect::<Vec<_>>(), vec![DEFAULT_GROUP_ID, third, second]);
    }

    #[test]
    fn move_group_at_the_edge_is_a_no_op() {
        let (_dir, conn) = setup();
        let before = list_groups(&conn).unwrap();
        move_group(&conn, DEFAULT_GROUP_ID, -1).unwrap();
        let after = list_groups(&conn).unwrap();
        assert_eq!(before.len(), after.len());
        assert_eq!(before[0].sort_order, after[0].sort_order);
    }
}
