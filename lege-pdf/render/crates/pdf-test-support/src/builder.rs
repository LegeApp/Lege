//! Programmatic PDF fixture builder — hand-rolled bytes, no external deps.
//!
//! Produces small but *structurally real* PDFs: classic xref tables,
//! cross-reference streams, object streams, incremental updates, hybrid
//! files, deliberate corruption (wrong `/Length`, missing `startxref`,
//! leading garbage). Test-only; panics on misuse instead of returning
//! errors.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Where an object added to the current revision lives.
#[derive(Debug, Clone, Copy)]
enum Entry {
    /// Header-relative byte offset, generation.
    Offset(u64, u16),
    /// Member of an object stream added in this revision.
    InStream { container: u32, index: u32 },
}

/// Incremental PDF byte builder.
#[derive(Debug)]
pub struct PdfBuilder {
    buf: Vec<u8>,
    /// Entries accumulated since the last `finish_*` call.
    revision: BTreeMap<u32, Entry>,
    /// Highest object number ever used (for /Size).
    max_number: u32,
    /// Offset of the previously written xref section (for /Prev).
    prev_xref: Option<u64>,
}

impl Default for PdfBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl PdfBuilder {
    pub fn new() -> Self {
        Self {
            buf: b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n".to_vec(),
            revision: BTreeMap::new(),
            max_number: 0,
            prev_xref: None,
        }
    }

    /// Raw bytes so far (usually called after a `finish_*`).
    pub fn into_bytes(self) -> Vec<u8> {
        assert!(
            self.revision.is_empty(),
            "unfinished revision: call a finish_* method"
        );
        self.buf
    }

