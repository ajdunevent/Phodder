//! GTK-, GStreamer- and D-Bus-free core: database access, models, filter
//! rules, the group-priority queue, retention and sync data, so they can be
//! unit-tested without a display or audio pipeline.

pub mod backup;
pub mod chapters;
pub mod db;
pub mod feed;
pub mod filter;
pub mod gpodder_sync;
pub mod groups;
pub mod models;
pub mod opml;
pub mod queue;
pub mod retention;
pub mod settings;
pub mod show_notes;
pub mod subscriptions;
pub mod sync_accounts;
pub mod transcript;

pub use db::open_connection;
