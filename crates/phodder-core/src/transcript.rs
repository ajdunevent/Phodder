//! Podcasting 2.0 transcripts (`podcast:transcript`): WebVTT, SRT, the
//! Podcast Index JSON format, HTML and plain text. The format is detected
//! from the content, since feeds' `type` attributes vary.
//!
//! Timed formats are regrouped into readable lines: JSON transcripts are
//! often one segment per word, and caption files often break mid-sentence.

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub start_ms: u64,
    pub end_ms: u64,
    pub speaker: Option<String>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Transcript {
    pub lines: Vec<Line>,
    /// False for HTML and plain text, whose lines have no times (all 0).
    pub timed: bool,
}

/// Preference among `podcast:transcript` types when a feed lists several:
/// timed formats first. Lower is better; unknown types sort last.
pub fn type_preference(mime_type: &str) -> u8 {
    match mime_type.trim().to_ascii_lowercase().as_str() {
        "text/vtt" => 0,
        "application/json" => 1,
        "application/x-subrip" | "application/srt" | "text/srt" => 2,
        "text/html" => 3,
        "text/plain" => 4,
        _ => 5,
    }
}

impl Transcript {
    /// The line playing at `position_ms`: the last one started by then.
    /// None before the first line, and always None when untimed.
    pub fn line_at(&self, position_ms: u64) -> Option<usize> {
        if !self.timed {
            return None;
        }
        match self.lines.partition_point(|line| line.start_ms <= position_ms) {
            0 => None,
            n => Some(n - 1),
        }
    }
}

/// None when there's nothing readable.
pub fn parse(text: &str) -> Option<Transcript> {
    let text = text.trim_start_matches('\u{feff}').trim();
    let transcript = if text.starts_with("WEBVTT") {
        timed(parse_cues(text, false))
    } else if text.starts_with('{') {
        timed(parse_json(text)?)
    } else if looks_like_srt(text) {
        timed(parse_cues(text, true))
    } else if text.starts_with('<') {
        untimed(&html_to_paragraphs(text))
    } else {
        untimed(&text.split("\n\n").map(str::to_string).collect::<Vec<_>>())
    };
    (!transcript.lines.is_empty()).then_some(transcript)
}

fn timed(segments: Vec<Line>) -> Transcript {
    Transcript { lines: merge_segments(segments), timed: true }
}

fn untimed(paragraphs: &[String]) -> Transcript {
    let lines = paragraphs
        .iter()
        .map(|p| collapse_whitespace(p))
        .filter(|p| !p.is_empty())
        .map(|text| Line { start_ms: 0, end_ms: 0, speaker: None, text })
        .collect();
    Transcript { lines, timed: false }
}

fn looks_like_srt(text: &str) -> bool {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    matches!((lines.next(), lines.next()), (Some(index), Some(timing)) if index.chars().all(|c| c.is_ascii_digit()) && timing.contains("-->"))
}

/// WebVTT and SRT: blocks separated by blank lines, each with a timing line
/// and text after it. VTT's NOTE, STYLE and REGION blocks have no timing
/// line and are skipped.
fn parse_cues(text: &str, srt: bool) -> Vec<Line> {
    let normalized = text.replace("\r\n", "\n");
    let mut cues = Vec::new();
    for block in normalized.split("\n\n") {
        let mut lines = block.lines();
        let Some(timing_index) = block.lines().position(|l| l.contains("-->")) else { continue };
        let timing = lines.nth(timing_index).unwrap_or_default();
        let mut parts = timing.split("-->");
        let (Some(start), Some(end)) = (parts.next(), parts.next()) else { continue };
        // VTT cue settings follow the end time.
        let end = end.split_whitespace().next().unwrap_or("");
        let (Some(start_ms), Some(end_ms)) = (parse_timestamp(start.trim()), parse_timestamp(end)) else { continue };
        let raw: Vec<&str> = lines.collect();
        if raw.is_empty() {
            continue;
        }
        let raw = raw.join(" ");
        let (speaker, body) = if srt { (None, raw.as_str()) } else { split_voice_tag(&raw) };
        let text = collapse_whitespace(&decode_entities(&strip_tags(body)));
        if !text.is_empty() {
            cues.push(Line { start_ms, end_ms, speaker, text });
        }
    }
    cues
}

/// `<v Speaker Name>text` or `<v.class Speaker>text`.
fn split_voice_tag(text: &str) -> (Option<String>, &str) {
    let Some(rest) = text.trim_start().strip_prefix("<v") else { return (None, text) };
    let Some(close) = rest.find('>') else { return (None, text) };
    let tag = &rest[..close];
    let name = tag.split_once(char::is_whitespace).map(|(_, name)| name.trim()).unwrap_or("");
    let speaker = (!name.is_empty()).then(|| decode_entities(name));
    (speaker, &rest[close + 1..])
}