    /// Current length — handy for asserting offsets in tests.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    fn note(&mut self, number: u32, entry: Entry) {
        self.max_number = self.max_number.max(number);
        self.revision.insert(number, entry);
    }

    /// Append `N 0 obj <body> endobj`.
    pub fn add_object(&mut self, number: u32, body: &str) -> u64 {
        let offset = self.buf.len() as u64;
        self.note(number, Entry::Offset(offset, 0));
        write!(self.buf_str(), "{number} 0 obj\n{body}\nendobj\n").unwrap();
        offset
    }

    /// Append a stream object with a correct `/Length`. `dict_extra` is
    /// spliced into the dictionary (e.g. `"/Type/XObject"`).
    pub fn add_stream(&mut self, number: u32, dict_extra: &str, data: &[u8]) -> u64 {
        self.add_stream_with_length(number, dict_extra, data, data.len() as i64)
    }

    /// Append a stream object with an explicit (possibly lying) `/Length`.
    pub fn add_stream_with_length(
        &mut self,
        number: u32,
        dict_extra: &str,
        data: &[u8],
        declared_len: i64,
    ) -> u64 {
        let offset = self.buf.len() as u64;
        self.note(number, Entry::Offset(offset, 0));
        write!(
            self.buf_str(),
            "{number} 0 obj\n<<{dict_extra}/Length {declared_len}>>\nstream\n"
        )
        .unwrap();
        self.buf.extend_from_slice(data);
        self.buf.extend_from_slice(b"\nendstream\nendobj\n");
        offset
    }

    /// Append an *uncompressed* `/Type /ObjStm` object stream containing
    /// `members` (object number, body source). Member entries are recorded
    /// for the next xref stream; the containing revision must therefore be
    /// finished with [`Self::finish_xref_stream`].
    pub fn add_object_stream(&mut self, number: u32, members: &[(u32, &str)]) -> u64 {
        let mut pairs = String::new();
        let mut bodies = String::new();
        for (member_number, body) in members {
            if !bodies.is_empty() {
                bodies.push(' ');
            }
            write!(pairs, "{member_number} {} ", bodies.len()).unwrap();
            bodies.push_str(body);
        }
        let first = pairs.len();
        let data = format!("{pairs}{bodies}");
        for (index, (member_number, _)) in members.iter().enumerate() {
            self.note(
                *member_number,
                Entry::InStream {
                    container: number,
                    index: index as u32,
                },
            );
        }
        self.add_stream(
            number,
            &format!("/Type/ObjStm/N {}/First {first}", members.len()),
            data.as_bytes(),
        )
    }

    /// Finish the revision with a classic xref table + trailer +
    /// `startxref`. `trailer_extra` must contain `/Root N 0 R` for the
    /// first revision. Panics if the revision contains object-stream
    /// members (those need an xref stream).
    pub fn finish_classic_xref(&mut self, trailer_extra: &str) {
        let xref_offset = self.buf.len() as u64;
        let mut entries: Vec<(u32, u64, u16)> = Vec::new();
        if self.prev_xref.is_none() {
            entries.push((0, 0, u16::MAX)); // free-list head
        }
        for (&number, entry) in &self.revision {
            match *entry {
                Entry::Offset(offset, generation) => entries.push((number, offset, generation)),
                Entry::InStream { .. } => {
                    panic!("object-stream members need finish_xref_stream")
                }
            }
        }
        self.revision.clear();

        self.buf.extend_from_slice(b"xref\n");
        for run in consecutive_runs(&entries) {
            writeln!(self.buf_str(), "{} {}", run[0].0, run.len()).unwrap();
            for &(number, offset, generation) in run {
                if number == 0 || (offset == 0 && generation == u16::MAX) {
                    write!(self.buf_str(), "0000000000 65535 f\r\n").unwrap();
                } else {
                    write!(self.buf_str(), "{offset:010} {generation:05} n\r\n").unwrap();
                }
            }
        }
        let size = self.max_number + 1;
        write!(self.buf_str(), "trailer\n<</Size {size}{trailer_extra}").unwrap();
        if let Some(prev) = self.prev_xref {
            write!(self.buf_str(), "/Prev {prev}").unwrap();
        }
        write!(self.buf_str(), ">>\nstartxref\n{xref_offset}\n%%EOF\n").unwrap();
        self.prev_xref = Some(xref_offset);
    }

    /// Finish the revision with an *uncompressed* cross-reference stream
    /// (object `number`), covering every object added this revision plus
    /// the stream itself. `trailer_extra` is spliced into the stream dict
    /// (e.g. `"/Root 1 0 R"`).
    pub fn finish_xref_stream(&mut self, number: u32, trailer_extra: &str) {
        let xref_offset = self.buf.len() as u64;
        self.note(number, Entry::Offset(xref_offset, 0));
        let mut rows: Vec<(u32, [u8; 7])> = Vec::new(); // W [1 4 2]
        if self.prev_xref.is_none() {
            rows.push((0, [0, 0, 0, 0, 0, 0xFF, 0xFF]));
        }
        for (&object_number, entry) in &self.revision {
            let row = match *entry {
                Entry::Offset(offset, generation) => {
                    let o = u32::try_from(offset)
                        .expect("fixture too large")
                        .to_be_bytes();
                    let g = generation.to_be_bytes();
                    [1, o[0], o[1], o[2], o[3], g[0], g[1]]
                }
                Entry::InStream { container, index } => {
                    let c = container.to_be_bytes();
                    let i = u16::try_from(index)
                        .expect("objstm too large")
                        .to_be_bytes();
                    [2, c[0], c[1], c[2], c[3], i[0], i[1]]
                }
            };
            rows.push((object_number, row));
        }
        self.revision.clear();

        let mut index = String::new();
        let mut data: Vec<u8> = Vec::new();
        let entries: Vec<(u32, [u8; 7])> = rows;
        let numbers: Vec<(u32, u64, u16)> = entries.iter().map(|&(n, _)| (n, 0, 0)).collect();
        for run in consecutive_runs(&numbers) {
            write!(index, " {} {}", run[0].0, run.len()).unwrap();
        }
        for &(_, row) in &entries {
            data.extend_from_slice(&row);
        }

        let mut dict = format!(
            "/Type/XRef/Size {}/W[1 4 2]/Index[{}]{trailer_extra}",
            self.max_number + 1,
            index.trim(),
        );
        if let Some(prev) = self.prev_xref {
            write!(dict, "/Prev {prev}").unwrap();
        }
        write!(
            self.buf_str(),
            "{number} 0 obj\n<<{dict}/Length {}>>\nstream\n",
            data.len()
        )
        .unwrap();
        self.buf.extend_from_slice(&data);
        write!(
            self.buf_str(),
            "\nendstream\nendobj\nstartxref\n{xref_offset}\n%%EOF\n"
        )
        .unwrap();
        self.prev_xref = Some(xref_offset);
    }

    /// `Vec<u8>` implements `fmt::Write` only via a wrapper; this gives the
    /// builder ergonomic `write!` calls (fixture bytes are ASCII anyway).
    fn buf_str(&mut self) -> BufWriter<'_> {
        BufWriter(&mut self.buf)
    }
}

