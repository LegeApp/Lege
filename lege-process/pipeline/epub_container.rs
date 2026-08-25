//! EPUB container assembly.
//!
//! Lege generates the XHTML itself (see `epub_pipeline::render_xhtml`) and
//! decides its own chapter and TOC structure; all a packager adds is the
//! fixed OCF skeleton around it -- the stored `mimetype` entry, the OCF
//! `container.xml`, an OPF package document, and the two navigation documents
//! (EPUB 3's `nav.xhtml` and the EPUB 2 `toc.ncx` that older readers still
//! want). That is a small, well-specified set of files.
//!
//! It used to come from `epub-builder`, which pinned `zip` 6 while the rest of
//! the processor is on `zip` 8, so both compiled into every binary (and `zip`
//! 6 dragged `time` and its proc-macro along). Writing the skeleton directly
//! against the `zip` the crate already uses removes that split, and puts EPUB
//! packaging next to the PDF and DjVu containers Lege likewise writes itself.
//!
//! The output targets EPUB 3.0 with an EPUB 2 fallback navigation document,
//! which is what `epub-builder`'s `inline_toc()` produced.

use anyhow::{Result, anyhow};
use std::io::{Seek, Write};
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

/// Where the package document and content live inside the archive.
const CONTENT_DIR: &str = "OEBPS";
const PACKAGE_PATH: &str = "OEBPS/content.opf";
const NAV_FILENAME: &str = "nav.xhtml";
const NCX_FILENAME: &str = "toc.ncx";

/// One XHTML document in the reading order.
#[derive(Debug, Clone)]
pub struct EpubChapter {
    /// File name inside [`CONTENT_DIR`], e.g. `chapter_0001.xhtml`.
    pub filename: String,
    /// Navigation label. Chapters without one stay out of the TOC but remain
    /// in the spine, which is how the previous packager behaved.
    pub title: Option<String>,
    /// Complete XHTML document, already escaped by the caller.
    pub xhtml: String,
}

/// Dublin Core metadata for the package document.
#[derive(Debug, Clone)]
pub struct EpubMetadata {
    pub title: String,
    pub author: String,
    /// BCP 47 language tag.
    pub language: String,
}

/// Escape text for insertion into XML character data or a quoted attribute.
///
/// The five predefined XML entities, which is all that XHTML content and OPF
/// metadata need. Kept here so the packager is self-contained.
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
    out
}

/// A stable document identifier derived from the book's own content.
///
/// EPUB requires `dc:identifier` to be unique for the publication but does not
/// require a UUID, and Lege has no random source wired in here. Hashing the
/// title and the chapter file names gives an id that is unique per book and
/// identical across re-runs, so re-processing the same PDF twice produces the
/// same identifier rather than two publications that readers treat as
/// unrelated. FNV-1a, 64-bit -- not a cryptographic claim, just a spread.
fn document_identifier(title: &str, chapters: &[EpubChapter]) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    let mut absorb = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
    };
    absorb(title.as_bytes());
    for chapter in chapters {
        absorb(b"\x00");
        absorb(chapter.filename.as_bytes());
        if let Some(chapter_title) = &chapter.title {
            absorb(chapter_title.as_bytes());
        }
    }
    format!("urn:lege:{hash:016x}")
}

/// Manifest item id for the chapter at `index`.
fn chapter_id(index: usize) -> String {
    format!("chapter{:04}", index + 1)
}

fn container_xml() -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <container version=\"1.0\" \
         xmlns=\"urn:oasis:names:tc:opendocument:xmlns:container\">\n\
         \x20 <rootfiles>\n\
         \x20   <rootfile full-path=\"{PACKAGE_PATH}\" \
         media-type=\"application/oebps-package+xml\"/>\n\
         \x20 </rootfiles>\n\
         </container>\n"
    )
}

