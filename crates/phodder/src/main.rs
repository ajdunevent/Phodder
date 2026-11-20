//! The Phodder binary, in one of two roles:
//!
//! - `--service` (or `--hidden`, which Launch on Startup passes): playback,
//!   MPRIS, refreshes, downloads and sync, with no GTK. See service/mod.rs.
//! - Otherwise the window, a separate process that talks to the service over
//!   D-Bus and starts it when needed. Closing the window quits this process;
//!   the service keeps playing and downloading when Run in Background is on.
//!
//! Splitting them means the background footprint carries no GTK or graphics
//! driver memory.

mod background_portal;
mod redownload;
mod transcripts;
mod i18n;
mod bluetooth_watch;
mod call_watch;
/// Sent with feed, artwork, download and directory requests. Some sites
/// refuse requests that carry no User-Agent at all, reqwest's default.
pub const USER_AGENT: &str = concat!("Phodder/", env!("CARGO_PKG_VERSION"), " (Linux podcast player)");

mod coordinator;
mod discovery;
mod downloader;
mod gpodder_credentials;
mod gpodder_sync;
mod gpodder_sync_runner;
mod mpris;
mod mpris_peer_watch;
mod network;
mod nextcloud_login;
mod feed_health;
mod player;
mod service;
mod ui;

use std::cell::RefCell;
use std::rc::Rc;
use tracing_appender::rolling;
use tracing_subscriber::EnvFilter;

pub(crate) const APP_ID: &str = "net.dunevent.Phodder";

/// `$XDG_DATA_HOME/phodder` (or `~/.local/share/phodder`), a lowercase name
/// rather than the reverse-DNS app ID, matching other apps on the target
/// system. Shared with downloader.rs so both use the same root.
pub(crate) fn data_dir() -> std::path::PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| {
            // Only when no home directory can be found. /tmp keeps the app
            // running, but data won't survive a reboot.
            tracing::warn!("couldn't determine XDG data directory (no HOME set?), falling back to /tmp");
            std::path::PathBuf::from("/tmp")
        })
        .join("phodder")
}

pub(crate) fn db_path() -> std::path::PathBuf {
    data_dir().join("phodder.db")
}

pub(crate) const RUN_IN_BACKGROUND_SETTING: &str = "run_in_background";

/// How long the window process waits for a request to show the window
/// before deciding it was started by a stray D-Bus call.
const STRAY_START_GRACE_SECS: u32 = 10;

/// Launch on Startup requires it. Unset means on only if Launch on Startup
/// already is, since autostart from before this setting implied it.
pub(crate) fn run_in_background() -> bool {
    phodder_core::open_connection(&db_path())
        .ok()
        .and_then(|conn| phodder_core::settings::get_setting_bool(&conn, RUN_IN_BACKGROUND_SETTING))
        .unwrap_or_else(is_autostart_enabled)
}

/// Whether this file exists is the Launch on Startup setting. Inside Flatpak
/// it's only that record: the Background portal writes the host's entry.
pub(crate) fn autostart_desktop_file_path() -> std::path::PathBuf {
    dirs::config_dir().unwrap_or_else(|| std::path::PathBuf::from("/tmp")).join("autostart").join(format!("{APP_ID}.desktop"))
}

pub(crate) fn is_autostart_enabled() -> bool {
    autostart_desktop_file_path().exists()
}