struct BufWriter<'a>(&'a mut Vec<u8>);

impl std::fmt::Write for BufWriter<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

/// Group `(number, …)` tuples (sorted) into consecutive-number runs.
fn consecutive_runs<T>(entries: &[(u32, T, u16)]) -> Vec<&[(u32, T, u16)]>
where
    T: Copy,
{
    let mut runs = Vec::new();
    let mut start = 0usize;
    for i in 1..=entries.len() {
        if i == entries.len() || entries[i].0 != entries[i - 1].0 + 1 {
            runs.push(&entries[start..i]);
            start = i;
        }
    }
    runs
}

/// Object-number layout used by the canned fixtures.
pub mod layout {
    pub const CATALOG: u32 = 1;
    pub const PAGES: u32 = 2;
    pub const OBJSTM: u32 = 100;
    pub const FONT: u32 = 101;
    pub const EXTRA: u32 = 102;
    pub const XREF_STREAM: u32 = 200;

    pub fn page(i: u32) -> u32 {
        10 + 2 * i
    }
    pub fn content(i: u32) -> u32 {
        11 + 2 * i
    }
    pub fn content_bytes(i: u32) -> Vec<u8> {
        format!("BT /F1 12 Tf 72 720 Td (Page {i}) Tj ET").into_bytes()
    }
    pub fn updated_content_bytes(i: u32) -> Vec<u8> {
        format!("BT /F1 12 Tf 72 720 Td (Updated page {i}) Tj ET").into_bytes()
    }
}

/// Write catalog + page tree + per-page content streams (one revision's
/// worth of body objects, no xref yet).
fn add_document_body(builder: &mut PdfBuilder, page_count: u32) {
    use layout::*;
    builder.add_object(CATALOG, &format!("<</Type/Catalog/Pages {PAGES} 0 R>>"));
    let kids: Vec<String> = (0..page_count)
        .map(|i| format!("{} 0 R", page(i)))
        .collect();
    builder.add_object(
        PAGES,
        &format!(
            "<</Type/Pages/Kids[{}]/Count {page_count}/MediaBox[0 0 612 792]>>",
            kids.join(" ")
        ),
    );
    for i in 0..page_count {
        // Page 1 carries its own rotation and crop box; everything else
        // inherits. Resources reference the object-stream font.
        let extras = if i == 1 {
            "/Rotate 90/CropBox[10 10 400 500]"
        } else {
            ""
        };
        builder.add_object(
            page(i),
            &format!(
                "<</Type/Page/Parent {PAGES} 0 R/Contents {} 0 R{extras}\
                 /Resources<</Font<</F1 {FONT} 0 R>>>>>>",
                content(i)
            ),
        );
        builder.add_stream(content(i), "", &content_bytes(i));
    }
}

/// A clean `page_count`-page PDF with a classic xref table. The font the
/// pages reference is a plain indirect object (no object streams).
pub fn multipage_classic(page_count: u32) -> Vec<u8> {
    let mut builder = PdfBuilder::new();
    add_document_body(&mut builder, page_count);
    builder.add_object(
        layout::FONT,
        "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>",
    );
    builder.finish_classic_xref("/Root 1 0 R");
    builder.into_bytes()
}

/// The Phase 1 exit-gate fixture: `page_count` pages (classic first
/// revision) + an incremental update whose xref stream adds an object
/// stream (containing the shared font) and rewrites page 0's content.
pub fn phase1_exit_fixture(page_count: u32) -> Vec<u8> {
    use layout::*;
    assert!(page_count >= 1);
    let mut builder = PdfBuilder::new();
    add_document_body(&mut builder, page_count);
    // First revision: the font object does not exist yet (forward ref from
    // the pages is fine — it resolves after the update).
    builder.finish_classic_xref("/Root 1 0 R");

    // Incremental update: object stream with the font + an extra dict,
    // plus a replacement content stream for page 0.
    builder.add_object_stream(
        OBJSTM,
        &[
            (FONT, "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>"),
            (EXTRA, "<</Kind(objstm-extra)>>"),
        ],
    );
    builder.add_stream(content(0), "", &updated_content_bytes(0));
    builder.finish_xref_stream(XREF_STREAM, "/Root 1 0 R");
    builder.into_bytes()
}

