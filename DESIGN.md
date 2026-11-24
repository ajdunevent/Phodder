# Phodder design

**App ID:** `net.dunevent.Phodder` &nbsp;|&nbsp; **License:** GPL-3.0-or-later (`COPYING`) &nbsp;|&nbsp; **Target:** GNOME Shell + Phosh (primary device: FuriLabs FLX1S+)

How Phodder works and why, including the decisions that the code alone
doesn't explain. Where something isn't built, the section says so.
Update this file in the same change as the code it describes.

---

## 1. Architecture

**One binary, two processes.** The `phodder` binary runs in one of two
roles, chosen in `main.rs` before GTK is touched:

- **The service** (`phodder --service`, or `--hidden` from Launch on
  Startup; `service/mod.rs`): playback, MPRIS, the refresh and download
  schedule, sync, retention, and the Bluetooth, call and network
  watchers. It never initializes GTK, so the graphics stack and its driver
  are never loaded. A `gio::Application` (NON_UNIQUE, the app ID) provides
  the GLib main loop, the network monitor and notifications.
  At startup an "unavailable" network reading counts only if it lasts 5
  seconds: under Flatpak the portal monitor reads unavailable until its
  first asynchronous reply.
- **The window** (`phodder`): libadwaita UI only. Closing it quits the
  process. It reads and writes the database directly (SQLite WAL handles
  the two processes) and talks to the service for everything that has to
  outlive it.

The split exists for memory: in the background only the service runs,
with no GTK, widget or GPU memory. Measured on the x86 test setup with
software rendering, the background footprint went from 173 MB (70 MB
private) to 60 MB (19 MB private); on the FLX1S+, from about 350 MB to
69 MB (27 MB private).

**The private interface** (bus name `net.dunevent.Phodder.Service`, path
`/net/dunevent/Phodder/Service`). Everything coming back to the window
is a method reply: Flatpak's D-Bus proxy can drop signals sent from one
instance of an app to another (the sender needs TALK on the receiver's
unique name, which it may not know the window owns), while it always
passes replies. Reproduced and verified with `xdg-dbus-proxy` and
Flatpak's policy in front of each process.
- `net.dunevent.Phodder.Service1` (`spawn = false`, so calls are handled
  in arrival order): `Command(json)`, a `ServiceCommand`
  (`service/commands.rs`) sent with no reply expected so commands keep
  their order without a round trip each; `Goodbye()`, sent as the window
  quits.
- `net.dunevent.Phodder.ServiceCalls1` (each call in its own task):
  `NextUpdates()` returns queued `UiUpdate`s as JSON, or nothing after 20
  seconds, and the window calls it again at once; the first call
  registers the window and queues the last player state, the feed status
  and errors held while no window was open (up to 20). `Request(json)`
  runs a command and returns its `Result<(), String>`.
- A window is forgotten on `Goodbye`, when its connection closes (if the
  service sees it), or after 60 seconds without asking for updates.

The window's `UiCommand`s are routed in `ui/mod.rs` (`UiCommand::route`):
player, queue, refresh, download, sync and retention commands go to the
service; previews, directory searches, artwork fetches, transcripts, OPML
and account sign-in run in the window's own Tokio runtime
(`ui/client.rs` holds the connection).

**Starting each other** uses D-Bus activation, the only way that works
in Flatpak, where a child would die with its parent's sandbox. Both
service files are installed from `data/dbus-1/`:
- The window starts the service with `StartServiceByName`, falling back
  to spawning `phodder --service` (development builds without the
  service files), and restarts it if it disappears.
- The service opens the window (MPRIS `Raise`, notification clicks) by
  calling `org.freedesktop.Application.Activate`/`ActivateAction` on the
  app ID; the desktop file is `DBusActivatable`. The window is built at
  startup so activation, actions and command lines all find it.
- The service registers its interfaces before claiming its name: a
  window waiting for the name calls the moment it appears, and zbus drops
  calls that arrive before an object exists.

**Lifetime**: a second service exits at once (the name is claimed
before the player starts). With **Run in Background** off, the service
stops when its last window closes, as closing the window always quit
before. Quit (main menu, Ctrl+Q) stops the service whatever the setting;
the window sends it with no-auto-start, as it does Goodbye, so neither
starts a stopped service again through D-Bus activation.
Any D-Bus call to `net.dunevent.Phodder` starts the window process, and
its window is built at startup, hidden, so the process would stay with no
window; it quits if nothing shows the window within 10 s. **Launch on Startup** requires Run in Background and starts only
the service; under Flatpak both ask the Background portal
(`org.freedesktop.portal.Background`) and revert the switch if refused.

