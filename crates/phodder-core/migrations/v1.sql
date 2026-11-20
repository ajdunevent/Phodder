-- The schema as of version 1, the first versioned release. Frozen: used to
-- bring unversioned databases into line, and to test that the migrations
-- in db.rs turn it into schema.sql.

-- ============================================================
-- Groups (subscription groups, and their queue-priority order)
-- ============================================================
-- "Now playing" isn't a group; see the notes at the bottom.

CREATE TABLE groups (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL,
    sort_order  REAL NOT NULL,      -- fractional index: a move is one UPDATE
    -- gdk::RGBA::to_str() output, e.g. "rgb(53,132,228)". NULL until set;
    -- build_window gives the seeded Default group the system accent color.
    color       TEXT,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

-- Every subscription has a group, so a Default group is seeded.
INSERT INTO groups (id, name, sort_order) VALUES (1, 'Default', 1.0);

-- ============================================================
-- Subscriptions
-- ============================================================

CREATE TABLE subscriptions (
    id                      INTEGER PRIMARY KEY,
    feed_url                TEXT NOT NULL UNIQUE,
    title                   TEXT NOT NULL,
    -- 1 while `title` is a custom name, so refreshes leave it alone.
    -- update_subscription_title clears it when the title is set back to
    -- feed_title.
    title_manually_set      INTEGER NOT NULL DEFAULT 0, -- bool
    -- The feed's title, updated on every refresh even when `title` is
    -- custom, so a rename can be reverted. NULL until the first refresh.
    feed_title               TEXT,
    artwork_url             TEXT,
    -- Cached artwork, written by downloader.rs; the UI only reads local
    -- files. NULL until a download succeeds.
    artwork_local_path      TEXT,
    -- Channel <description>, updated on every refresh.
    description             TEXT,
    group_id                INTEGER NOT NULL REFERENCES groups(id) ON DELETE RESTRICT,

    -- per-subscription download automation
    auto_download_enabled   INTEGER NOT NULL DEFAULT 1,   -- bool
    notify_on_new_episodes  INTEGER NOT NULL DEFAULT 0,   -- bool

    -- JSON array of {"pattern", "target": "title"|"description"|"both",
    -- "action": "allow"|"block"}; the first matching rule wins (see
    -- filter::evaluate).
    download_filter_rules   TEXT NOT NULL DEFAULT '[]',

    -- Chapter titles to auto-skip, same rule shape. Not used yet.
    chapter_skip_rules      TEXT NOT NULL DEFAULT '[]',

    -- podcast-namespace fields (optional; absent on most feeds today)
    podcast_medium          TEXT,             -- 'podcast'|'music'|'video'|... or NULL
    funding_url             TEXT,
    funding_label           TEXT,

    -- conditional GET
    etag                    TEXT,
    last_modified           TEXT,
    last_refreshed_at       TEXT,
    -- Last whole-feed parse; a full parse runs daily behind the
    -- early-stopping one (see refresh_subscription).
    last_full_refresh_at    TEXT,

    -- Retention. NULL inherits the `settings` default, except
    -- delete_after_minutes, which every subscription sets itself (new ones
    -- start with the Settings value; 10080 is the fallback).
    delete_after_play        INTEGER,   -- bool
    delete_after_minutes     INTEGER NOT NULL DEFAULT 10080,
                                         -- from episodes.publish_date
    played_threshold_type    TEXT CHECK (played_threshold_type IN ('percent_remaining','seconds_remaining') OR played_threshold_type IS NULL),
    played_threshold_value   REAL,
    size_limit_bytes         INTEGER,   -- NULL = no cap. Evicts oldest
                                         -- downloads first, unplayed too,
                                         -- skipping manual and in-progress ones.
    episode_count_limit      INTEGER,   -- NULL = no cap; evicts the same way.

    -- 0 marks an unsubscribed row kept until its removal is synced (only
    -- when a sync account exists). Episodes are deleted at once, and the
    -- UI shows only active rows. A separate snapshot table was tried and
    -- reverted.
    active                   INTEGER NOT NULL DEFAULT 1,   -- bool
    -- The account this row's `active` state was last reported to.
    -- Compared with the enabled account's id, so switching accounts
    -- reports everything again. Synced tombstones are hard-deleted.
    synced_with_account_id   INTEGER REFERENCES sync_accounts(id) ON DELETE SET NULL,

    created_at              TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

CREATE INDEX idx_subscriptions_group ON subscriptions(group_id);

-- Value4Value recipient splits (podcast:value / podcast:valueRecipient).
-- Subscription-level by default; episode_id lets a specific episode override
-- the subscription's split if a feed defines one at the item level.
CREATE TABLE value_recipients (
    id              INTEGER PRIMARY KEY,
    subscription_id INTEGER NOT NULL REFERENCES subscriptions(id) ON DELETE CASCADE,
    episode_id      INTEGER REFERENCES episodes(id) ON DELETE CASCADE,
    address_type    TEXT NOT NULL,      -- 'node' | 'lnaddress' | 'keysend'
    address         TEXT NOT NULL,
    name            TEXT,               -- e.g. "Host", "Producer"
    split_percent   REAL NOT NULL
);

-- ============================================================
-- Episodes
-- ============================================================

CREATE TABLE episodes (
    id                  INTEGER PRIMARY KEY,
    subscription_id     INTEGER NOT NULL REFERENCES subscriptions(id) ON DELETE CASCADE,
    guid                TEXT NOT NULL,          -- feed-provided GUID, for dedup on refresh
    title               TEXT NOT NULL,
    show_notes_html     TEXT,                   -- content:encoded (preferred) or description
    show_notes_text     TEXT,                   -- HTML-stripped, used by filter matching
    audio_url           TEXT NOT NULL,
    duration_ms         INTEGER,
    publish_date        TEXT NOT NULL,

    -- Episode artwork, usually NULL; cached like the show's.
    artwork_url          TEXT,
    artwork_local_path   TEXT,

    -- podcast-namespace optional companions
    chapters_url        TEXT,
    transcript_url       TEXT,

    -- download state machine
    download_status     TEXT NOT NULL DEFAULT 'none'
                         CHECK (download_status IN
                             ('none','queued','downloading','downloaded','failed')),
    download_progress    REAL,                  -- 0.0-1.0, NULL unless downloading
    retry_count           INTEGER NOT NULL DEFAULT 0,  -- resets to 0 on successful download
    last_error_reason     TEXT,                  -- e.g. 'timeout', 'http_404', 'insufficient_storage';
                                                   -- shown to the user on the episode's status flag
    local_file_path      TEXT,
    file_size_bytes       INTEGER,                -- set on download completion; shown in
                                                   -- episode lists / detail view
    downloaded_at         TEXT,                  -- drives default "queue by download time" sort
    -- Set by downloads the user started from Episodes or History; age
    -- and cap retention skip these (delete-after-play still applies).
    -- Retrying a failed download doesn't set it.
    manually_downloaded   INTEGER NOT NULL DEFAULT 0,   -- bool
    -- A finished download is enqueued unless this is set (download-only).
    -- download_episode reads and clears it, so it can't affect a later
    -- download of the same episode.
    skip_auto_enqueue     INTEGER NOT NULL DEFAULT 0,   -- bool

    -- playback state
    played               INTEGER NOT NULL DEFAULT 0,   -- bool
    -- Finished on this device. Sync sets `played` for episodes finished
    -- elsewhere, which retention should honor but History shouldn't show.
    -- Only phodder-playback's mark_played sets this.
    played_locally       INTEGER NOT NULL DEFAULT 0,   -- bool
    -- History order: an incrementing integer set when an episode is
    -- marked played. NULL if never played.
    played_order         INTEGER,
    playback_position_ms INTEGER NOT NULL DEFAULT 0,
    position_updated_at  TEXT,

    -- Playback queue: NULL = not queued. Fractional index within the
    -- subscription's group, like groups.sort_order.
    group_queue_position REAL,

    created_at           TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),

    UNIQUE(subscription_id, guid)
);

CREATE INDEX idx_episodes_subscription ON episodes(subscription_id);
CREATE INDEX idx_episodes_download_status ON episodes(download_status);
CREATE INDEX idx_episodes_queue ON episodes(group_queue_position)
    WHERE group_queue_position IS NOT NULL;

-- Chapters from podcast:chapters JSON, fetched once after download.
-- Chapters embedded in the file are read at playback instead.
CREATE TABLE chapters (
    id          INTEGER PRIMARY KEY,
    episode_id  INTEGER NOT NULL REFERENCES episodes(id) ON DELETE CASCADE,
    start_ms    INTEGER NOT NULL,
    title       TEXT,
    image_url   TEXT,
    url         TEXT
);

CREATE INDEX idx_chapters_episode ON chapters(episode_id);

-- ============================================================
-- Sync (gpodder protocol: gpodder.net or Nextcloud GPodder Sync)
-- ============================================================

CREATE TABLE sync_accounts (
    id                  INTEGER PRIMARY KEY,
    kind                TEXT NOT NULL
                         CHECK (kind IN ('gpodder_net','nextcloud_gpodder','goa_nextcloud')),
    server_url          TEXT,             -- NULL for gpodder.net (implicit)
    login_name          TEXT NOT NULL,

    -- A lookup key into oo7, never the secret. For goa_nextcloud, the GOA
    -- account id.
    credential_ref       TEXT,

    -- Subscription sync cursor, UTC.
    sync_since_cursor_utc TEXT,
    last_sync_at_utc      TEXT,

    -- Episode-action sync has its own cursor, UTC.
    episode_actions_sync_cursor_utc TEXT,
    episode_actions_last_sync_at_utc TEXT,

    enabled              INTEGER NOT NULL DEFAULT 1
);

-- Episode actions waiting to be pushed; marked pushed once the server
-- accepts them. Timestamps are UTC.
CREATE TABLE sync_outbox (
    id              INTEGER PRIMARY KEY,
    sync_account_id INTEGER NOT NULL REFERENCES sync_accounts(id) ON DELETE CASCADE,
    episode_id      INTEGER NOT NULL REFERENCES episodes(id) ON DELETE CASCADE,
    action_type     TEXT NOT NULL CHECK (action_type IN ('play','download','delete','new')),
    position_ms     INTEGER,                 -- only meaningful for 'play'
    action_at_utc   TEXT NOT NULL,            -- UTC, always
    pushed          INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX idx_sync_outbox_pending ON sync_outbox(sync_account_id) WHERE pushed = 0;

-- ============================================================
-- Value4Value boosts. Not used yet.
-- ============================================================

CREATE TABLE boosts_sent (
    id           INTEGER PRIMARY KEY,
    episode_id   INTEGER NOT NULL REFERENCES episodes(id) ON DELETE CASCADE,
    amount_sats  INTEGER NOT NULL,
    message      TEXT,
    sent_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    succeeded    INTEGER NOT NULL             -- bool; NWC payment result
);

-- ============================================================
-- App-wide settings (key/value)
-- ============================================================

CREATE TABLE settings (
    key    TEXT PRIMARY KEY,
    value  TEXT NOT NULL
);

-- ============================================================
-- Queue notes:
--
-- "Now playing" is held by the playback engine, not stored. Its
-- position is saved to playback_position_ms.
--
-- "What plays next" = episodes JOIN subscriptions JOIN groups
-- WHERE group_queue_position IS NOT NULL
-- ORDER BY groups.sort_order, episodes.group_queue_position LIMIT 1.
-- ============================================================