/// Single-revision file indexed *only* by a cross-reference stream, with
/// the font inside an object stream.
pub fn xref_stream_fixture(page_count: u32) -> Vec<u8> {
    use layout::*;
    let mut builder = PdfBuilder::new();
    add_document_body(&mut builder, page_count);
    builder.add_object_stream(
        OBJSTM,
        &[(FONT, "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>")],
    );
    builder.finish_xref_stream(XREF_STREAM, "/Root 1 0 R");
    builder.into_bytes()
}

/// Prefix a fixture with garbage. Structural offsets are header-relative,
/// so the fixture stays valid — exactly the real-world case this exercises.
pub fn with_leading_garbage(fixture: Vec<u8>, garbage: &[u8]) -> Vec<u8> {
    let mut out = garbage.to_vec();
    out.extend_from_slice(&fixture);
    out
}

/// Corrupt a fixture by deleting its final `startxref` block. The xref
/// table itself survives, so a reader can still recover it by scanning.
pub fn without_startxref(mut fixture: Vec<u8>) -> Vec<u8> {
    let pos = fixture
        .windows(9)
        .rposition(|w| w == b"startxref")
        .expect("fixture has no startxref");
    fixture.truncate(pos);
    fixture.extend_from_slice(b"%%EOF\n");
    fixture
}

/// Corrupt a fixture by deleting the entire xref section (table, trailer,
/// startxref) — only object bodies remain, forcing a full rebuild.
pub fn without_xref(mut fixture: Vec<u8>) -> Vec<u8> {
    let pos = fixture
        .windows(5)
        .rposition(|w| w == b"\nxref")
        .expect("fixture has no xref table");
    fixture.truncate(pos + 1);
    fixture.extend_from_slice(b"%%EOF\n");
    fixture
}

/// A one-page document whose content stream declares a wrong `/Length`.
pub fn wrong_length_fixture() -> Vec<u8> {
    use layout::*;
    let mut builder = PdfBuilder::new();
    builder.add_object(CATALOG, &format!("<</Type/Catalog/Pages {PAGES} 0 R>>"));
    builder.add_object(
        PAGES,
        &format!(
            "<</Type/Pages/Kids[{} 0 R]/Count 1/MediaBox[0 0 612 792]>>",
            page(0)
        ),
    );
    builder.add_object(
        page(0),
        &format!(
            "<</Type/Page/Parent {PAGES} 0 R/Contents {} 0 R>>",
            content(0)
        ),
    );
    let data = content_bytes(0);
    builder.add_stream_with_length(content(0), "", &data, 3); // lies
    builder.finish_classic_xref("/Root 1 0 R");
    builder.into_bytes()
}

/// A one-page document carrying an `/Encrypt` dictionary (RC4 V2/R3) — a
/// supported scheme, so it *opens* (the derived key is exercised even though
/// this fixture has no encrypted payload to check).
pub fn encrypted_fixture() -> Vec<u8> {
    use layout::*;
    let mut builder = PdfBuilder::new();
    builder.add_object(CATALOG, &format!("<</Type/Catalog/Pages {PAGES} 0 R>>"));
    builder.add_object(
        PAGES,
        &format!("<</Type/Pages/Kids[{} 0 R]/Count 1>>", page(0)),
    );
    builder.add_object(page(0), &format!("<</Type/Page/Parent {PAGES} 0 R>>"));
    // A conforming /U for the empty user password: `DocumentSnapshot::open`
    // genuinely validates /U (Algorithm 6), so the fixture must store the
    // value a real writer would.
    let enc = pdf_security::EncryptDict {
        v: 2,
        r: 3,
        o: b"o".to_vec(),
        u: vec![],
        ue: vec![],
        oe: vec![],
        p: -44,
        perms: vec![],
        key_bytes: 16,
        encrypt_metadata: true,
        cipher: pdf_security::Cipher::Rc4,
        file_id: vec![0x01],
    };
    let u = pdf_security::StandardHandler::compute_user_entry(&enc, "");
    let u_hex: String = u.iter().map(|b| format!("{b:02x}")).collect();
    builder.add_object(
        50,
        &format!("<</Filter/Standard/V 2/R 3/Length 128/P -44/O(o)/U<{u_hex}>>>"),
    );
    builder.finish_classic_xref("/Root 1 0 R/Encrypt 50 0 R/ID[<01><02>]");
    builder.into_bytes()
}