/// The OPF package document: Dublin Core metadata, the manifest of every file
/// in the publication, and the spine that orders them.
fn package_document(
    metadata: &EpubMetadata,
    chapters: &[EpubChapter],
    identifier: &str,
    modified: &str,
) -> String {
    let mut manifest = String::new();
    let mut spine = String::new();

    // The navigation document is both a manifest item (with the EPUB 3 `nav`
    // property) and the first thing in the reading order, matching the inline
    // table of contents the previous packager emitted.
    manifest.push_str(&format!(
        "    <item id=\"nav\" href=\"{NAV_FILENAME}\" \
         media-type=\"application/xhtml+xml\" properties=\"nav\"/>\n"
    ));
    manifest.push_str(&format!(
        "    <item id=\"ncx\" href=\"{NCX_FILENAME}\" \
         media-type=\"application/x-dtbncx+xml\"/>\n"
    ));
    spine.push_str("    <itemref idref=\"nav\"/>\n");

    for (index, chapter) in chapters.iter().enumerate() {
        let id = chapter_id(index);
        let href = xml_escape(&chapter.filename);
        manifest.push_str(&format!(
            "    <item id=\"{id}\" href=\"{href}\" media-type=\"application/xhtml+xml\"/>\n"
        ));
        spine.push_str(&format!("    <itemref idref=\"{id}\"/>\n"));
    }

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.0\" \
         unique-identifier=\"bookid\">\n\
         \x20 <metadata xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\n\
         \x20   <dc:identifier id=\"bookid\">{identifier}</dc:identifier>\n\
         \x20   <dc:title>{title}</dc:title>\n\
         \x20   <dc:creator>{author}</dc:creator>\n\
         \x20   <dc:language>{language}</dc:language>\n\
         \x20   <meta property=\"dcterms:modified\">{modified}</meta>\n\
         \x20 </metadata>\n\
         \x20 <manifest>\n{manifest}\x20 </manifest>\n\
         \x20 <spine toc=\"ncx\">\n{spine}\x20 </spine>\n\
         </package>\n",
        identifier = xml_escape(identifier),
        title = xml_escape(&metadata.title),
        author = xml_escape(&metadata.author),
        language = xml_escape(&metadata.language),
    )
}

/// Chapters that carry a navigation label, with their manifest index.
///
/// EPUB 3 requires the `toc` nav to be non-empty, so when no chapter was
/// titled the first one is listed under the book's own title rather than
/// emitting an invalid publication.
fn toc_entries<'a>(
    metadata: &'a EpubMetadata,
    chapters: &'a [EpubChapter],
) -> Vec<(&'a str, String)> {
    let titled: Vec<(&str, String)> = chapters
        .iter()
        .filter_map(|chapter| {
            chapter
                .title
                .as_deref()
                .filter(|title| !title.trim().is_empty())
                .map(|title| (chapter.filename.as_str(), title.to_string()))
        })
        .collect();
    if !titled.is_empty() {
        return titled;
    }
    match chapters.first() {
        Some(first) => vec![(first.filename.as_str(), metadata.title.clone())],
        None => Vec::new(),
    }
}

/// EPUB 3 navigation document.
fn nav_document(metadata: &EpubMetadata, chapters: &[EpubChapter]) -> String {
    let mut items = String::new();
    for (href, title) in toc_entries(metadata, chapters) {
        items.push_str(&format!(
            "      <li><a href=\"{}\">{}</a></li>\n",
            xml_escape(href),
            xml_escape(&title)
        ));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE html>\n\
         <html xmlns=\"http://www.w3.org/1999/xhtml\" \
         xmlns:epub=\"http://www.idpf.org/2007/ops\">\n\
         <head><meta charset=\"utf-8\"/><title>{title}</title></head>\n\
         <body>\n\
         \x20 <nav epub:type=\"toc\" id=\"toc\">\n\
         \x20   <h1>Contents</h1>\n\
         \x20   <ol>\n{items}\x20   </ol>\n\
         \x20 </nav>\n\
         </body>\n\
         </html>\n",
        title = xml_escape(&metadata.title),
    )
}

/// EPUB 2 `toc.ncx`, kept for readers that ignore the EPUB 3 nav document.
fn ncx_document(metadata: &EpubMetadata, chapters: &[EpubChapter], identifier: &str) -> String {
    let mut points = String::new();
    for (order, (href, title)) in toc_entries(metadata, chapters).into_iter().enumerate() {
        let play_order = order + 1;
        points.push_str(&format!(
            "    <navPoint id=\"navpoint{play_order}\" playOrder=\"{play_order}\">\n\
             \x20     <navLabel><text>{}</text></navLabel>\n\
             \x20     <content src=\"{}\"/>\n\
             \x20   </navPoint>\n",
            xml_escape(&title),
            xml_escape(href),
        ));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <ncx xmlns=\"http://www.daisy.org/z3986/2005/ncx/\" version=\"2005-1\">\n\
         \x20 <head>\n\
         \x20   <meta name=\"dtb:uid\" content=\"{identifier}\"/>\n\
         \x20   <meta name=\"dtb:depth\" content=\"1\"/>\n\
         \x20   <meta name=\"dtb:totalPageCount\" content=\"0\"/>\n\
         \x20   <meta name=\"dtb:maxPageNumber\" content=\"0\"/>\n\
         \x20 </head>\n\
         \x20 <docTitle><text>{title}</text></docTitle>\n\
         \x20 <navMap>\n{points}\x20 </navMap>\n\
         </ncx>\n",
        identifier = xml_escape(identifier),
        title = xml_escape(&metadata.title),
    )
}