/// "01:02:03.456", "02:03.456" or SRT's "01:02:03,456".
fn parse_timestamp(text: &str) -> Option<u64> {
    let text = text.replace(',', ".");
    let (clock, fraction) = text.split_once('.').unwrap_or((&text, "0"));
    let parts: Vec<u64> = clock.split(':').map(|p| p.trim().parse().ok()).collect::<Option<_>>()?;
    let seconds = match parts.as_slice() {
        [h, m, s] => h * 3600 + m * 60 + s,
        [m, s] => m * 60 + s,
        _ => return None,
    };
    let fraction: String = fraction.chars().take(3).collect();
    let millis = format!("{fraction:0<3}").parse::<u64>().ok()?;
    Some(seconds * 1000 + millis)
}

#[derive(Deserialize)]
struct JsonTranscript {
    segments: Vec<JsonSegment>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JsonSegment {
    #[serde(default)]
    speaker: Option<String>,
    start_time: f64,
    #[serde(default)]
    end_time: Option<f64>,
    body: String,
}

fn parse_json(text: &str) -> Option<Vec<Line>> {
    let transcript: JsonTranscript = serde_json::from_str(text).ok()?;
    Some(
        transcript
            .segments
            .into_iter()
            .filter(|s| !s.body.trim().is_empty())
            .map(|s| {
                let start_ms = (s.start_time.max(0.0) * 1000.0) as u64;
                Line {
                    start_ms,
                    end_ms: s.end_time.map(|t| (t.max(0.0) * 1000.0) as u64).unwrap_or(start_ms),
                    speaker: s.speaker.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()),
                    text: collapse_whitespace(&s.body),
                }
            })
            .collect(),
    )
}

/// Lines end at a sentence end once they have a few words, at a clause end
/// once they're long, at a speaker change, at a pause, or at a hard limit.
const MIN_WORDS_BEFORE_SENTENCE_BREAK: usize = 6;
const MIN_WORDS_BEFORE_CLAUSE_BREAK: usize = 18;
const MAX_WORDS: usize = 30;
const MAX_SPAN_MS: u64 = 15_000;
const PAUSE_MS: u64 = 2_000;

fn merge_segments(segments: Vec<Line>) -> Vec<Line> {
    let mut lines: Vec<Line> = Vec::new();
    let mut current: Option<Line> = None;
    for segment in segments {
        if let Some(line) = &mut current {
            let words = line.text.split_whitespace().count();
            let ends_sentence = (line.text.ends_with(['.', '?', '!', '…', '"', '”']) && words >= MIN_WORDS_BEFORE_SENTENCE_BREAK)
                || (line.text.ends_with([',', ';', ':', '—']) && words >= MIN_WORDS_BEFORE_CLAUSE_BREAK);
            let new_speaker = segment.speaker.is_some() && segment.speaker != line.speaker;
            let paused = segment.start_ms > line.end_ms + PAUSE_MS;
            let too_long = words >= MAX_WORDS || segment.start_ms.saturating_sub(line.start_ms) > MAX_SPAN_MS;
            if !(ends_sentence || new_speaker || paused || too_long) {
                // Word-level JSON puts punctuation in its own segments.
                if !segment.text.starts_with([',', '.', '?', '!', ';', ':']) {
                    line.text.push(' ');
                }
                line.text.push_str(&segment.text);
                line.end_ms = line.end_ms.max(segment.end_ms);
                continue;
            }
        }
        if let Some(line) = current.take() {
            lines.push(line);
        }
        current = Some(segment);
    }
    lines.extend(current);
    lines
}

/// Paragraphs from HTML: a break at each block-level tag.
fn html_to_paragraphs(html: &str) -> Vec<String> {
    let mut paragraphs = Vec::new();
    let mut current = String::new();
    let mut rest = html;
    while let Some(open) = rest.find('<') {
        current.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('>') else { break };
        let tag = rest[open + 1..open + close].trim_start_matches('/').to_ascii_lowercase();
        let name = tag.split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or("");
        if matches!(name, "p" | "br" | "div" | "li" | "h1" | "h2" | "h3" | "h4" | "tr") {
            paragraphs.push(std::mem::take(&mut current));
        } else {
            current.push(' ');
        }
        rest = &rest[open + close + 1..];
    }
    current.push_str(rest);
    paragraphs.push(current);
    paragraphs.into_iter().map(|p| decode_entities(&p)).collect()
}

