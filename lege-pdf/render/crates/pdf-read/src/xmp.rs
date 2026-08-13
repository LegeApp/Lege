//! Extraction of the `xmpMM` media-management properties from an XMP packet.
//!
//! XMP is RDF/XML, and a conforming writer may express the same property in
//! two shapes — as a child element, or as an attribute on `rdf:Description`:
//!
//! ```xml
//! <rdf:Description xmpMM:DocumentID="xmp.did:1234"/>
//! <rdf:Description><xmpMM:DocumentID>xmp.did:1234</xmpMM:DocumentID></rdf:Description>
//! ```
//!
//! Both are read here. Namespace *prefixes* are conventional but not fixed by
//! the spec, so matching is on the local name after the colon rather than on
//! the literal `xmpMM:`. That trades a theoretical collision — another
//! namespace also defining `DocumentID` — for working on real files that use a
//! non-standard prefix. In practice the media-management names are distinctive
//! enough that the trade is one-sided.
//!
//! This is a targeted scanner, not a general XML parser: it reads the handful
//! of properties that carry document lineage and ignores everything else. It
//! never fails — a malformed or truncated packet simply yields fewer fields.

/// Media-management properties describing where a document came from.
///
/// These are the file's own claims about its descent. `document_id` is meant to
/// persist across a document's entire edit history while `instance_id` changes
/// on every save, so two files sharing a `document_id` with different
/// `instance_id`s are, by the format's own design, two saves of one lineage.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct XmpLineage {
    /// `xmpMM:DocumentID` — stable across the document's edit history.
    pub document_id: Option<String>,
    /// `xmpMM:InstanceID` — changes on every save.
    pub instance_id: Option<String>,
    /// `xmpMM:OriginalDocumentID` — the identity of the original this
    /// descends from, when a writer records it.
    pub original_document_id: Option<String>,
    /// `stRef:documentID` inside `xmpMM:DerivedFrom`.
    pub derived_from_document_id: Option<String>,
    /// `stRef:instanceID` inside `xmpMM:DerivedFrom`. When this names another
    /// file's `instance_id`, the descent is written down explicitly.
    pub derived_from_instance_id: Option<String>,
    /// `xmpMM:History` events, in document order.
    pub history: Vec<XmpHistoryEvent>,
}

impl XmpLineage {
    /// Whether the packet carried any media-management property at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.document_id.is_none()
            && self.instance_id.is_none()
            && self.original_document_id.is_none()
            && self.derived_from_document_id.is_none()
            && self.derived_from_instance_id.is_none()
            && self.history.is_empty()
    }
}

/// One `xmpMM:History` entry: a save, print, or conversion the writer recorded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct XmpHistoryEvent {
    /// `stEvt:action`, e.g. `created`, `saved`, `converted`.
    pub action: Option<String>,
    /// `stEvt:when` — the claimed timestamp, verbatim and unparsed.
    pub when: Option<String>,
    /// `stEvt:softwareAgent` — the tool that performed the action, as it
    /// named itself.
    pub software_agent: Option<String>,
    /// `stEvt:changed` — which parts the writer says changed.
    pub changed: Option<String>,
    /// `stEvt:instanceID` — the instance this event produced.
    pub instance_id: Option<String>,
}

/// Largest packet scanned. An XMP packet far past this is not carrying
/// media-management metadata; refusing to scan it bounds the work.
const MAX_PACKET: usize = 8 * 1024 * 1024;