/// A one-page document carrying an AES-256 (`/V 5 /R 6`) `/Encrypt` dictionary
/// — a scheme the standard handler declines, so open must fail with a typed
/// security error rather than pretend the file parsed.
pub fn aes256_encrypted_fixture() -> Vec<u8> {
    use layout::*;
    let mut builder = PdfBuilder::new();
    builder.add_object(CATALOG, &format!("<</Type/Catalog/Pages {PAGES} 0 R>>"));
    builder.add_object(
        PAGES,
        &format!("<</Type/Pages/Kids[{} 0 R]/Count 1>>", page(0)),
    );
    builder.add_object(page(0), &format!("<</Type/Page/Parent {PAGES} 0 R>>"));
    builder.add_object(
        50,
        "<</Filter/Standard/V 5/R 6/Length 256/P -44\
         /CF<</StdCF<</CFM/AESV3>>>>/StmF/StdCF/StrF/StdCF/O(o)/U(u)>>",
    );
    builder.finish_classic_xref("/Root 1 0 R/Encrypt 50 0 R/ID[<01><02>]");
    builder.into_bytes()
}

/// Hybrid-reference fixture: classic table for the base objects plus an
/// /XRefStm whose stream indexes the object-stream members.
pub fn hybrid_fixture(page_count: u32) -> Vec<u8> {
    use layout::*;
    // Build the pieces manually: body, xref stream (not startxref'd), then
    // the classic table pointing at the stream via /XRefStm.
    let mut builder = PdfBuilder::new();
    add_document_body(&mut builder, page_count);
    builder.add_object_stream(
        OBJSTM,
        &[(FONT, "<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>")],
    );

    // Snapshot the entries the classic table must NOT know about (the
    // members) vs the ones it lists. We reach into the byte stream: write
    // the xref stream with finish_xref_stream, then strip its trailing
    // startxref block and append the classic section.
    let objstm_and_member_revision_done = {
        builder.finish_xref_stream(XREF_STREAM, "");
        true
    };
    assert!(objstm_and_member_revision_done);
    let mut bytes = builder.into_bytes();
    // Remove the xref stream's startxref/%%EOF tail; remember its offset.
    let sx = bytes
        .windows(9)
        .rposition(|w| w == b"startxref")
        .expect("startxref");
    let stream_offset: u64 = {
        let tail = &bytes[sx..];
        let text = std::str::from_utf8(tail).expect("ascii tail");
        text.lines()
            .nth(1)
            .expect("offset line")
            .trim()
            .parse()
            .expect("offset")
    };
    bytes.truncate(sx);

    // Classic table: catalog, pages, page/content objects (offsets must be
    // re-derived — scan for their headers, test-support style).
    let mut entries: Vec<(u32, u64)> = Vec::new();
    for number in [CATALOG, PAGES]
        .into_iter()
        .chain((0..page_count).flat_map(|i| [page(i), content(i)]))
    {
        let needle = format!("{number} 0 obj");
        let off = bytes
            .windows(needle.len())
            .position(|w| w == needle.as_bytes())
            .unwrap_or_else(|| panic!("object {number} not found")) as u64;
        entries.push((number, off));
    }
    entries.sort_unstable();

    let table_offset = bytes.len();
    let mut out = BufWriter(&mut bytes);
    write!(out, "xref\n0 1\n0000000000 65535 f\r\n").unwrap();
    let mut i = 0usize;
    while i < entries.len() {
        let mut j = i + 1;
        while j < entries.len() && entries[j].0 == entries[j - 1].0 + 1 {
            j += 1;
        }
        writeln!(out, "{} {}", entries[i].0, j - i).unwrap();
        for &(_, off) in &entries[i..j] {
            write!(out, "{off:010} 00000 n\r\n").unwrap();
        }
        i = j;
    }
    write!(
        out,
        "trailer\n<</Size {}/Root 1 0 R/XRefStm {stream_offset}>>\nstartxref\n{table_offset}\n%%EOF\n",
        XREF_STREAM + 1
    )
    .unwrap();
    bytes
}