/// Removes the file when `enabled` is false. Autostart runs `--hidden`, so
/// login starts background refresh without opening a window. Uses
/// `current_exe()` so it works when the binary isn't on `$PATH`.
pub(crate) fn set_autostart_enabled(enabled: bool) -> std::io::Result<()> {
    let path = autostart_desktop_file_path();
    if !enabled {
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let exe = std::env::current_exe()?;
    let contents = format!(
        "[Desktop Entry]\nType=Application\nName=Phodder\nExec={} --hidden\nX-GNOME-Autostart-enabled=true\n",
        exe.display()
    );
    std::fs::write(&path, contents)
}

fn main() -> anyhow::Result<()> {
    limit_allocator_arenas();
    i18n::init();
    // Must live for all of main(); see init_logging.
    let _log_guard = init_logging();

    if let Err(e) = std::fs::create_dir_all(data_dir()) {
        tracing::error!("failed to create data directory {}: {e}", data_dir().display());
        return Err(e.into());
    }

    // Synchronous, before either role starts: the window queries the
    // database at once, and schema setup on another thread failed with "no
    // such table" on fresh installs.
    let conn = phodder_core::open_connection(&db_path()).inspect_err(|e| tracing::error!("failed to open database: {e}"))?;
    use phodder_core::db::MigrationOutcome;
    match phodder_core::db::migrate(&conn) {
        Ok(MigrationOutcome::UpToDate) => {}
        Ok(MigrationOutcome::Created) => tracing::info!("created a new database"),
        Ok(MigrationOutcome::Adopted { rebuilt }) => {
            tracing::info!("brought the database to schema version 1 (rebuilt or added: {rebuilt:?}); the original is saved beside it")
        }
        Ok(MigrationOutcome::Upgraded { from }) => {
            tracing::info!("upgraded the database from schema version {from} to {}", phodder_core::db::SCHEMA_VERSION)
        }
        Ok(MigrationOutcome::Newer(version)) => tracing::warn!(
            "the database is schema version {version}, from a newer Phodder than this one ({}); some things may not work",
            phodder_core::db::SCHEMA_VERSION
        ),
        // Running on a half-upgraded database could damage it; the copy
        // saved before migrating is untouched.
        Err(e) => {
            tracing::error!("couldn't upgrade the database: {e}");
            return Err(e.into());
        }
    }
    drop(conn);

    if std::env::args().skip(1).any(|a| a == "--service" || a == "--hidden") {
        return service::run();
    }
    run_ui()
}

/// glibc otherwise gives each thread that allocates its own arena, up to
/// eight per core, and freed memory in them is rarely returned. Must run
/// before any other thread starts.
fn limit_allocator_arenas() {
    const M_ARENA_MAX: i32 = -8;
    extern "C" {
        fn mallopt(param: i32, value: i32) -> i32;
    }
    // SAFETY: mallopt only adjusts allocator tuning; no other thread exists yet.
    unsafe {
        mallopt(M_ARENA_MAX, 2);
    }
}

/// Hands the allocator's free memory back to the system. glibc keeps what a
/// burst of work freed (parsing large feeds, decoding artwork) and returns
/// it only from the top of the heap, so without this the service stays at
/// its peak size.
pub(crate) fn release_free_memory() {
    extern "C" {
        fn malloc_trim(pad: usize) -> i32;
    }
    // SAFETY: malloc_trim only returns free pages; it's safe from any thread.
    unsafe {
        malloc_trim(0);
    }
}

/// Two workers for either role: the work is mostly waiting on the network,
/// the disk and D-Bus.
pub(crate) fn build_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("couldn't start the Tokio runtime")
}

fn run_ui() -> anyhow::Result<()> {
    use gtk4::prelude::*;

    libadwaita::init()?;

    // Compiled icon resources (build.rs, resources/phodder.gresource.xml),
    // embedded with include_bytes!. Registered before any widget loads an
    // icon.
    let resource_bytes = include_bytes!(concat!(env!("OUT_DIR"), "/phodder.gresource"));
    let resource = gtk4::gio::Resource::from_data(&gtk4::glib::Bytes::from_static(resource_bytes))
        .expect("failed to load phodder's own compiled GResource -- build.rs should have produced a valid one");
    gtk4::gio::resources_register(&resource);
    // The path's status/ subdirectory isn't named here: icons live under
    // scalable/status; GTK appends that when resolving a name.
    gtk4::IconTheme::for_display(&gtk4::gdk::Display::default().expect("no default GDK display available"))
        .add_resource_path("/net/dunevent/phodder/icons");

    // HANDLES_COMMAND_LINE: a second launch reaches this instance's handler.
    // D-Bus activation (from the service, or a notification) arrives as
    // activate or as an app action instead, so the window is built at
    // startup where all three paths find it.
    let app = libadwaita::Application::new(Some(APP_ID), gtk4::gio::ApplicationFlags::HANDLES_COMMAND_LINE);
    let window: Rc<RefCell<Option<libadwaita::ApplicationWindow>>> = Rc::new(RefCell::new(None));
    let runtime: Rc<RefCell<Option<tokio::runtime::Handle>>> = Rc::new(RefCell::new(None));
    {
        let window = window.clone();
        let runtime = runtime.clone();
        app.connect_startup(move |app| {
            let bridge = ui::create_bridge();
            let tokio_handle = spawn_ui_runtime(bridge.command_rx, bridge.update_tx);
            *runtime.borrow_mut() = Some(tokio_handle.clone());
            let built = ui::build_window(app, bridge.command_tx, bridge.update_rx, &db_path(), tokio_handle);
            // Any D-Bus call to the app's name starts this process, and a hidden
            // window holds it open; only activate, a command line or show-podcast
            // presents the window, and they arrive straight after startup.
            let app_weak = app.downgrade();
            let window_weak = built.downgrade();
            gtk4::glib::timeout_add_seconds_local_once(STRAY_START_GRACE_SECS, move || {
                if let (Some(app), Some(window)) = (app_weak.upgrade(), window_weak.upgrade()) {
                    if !window.is_visible() {
                        tracing::info!("started without being asked to show the window; quitting");
                        app.quit();
                    }
                }
            });
            *window.borrow_mut() = Some(built);
        });
    }
    let present = move || {
        if let Some(window) = window.borrow().as_ref() {
            window.present();
        }
    };
    let present_for_activate = present.clone();
    app.connect_activate(move |_| present_for_activate());
    app.connect_command_line(move |_, _| {
        present();
        gtk4::glib::ExitCode::SUCCESS
    });

    let exit_code = app.run();
    if let Some(tokio_handle) = runtime.borrow().as_ref() {
        ui::client::say_goodbye(tokio_handle);
    }
    tracing::info!("phodder window exiting with code {exit_code:?}");
    Ok(())
}

