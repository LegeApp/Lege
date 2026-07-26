//! Differential text-extraction oracle using PDFium's public API only.
//!
//! This crate is intentionally outside the workspace: PDFium is loaded at
//! runtime and remains a development oracle, never an engine dependency.

use std::ffi::{CString, c_char, c_double, c_float, c_int, c_uint, c_void};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use libloading::{Library, Symbol};
use pdf_content::PageCompiler;
use pdf_document::{DocumentLimits, DocumentSnapshot, PageIndex, ParseContext};
use pdf_source::{MmapSource, PdfSource};

type Handle = *mut c_void;

#[derive(Debug, Clone, Copy, Default)]
#[repr(C)]
struct FsRectF {
    left: c_float,
    top: c_float,
    right: c_float,
    bottom: c_float,
}

#[derive(Debug, Clone, Copy, Default)]
struct Bounds {
    left: f64,
    bottom: f64,
    right: f64,
    top: f64,
}

#[derive(Debug, Clone, Default)]
struct OracleChar {
    unicode: u32,
    tight: Bounds,
    loose: Bounds,
    generated: bool,
    hyphen: bool,
    unicode_map_error: bool,
    object: usize,
}

#[derive(Debug, Clone, Default)]
struct OraclePage {
    text: Vec<u16>,
    chars: Vec<OracleChar>,
    rects: Vec<(Bounds, Vec<u16>)>,
}

struct Pdfium {
    library: Library,
}

impl Pdfium {
    unsafe fn load(path: &Path) -> Result<Self, String> {
        let library = unsafe { Library::new(path) }
            .map_err(|error| format!("loading {}: {error}", path.display()))?;
        unsafe {
            let init: Symbol<unsafe extern "C" fn()> = library
                .get(b"FPDF_InitLibrary\0")
                .map_err(|error| error.to_string())?;
            init();
        }
        Ok(Self { library })
    }

    fn page_count(&self, pdf: &Path) -> Result<i32, String> {
        unsafe {
            let (document, close_document) = self.open_document(pdf)?;
            let count: Symbol<unsafe extern "C" fn(Handle) -> c_int> = self
                .library
                .get(b"FPDF_GetPageCount\0")
                .map_err(|error| error.to_string())?;
            let result = count(document);
            close_document(document);
            Ok(result)
        }
    }

    fn extract(&self, pdf: &Path, page_index: i32) -> Result<OraclePage, String> {
        unsafe {
            let (document, close_document) = self.open_document(pdf)?;
            let load_page: Symbol<unsafe extern "C" fn(Handle, c_int) -> Handle> = self
                .library
                .get(b"FPDF_LoadPage\0")
                .map_err(|error| error.to_string())?;
            let close_page: Symbol<unsafe extern "C" fn(Handle)> = self
                .library
                .get(b"FPDF_ClosePage\0")
                .map_err(|error| error.to_string())?;
            let text_load: Symbol<unsafe extern "C" fn(Handle) -> Handle> = self
                .library
                .get(b"FPDFText_LoadPage\0")
                .map_err(|error| error.to_string())?;
            let text_close: Symbol<unsafe extern "C" fn(Handle)> = self
                .library
                .get(b"FPDFText_ClosePage\0")
                .map_err(|error| error.to_string())?;

            let page = load_page(document, page_index);
            if page.is_null() {
                close_document(document);
                return Err(format!("PDFium could not load page {page_index}"));
            }
            let text_page = text_load(page);
            if text_page.is_null() {
                close_page(page);
                close_document(document);
                return Err(format!("PDFium could not load text page {page_index}"));
            }
            let result = self.extract_loaded(text_page);
            text_close(text_page);
            close_page(page);
            close_document(document);
            result
        }
    }