/// Read the media-management properties out of an XMP packet.
#[must_use]
pub fn parse(packet: &[u8]) -> XmpLineage {
    let packet = &packet[..packet.len().min(MAX_PACKET)];
    // XMP is UTF-8 in practice; a lossy read keeps a mis-encoded packet from
    // costing us the fields that did decode.
    let xml = String::from_utf8_lossy(packet);
    let xml = xml.as_ref();

    let mut lineage = XmpLineage {
        document_id: property(xml, "DocumentID"),
        instance_id: property(xml, "InstanceID"),
        original_document_id: property(xml, "OriginalDocumentID"),
        ..XmpLineage::default()
    };

    // `DerivedFrom` is a structured property: its parts are either attributes
    // on its own tag or child elements inside it.
    if let Some(tag) = find_open_tag(xml, "DerivedFrom", 0) {
        lineage.derived_from_document_id = attribute(tag.attrs, "documentID");
        lineage.derived_from_instance_id = attribute(tag.attrs, "instanceID");
        if let Some(inner) = inner_region(xml, &tag, "DerivedFrom") {
            lineage.derived_from_document_id = lineage
                .derived_from_document_id
                .or_else(|| element_text(inner, "documentID"));
            lineage.derived_from_instance_id = lineage
                .derived_from_instance_id
                .or_else(|| element_text(inner, "instanceID"));
        }
    }

    if let Some(tag) = find_open_tag(xml, "History", 0)
        && let Some(inner) = inner_region(xml, &tag, "History")
    {
        lineage.history = parse_history(inner);
    }

    lineage
}

/// Every `rdf:li` inside a `History` region, in order.
fn parse_history(region: &str) -> Vec<XmpHistoryEvent> {
    /// Cap on recorded events, so a pathological packet cannot balloon the
    /// report.
    const MAX_EVENTS: usize = 4096;

    let mut events = Vec::new();
    let mut at = 0usize;
    while events.len() < MAX_EVENTS {
        let Some(tag) = find_open_tag(region, "li", at) else {
            break;
        };
        at = tag.end;

        let mut event = XmpHistoryEvent {
            action: attribute(tag.attrs, "action"),
            when: attribute(tag.attrs, "when"),
            software_agent: attribute(tag.attrs, "softwareAgent"),
            changed: attribute(tag.attrs, "changed"),
            instance_id: attribute(tag.attrs, "instanceID"),
        };

        if let Some(inner) = inner_region(region, &tag, "li") {
            at = tag.end + inner.len();
            event.action = event.action.or_else(|| element_text(inner, "action"));
            event.when = event.when.or_else(|| element_text(inner, "when"));
            event.software_agent = event
                .software_agent
                .or_else(|| element_text(inner, "softwareAgent"));
            event.changed = event.changed.or_else(|| element_text(inner, "changed"));
            event.instance_id = event
                .instance_id
                .or_else(|| element_text(inner, "instanceID"));
        }

        events.push(event);
    }
    events
}

/// A property in either shape: child element first, then attribute.
fn property(xml: &str, local: &str) -> Option<String> {
    element_text(xml, local).or_else(|| {
        let mut at = 0usize;
        while let Some(tag) = next_tag(xml, at) {
            at = tag.end;
            if let Some(value) = attribute(tag.attrs, local) {
                return Some(value);
            }
        }
        None
    })
}

/// An open tag, with its attribute text.
struct OpenTag<'a> {
    /// Attribute text between the tag name and the closing `>`.
    attrs: &'a str,
    /// Byte offset just past the tag's `>`.
    end: usize,
    /// The tag closed itself (`<foo/>`), so it has no inner region.
    self_closing: bool,
}

/// The next tag at or after `from`, whatever its name.
fn next_tag(xml: &str, from: usize) -> Option<OpenTag<'_>> {
    let mut at = from;
    loop {
        let rest = xml.get(at..)?;
        let open = at + rest.find('<')?;
        let after = open + 1;
        let tail = xml.get(after..)?;
        let close = after + tail.find('>')?;
        let body = xml.get(after..close)?;
        at = close + 1;
        // Skip closing tags, comments, declarations and processing
        // instructions — none of them carry properties.
        if body.starts_with('/') || body.starts_with('!') || body.starts_with('?') {
            continue;
        }
        let self_closing = body.ends_with('/');
        let body = body.strip_suffix('/').unwrap_or(body);
        let split = body
            .find(|c: char| c.is_ascii_whitespace())
            .unwrap_or(body.len());
        let attrs = xml.get(after + split..close)?;
        return Some(OpenTag {
            attrs,
            end: close + 1,
            self_closing,
        });
    }
}