**Memory tuning**: `M_ARENA_MAX` is 2 (set before any thread starts) and
each process's Tokio runtime has two workers, since glibc otherwise
grows an allocator arena per allocating thread. The service calls
`malloc_trim` after each refresh and download pass: glibc otherwise keeps
what parsing large feeds freed, and with five 7 MB feeds the service
stayed at 152 MB instead of 79 MB.

**History**: Phodder began as four processes (`phodder-player`,
`-coordinator`, `-downloader` and the UI) and was collapsed into one,
since Tokio task isolation already covered the failure worth isolating
(malformed feeds). The current two-process split is for memory, not
fault isolation: "the coordinator", "the player" and "the downloader"
elsewhere in this document are modules in the service.

| Crate | Kind | Responsibility |
|---|---|---|
| `phodder-core` | lib | GTK/D-Bus/GStreamer-free. DB schema access, domain models, the download/chapter-skip filter DSL, the group-priority queue algorithm, retention/eviction logic. Unit-testable without a display server. |
| `phodder-playback` | lib | Playback engine. One task owns the GStreamer pipeline and all playback state; commands and bus messages are handled in one loop with an explicit Idle/Loading/Ready state machine. GTK- and D-Bus-free, with integration tests that play real MP3s through a synced fakesink. |
| `phodder` | bin | Both roles. Service: `service/` (process, interface, command handlers), MPRIS and the engine adapter (`player.rs`/`mpris.rs`), retention (`coordinator.rs`), refresh and downloads (`downloader.rs`). Window: `ui/` (`mod.rs` holds the message types and routing, `client.rs` the connection to the service, `window.rs` the main window and player sheet, one module per page and dialog, shared helpers in `widgets.rs`). |

## 2. Data model

Full schema lives in `crates/phodder-core/schema.sql` (SQLite, WAL mode,
`synchronous=NORMAL`, `foreign_keys=ON` set per-connection). Key design
choices worth remembering:

- **"Now playing" is never a DB column** — it's held in-process by the
  `phodder-playback` engine, virtualized as "group 0 / slot 0" in the
  priority model. "What plays
  next" is a single query (front of the highest-priority non-empty real
  group), not a maintained table.
- **Fractional-index sort columns** (`groups.sort_order`,
  `episodes.group_queue_position`, both `REAL`) — drag-reorder is one
  `UPDATE`, never a cascading renumber.
- **Filters stored as JSON** (`subscriptions.download_filter_rules`,
  `.chapter_skip_rules`) — one glob-line DSL, two use sites (download
  allow/blocklist, chapter-title auto-skip). Chapter-skip decisions are
  **not cached** on `chapters` rows — evaluated fresh at playback time so
  rule edits retroactively apply.
- **Nullable per-subscription overrides, NULL = inherit global default**
  from `settings` — used for retention/file management. Expiry
  (`delete_after_minutes`) is the exception: every subscription has its
  own, starting from the Settings default.
- **Schema versions** (`phodder-core/src/db.rs`): the version is
  SQLite's `user_version`. `schema.sql` is always the latest schema and
  creates new databases; `MIGRATIONS` upgrades existing ones one step at a
  time, each in a transaction with foreign keys off (so a step can rebuild
  a table) and checked before committing. A change to `schema.sql` needs a
  step and a `SCHEMA_VERSION` bump; a test applies the steps to the frozen
  version 1 (`migrations/v1.sql`) and compares the result with
  `schema.sql`. Databases from before versioning are made to match
  version 1 exactly: missing tables and indexes are created, and tables
  whose columns or foreign keys differ are rebuilt with their rows. A
  copy, `phodder.db.before-v<N>`, is saved before any change. A database
  from a newer Phodder is left alone with a warning; a failed migration
  stops startup rather than run on a half-upgraded file.
- **All sync timestamps explicitly UTC** (`_utc` suffix) — defends
  against the real AntennaPod timezone-sync bug found during research.
- **Credentials never stored in the DB** — `sync_accounts.credential_ref`
  is a lookup key into `oo7`. The
  Podcast Index API secret is in `oo7` too; only its key is a setting.

## 3. UI shape

- Tabs: **Queue**, **Library** (downloads, with an All / Pending /
  Downloaded / Failed filter), **Subscriptions** (grouped like the Queue,
  with group-colored bars), **History** (played on this device). Header
  switcher on wide windows, bottom bar on narrow ones.
- **One quick action per episode row**, the first that applies:
  - Queue: Play, or Pause for the playing episode.
  - Library: Retry (failed) → Delete (downloaded); pending rows get none.
  - A podcast's Episodes tab: Download → Retry → "+" → Mark Played →
    Delete.
  - History: Delete (file exists) → Clear.
  Tapping a row opens episode details, which offer every action that
  applies (one list, filtered by where it was opened).