    unsafe fn open_document(
        &self,
        pdf: &Path,
    ) -> Result<(Handle, Symbol<'_, unsafe extern "C" fn(Handle)>), String> {
        let load: Symbol<unsafe extern "C" fn(*const c_char, *const c_char) -> Handle> =
            unsafe { self.library.get(b"FPDF_LoadDocument\0") }
                .map_err(|error| error.to_string())?;
        let close: Symbol<unsafe extern "C" fn(Handle)> =
            unsafe { self.library.get(b"FPDF_CloseDocument\0") }
                .map_err(|error| error.to_string())?;
        let path =
            CString::new(pdf.to_string_lossy().as_bytes()).map_err(|error| error.to_string())?;
        let document = unsafe { load(path.as_ptr(), std::ptr::null()) };
        if document.is_null() {
            return Err(format!("PDFium could not open {}", pdf.display()));
        }
        Ok((document, close))
    }

    unsafe fn extract_loaded(&self, text_page: Handle) -> Result<OraclePage, String> {
        macro_rules! symbol {
            ($name:literal, $ty:ty) => {{
                let value: Symbol<$ty> =
                    unsafe { self.library.get(concat!($name, "\0").as_bytes()) }
                        .map_err(|error| error.to_string())?;
                value
            }};
        }
        let count_chars = symbol!("FPDFText_CountChars", unsafe extern "C" fn(Handle) -> c_int);
        let get_text = symbol!(
            "FPDFText_GetText",
            unsafe extern "C" fn(Handle, c_int, c_int, *mut u16) -> c_int
        );
        let get_unicode = symbol!(
            "FPDFText_GetUnicode",
            unsafe extern "C" fn(Handle, c_int) -> c_uint
        );
        let get_char_box = symbol!(
            "FPDFText_GetCharBox",
            unsafe extern "C" fn(
                Handle,
                c_int,
                *mut c_double,
                *mut c_double,
                *mut c_double,
                *mut c_double,
            ) -> c_int
        );
        let get_loose_box = symbol!(
            "FPDFText_GetLooseCharBox",
            unsafe extern "C" fn(Handle, c_int, *mut FsRectF) -> c_int
        );
        let is_generated = symbol!(
            "FPDFText_IsGenerated",
            unsafe extern "C" fn(Handle, c_int) -> c_int
        );
        let is_hyphen = symbol!(
            "FPDFText_IsHyphen",
            unsafe extern "C" fn(Handle, c_int) -> c_int
        );
        let has_unicode_error = symbol!(
            "FPDFText_HasUnicodeMapError",
            unsafe extern "C" fn(Handle, c_int) -> c_int
        );
        let get_object = symbol!(
            "FPDFText_GetTextObject",
            unsafe extern "C" fn(Handle, c_int) -> Handle
        );
        let count_rects = symbol!(
            "FPDFText_CountRects",
            unsafe extern "C" fn(Handle, c_int, c_int) -> c_int
        );
        let get_rect = symbol!(
            "FPDFText_GetRect",
            unsafe extern "C" fn(
                Handle,
                c_int,
                *mut c_double,
                *mut c_double,
                *mut c_double,
                *mut c_double,
            ) -> c_int
        );
        let bounded_text = symbol!(
            "FPDFText_GetBoundedText",
            unsafe extern "C" fn(
                Handle,
                c_double,
                c_double,
                c_double,
                c_double,
                *mut u16,
                c_int,
            ) -> c_int
        );

        let count = unsafe { count_chars(text_page) }.max(0);
        let mut text = vec![0u16; count as usize + 1];
        let written = unsafe { get_text(text_page, 0, count, text.as_mut_ptr()) }.max(0) as usize;
        text.truncate(written.saturating_sub(1).min(text.len()));

        let mut chars = Vec::with_capacity(count as usize);
        let mut object_handles: Vec<Handle> = Vec::new();
        for index in 0..count {
            let mut tight = Bounds::default();
            unsafe {
                get_char_box(
                    text_page,
                    index,
                    &mut tight.left,
                    &mut tight.right,
                    &mut tight.bottom,
                    &mut tight.top,
                );
            }
            let mut loose = FsRectF::default();
            unsafe {
                get_loose_box(text_page, index, &mut loose);
            }
            let handle = unsafe { get_object(text_page, index) };
            let object = object_handles
                .iter()
                .position(|&known| known == handle)
                .unwrap_or_else(|| {
                    object_handles.push(handle);
                    object_handles.len() - 1
                });
            chars.push(OracleChar {
                unicode: unsafe { get_unicode(text_page, index) },
                tight,
                loose: Bounds {
                    left: f64::from(loose.left),
                    bottom: f64::from(loose.bottom),
                    right: f64::from(loose.right),
                    top: f64::from(loose.top),
                },
                generated: unsafe { is_generated(text_page, index) } == 1,
                hyphen: unsafe { is_hyphen(text_page, index) } == 1,
                unicode_map_error: unsafe { has_unicode_error(text_page, index) } == 1,
                object,
            });
        }

        let rect_count = unsafe { count_rects(text_page, 0, count) }.max(0);
        let mut rects = Vec::with_capacity(rect_count as usize);
        for index in 0..rect_count {
            let mut bounds = Bounds::default();
            if unsafe {
                get_rect(
                    text_page,
                    index,
                    &mut bounds.left,
                    &mut bounds.top,
                    &mut bounds.right,
                    &mut bounds.bottom,
                )
            } == 0
            {
                continue;
            }
            let needed = unsafe {
                bounded_text(
                    text_page,
                    bounds.left,
                    bounds.top,
                    bounds.right,
                    bounds.bottom,
                    std::ptr::null_mut(),
                    0,
                )
            }
            .max(0);
            let mut contents = vec![0u16; needed as usize + 1];
            let written = unsafe {
                bounded_text(
                    text_page,
                    bounds.left,
                    bounds.top,
                    bounds.right,
                    bounds.bottom,
                    contents.as_mut_ptr(),
                    contents.len() as c_int,
                )
            }
            .max(0) as usize;
            contents.truncate(written.min(contents.len()));
            rects.push((bounds, contents));
        }
        Ok(OraclePage { text, chars, rects })
    }
}