/// The next open tag at or after `from` whose local name is `local`.
fn find_open_tag<'a>(xml: &'a str, local: &str, from: usize) -> Option<OpenTag<'a>> {
    let mut at = from;
    loop {
        let rest = xml.get(at..)?;
        let open = at + rest.find('<')?;
        let after = open + 1;
        let tail = xml.get(after..)?;
        let close = after + tail.find('>')?;
        let body = xml.get(after..close)?;
        at = close + 1;
        if body.starts_with('/') || body.starts_with('!') || body.starts_with('?') {
            continue;
        }
        let self_closing = body.ends_with('/');
        let trimmed = body.strip_suffix('/').unwrap_or(body);
        let split = trimmed
            .find(|c: char| c.is_ascii_whitespace())
            .unwrap_or(trimmed.len());
        let name = trimmed.get(..split)?;
        if local_name(name) == local {
            return Some(OpenTag {
                attrs: xml.get(after + split..close)?,
                end: close + 1,
                self_closing,
            });
        }
    }
}

/// The text between an open tag and its matching close tag.
///
/// Same-named nesting is not expected among the properties read here, so the
/// first matching close tag ends the region.
fn inner_region<'a>(xml: &'a str, tag: &OpenTag<'_>, local: &str) -> Option<&'a str> {
    if tag.self_closing {
        return None;
    }
    let rest = xml.get(tag.end..)?;
    let mut at = 0usize;
    loop {
        let open = at + rest.get(at..)?.find("</")?;
        let after = open + 2;
        let close = after + rest.get(after..)?.find('>')?;
        let name = rest.get(after..close)?.trim();
        if local_name(name) == local {
            return rest.get(..open);
        }
        at = close + 1;
    }
}

/// The text of the first child element with local name `local`.
fn element_text(xml: &str, local: &str) -> Option<String> {
    let tag = find_open_tag(xml, local, 0)?;
    let inner = inner_region(xml, &tag, local)?;
    // A property element holding child elements is a structured value, not a
    // simple one; reading its concatenated text would invent a value.
    if inner.contains('<') {
        return None;
    }
    let text = decode_entities(inner.trim());
    if text.is_empty() { None } else { Some(text) }
}

/// The value of an attribute whose local name is `local`.
fn attribute(attrs: &str, local: &str) -> Option<String> {
    let bytes = attrs.as_bytes();
    let mut at = 0usize;
    while at < bytes.len() {
        // Attribute name runs to '='.
        let eq = at + attrs.get(at..)?.find('=')?;
        let name = attrs.get(at..eq)?.trim();
        let after = attrs.get(eq + 1..)?;
        let quote = after.find(['"', '\''])?;
        let quote_char = after.as_bytes().get(quote).copied()? as char;
        let value_start = eq + 1 + quote + 1;
        let end = value_start + attrs.get(value_start..)?.find(quote_char)?;
        if !name.is_empty() && local_name(name) == local {
            let text = decode_entities(attrs.get(value_start..end)?);
            return if text.is_empty() { None } else { Some(text) };
        }
        at = end + 1;
    }
    None
}

/// The part of a qualified name after its namespace prefix.
fn local_name(name: &str) -> &str {
    match name.rsplit_once(':') {
        Some((_, local)) => local,
        None => name,
    }
}