- Player: `AdwBottomSheet`. The mini player is always visible above the
  tab bar ("Nothing playing / N episodes queued" when idle). Expanded:
  artwork sized to what's left, title, current chapter, the transcript
  caption (below), `position · scrub bar with chapter ticks · duration`
  on one row, then `restart · previous chapter · back · play/pause ·
  forward · next chapter · next episode`, then `details · sleep` left and
  `speed` right. The details button slides to the episode's details.
  Switching tabs collapses the player.
- Transcript caption: for an episode with a timed transcript, three lines
  under the chapter: the previous line dimmed, the current line (up to
  three wrapped lines, the speaker first when it changes), the next line
  dimmed. Tapping it opens details on the Transcript tab at the current
  line. Not announced as it changes; a screen reader can move to it.
- Episode details: with a transcript, two tabs, **Show Notes** (header,
  actions, chapters, notes) and **Transcript**. Open details refresh on
  every data change (a finished download, sync, a queue change): the
  header and actions in place, or the whole view when the download state
  or transcript changes. The player's details
  remember the tab across episodes. The Transcript tab follows the
  current line while it plays, until the user scrolls by any means
  (wheel, touch, scrollbar, arrow keys); a floating button resumes
  following. Activating a line plays from its start, with no distance
  limit; a tap within 400 ms of pointer scrolling (including a fling's
  deceleration) only stops the scroll. One Tab stop: arrows move between
  lines, Tab leaves, and tabbing in lands on the current line. Untimed
  transcripts (HTML, text) show as paragraphs with no caption.
- Sleep timer: 5–60 minutes or **End of episode** (the next episode loads
  paused). The countdown keeps running while paused.
- Podcast pages: tapping a subscription opens **Details | Episodes |
  Settings**. Add Podcast's directory previews use the same Details and
  Episodes tabs, read-only, with Back and Add.
- Settings: libadwaita preference groups (General, Updates, Sync,
  Playback, Storage, Podcast Directories) of switch, combo and expander
  rows, saved on every change. Sync lives here, not in its own dialog or
  menu entry. Numbers are a GtkSpinButton in an AdwActionRow: AdwSpinRow
  was missing from the accessibility tree. Expander switches are named by
  their title alone (libadwaita labels them by their rows too).
- Dialogs: a header bar with the title (or tabs, or nothing where the
  content shows the title) and a close button, floating or as the narrow
  window's bottom sheet; no Done buttons, since everything saves as it
  changes. Back is in the header for a sheet that replaced another and
  for Add Podcast's previews. A new subscription keeps Cancel / Done,
  where Done confirms it. Button rows get their style class added after
  construction: the builder's `css-classes` replaces every class,
  including the one that styles a button row.
- Manage Groups: one entry row per group (colour, name edited in place,
  move, delete) and a "New group" row last. A podcast's Settings tab:
  preference groups; each download filter rule is an expander titled by
  its pattern ("Block · Title" below) with its fields inside, then Add
  Rule; Skip Chapters is a list with an entry row last.
- Show notes: `html2pango` into a `GtkLabel` (no WebKitGTK). Long text
  isn't selectable, so touch drags scroll. Timestamps are links that seek.
- Keyboard: Space play/pause (unless a button, switch, checkbox or row
  has focus, which Space presses), Ctrl+←/→ skip, Ctrl+Shift+←/→
  chapters, Alt+↑/↓ moves a focused Queue episode within its group,
  Alt+1–4 tabs, Ctrl+R refresh, Ctrl+N Add Podcast, Ctrl+, Settings,
  Ctrl+? Keyboard Shortcuts, Ctrl+W close window, Ctrl+Q quit.
- New-episode notifications: per podcast ("Notify on new episodes", off
  by default), one per podcast per refresh, never for a podcast's first
  refresh (its back catalog). Clicking opens the podcast's Episodes tab.
- About Phodder (main menu): an `AdwAboutDialog` built from the bundled
  metainfo (name, developer, license, website, issue tracker, release
  notes), plus Donate and Source Code links, which libadwaita doesn't read
  from metainfo.