impl Drop for Pdfium {
    fn drop(&mut self) {
        unsafe {
            if let Ok(destroy) = self
                .library
                .get::<unsafe extern "C" fn()>(b"FPDF_DestroyLibrary\0")
            {
                destroy();
            }
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("pdfium-text-diff: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if !(2..=3).contains(&args.len()) {
        return Err("usage: pdfium-text-diff <libpdfium.so> <file.pdf> [zero-based-page]".into());
    }
    let library = PathBuf::from(&args[0]);
    let pdf = PathBuf::from(&args[1]);
    let requested = args
        .get(2)
        .map(|value| value.to_string_lossy().parse::<i32>())
        .transpose()
        .map_err(|error| format!("invalid page: {error}"))?;
    let pdfium = unsafe { Pdfium::load(&library)? };
    let source: Arc<dyn PdfSource> = Arc::new(
        MmapSource::open(&pdf).map_err(|error| format!("opening {}: {error}", pdf.display()))?,
    );
    let document = DocumentSnapshot::open(source, DocumentLimits::default())
        .map_err(|error| format!("opening our document: {error}"))?;
    let count = pdfium.page_count(&pdf)?.min(document.page_count() as i32);
    let pages: Box<dyn Iterator<Item = i32>> = if let Some(page) = requested {
        Box::new(std::iter::once(page))
    } else {
        Box::new(0..count)
    };

    println!(
        "page,text_edit_distance,char_count_delta,char_field_mismatches,tight_box_mismatches,loose_box_mismatches,rect_count_delta,rect_text_mismatches"
    );
    let mut context = ParseContext::new();
    let compiler = PageCompiler::new();
    for index in pages {
        let oracle = pdfium.extract(&pdf, index)?;
        let semantic = compiler
            .compile_semantic(&document, PageIndex(index as u32), &mut context)
            .map_err(|error| format!("compiling page {index}: {error}"))?;
        let ours = pdf_text::TextPage::build(&semantic, &pdf_text::TextPageOptions::default());
        let edit = edit_distance(ours.all_text_utf16(), &oracle.text);
        let char_delta = ours.char_count() as isize - oracle.chars.len() as isize;
        let char_field_mismatches = ours
            .chars()
            .iter()
            .zip(&oracle.chars)
            .filter(|(ours, reference)| {
                ours.unicode != reference.unicode
                    || (ours.char_type == pdf_text::CharType::Generated) != reference.generated
                    || (ours.char_type == pdf_text::CharType::Hyphen) != reference.hyphen
                    || (ours.char_type == pdf_text::CharType::NotUnicode)
                        != reference.unicode_map_error
            })
            .count();
        let tight_box_mismatches = ours
            .chars()
            .iter()
            .zip(&oracle.chars)
            .filter(|(ours, reference)| {
                differs(ours.char_box.x0, reference.tight.left)
                    || differs(ours.char_box.y0, reference.tight.bottom)
                    || differs(ours.char_box.x1, reference.tight.right)
                    || differs(ours.char_box.y1, reference.tight.top)
            })
            .count();
        let loose_box_mismatches = ours
            .chars()
            .iter()
            .zip(&oracle.chars)
            .filter(|(ours, reference)| {
                differs(ours.loose_char_box.x0, reference.loose.left)
                    || differs(ours.loose_char_box.y0, reference.loose.bottom)
                    || differs(ours.loose_char_box.x1, reference.loose.right)
                    || differs(ours.loose_char_box.y1, reference.loose.top)
            })
            .count();
        let our_rects = ours.rects(0, ours.char_count());
        let rect_delta = our_rects.len() as isize - oracle.rects.len() as isize;
        let rect_text_mismatches = our_rects
            .iter()
            .zip(&oracle.rects)
            .filter(|(rect, (_, text))| ours.text_in_rect_utf16(**rect) != *text)
            .count();
        println!(
            "{index},{edit},{char_delta},{char_field_mismatches},{tight_box_mismatches},{loose_box_mismatches},{rect_delta},{rect_text_mismatches}"
        );
        if std::env::var_os("PDFIUM_TEXT_DIFF_VERBOSE").is_some() {
            eprintln!(
                "page {index} ours({})={:?}",
                ours.char_count(),
                ours.all_text()
            );
            eprintln!(
                "page {index} pdfium({})={:?}",
                oracle.chars.len(),
                String::from_utf16_lossy(&oracle.text)
            );
            for (char_index, (ours, reference)) in
                ours.chars().iter().zip(&oracle.chars).enumerate().take(40)
            {
                eprintln!(
                    "  {char_index}: ours U+{:04X} {:?} [{:.2},{:.2},{:.2},{:.2}] | pdfium U+{:04X} gen={} hyphen={} maperr={} obj={} [{:.2},{:.2},{:.2},{:.2}] loose=[{:.2},{:.2},{:.2},{:.2}]",
                    ours.unicode,
                    ours.char_type,
                    ours.char_box.x0,
                    ours.char_box.y0,
                    ours.char_box.x1,
                    ours.char_box.y1,
                    reference.unicode,
                    reference.generated,
                    reference.hyphen,
                    reference.unicode_map_error,
                    reference.object,
                    reference.tight.left,
                    reference.tight.bottom,
                    reference.tight.right,
                    reference.tight.top,
                    reference.loose.left,
                    reference.loose.bottom,
                    reference.loose.right,
                    reference.loose.top,
                );
            }
        }
    }
    Ok(())
}

fn differs(left: f64, right: f64) -> bool {
    (left - right).abs() > 0.01
}

fn edit_distance(left: &[u16], right: &[u16]) -> usize {
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0usize; right.len() + 1];
    for (left_index, &left_unit) in left.iter().enumerate() {
        current[0] = left_index + 1;
        for (right_index, &right_unit) in right.iter().enumerate() {
            current[right_index + 1] = if left_unit == right_unit {
                previous[right_index]
            } else {
                1 + previous[right_index]
                    .min(current[right_index])
                    .min(previous[right_index + 1])
            };
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_distance_counts_utf16_units() {
        assert_eq!(edit_distance(&[1, 2, 3], &[1, 4, 3]), 1);
        assert_eq!(edit_distance(&[], &[1, 2]), 2);
    }
}
