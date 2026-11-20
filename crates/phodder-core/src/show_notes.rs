//! Show-notes text processing.

pub const SEEK_LINK_SCHEME: &str = "phodder-seek:";

/// Wraps timestamps (`M:SS`, `MM:SS`, `H:MM:SS`) found in text content in
/// `<a href="phodder-seek:MILLISECONDS">` links. Tag contents and existing
/// links are left alone.
pub fn linkify_timestamps(html: &str) -> String {
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len() + 64);
    let mut i = 0;
    let mut link_depth = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            let end = html[i..].find('>').map_or(bytes.len(), |e| i + e + 1);
            let tag = html[i..end].to_ascii_lowercase();
            if tag.starts_with("<a ") || tag == "<a>" {
                link_depth += 1;
            } else if tag.starts_with("</a") {
                link_depth = link_depth.saturating_sub(1);
            }
            out.push_str(&html[i..end]);
            i = end;
            continue;
        }
        if link_depth == 0 && bytes[i].is_ascii_digit() && (i == 0 || !is_time_char(bytes[i - 1])) {
            if let Some((len, ms)) = parse_timestamp(&bytes[i..]) {
                let text = &html[i..i + len];
                out.push_str(&format!("<a href=\"{SEEK_LINK_SCHEME}{ms}\">{text}</a>"));
                i += len;
                continue;
            }
        }
        let ch = html[i..].chars().next().expect("index is on a char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Parses a seek link produced by [`linkify_timestamps`].
pub fn parse_seek_link(uri: &str) -> Option<u64> {
    uri.strip_prefix(SEEK_LINK_SCHEME)?.parse().ok()
}

fn is_time_char(b: u8) -> bool {
    b.is_ascii_digit() || b == b':'
}

/// Returns the matched length and position in milliseconds.
fn parse_timestamp(s: &[u8]) -> Option<(usize, u64)> {
    let mut groups: Vec<(u64, usize)> = Vec::with_capacity(3);
    let mut i = 0;
    loop {
        let start = i;
        while i < s.len() && s[i].is_ascii_digit() {
            i += 1;
        }
        let digits = i - start;
        if digits == 0 || digits > 2 {
            return None;
        }
        let value: u64 = std::str::from_utf8(&s[start..i]).ok()?.parse().ok()?;
        groups.push((value, digits));
        if groups.len() < 3 && i + 1 < s.len() && s[i] == b':' && s[i + 1].is_ascii_digit() {
            i += 1;
            continue;
        }
        break;
    }
    if i < s.len() && is_time_char(s[i]) {
        return None;
    }
    let (h, m, sec) = match groups.as_slice() {
        [(m, _), (sec, 2)] => (0, *m, *sec),
        [(h, _), (m, 2), (sec, 2)] => (*h, *m, *sec),
        _ => return None,
    };
    if sec >= 60 || (groups.len() == 3 && m >= 60) {
        return None;
    }
    Some((i, ((h * 60 + m) * 60 + sec) * 1000))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_timestamps_in_text() {
        assert_eq!(
            linkify_timestamps("<p>Intro 0:00, news at 12:34 and 1:02:03.</p>"),
            "<p>Intro <a href=\"phodder-seek:0\">0:00</a>, news at <a href=\"phodder-seek:754000\">12:34</a> \
             and <a href=\"phodder-seek:3723000\">1:02:03</a>.</p>"
        );
    }

    #[test]
    fn leaves_tags_links_and_non_times_alone() {
        let html = r#"<a href="https://x.com/12:34">at 5:00</a> <span data-t="1:00">ratio 3:2, 10:60, 123:45, 2026:01:01</span>"#;
        assert_eq!(linkify_timestamps(html), html);
    }

    #[test]
    fn handles_multibyte_text() {
        assert_eq!(linkify_timestamps("Café — 2:30"), "Café — <a href=\"phodder-seek:150000\">2:30</a>");
    }

    #[test]
    fn parses_seek_links() {
        assert_eq!(parse_seek_link("phodder-seek:754000"), Some(754_000));
        assert_eq!(parse_seek_link("https://example.com"), None);
    }
}