- Selection mode on Queue, Library, History and a podcast's Episodes tab.
  Entered by long press (touch only), right click, the header Select button
  (Queue, Library, History) or the Select button above a podcast's episode
  list, since that dialog's header holds its tabs; there a pressed Select
  button in the selection bar leaves the mode. The bar shows the count (with
  Select All/None) and every action, dimming any that doesn't apply to every
  selected episode. Opposite pairs (Add/Remove from Queue, Mark
  Played/Unplayed) share a button so the bar fits a phone. Actions run on
  one tap. An action ends the mode. History has no Delete All or Clear
  All; Select All covers both. Select All, the count and actions cover
  only rows the current filter shows; hidden rows stay checked. On a
  podcast's Episodes tab with more than one page, the menu offers Select
  Page and "Select All {n} Episodes", and selections on other pages count.
  Downloading more than 20 at once asks first, with the total size when
  the feed gives sizes. Switching tabs leaves the mode.
- **Swipe actions dropped** (no stock libadwaita widget for it).
- Skip intervals (forward/backward) are independently configurable and
  apply to the buttons, shortcuts and MPRIS.

## 4. Group/queue priority algorithm

- Every subscription belongs to exactly one group (`Default` seeded on
  install) — group membership is mandatory, never a null-checked path.
- Groups are ordered and user-reorderable (fractional index); empty
  groups persist so they stay rearrangeable for future episodes.
- Within a group: new episodes always insert at the bottom by download
  time (auto-sort simplified to this single rule); manual drag reorder,
  "send to front/back of group" always available; moving an episode out
  of its group is UI-prevented.