/// The window's Tokio runtime: its connection to the service and the work
/// only it needs, like previews and directory searches.
fn spawn_ui_runtime(
    command_rx: tokio::sync::mpsc::UnboundedReceiver<ui::UiCommand>,
    update_tx: async_channel::Sender<ui::UiUpdate>,
) -> tokio::runtime::Handle {
    let runtime = build_runtime();
    let handle = runtime.handle().clone();
    std::thread::Builder::new()
        .name("ui-backend".into())
        .spawn(move || {
            runtime.block_on(async move {
                let client = match ui::client::ServiceClient::start(update_tx.clone()).await {
                    Ok(client) => client,
                    Err(e) => {
                        tracing::error!("couldn't connect to the Phodder service: {e:#}");
                        let message = i18n::gettext_f("Couldn't start playback and downloads: {error}", &[("error", &format!("{e:#}"))]);
                        let _ = update_tx.send(ui::UiUpdate::Error(message)).await;
                        return;
                    }
                };
                ui::run_command_loop(command_rx, client, update_tx, db_path()).await;
            })
        })
        .expect("couldn't start the window's backend thread");
    handle
}

/// The caller must hold the returned guard for the whole program: dropping it
/// stops the non-blocking writer's worker thread, and nothing gets logged.
fn init_logging() -> tracing_appender::non_blocking::WorkerGuard {
    // Logs go in the data directory so they survive a reboot. The appender
    // creates the directory.
    let file_appender = rolling::daily(data_dir().join("logs"), "phodder.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    // Two fmt layers with the same EnvFilter write to the file and to stdout;
    // fmt()'s single-writer builder can't attach a second writer, and `cargo
    // run` showed nothing.
    // Without RUST_LOG the log records what playback and downloads did, so
    // a bug report has something to go on.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn,phodder=info,phodder_playback=info"));
    use tracing_subscriber::prelude::*;
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(non_blocking))
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stdout))
        .init();
    install_panic_hook();
    guard
}

/// Writes panics to `<data dir>/logs/crash.log` before the default hook
/// prints them. A panic in a GTK callback aborts the process at once, too
/// soon for the non-blocking log writer, so the file is written directly.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        let thread = std::thread::current();
        let report = format!(
            "{} phodder {} panicked on thread {}: {info}\n{backtrace}\n",
            chrono::Utc::now().to_rfc3339(),
            env!("CARGO_PKG_VERSION"),
            thread.name().unwrap_or("unnamed"),
        );
        let path = data_dir().join("logs").join("crash.log");
        let written = std::fs::create_dir_all(data_dir().join("logs"))
            .and_then(|_| std::fs::OpenOptions::new().create(true).append(true).open(&path))
            .and_then(|mut file| std::io::Write::write_all(&mut file, report.as_bytes()));
        if let Err(e) = written {
            eprintln!("couldn't write {}: {e}", path.display());
        }
        default_hook(info);
    }));
}
