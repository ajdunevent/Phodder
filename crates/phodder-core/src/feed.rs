//! Turns feed bytes into structured data; downloader.rs does the HTTP fetch.
//! RSS 2.0 and iTunes fields come from the `rss` crate; Podcasting 2.0
//! chapters and transcript URLs from its namespaced-extension map.
//!
//! `parse_feed_early_stop` walks items with quick_xml and stops when the
//! caller says so, reusing `parse_episode` for each item through a synthetic
//! envelope.

use chrono::{DateTime, Utc};

#[derive(Debug, thiserror::Error)]
pub enum FeedParseError {
    #[error("XML/RSS parse error: {0}")]
    Rss(#[from] rss::Error),
    #[error("feed is missing a required field: {0}")]
    MissingField(&'static str),
    #[error("episode is missing a required field: {0} (guid: {1:?})")]
    MissingEpisodeField(&'static str, Option<String>),
}

#[derive(Debug, Clone)]
pub struct ParsedFeed {
    pub title: String,
    pub artwork_url: Option<String>,
    /// `None` when missing or empty; some feeds have an empty
    /// `<description></description>`.
    pub description: Option<String>,
    pub episodes: Vec<ParsedEpisode>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ParsedEpisode {
    pub guid: String,
    pub title: String,
    /// `content:encoded` when present (usually the richest), else
    /// `<description>` or `itunes:summary`.
    pub show_notes_html: Option<String>,
    pub audio_url: String,
    /// The enclosure's declared `length`. `None` when missing or not a
    /// number, which happens.
    pub file_size_bytes: Option<i64>,
    pub duration_ms: Option<i64>,
    pub publish_date: DateTime<Utc>,
    pub chapters_url: Option<String>,
    pub transcript_url: Option<String>,
    /// Per-episode `<itunes:image>`; usually `None`.
    pub artwork_url: Option<String>,
}

pub fn parse_feed(bytes: &[u8]) -> Result<ParsedFeed, FeedParseError> {
    let channel = rss::Channel::read_from(bytes)?;

    let artwork_url = channel
        .itunes_ext()
        .and_then(|it| it.image())
        .map(str::to_string)
        .or_else(|| channel.image().map(|img| img.url().to_string()));

    let description = {
        let desc = channel.description();
        if desc.trim().is_empty() {
            None
        } else {
            Some(desc.to_string())
        }
    };

    let episodes = channel
        .items()
        .iter()
        .filter_map(|item| match parse_episode(item) {
            Ok(ep) => Some(ep),
            Err(e) => {
                // One malformed item is skipped, not the whole feed.
                tracing::warn!("skipping unparseable feed item: {e}");
                None
            }
        })
        .collect();

    Ok(ParsedFeed {
        title: channel.title().to_string(),
        artwork_url,
        description,
        episodes,
    })
}

fn parse_episode(item: &rss::Item) -> Result<ParsedEpisode, FeedParseError> {
    let guid = item
        .guid()
        .map(|g| g.value().to_string())
        .or_else(|| item.link().map(str::to_string))
        .ok_or(FeedParseError::MissingEpisodeField("guid", None))?;

    let title = item
        .title()
        .map(str::to_string)
        .ok_or_else(|| FeedParseError::MissingEpisodeField("title", Some(guid.clone())))?;

    let audio_url = item
        .enclosure()
        .map(|e| e.url().to_string())
        .ok_or_else(|| FeedParseError::MissingEpisodeField("enclosure/audio_url", Some(guid.clone())))?;

    let file_size_bytes = item.enclosure().and_then(|e| e.length().parse::<i64>().ok());

    // See ParsedEpisode::show_notes_html.
    let show_notes_html = item
        .content()
        .map(str::to_string)
        .or_else(|| item.description().map(str::to_string))
        .or_else(|| item.itunes_ext().and_then(|it| it.summary()).map(str::to_string));

    let duration_ms = item
        .itunes_ext()
        .and_then(|it| it.duration())
        .and_then(parse_itunes_duration_to_ms);

    let publish_date = item
        .pub_date()
        .and_then(|d| DateTime::parse_from_rfc2822(d).ok())
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|| {
            // publish_date only orders episodes, so a missing one falls back
            // to now.
            tracing::warn!("episode {guid:?} has a missing or unparseable pubDate, defaulting to now");
            Utc::now()
        });

    let (chapters_url, transcript_url) = parse_podcast_namespace_extensions(item);

    let artwork_url = item.itunes_ext().and_then(|it| it.image()).map(str::to_string);

    Ok(ParsedEpisode {
        guid,
        title,
        show_notes_html,
        audio_url,
        file_size_bytes,
        duration_ms,
        publish_date,
        chapters_url,
        transcript_url,
        artwork_url,
    })
}

/// `itunes:duration` as plain seconds ("1958") or H:MM:SS / MM:SS ("32:37",
/// "1:32:37"); feeds use both.
fn parse_itunes_duration_to_ms(raw: &str) -> Option<i64> {
    let raw = raw.trim();
    if let Ok(seconds) = raw.parse::<i64>() {
        return Some(seconds * 1000);
    }
    let parts: Vec<&str> = raw.split(':').collect();
    let mut seconds: i64 = 0;
    for part in &parts {
        seconds = seconds * 60 + part.parse::<i64>().ok()?;
    }
    Some(seconds * 1000)
}

/// `(chapters_url, transcript_url)`, `None` when the feed doesn't use the
/// podcast namespace.
fn parse_podcast_namespace_extensions(item: &rss::Item) -> (Option<String>, Option<String>) {
    let extensions = item.extensions();
    let Some(podcast_ns) = extensions.get("podcast") else {
        return (None, None);
    };

    let chapters_url = podcast_ns
        .get("chapters")
        .and_then(|exts| exts.first())
        .and_then(|ext| ext.attrs().get("url"))
        .cloned();

    // Feeds often list several formats; timed ones are preferred.
    let transcript_url = podcast_ns
        .get("transcript")
        .and_then(|exts| {
            exts.iter()
                .filter(|ext| ext.attrs().contains_key("url"))
                .min_by_key(|ext| crate::transcript::type_preference(ext.attrs().get("type").map(String::as_str).unwrap_or("")))
        })
        .and_then(|ext| ext.attrs().get("url"))
        .cloned();

    (chapters_url, transcript_url)
}

/// Namespaces the item and channel fragments may use.
const SYNTHETIC_RSS_HEADER: &[u8] = br#"<rss version="2.0" xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd" xmlns:content="http://purl.org/rss/1.0/modules/content/" xmlns:podcast="https://podcastindex.org/namespace/1.0" xmlns:atom="http://www.w3.org/2005/Atom"><channel>"#;
const SYNTHETIC_RSS_FOOTER: &[u8] = b"</channel></rss>";

#[derive(Debug, Clone, Default)]
pub struct ChannelMetadata {
    pub title: Option<String>,
    pub artwork_url: Option<String>,
    pub description: Option<String>,
}

pub enum EarlyStopOutcome {
    /// Stopped where `should_stop` said to. Episodes are in feed order; the
    /// caller's stop condition, not this function, relies on newest-first.
    Stopped { episodes: Vec<ParsedEpisode>, metadata: ChannelMetadata },
    /// `should_stop` never returned true.
    ExhaustedFeed { episodes: Vec<ParsedEpisode>, metadata: ChannelMetadata },
    /// Something outside this parser's scope: fall back to `parse_feed`. A
    /// gap here means slow but correct, never wrong data.
    Unsupported,
}

/// Walks the already-downloaded bytes item by item with quick_xml, stopping
/// when `should_stop` says to.
///
/// Field extraction reuses `parse_episode`: each item's inner XML (located
/// with `read_to_end_into`, which handles nested same-named elements) is
/// wrapped in a synthetic `<rss><channel><item>` envelope and parsed with
/// `rss::Channel::read_from`. Channel metadata comes the same way from the
/// bytes before the first item.
///
/// Handles RSS 2.0 with `<channel>` and `<item>` only. Another root (RSS 1.0
/// RDF), a quick_xml error, or a fragment the `rss` crate can't read returns
/// `Unsupported`.
pub fn parse_feed_early_stop(bytes: &[u8], mut should_stop: impl FnMut(&ParsedEpisode) -> bool) -> EarlyStopOutcome {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;

    let mut reader = Reader::from_reader(bytes);
    let mut event_buf = Vec::new();
    let mut scratch = Vec::new();

    let channel_start = loop {
        match reader.read_event_into(&mut event_buf) {
            Ok(Event::Start(e)) if e.name().as_ref() == b"channel" => break reader.buffer_position(),
            Ok(Event::Eof) => return EarlyStopOutcome::Unsupported,
            Ok(_) => {}
            Err(_) => return EarlyStopOutcome::Unsupported,
        }
        event_buf.clear();
    };

    let mut episodes = Vec::new();
    let mut metadata: Option<ChannelMetadata> = None;

    loop {
        event_buf.clear();
        // Position before this event: where the first <item> starts, when
        // that's next.
        let pos_before_event = reader.buffer_position();
        match reader.read_event_into(&mut event_buf) {
            Ok(Event::Start(e)) if e.name().as_ref() == b"item" => {
                if metadata.is_none() {
                    // Channel metadata precedes the first <item>.
                    metadata = Some(extract_channel_metadata(&bytes[channel_start as usize..pos_before_event as usize]));
                }
                scratch.clear();
                match reader.read_to_end_into(e.name(), &mut scratch) {
                    Ok(span) => {
                        let item_inner = &bytes[span.start as usize..span.end as usize];
                        match parse_single_item(item_inner) {
                            ItemParse::Episode(episode) => {
                                let stop = should_stop(&episode);
                                episodes.push(episode);
                                if stop {
                                    return EarlyStopOutcome::Stopped { episodes, metadata: metadata.unwrap_or_default() };
                                }
                            }
                            ItemParse::MissingField => {}
                            ItemParse::Unparseable => return EarlyStopOutcome::Unsupported,
                        }
                    }
                    Err(_) => return EarlyStopOutcome::Unsupported,
                }
            }
            Ok(Event::End(e)) if e.name().as_ref() == b"channel" => {
                if metadata.is_none() {
                    // No items: everything here is metadata.
                    metadata = Some(extract_channel_metadata(&bytes[channel_start as usize..pos_before_event as usize]));
                }
                return EarlyStopOutcome::ExhaustedFeed { episodes, metadata: metadata.unwrap_or_default() };
            }
            Ok(Event::Eof) => return EarlyStopOutcome::Unsupported,
            Ok(_) => {}
            Err(_) => return EarlyStopOutcome::Unsupported,
        }
    }
}

/// Parses channel metadata from a fragment through a synthetic document.
/// Returns defaults on failure: episodes can still be processed without a
/// title this refresh.
fn extract_channel_metadata(fragment: &[u8]) -> ChannelMetadata {
    let mut envelope = Vec::with_capacity(fragment.len() + SYNTHETIC_RSS_HEADER.len() + SYNTHETIC_RSS_FOOTER.len());
    envelope.extend_from_slice(SYNTHETIC_RSS_HEADER);
    envelope.extend_from_slice(fragment);
    envelope.extend_from_slice(SYNTHETIC_RSS_FOOTER);

    let Ok(channel) = rss::Channel::read_from(&envelope[..]) else {
        return ChannelMetadata::default();
    };
    let artwork_url = channel
        .itunes_ext()
        .and_then(|it| it.image())
        .map(str::to_string)
        .or_else(|| channel.image().map(|img| img.url().to_string()));
    let description = {
        let desc = channel.description();
        if desc.trim().is_empty() { None } else { Some(desc.to_string()) }
    };
    ChannelMetadata { title: Some(channel.title().to_string()), artwork_url, description }
}

/// Parses one item's inner XML (without its `<item>` tags) through a
/// synthetic document with `parse_episode`.
///
/// An item missing a required field (no enclosure, say) is skipped, as
/// `parse_feed` skips it. An item whose XML doesn't parse makes the caller
/// fall back to a full parse: an early-stopped list silently missing a real
/// episode is worse than a full parse's isolated skip.
enum ItemParse {
    Episode(ParsedEpisode),
    MissingField,
    Unparseable,
}

fn parse_single_item(item_inner: &[u8]) -> ItemParse {
    let mut envelope =
        Vec::with_capacity(item_inner.len() + SYNTHETIC_RSS_HEADER.len() + SYNTHETIC_RSS_FOOTER.len() + 13);
    envelope.extend_from_slice(SYNTHETIC_RSS_HEADER);
    envelope.extend_from_slice(b"<item>");
    envelope.extend_from_slice(item_inner);
    envelope.extend_from_slice(b"</item>");
    envelope.extend_from_slice(SYNTHETIC_RSS_FOOTER);

    let Ok(channel) = rss::Channel::read_from(&envelope[..]) else { return ItemParse::Unparseable };
    let Some(item) = channel.items().first() else { return ItemParse::Unparseable };
    match parse_episode(item) {
        Ok(episode) => ItemParse::Episode(episode),
        Err(FeedParseError::MissingEpisodeField(..)) => ItemParse::MissingField,
        Err(_) => ItemParse::Unparseable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_FEED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd">
  <channel>
    <title>Test Podcast</title>
    <description>A podcast about testing podcast parsers.</description>
    <itunes:image href="https://example.com/art.jpg"/>
    <item>
      <title>Episode One</title>
      <guid>ep-1</guid>
      <description>Show notes here</description>
      <enclosure url="https://example.com/ep1.mp3" length="123" type="audio/mpeg"/>
      <pubDate>Wed, 02 Oct 2024 15:00:00 GMT</pubDate>
      <itunes:duration>32:37</itunes:duration>
      <itunes:image href="https://example.com/ep1-art.jpg"/>
    </item>
    <item>
      <title>Episode Two (plain seconds duration)</title>
      <guid>ep-2</guid>
      <enclosure url="https://example.com/ep2.mp3" length="456" type="audio/mpeg"/>
      <pubDate>Thu, 03 Oct 2024 15:00:00 GMT</pubDate>
      <itunes:duration>1958</itunes:duration>
    </item>
    <item>
      <title>Episode Missing Enclosure (should be skipped, not fail the whole parse)</title>
      <guid>ep-3</guid>
      <pubDate>Fri, 04 Oct 2024 15:00:00 GMT</pubDate>
    </item>
  </channel>
</rss>"#;

    const SAMPLE_FEED_NO_DESCRIPTION: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>Minimal Podcast</title>
    <description></description>
    <item>
      <title>Only Episode</title>
      <guid>ep-1</guid>
      <enclosure url="https://example.com/ep1.mp3" length="123" type="audio/mpeg"/>
      <pubDate>Wed, 02 Oct 2024 15:00:00 GMT</pubDate>
    </item>
  </channel>
</rss>"#;

    #[test]
    fn parses_title_and_artwork() {
        let feed = parse_feed(SAMPLE_FEED.as_bytes()).unwrap();
        assert_eq!(feed.title, "Test Podcast");
        assert_eq!(feed.artwork_url.as_deref(), Some("https://example.com/art.jpg"));
    }

    #[test]
    fn parses_channel_description() {
        let feed = parse_feed(SAMPLE_FEED.as_bytes()).unwrap();
        assert_eq!(feed.description.as_deref(), Some("A podcast about testing podcast parsers."));
    }

    #[test]
    fn empty_channel_description_parses_as_none_not_an_empty_string() {
        let feed = parse_feed(SAMPLE_FEED_NO_DESCRIPTION.as_bytes()).unwrap();
        assert_eq!(feed.description, None);
    }

    #[test]
    fn parses_per_episode_artwork_when_present_and_none_when_absent() {
        let feed = parse_feed(SAMPLE_FEED.as_bytes()).unwrap();
        let ep1 = feed.episodes.iter().find(|e| e.guid == "ep-1").unwrap();
        assert_eq!(ep1.artwork_url.as_deref(), Some("https://example.com/ep1-art.jpg"));
        let ep2 = feed.episodes.iter().find(|e| e.guid == "ep-2").unwrap();
        assert_eq!(ep2.artwork_url, None);
    }

    #[test]
    fn parses_valid_episodes_and_skips_the_malformed_one() {
        let feed = parse_feed(SAMPLE_FEED.as_bytes()).unwrap();
        // One of three items lacks a required field: two parse.
        assert_eq!(feed.episodes.len(), 2);
    }

    #[test]
    fn parses_declared_file_size_from_enclosure_length() {
        let feed = parse_feed(SAMPLE_FEED.as_bytes()).unwrap();
        let ep1 = feed.episodes.iter().find(|e| e.guid == "ep-1").unwrap();
        assert_eq!(ep1.file_size_bytes, Some(123));
        let ep2 = feed.episodes.iter().find(|e| e.guid == "ep-2").unwrap();
        assert_eq!(ep2.file_size_bytes, Some(456));
    }

    #[test]
    fn parses_hms_duration_format() {
        let feed = parse_feed(SAMPLE_FEED.as_bytes()).unwrap();
        let ep1 = feed.episodes.iter().find(|e| e.guid == "ep-1").unwrap();
        assert_eq!(ep1.duration_ms, Some((32 * 60 + 37) * 1000));
    }

    #[test]
    fn parses_plain_seconds_duration_format() {
        let feed = parse_feed(SAMPLE_FEED.as_bytes()).unwrap();
        let ep2 = feed.episodes.iter().find(|e| e.guid == "ep-2").unwrap();
        assert_eq!(ep2.duration_ms, Some(1958 * 1000));
    }

    #[test]
    fn parses_publish_date_from_rfc2822() {
        let feed = parse_feed(SAMPLE_FEED.as_bytes()).unwrap();
        let ep1 = feed.episodes.iter().find(|e| e.guid == "ep-1").unwrap();
        assert_eq!(ep1.publish_date.to_rfc3339(), "2024-10-02T15:00:00+00:00");
    }

    #[test]
    fn duration_parser_handles_all_three_formats_directly() {
        assert_eq!(parse_itunes_duration_to_ms("1958"), Some(1958000));
        assert_eq!(parse_itunes_duration_to_ms("32:37"), Some(1957000));
        assert_eq!(parse_itunes_duration_to_ms("1:32:37"), Some(5557000));
        assert_eq!(parse_itunes_duration_to_ms("not a duration"), None);
    }

    #[test]
    fn feed_with_no_podcast_namespace_returns_none_for_both_urls_not_an_error() {
        let feed = parse_feed(SAMPLE_FEED.as_bytes()).unwrap();
        let ep1 = feed.episodes.iter().find(|e| e.guid == "ep-1").unwrap();
        assert_eq!(ep1.chapters_url, None);
        assert_eq!(ep1.transcript_url, None);
    }

    #[test]
    fn invalid_xml_is_a_parse_error() {
        let result = parse_feed(b"not xml at all");
        assert!(result.is_err());
    }

    #[test]
    fn early_stop_stops_at_the_requested_episode_not_the_whole_feed() {
        let mut seen = 0;
        let outcome = parse_feed_early_stop(SAMPLE_FEED.as_bytes(), |_| {
            seen += 1;
            seen == 1 // Stop after the first (newest) episode.
        });
        match outcome {
            EarlyStopOutcome::Stopped { episodes, .. } => {
                assert_eq!(episodes.len(), 1);
                assert_eq!(episodes[0].guid, "ep-1");
            }
            _ => panic!("expected Stopped, got a different outcome"),
        }
    }

    #[test]
    fn early_stop_exhausts_the_feed_when_should_stop_never_fires() {
        let outcome = parse_feed_early_stop(SAMPLE_FEED.as_bytes(), |_| false);
        match outcome {
            // The third item lacks an enclosure, as in parse_feed's test.
            EarlyStopOutcome::ExhaustedFeed { episodes, .. } => assert_eq!(episodes.len(), 2),
            _ => panic!("expected ExhaustedFeed, got a different outcome"),
        }
    }

    #[test]
    fn early_stop_extracts_the_same_channel_metadata_as_the_full_parse() {
        let full = parse_feed(SAMPLE_FEED.as_bytes()).unwrap();
        let EarlyStopOutcome::ExhaustedFeed { metadata, .. } = parse_feed_early_stop(SAMPLE_FEED.as_bytes(), |_| false)
        else {
            panic!("expected ExhaustedFeed");
        };
        assert_eq!(metadata.title.as_deref(), Some(full.title.as_str()));
        assert_eq!(metadata.artwork_url, full.artwork_url);
        assert_eq!(metadata.description, full.description);
    }

    #[test]
    fn early_stop_extracts_identical_per_episode_fields_to_the_full_parse() {
        // The envelope approach is only sound if parsing an item alone
        // matches parsing it in the full document.
        let full = parse_feed(SAMPLE_FEED.as_bytes()).unwrap();
        let EarlyStopOutcome::ExhaustedFeed { episodes: early, .. } =
            parse_feed_early_stop(SAMPLE_FEED.as_bytes(), |_| false)
        else {
            panic!("expected ExhaustedFeed");
        };
        assert_eq!(early.len(), full.episodes.len());
        for (a, b) in early.iter().zip(full.episodes.iter()) {
            assert_eq!(a.guid, b.guid);
            assert_eq!(a.title, b.title);
            assert_eq!(a.show_notes_html, b.show_notes_html);
            assert_eq!(a.audio_url, b.audio_url);
            assert_eq!(a.file_size_bytes, b.file_size_bytes);
            assert_eq!(a.duration_ms, b.duration_ms);
            assert_eq!(a.publish_date, b.publish_date);
            assert_eq!(a.artwork_url, b.artwork_url);
        }
    }

    #[test]
    fn early_stop_falls_back_to_unsupported_on_invalid_xml() {
        let outcome = parse_feed_early_stop(b"not xml at all", |_| false);
        assert!(matches!(outcome, EarlyStopOutcome::Unsupported));
    }

    #[test]
    fn early_stop_falls_back_to_unsupported_when_theres_no_channel_element() {
        let outcome = parse_feed_early_stop(b"<rss version=\"2.0\"></rss>", |_| false);
        assert!(matches!(outcome, EarlyStopOutcome::Unsupported));
    }
    #[test]
    fn podcast_namespace_chapters_and_transcript_urls_are_extracted() {
        let feed = br#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:podcast="https://podcastindex.org/namespace/1.0">
  <channel>
    <title>Namespaced</title>
    <description>d</description>
    <item>
      <title>Ep</title>
      <guid>ns-1</guid>
      <pubDate>Mon, 02 Jan 2026 00:00:00 GMT</pubDate>
      <enclosure url="https://example.com/ep.mp3" length="1" type="audio/mpeg"/>
      <podcast:chapters url="https://example.com/ep.chapters.json" type="application/json+chapters"/>
      <podcast:transcript url="https://example.com/ep.html" type="text/html"/>
      <podcast:transcript url="https://example.com/ep.vtt" type="text/vtt"/>
    </item>
  </channel>
</rss>"#;
        let parsed = parse_feed(feed).unwrap();
        let ep = &parsed.episodes[0];
        assert_eq!(ep.chapters_url.as_deref(), Some("https://example.com/ep.chapters.json"));
        assert_eq!(ep.transcript_url.as_deref(), Some("https://example.com/ep.vtt"));
    }
}