/// The five predefined XML entities. XMP values are plain text — identifiers,
/// timestamps and tool names — so nothing more exotic is needed.
fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        // Ampersand last, so "&amp;lt;" does not become "<".
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_attribute_shape() {
        let xml = r#"<rdf:RDF><rdf:Description rdf:about=""
            xmpMM:DocumentID="xmp.did:AAAA" xmpMM:InstanceID="xmp.iid:BBBB"/></rdf:RDF>"#;
        let lineage = parse(xml.as_bytes());
        assert_eq!(lineage.document_id.as_deref(), Some("xmp.did:AAAA"));
        assert_eq!(lineage.instance_id.as_deref(), Some("xmp.iid:BBBB"));
    }

    #[test]
    fn reads_the_element_shape() {
        let xml = r"<rdf:Description>
            <xmpMM:DocumentID>xmp.did:AAAA</xmpMM:DocumentID>
            <xmpMM:InstanceID>xmp.iid:BBBB</xmpMM:InstanceID>
        </rdf:Description>";
        let lineage = parse(xml.as_bytes());
        assert_eq!(lineage.document_id.as_deref(), Some("xmp.did:AAAA"));
        assert_eq!(lineage.instance_id.as_deref(), Some("xmp.iid:BBBB"));
    }

    #[test]
    fn a_non_standard_prefix_still_reads() {
        let xml = r#"<rdf:Description mm:DocumentID="xmp.did:AAAA"/>"#;
        assert_eq!(
            parse(xml.as_bytes()).document_id.as_deref(),
            Some("xmp.did:AAAA")
        );
    }

    #[test]
    fn reads_derived_from_in_both_shapes() {
        let attr = r#"<xmpMM:DerivedFrom stRef:documentID="xmp.did:AAAA"
            stRef:instanceID="xmp.iid:BBBB"/>"#;
        let lineage = parse(attr.as_bytes());
        assert_eq!(
            lineage.derived_from_document_id.as_deref(),
            Some("xmp.did:AAAA")
        );
        assert_eq!(
            lineage.derived_from_instance_id.as_deref(),
            Some("xmp.iid:BBBB")
        );

        let nested = r#"<xmpMM:DerivedFrom rdf:parseType="Resource">
            <stRef:documentID>xmp.did:CCCC</stRef:documentID>
            <stRef:instanceID>xmp.iid:DDDD</stRef:instanceID>
        </xmpMM:DerivedFrom>"#;
        let lineage = parse(nested.as_bytes());
        assert_eq!(
            lineage.derived_from_document_id.as_deref(),
            Some("xmp.did:CCCC")
        );
        assert_eq!(
            lineage.derived_from_instance_id.as_deref(),
            Some("xmp.iid:DDDD")
        );
    }

    #[test]
    fn reads_a_history_sequence() {
        let xml = r#"<xmpMM:History><rdf:Seq>
            <rdf:li stEvt:action="created" stEvt:when="2024-12-01T12:00:00Z"
                stEvt:softwareAgent="Microsoft Word"/>
            <rdf:li rdf:parseType="Resource">
                <stEvt:action>saved</stEvt:action>
                <stEvt:when>2026-07-13T09:30:00Z</stEvt:when>
                <stEvt:softwareAgent>Adobe Acrobat 23.0</stEvt:softwareAgent>
                <stEvt:changed>/</stEvt:changed>
            </rdf:li>
        </rdf:Seq></xmpMM:History>"#;
        let history = parse(xml.as_bytes()).history;
        assert_eq!(history.len(), 2);

        assert_eq!(history[0].action.as_deref(), Some("created"));
        assert_eq!(history[0].software_agent.as_deref(), Some("Microsoft Word"));
        assert_eq!(history[1].action.as_deref(), Some("saved"));
        assert_eq!(
            history[1].software_agent.as_deref(),
            Some("Adobe Acrobat 23.0")
        );
        assert_eq!(history[1].changed.as_deref(), Some("/"));
    }

    #[test]
    fn entities_are_decoded() {
        let xml = r#"<rdf:Description xmpMM:DocumentID="a&amp;b"/>"#;
        assert_eq!(parse(xml.as_bytes()).document_id.as_deref(), Some("a&b"));
    }

    #[test]
    fn a_structured_value_is_not_flattened_into_a_string() {
        // DerivedFrom holds child elements; reading its concatenated text
        // would invent an identifier that appears nowhere in the file.
        let xml = r"<xmpMM:DerivedFrom><stRef:instanceID>x</stRef:instanceID></xmpMM:DerivedFrom>";
        assert_eq!(element_text(xml, "DerivedFrom"), None);
    }

    #[test]
    fn malformed_input_yields_fewer_fields_rather_than_failing() {
        for bad in [
            "",
            "<",
            "<rdf:Description",
            "<rdf:Description xmpMM:DocumentID=",
            "<rdf:Description xmpMM:DocumentID=\"unterminated",
            "<xmpMM:History><rdf:Seq><rdf:li",
            "\u{feff}\u{0}\u{1}not xml at all",
        ] {
            let lineage = parse(bad.as_bytes());
            assert!(lineage.is_empty(), "unexpected fields from {bad:?}");
        }
    }

    #[test]
    fn a_packet_with_no_media_management_is_empty() {
        let xml = r"<rdf:Description><dc:title>Some title</dc:title></rdf:Description>";
        assert!(parse(xml.as_bytes()).is_empty());
    }
}
