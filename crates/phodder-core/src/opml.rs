//! OPML import and export. Any `<outline>` with an `xmlUrl` is a feed, at any
//! nesting depth; folders aren't preserved, since groups don't map onto
//! arbitrary OPML nesting.

use quick_xml::events::{BytesText, Event};
use quick_xml::reader::Reader;
use quick_xml::writer::Writer;

#[derive(Debug, Clone, PartialEq)]
pub struct ImportedFeed {
    pub title: Option<String>,
    pub feed_url: String,
}

/// Malformed XML is an error; a well-formed file with no feeds returns an
/// empty list.
pub fn parse_opml(xml: &str) -> Result<Vec<ImportedFeed>, quick_xml::Error> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut feeds = Vec::new();
    let mut buf = Vec::new();
    // quick-xml reports EOF on a truncated document without an error, so
    // unclosed elements are tracked here.
    let mut open_elements: Vec<String> = Vec::new();

    loop {
        buf.clear();
        let event = reader.read_event_into(&mut buf);
        match &event {
            Ok(Event::Start(e)) => open_elements.push(String::from_utf8_lossy(e.name().as_ref()).into_owned()),
            Ok(Event::End(_)) => {
                open_elements.pop();
            }
            Ok(Event::Eof) => {
                if let Some(tag) = open_elements.pop() {
                    return Err(quick_xml::Error::IllFormed(quick_xml::errors::IllFormedError::MissingEndTag(tag)));
                }
            }
            _ => {}
        }
        match event {
            Ok(Event::Eof) => break,
            Ok(Event::Empty(e)) | Ok(Event::Start(e)) if e.name().as_ref() == b"outline" => {
                let mut feed_url = None;
                let mut title = None;
                for attr in e.attributes().flatten() {
                    match attr.key.as_ref() {
                        b"xmlUrl" => feed_url = Some(attr.normalized_value(quick_xml::XmlVersion::Implicit1_0)?.into_owned()),
                        // `text` is the spec's required label; `title` is an
                        // optional refinement and wins when both are present.
                        b"title" => title = Some(attr.normalized_value(quick_xml::XmlVersion::Implicit1_0)?.into_owned()),
                        b"text" if title.is_none() => {
                            title = Some(attr.normalized_value(quick_xml::XmlVersion::Implicit1_0)?.into_owned())
                        }
                        _ => {}
                    }
                }
                if let Some(feed_url) = feed_url {
                    feeds.push(ImportedFeed { title, feed_url });
                }
            }
            Ok(_) => {}
            Err(e) => return Err(e),
        }
    }

    Ok(feeds)
}

pub struct ExportedFeed<'a> {
    pub title: &'a str,
    pub feed_url: &'a str,
}

/// OPML 2.0 with a flat list of outlines. quick-xml's writer escapes
/// attributes; titles routinely contain `&` and quotes.
pub fn generate_opml(feeds: &[ExportedFeed]) -> Result<String, quick_xml::Error> {
    let mut buffer = Vec::new();
    let mut writer = Writer::new_with_indent(&mut buffer, b' ', 2);

    writer.write_event(Event::Decl(quick_xml::events::BytesDecl::new("1.0", Some("UTF-8"), None)))?;

    writer.create_element("opml").with_attribute(("version", "2.0")).write_inner_content(|writer| {
        writer.create_element("head").write_inner_content(|writer| {
            writer.create_element("title").write_text_content(BytesText::new("Phodder Subscriptions"))?;
            Ok(())
        })?;
        writer.create_element("body").write_inner_content(|writer| {
            for feed in feeds {
                writer
                    .create_element("outline")
                    .with_attributes([("text", feed.title), ("title", feed.title), ("type", "rss"), ("xmlUrl", feed.feed_url)])
                    .write_empty()?;
            }
            Ok(())
        })?;
        Ok(())
    })?;

    Ok(String::from_utf8(buffer).expect("quick-xml only ever writes valid UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_simple_flat_opml_file() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<opml version="2.0">
  <head><title>My Podcasts</title></head>
  <body>
    <outline text="Show One" title="Show One" type="rss" xmlUrl="https://example.com/one.xml"/>
    <outline text="Show Two" type="rss" xmlUrl="https://example.com/two.xml"/>
  </body>
</opml>"#;
        let feeds = parse_opml(xml).unwrap();
        assert_eq!(
            feeds,
            vec![
                ImportedFeed { title: Some("Show One".to_string()), feed_url: "https://example.com/one.xml".to_string() },
                ImportedFeed { title: Some("Show Two".to_string()), feed_url: "https://example.com/two.xml".to_string() },
            ]
        );
    }

    #[test]
    fn parses_nested_folder_outlines_without_preserving_the_folder_structure() {
        // Other apps export folders as outlines without `xmlUrl` containing
        // feed outlines.
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<opml version="2.0">
  <body>
    <outline text="Tech">
      <outline text="Nested Show" xmlUrl="https://example.com/nested.xml"/>
    </outline>
  </body>
</opml>"#;
        let feeds = parse_opml(xml).unwrap();
        assert_eq!(feeds, vec![ImportedFeed { title: Some("Nested Show".to_string()), feed_url: "https://example.com/nested.xml".to_string() }]);
    }

    #[test]
    fn an_outline_with_no_xml_url_is_skipped_not_an_error() {
        let xml = r#"<opml version="2.0"><body><outline text="Not a feed, no xmlUrl"/></body></opml>"#;
        assert_eq!(parse_opml(xml).unwrap(), vec![]);
    }

    #[test]
    fn malformed_xml_is_a_real_error() {
        assert!(parse_opml("<opml><body>").is_err());
    }

    #[test]
    fn generates_opml_that_this_module_s_own_parser_can_read_back() {
        // Round trip: the generator and parser must agree.
        let feeds = vec![
            ExportedFeed { title: "Show & Tell", feed_url: "https://example.com/show.xml" },
            ExportedFeed { title: "Simple Show", feed_url: "https://example.com/simple.xml" },
        ];
        let xml = generate_opml(&feeds).unwrap();
        let parsed = parse_opml(&xml).unwrap();
        assert_eq!(
            parsed,
            vec![
                ImportedFeed { title: Some("Show & Tell".to_string()), feed_url: "https://example.com/show.xml".to_string() },
                ImportedFeed { title: Some("Simple Show".to_string()), feed_url: "https://example.com/simple.xml".to_string() },
            ]
        );
    }

    #[test]
    fn generated_opml_correctly_escapes_special_characters_in_titles() {
        let feeds = vec![ExportedFeed { title: "Cars & Trucks <Weekly>", feed_url: "https://example.com/cars.xml" }];
        let xml = generate_opml(&feeds).unwrap();
        assert!(xml.contains("Cars &amp; Trucks &lt;Weekly&gt;"));
        assert!(!xml.contains("Cars & Trucks <Weekly>"));
    }
}