- "Now playing" = virtual group 0/slot 0. Playing anything moves it
  there. An interrupted episode with at least 30 seconds of progress
  resumes next: within the same group it goes right after the new
  episode, from another group to the front of its own group
  (`place_after_interruption`). One with less counts as not started and
  keeps its place, so a moment of progress (an episode loaded just
  before a crash, say) never overrides the order the user set. The queue
  changes only once the chosen episode is ready to play: one that fails
  (missing file, won't decode) leaves the order alone and the previously
  loaded episode comes back paused, instead of whatever is next. Other
  play failures are shown in the banner. "What's next" (recomputed whenever
  the current episode ends, not just on interruption) is always the
  front of the highest-priority non-empty *real* group — this is what
  makes "Daily News always wins" work without any explicit interrupt
  mechanism.
- No group interrupts a currently-playing episode (the "emergency
  broadcast" idea was explicitly dropped).

## 5. Downloads, filters, retry, retention

- Metered connections (NetworkManager's flag): two settings decide what
  the automatic pass (scheduled, and when the network returns) does.
  "Update feeds on metered connections" (on by default) refreshes feeds
  and syncs; "Download episodes on metered connections" (off by default)
  downloads. Episodes found meanwhile stay queued for a pass that allows
  downloads. Refresh All and downloads the user starts aren't gated.

- Damaged downloads are fetched again (`redownload.rs`): when a file is
  missing, or won't decode in a way a new download can fix (a stream
  error, or a missing decoder for a non-media type such as an HTML error
  page saved as the episode; not a missing audio codec), the episode is
  reset to download, keeping its queue place and position. Gated like
  automatic downloads: offline, or on a metered connection without
  "Download episodes on metered connections", it waits for the next pass
  that allows it. Exempt from the feed's
  download rules (the episode was already chosen). Once per episode per
  run. If the user had pressed Play, it plays when the download finishes
  within 15 minutes, unless something else is playing by then. A toast
  says what happened.

- Per-subscription auto-download toggle, download allow/blocklist filter
  (glob-line DSL: pattern + target [title/description/both] + action
  [allow/block]; checked top to bottom, the first matching rule wins).
  Queued automatic downloads are re-checked against the current rules
  just before they start; manual downloads always bypass filters.
- Chapter-title auto-skip, per podcast ("Skip Chapters" in its Settings
  tab), same matcher, no seeded defaults. Playback that runs into a
  matching chapter jumps to the next; a chapter the user seeks into, or
  resumes partway through, plays. Works with `podcast:chapters` JSON and
  ID3 chapters embedded in the file.
- Retry: default 5 attempts (configurable count), exponential backoff,
  `episodes.retry_count` + `last_error_reason`; warning flag during
  retries, error flag once exhausted; reason shown on the episode.
- Concurrency: 4 on WiFi/ethernet, 2 on cellular (via NetworkManager
  connection type) — not user-configurable, a sane per-session default
  instead of a flat constant.
- Disk space: `statvfs` preflight before every download; `ENOSPC`
  mid-download skips backoff and fails immediately (not a transient
  error worth waiting out).
- Retention/file management: `delete_after_play`, `delete_after_minutes`
  (from **release** date), `size_limit_bytes` /
  `episode_count_limit` (true rotation — can evict unplayed episodes),
  all per-subscription with global-default fallback. Currently-playing,
  queued and in-progress episodes are **never** evicted regardless of policy
  aggressiveness (enforced in SQL, not left to callers). Deleting a
  subscription deletes its remaining downloaded files.
- Retention sweeps a subscription immediately when one of its episodes
  finishes downloading, is unloaded from the player (finished, skipped,
  replaced) or is marked played, plus a periodic pass every 30 minutes.
  The periodic timer counts only awake time, so it can't be relied on
  for delete-after-play on a phone that suspends.
- File sizes shown in episode lists/detail (`episodes.file_size_bytes`,
  set on download completion).
- "Played" threshold: seconds remaining, global (Settings). Sync uses the
  same threshold when deciding whether another device's progress means
  played.

## 6. Sync (gpodder protocol)

- Targets: **gpodder.net** and **Nextcloud's GPodder Sync app** only
  (both speak the same gpodder HTTP+JSON API).
- Scope: subscriptions + episode actions/play state only (matches
  AntennaPod/Kasts) — queue/group structure stays local-only, not part
  of the protocol.
- Conflict resolution: plain last-write-wins (the protocol's native
  behavior) — deliberately not building anything smarter, since the
  protocol has no room to express more than one timestamp per action
  anyway. All timestamps generated/transmitted in UTC.
- Triggers: periodic + on-open (with feed updates, so under "Update feeds
  on metered connections"), manual button tucked in settings (not
  prominent, runs regardless of metered status).
- **Two services**, chosen in Settings' Sync group:
  1. **Nextcloud GPodder Sync** — server address, then Nextcloud
     **Login Flow v2** in the browser (2FA-safe, yields a revocable app
     password, revoked on Disconnect).
  2. **gpodder.net** — username and password, plus a server field
     (default https://gpodder.net) for self-hosted copies. Connecting
     checks the login, registers this device ("Phodder", type mobile)
     and joins it to a sync group with the account's other devices:
     gpodder.net keeps a subscription list per device and shares only
     within a group. Episode actions carry this device's ID, with
     timestamps in the documented `2009-12-12T09:00:00` UTC form.
     Tested against a fake server, not the real gpodder.net.
- Each sync pass (refresh, Sync Now, the half-hourly sweep) sends a
  `SyncStatus`: None when it worked, otherwise the service ("gPodder" or
  "Nextcloud"), how many feeds failed (None for the whole sync, such as a
  missing password), and why. The problem bar leads with it ("Nextcloud
  sync failed for all feeds") until a sync succeeds, and its Details row
  opens Settings scrolled to Sync. Failures used to reach only the log, so a missing keyring
  password went unnoticed.
- **GNOME Online Accounts: dropped.** Under Flatpak it needs a permission
  that exposes every online account.
- Credential storage crate: `oo7` (not classic `libsecret` C bindings) —
  pure Rust, no extra system lib, and its file-backend-via-portal
  behavior specifically solves libsecret's known Flatpak sandboxing
  problems.

## 7. Private feeds

- Token-in-URL feeds (Patreon, Supercast and the like) work through "Or
  Add by Feed URL" with no special handling.
- **Password-protected (HTTP Basic Auth) feeds: dropped.** Private feeds
  in practice are gated by an unguessable URL, not a login.

## 8. Discovery

- **Add Podcast** page (from Subscriptions): a search bar, the enabled
  directories' Popular lists until something is searched, and "Or Add by
  Feed URL" at the bottom. Picking a result opens a read-only preview
  (Details | Episodes, Back and Add).
- Sources, each switchable in Settings → Podcast Directories:
  **Apple Podcasts** (iTunes Search API and Apple's top chart, keyless),
  **gpodder.net** (search and toplist, keyless), and **Podcast Index**
  with the user's own free key and secret (search and trending). fyyd was
  in the original plan and isn't built.
- All enabled sources are queried at once; the page waits for all of
  them (10-second timeout each). A source that fails while others answer
  is named in a note; a rejected Podcast Index key gets its own message.
  Apple limits each address to about 20 requests a minute and answers
  with 403 past that. The Popular list is cached for an hour.
- Directory artwork is cached in `~/.cache/phodder/discover`, apart from
  subscription artwork. Files unused for 7 days are deleted at startup;
  adding a podcast copies its artwork to the permanent folder.
- After each refresh, artwork whose cached file is missing (paths from
  before the Flatpak, for one) is fetched again from its URL; without a
  URL an episode's path is cleared so it shows its show's artwork. A feed
  refresh re-caches a show's artwork, but an episode's is only cached when
  the episode is added.
- The Refresh button's icon (Adwaita's view-refresh, bundled as
  `phodder-refresh-symbolic` so no theme replaces it) turns about its
  loop's centre, (7.5, 8.5), while any refresh runs, as Owlet's does, from a `Refreshing` update the service sends when the first
  overlapping refresh starts and the last one ends; new windows get it in
  their catch-up.
- Dedup by normalized feed URL. Results already subscribed to show a
  checkmark and open the existing podcast.
- Preview playback: each episode in a directory preview has Play/Pause.
  The engine streams the audio URL (allowed on metered connections) as an
  episode that isn't in the database: `current_id()` is None for it, so
  nothing is saved (position, History, sync milestones). The episode it
  replaced has its position saved; when the preview ends nothing is
  loaded, and Play resumes that episode (`last_in_progress`).

## 9. OPML

- **File-only** import (no URL-paste import).
- OPML folders map directly onto the group system: nested `<outline>`
  folders ↔ `groups` rows (matched by name on import, created if absent);
  feeds outside any folder → Default group. Export walks `groups` in
  `sort_order`, nesting subscriptions inside matching folder outlines.
- Import inserts subscription rows immediately from `text`/`xmlUrl`;
  episode lists populate asynchronously via the normal refresh cycle —
  never blocks the import UI waiting on N feed fetches.
- Export is plain/standard OPML only (no custom attributes for
  app-specific data) for maximum portability.
- **Back Up Library / Restore Library** (main menu), separate from OPML:
  every table as JSON rows (`phodder-core/src/backup.rs`), without audio,
  cached artwork, local paths or passwords (passwords are in the keyring;
  a sync account restored elsewhere asks again). Restore replaces
  everything after a warning, in one transaction, matching columns by
  name so older backups restore into newer schemas. A download survives
  when the restored episode (feed URL + GUID) had its file here; other
  queued episodes download again, the rest stay not downloaded, and files
  outside the restored library are deleted. The window copies the chosen
  file into the data directory for the service, which stops playback and
  downloads first.

## 10. Podcasting 2.0 / Value4Value

- Parse `podcast:chapters`, `podcast:transcript`, `podcast:funding`,
  `podcast:person`, `podcast:medium` where present; fully optional,
  backwards-compatible (no special-casing "2.0 feeds"). **Built so far**:
  chapters (fetched after download, shown in details and on the scrub
  bar) and transcripts. V4V is not built.
- Transcripts (`phodder-core/src/transcript.rs`): WebVTT (with voice
  tags), SRT, Podcast Index JSON, HTML and plain text, detected from the
  content. When a feed lists several, timed formats win. Timed segments
  are regrouped into lines at sentence ends (6+ words), clause ends (18+
  words), speaker changes, 2-second pauses, or 30 words / 15 seconds,
  since JSON is often one word per segment. Diarization labels
  ("SPEAKER_00") show as "Speaker 1"; speakers show only when there are
  several. Fetched after download and on demand, cached as
  `transcript-<id>.txt` next to the audio, pruned at startup when the
  episode has no file. No schema change.
- V4V (planned, not built): manual one-off boosts first; streaming
  (per-minute) sats later; boostagram display not planned.
- Boosts would be sent via **Nostr Wallet Connect (NWC)** — app is purely
  parser/transmitter, never a wallet/node/custodian. User connects their
  own NWC-compatible wallet (Alby, etc.) via a pasted/scanned connection
  URI. Feed-defined recipient splits (`podcast:valueRecipient`) fan out
  a single boost into weighted sub-payments.

## 11. MPRIS, Bluetooth, phone calls, ducking

- MPRIS (`org.mpris.MediaPlayer2` + `.Player` via `zbus`) is the
  foundation — Bluetooth AVRCP hardware buttons route through it for
  free via `mpris-proxy` (BlueZ), no separate AVRCP code needed.
- A load that fails for any reason but the file (a timeout, an audio
  output error; a car's Bluetooth still connecting) is retried three
  times, 3 s apart, then the episode is held paused. Only file problems
  and missing decoders mark an episode failed and skip to the next one.
  Before, a reconnecting car's 15 s load timeout skipped the episode.
- MPRIS Stop pauses, keeping the episode loaded: car stereos send Stop
  when switched off. Play with nothing loaded resumes the most recently
  played unfinished queued episode (`last_in_progress`) before falling
  back to the front of the queue, which may have changed meanwhile.
- MPRIS Previous and Next skip back and forward by the skip intervals; a
  second press within 700 ms restarts the episode or moves to the next
  one (undoing the first press's skip, so the episode left keeps its
  place). Headsets have only these buttons, and when Previous meant
  restart, a stray press (likely what restarted episodes on Bluetooth
  reconnects) lost the listener's place.
  Long presses aren't handled: they don't reach MPRIS as anything
  distinct.
- Phodder gives up its MPRIS name while another player plays
  (`mpris_presence.rs`). BlueZ, through mpris-proxy, sends a headset's
  buttons to the first registered player and moves on only when that
  player leaves the bus, whether or not it is playing; with Phodder's
  service registered from login, a paused Phodder took presses meant for
  other players. The name is given up when a peer reports Playing (or is
  playing at startup), and taken back when Phodder plays or that peer
  leaves the bus; the peer merely pausing doesn't bring it back, so the
  next press resumes the peer. Going back to Phodder while the peer is
  still open leaves BlueZ on the peer unless that app does the same (as
  Paradon is meant to) or BlueZ is patched to follow the playing player.
- Bluetooth auto-pause/resume is **not** something MPRIS provides —
  `bluetooth_watch.rs` follows where the stream plays: pulsesink's
  `current-device` (read through `Player::output_device`) names a
  Bluetooth output after the device (`bluez_output.AA_BB_CC_DD_EE_FF.1`).
  Playing audio that leaves a Bluetooth output for another one pauses at
  once; the paused stream moving back to that device resumes it (read
  every second while paused, since pulsesink only signals the change while
  playing). BlueZ (`Device1.Connected`, every device under `/org/bluez`)
  covers the rest: a disconnect pauses when the output can't be read
  (pipewiresink) and the device is audio (icon `audio-*` or an A2DP, HSP
  or HFP UUID), and a reconnect resumes once the output is back on that
  device, or after 8 s. BlueZ alone paused up to ~20 s late for a device
  out of range, resumed before the audio link was up, and ignored
  devices whose icon wasn't exactly `audio-card` (most earbuds). Only
  playback this watcher paused resumes, and only for the same device.
  Switching output away from a Bluetooth device by hand while playing
  pauses too.
- An output error (a device vanishing mid-stream) reloads the episode at
  its position once. A second error within 30 s, or one during that
  reload, keeps the episode loaded and paused with the pipeline down, so
  the player still shows it; Play reloads it. Unloading it instead made
  the player show "Nothing playing" until the next resume.
- Ducking: the stream is labeled `media.role` = music (PulseAudio) /
  Music (PipeWire), so the sound server's role policy can lower it for
  notifications, navigation prompts or a screen reader. Phodder does no
  ducking itself; whether it happens is the system's configuration.
  Not for phone calls (see below).
- Phone calls: **explicit hard pause**, not ducking (ducking is
  unintelligible at the user's 2.5x playback speed) — pause spans the
  *entire* call lifetime (ringing through answered/active), single
  resume only on true termination (never merely "ringing stopped because
  answered"). Detected via ModemManager's `Modem.Voice` interface (same
  backend GNOME Calls uses), in `call_watch.rs`. A call counts as ended
  when its state becomes Terminated or when it's deleted with no other
  call in progress: a missed call can be deleted without Terminated ever
  arriving, which left playback paused after a call went to voicemail.
  Every voice-capable modem is watched, including ones ModemManager adds
  later (ObjectManager InterfacesAdded); a call ending on one modem
  doesn't resume while another has a call.
- Pause when another MPRIS peer starts playing: `mpris_peer_watch.rs`
  enumerates `org.mpris.MediaPlayer2.*` peers, subscribes to
  `PlaybackStatus`, pauses self on a peer's transition to `Playing`.
- Pitch is kept at other speeds by `scaletempo`, created fresh for each
  episode: one reused across episodes with different sample rates or
  channel counts crashed the app at speeds other than 1x (GStreamer
  1.26, `episodes_in_different_formats_play_at_double_speed`).
- Speed is chosen on a slider from 0.5× to 4× in 0.25× steps, in the
  player and in a podcast's Settings ("Override speed"; off uses the player's). A drag sends only the value it
  settles on, since each change is saved and applied with a seek.
- Speed: the player's speed is global. A podcast can set its own, used
  for its episodes; changing the speed while one plays sets the player's
  speed and lasts for that episode only (`rate_chosen_for`), never
  changing the podcast's.
- Trim Silence (Settings, overridable per podcast: same as player, on,
  off): a pad probe ahead of scaletempo (`trim.rs`) drops audio quieter
  than about -30 dBFS after the first 250 ms of each quiet run and moves
  later buffers earlier. Reported positions are on that shortened
  timeline, so each is mapped back through the offsets in force; seeks
  and flushes start the offset over. No plugins-bad element is needed.

## 11a. Diagnostics

- Logs go to stdout and to a daily `phodder.log.YYYY-MM-DD` in
  `<data dir>/logs/` at `warn`, plus `info` for Phodder's own crates
  (episode loads, refreshes, sync); `RUST_LOG` overrides.
- A Rust panic is written with a backtrace to `<data dir>/logs/crash.log`
  (Flatpak: `~/.var/app/net.dunevent.Phodder/data/phodder/logs/`) before
  the app exits. A crash inside a C library (a segfault) isn't caught
  there; running from a terminal or a core dump shows those.

## 12. Packaging

- **Flatpak only.** (.deb was dropped.)
- Flatpak: `build-aux/net.dunevent.Phodder.json` (GNOME 51 runtime, Rust
  SDK extension) and `build-aux/dev.sh`, which builds x86_64 and/or
  aarch64 bundles into `dist/` or installs for the current user, with
  incremental rebuilds kept in `.flatpak/`. AAC comes from the runtime's
  codecs-extra extension, installed automatically with GNOME 49 and later.
  Permissions: network, audio, display, MPRIS name and peers, and the
  system bus for NetworkManager, ModemManager and BlueZ.
  `build-aux/install.sh` installs the binary, data files and compiled
  translations for both `dev.sh` and the manifest.
- Releasing: add a `<release>` to the top of the metainfo, with the same
  version in the workspace `Cargo.toml`, and push to `main` on Forgejo.
  `.forgejo/workflows/tag.yml` tags it there; the GitHub mirror carries
  the tag over, and `.github/workflows/release.yml` (the FrogPak
  releaser) builds both bundles, publishes the release and notifies
  AJHub. Tags are never made on GitHub: the mirror's next sync would
  delete them.
- Published through AJHub and FuriHub, not Flathub. Flathub would need
  offline crate sources (`flatpak-cargo-generator`) instead of the
  build's network access.
- App icon: wheatgrass in a slate tray with a play button, built from
  individual blades in three GNOME greens plus a mirrored, colour-swapped
  copy behind them. `data/icons/source/gen.py` lays out the blades and
  `flatten.py` (needs shapely) merges them into one path per colour and
  writes the installed SVG. It spans y 4 to 120, as tall as GNOME's other
  square-ish icons. The play button, a slate triangle (the tray's colour)
  with a thick white outline, is the last two paths; `flatten.py` finds it
  by its white outline path.
- `oo7` chosen specifically because its sandboxed file-backend (via the
  `org.freedesktop.portal.Secrets` portal) avoids classic libsecret's
  known Flatpak compatibility problems — one code path works correctly
  on both targets.

## 13. Accessibility & localization

- Screen readers: every control has a name. Icon-only buttons take it
  from their tooltip; others use `set_accessible_label` (ui/widgets.rs).
  Artwork and icons beside text that says the same thing are hidden from
  screen readers (`decorative_image`). Rows built from a wrapper
  ListBoxRow name the wrapper and hide the inner AdwActionRow, so each
  row is one stop. Things that change away from focus are announced
  (`announce`): search results, the banner, sync status, Queue moves,
  episode pages, armed two-step buttons.
- Everything works from the keyboard: row quick-action buttons respond to
  the keyboard as well as the pointer, focus returns to the same control
  after an action rebuilds a page, and the Queue reorders with Alt+↑/↓.
- Workarounds for GTK/libadwaita gaps, both in code: popover menu items
  (GTK 4.20 labels them by a hidden label, so they had no name) and the
  Keyboard Shortcuts dialog's rows (unnamed; now "Action, keys" with key
  names spelled out).
- Checked with an AT-SPI script that walks the tree and flags unnamed
  controls, plus keyboard-only runs. Not yet tested with Orca itself.
- Translations: gettext (`gettext-rs`, domain `phodder`). Strings are
  marked with `gettext`, `ngettext`, `gettext_f`/`ngettext_f` (named
  `{placeholders}`, so translators can reorder) and `N_` for tables
  translated where shown (crates/phodder/src/i18n.rs). No sentences are
  built by concatenation. `po/update-pot.sh` extracts `po/phodder.pot`
  (including the desktop entry and metainfo) from `po/POTFILES.in` and
  merges it into each language in `po/LINGUAS`. No translations yet.
- Not translated: the app name, directory names, log messages, and
  error details from libraries (shown after a translated prefix).

## 14. Explicitly out of scope / deferred

- Streaming (per-minute) V4V sats, boostagram display
- Cancelling downloads from the UI (downloads are quick; not offered)
- Emergency-broadcast-style group interrupt behavior (dropped entirely)
- Swipe-to-delete/dismiss episode rows (dropped; no stock libadwaita widget)
- Systemd-based process supervision (architecturally rejected)

**Not built yet**: Value4Value boosts (§10), testing with Orca, actual
translations.