fn strip_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for c in text.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn decode_entities(text: &str) -> String {
    text.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vtt_cues_with_voices_become_lines_split_at_speaker_changes() {
        let vtt = "WEBVTT\n\nNOTE a comment\n\n1\n00:00:01.000 --> 00:00:03.500 align:start\n<v Alice>Welcome back to the show.\n\n00:00:03.500 --> 00:00:06.000\n<v Bob>Thanks for having me &amp; my dog.\n";
        let t = parse(vtt).unwrap();
        assert!(t.timed);
        assert_eq!(t.lines.len(), 2);
        assert_eq!(t.lines[0], Line { start_ms: 1000, end_ms: 3500, speaker: Some("Alice".into()), text: "Welcome back to the show.".into() });
        assert_eq!(t.lines[1].speaker.as_deref(), Some("Bob"));
        assert_eq!(t.lines[1].text, "Thanks for having me & my dog.");
    }

    #[test]
    fn mid_sentence_caption_cues_are_joined() {
        let vtt = "WEBVTT\n\n00:01.000 --> 00:02.000\nso what I was\n\n00:02.000 --> 00:03.000\ngoing to say is this.\n\n00:03.000 --> 00:04.000\nNext thought here, which continues.\n";
        let t = parse(vtt).unwrap();
        assert_eq!(t.lines.len(), 2);
        assert_eq!(t.lines[0].text, "so what I was going to say is this.");
        assert_eq!((t.lines[0].start_ms, t.lines[0].end_ms), (1000, 3000));
    }

    #[test]
    fn srt_times_use_commas() {
        let srt = "1\r\n00:00:01,250 --> 00:00:02,000\r\n<i>Hello there, everyone listening today.</i>\r\n\r\n2\r\n01:00:00,000 --> 01:00:01,000\r\nAn hour later.\r\n";
        let t = parse(srt).unwrap();
        assert_eq!(t.lines[0].start_ms, 1250);
        assert_eq!(t.lines[0].text, "Hello there, everyone listening today.");
        assert_eq!(t.lines[1].start_ms, 3_600_000);
    }

    #[test]
    fn word_level_json_is_grouped_into_sentences() {
        let json = r#"{"version":"1.0.0","segments":[
            {"speaker":"Ann","startTime":0.0,"endTime":0.4,"body":"This"},
            {"speaker":"Ann","startTime":0.4,"endTime":0.6,"body":"is"},
            {"speaker":"Ann","startTime":0.6,"endTime":0.9,"body":"the"},
            {"speaker":"Ann","startTime":0.9,"endTime":1.2,"body":"first"},
            {"speaker":"Ann","startTime":1.2,"endTime":1.5,"body":"full"},
            {"speaker":"Ann","startTime":1.5,"endTime":1.9,"body":"sentence"},
            {"speaker":"Ann","startTime":1.9,"endTime":2.0,"body":"."},
            {"speaker":"Ann","startTime":2.1,"endTime":2.4,"body":"Second"},
            {"speaker":"Ben","startTime":2.5,"endTime":2.9,"body":"Hi"}
        ]}"#;
        let t = parse(json).unwrap();
        let texts: Vec<&str> = t.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["This is the first full sentence.", "Second", "Hi"]);
        assert_eq!(t.lines[2].speaker.as_deref(), Some("Ben"));
        assert_eq!(t.lines[1].start_ms, 2100);
    }

    #[test]
    fn a_pause_starts_a_new_line() {
        let vtt = "WEBVTT\n\n00:01.000 --> 00:02.000\nbefore the break\n\n00:09.000 --> 00:10.000\nafter it\n";
        assert_eq!(parse(vtt).unwrap().lines.len(), 2);
    }

    #[test]
    fn html_and_text_are_untimed_paragraphs() {
        let html = "<p><cite>Ann:</cite> Hello &amp; welcome.</p><p>Second   paragraph.</p>";
        let t = parse(html).unwrap();
        assert!(!t.timed);
        let texts: Vec<&str> = t.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["Ann: Hello & welcome.", "Second paragraph."]);
        assert_eq!(parse("One.\n\nTwo.").unwrap().lines.len(), 2);
        assert!(parse("   ").is_none());
    }

    #[test]
    fn line_at_finds_the_last_started_line() {
        let vtt = "WEBVTT\n\n00:01.000 --> 00:02.000\nOne line that ends here.\n\n00:05.000 --> 00:06.000\nAnother line that ends here.\n";
        let t = parse(vtt).unwrap();
        assert_eq!(t.line_at(0), None);
        assert_eq!(t.line_at(1000), Some(0));
        assert_eq!(t.line_at(4999), Some(0));
        assert_eq!(t.line_at(5000), Some(1));
        assert_eq!(t.line_at(99_999), Some(1));
    }

    #[test]
    fn timed_types_are_preferred() {
        assert!(type_preference("text/vtt") < type_preference("text/html"));
        assert!(type_preference("application/json") < type_preference("text/plain"));
        assert!(type_preference("application/x-subrip") < type_preference("text/html"));
    }
}