/// Write a complete EPUB publication into `sink`.
///
/// `cancelled` is polled once per chapter so a long book stops promptly; the
/// caller is responsible for making sure a cancelled write never replaces a
/// good destination file (see `epub_pipeline`'s temporary-file publish).
///
/// Fails rather than emitting an invalid publication when `chapters` is empty.
pub fn write_epub<W: Write + Seek>(
    sink: W,
    metadata: &EpubMetadata,
    chapters: &[EpubChapter],
    modified: &str,
    cancelled: &dyn Fn() -> bool,
) -> Result<()> {
    if chapters.is_empty() {
        return Err(anyhow!("[EPUB] refusing to package a book with no chapters"));
    }

    let identifier = document_identifier(&metadata.title, chapters);
    let mut archive = zip::ZipWriter::new(sink);
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    // OCF requires `mimetype` to be the first entry, stored uncompressed and
    // with no extra field, so a reader can identify the file from its first
    // bytes without inflating anything.
    archive
        .start_file("mimetype", stored)
        .map_err(|e| anyhow!("[EPUB] cannot start mimetype entry: {e}"))?;
    archive
        .write_all(b"application/epub+zip")
        .map_err(|e| anyhow!("[EPUB] cannot write mimetype: {e}"))?;

    let mut write_entry = |archive: &mut zip::ZipWriter<W>, path: &str, body: &str| -> Result<()> {
        archive
            .start_file(path, deflated)
            .map_err(|e| anyhow!("[EPUB] cannot start {path}: {e}"))?;
        archive
            .write_all(body.as_bytes())
            .map_err(|e| anyhow!("[EPUB] cannot write {path}: {e}"))
    };

    write_entry(&mut archive, "META-INF/container.xml", &container_xml())?;
    write_entry(
        &mut archive,
        PACKAGE_PATH,
        &package_document(metadata, chapters, &identifier, modified),
    )?;
    write_entry(
        &mut archive,
        &format!("{CONTENT_DIR}/{NAV_FILENAME}"),
        &nav_document(metadata, chapters),
    )?;
    write_entry(
        &mut archive,
        &format!("{CONTENT_DIR}/{NCX_FILENAME}"),
        &ncx_document(metadata, chapters, &identifier),
    )?;

    for chapter in chapters {
        if cancelled() {
            return Err(anyhow!("[EPUB] packaging cancelled"));
        }
        write_entry(
            &mut archive,
            &format!("{CONTENT_DIR}/{}", chapter.filename),
            &chapter.xhtml,
        )?;
    }

    archive
        .finish()
        .map_err(|e| anyhow!("[EPUB] cannot finalize the archive: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read};

    fn chapter(name: &str, title: Option<&str>) -> EpubChapter {
        EpubChapter {
            filename: name.to_string(),
            title: title.map(str::to_string),
            xhtml: format!("<html><body><p>{name}</p></body></html>"),
        }
    }

    fn metadata() -> EpubMetadata {
        EpubMetadata {
            title: "A & B".to_string(),
            author: "Lege".to_string(),
            language: "en".to_string(),
        }
    }

    fn pack(chapters: &[EpubChapter]) -> Vec<u8> {
        let mut buffer = Cursor::new(Vec::new());
        write_epub(&mut buffer, &metadata(), chapters, "2024-01-01T00:00:00Z", &|| {
            false
        })
        .expect("packaging should succeed");
        buffer.into_inner()
    }

    fn read_entry(bytes: &[u8], name: &str) -> String {
        let mut archive =
            zip::ZipArchive::new(Cursor::new(bytes.to_vec())).expect("output is a zip");
        let mut entry = archive.by_name(name).expect("entry exists");
        let mut text = String::new();
        entry.read_to_string(&mut text).expect("entry is text");
        text
    }

    #[test]
    fn mimetype_is_the_first_entry_and_stored() {
        let bytes = pack(&[chapter("chapter_0001.xhtml", Some("One"))]);
        // OCF: the stored, uncompressed `mimetype` must sit at offset 0 so the
        // magic bytes identify the file without inflating anything.
        assert_eq!(&bytes[0..4], b"PK\x03\x04");
        assert!(
            bytes.windows(8).next() == Some(&bytes[0..8]) && bytes[30..38] == *b"mimetype",
            "mimetype must be the first entry"
        );
        let mut archive =
            zip::ZipArchive::new(Cursor::new(bytes.clone())).expect("output is a zip");
        let entry = archive.by_index(0).expect("first entry");
        assert_eq!(entry.name(), "mimetype");
        assert_eq!(entry.compression(), CompressionMethod::Stored);
    }

    #[test]
    fn every_required_document_is_present() {
        let bytes = pack(&[
            chapter("chapter_0001.xhtml", Some("One")),
            chapter("chapter_0002.xhtml", None),
        ]);
        for name in [
            "mimetype",
            "META-INF/container.xml",
            "OEBPS/content.opf",
            "OEBPS/nav.xhtml",
            "OEBPS/toc.ncx",
            "OEBPS/chapter_0001.xhtml",
            "OEBPS/chapter_0002.xhtml",
        ] {
            let _ = read_entry(&bytes, name);
        }
    }

    #[test]
    fn spine_carries_every_chapter_and_the_nav_leads() {
        let bytes = pack(&[
            chapter("chapter_0001.xhtml", Some("One")),
            chapter("chapter_0002.xhtml", None),
        ]);
        let opf = read_entry(&bytes, "OEBPS/content.opf");
        let nav_at = opf.find("<itemref idref=\"nav\"/>").expect("nav in spine");
        let first = opf
            .find("<itemref idref=\"chapter0001\"/>")
            .expect("chapter 1 in spine");
        let second = opf
            .find("<itemref idref=\"chapter0002\"/>")
            .expect("untitled chapters stay in the reading order");
        assert!(nav_at < first && first < second, "spine order is wrong");
        assert!(opf.contains("href=\"chapter_0002.xhtml\""));
    }

    #[test]
    fn metadata_is_xml_escaped() {
        let bytes = pack(&[chapter("chapter_0001.xhtml", Some("Cats & Dogs"))]);
        let opf = read_entry(&bytes, "OEBPS/content.opf");
        assert!(opf.contains("<dc:title>A &amp; B</dc:title>"), "{opf}");
        let nav = read_entry(&bytes, "OEBPS/nav.xhtml");
        assert!(nav.contains("Cats &amp; Dogs"), "{nav}");
        assert!(!nav.contains("Cats & Dogs"));
    }

    #[test]
    fn untitled_chapters_stay_out_of_the_toc() {
        let bytes = pack(&[
            chapter("chapter_0001.xhtml", Some("One")),
            chapter("chapter_0002.xhtml", None),
        ]);
        let nav = read_entry(&bytes, "OEBPS/nav.xhtml");
        assert!(nav.contains("chapter_0001.xhtml"));
        assert!(!nav.contains("chapter_0002.xhtml"));
    }

    #[test]
    fn a_book_with_no_titles_still_has_a_navigable_toc() {
        // EPUB 3 rejects an empty `toc` nav, so the book title stands in.
        let bytes = pack(&[chapter("chapter_0001.xhtml", None)]);
        let nav = read_entry(&bytes, "OEBPS/nav.xhtml");
        assert!(nav.contains("chapter_0001.xhtml"), "{nav}");
        assert!(nav.contains("A &amp; B"));
        let ncx = read_entry(&bytes, "OEBPS/toc.ncx");
        assert!(ncx.contains("<navPoint id=\"navpoint1\" playOrder=\"1\">"), "{ncx}");
    }

    #[test]
    fn identifier_is_stable_across_runs_and_varies_by_book() {
        let chapters = [chapter("chapter_0001.xhtml", Some("One"))];
        assert_eq!(
            document_identifier("Book", &chapters),
            document_identifier("Book", &chapters)
        );
        assert_ne!(
            document_identifier("Book", &chapters),
            document_identifier("Other", &chapters)
        );
    }

    #[test]
    fn cancellation_stops_before_writing_a_chapter() {
        let mut buffer = Cursor::new(Vec::new());
        let error = write_epub(
            &mut buffer,
            &metadata(),
            &[chapter("chapter_0001.xhtml", Some("One"))],
            "2024-01-01T00:00:00Z",
            &|| true,
        )
        .expect_err("a cancelled write must fail");
        assert!(error.to_string().contains("cancelled"), "{error}");
    }

    #[test]
    fn an_empty_book_is_refused() {
        let mut buffer = Cursor::new(Vec::new());
        let error = write_epub(
            &mut buffer,
            &metadata(),
            &[],
            "2024-01-01T00:00:00Z",
            &|| false,
        )
        .expect_err("an empty book is not a publication");
        assert!(error.to_string().contains("no chapters"), "{error}");
    }

    #[test]
    fn escapes_the_five_xml_entities() {
        assert_eq!(xml_escape("a<b>&\"c'd"), "a&lt;b&gt;&amp;&quot;c&#x27;d");
    }
}
