//! A minimal runtime binding to a prebuilt `libpdfium`.
//!
//! Only the handful of entry points a page render needs. The library is
//! `dlopen`ed rather than linked so the engine keeps no build-time
//! relationship with PDFium — it is an oracle we grade against, not a
//! dependency.
//!
//! PDFium is not thread-safe across documents without its own init dance, so
//! this is deliberately single-threaded.

use std::ffi::{c_char, c_int, c_ulong, c_void, CString};
use std::path::Path;
use std::time::{Duration, Instant};

use libloading::{Library, Symbol};

type Handle = *mut c_void;

/// `FPDF_ANNOT` render flag: draw annotation static appearances
/// (public/fpdfview.h). Our engine matches with
/// `PageCompiler::with_annotations(true)`.
const FPDF_ANNOT: c_int = 0x01;

pub struct Pdfium {
    lib: Library,
}

/// One rendered page: BGRA, top-down.
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

impl Pdfium {
    /// Load `libpdfium.so` and initialise it.
    ///
    /// # Safety
    /// Loading arbitrary native code is inherently unsafe; the caller vouches
    /// for the path.
    pub unsafe fn load(path: &Path) -> Result<Pdfium, String> {
        let lib = unsafe { Library::new(path) }.map_err(|e| format!("dlopen {path:?}: {e}"))?;
        unsafe {
            let init: Symbol<unsafe extern "C" fn()> =
                lib.get(b"FPDF_InitLibrary\0").map_err(|e| e.to_string())?;
            init();
        }
        Ok(Pdfium { lib })
    }

    /// Render page `index` of `pdf` at `scale` (1.0 = 72 dpi), on white.
    pub fn render(&self, pdf: &Path, index: i32, scale: f64) -> Result<Bitmap, String> {
        unsafe {
            let load_doc: Symbol<unsafe extern "C" fn(*const c_char, *const c_char) -> Handle> =
                self.lib
                    .get(b"FPDF_LoadDocument\0")
                    .map_err(|e| e.to_string())?;
            let close_doc: Symbol<unsafe extern "C" fn(Handle)> = self
                .lib
                .get(b"FPDF_CloseDocument\0")
                .map_err(|e| e.to_string())?;
            let load_page: Symbol<unsafe extern "C" fn(Handle, c_int) -> Handle> = self
                .lib
                .get(b"FPDF_LoadPage\0")
                .map_err(|e| e.to_string())?;
            let close_page: Symbol<unsafe extern "C" fn(Handle)> = self
                .lib
                .get(b"FPDF_ClosePage\0")
                .map_err(|e| e.to_string())?;
            let page_w: Symbol<unsafe extern "C" fn(Handle) -> f32> = self
                .lib
                .get(b"FPDF_GetPageWidthF\0")
                .map_err(|e| e.to_string())?;
            let page_h: Symbol<unsafe extern "C" fn(Handle) -> f32> = self
                .lib
                .get(b"FPDF_GetPageHeightF\0")
                .map_err(|e| e.to_string())?;
            let bmp_create: Symbol<unsafe extern "C" fn(c_int, c_int, c_int) -> Handle> = self
                .lib
                .get(b"FPDFBitmap_Create\0")
                .map_err(|e| e.to_string())?;
            let bmp_fill: Symbol<
                unsafe extern "C" fn(Handle, c_int, c_int, c_int, c_int, c_ulong),
            > = self
                .lib
                .get(b"FPDFBitmap_FillRect\0")
                .map_err(|e| e.to_string())?;
            let bmp_buf: Symbol<unsafe extern "C" fn(Handle) -> *mut c_void> = self
                .lib
                .get(b"FPDFBitmap_GetBuffer\0")
                .map_err(|e| e.to_string())?;
            let bmp_stride: Symbol<unsafe extern "C" fn(Handle) -> c_int> = self
                .lib
                .get(b"FPDFBitmap_GetStride\0")
                .map_err(|e| e.to_string())?;
            let bmp_destroy: Symbol<unsafe extern "C" fn(Handle)> = self
                .lib
                .get(b"FPDFBitmap_Destroy\0")
                .map_err(|e| e.to_string())?;
            let render: Symbol<
                unsafe extern "C" fn(Handle, Handle, c_int, c_int, c_int, c_int, c_int, c_int),
            > = self
                .lib
                .get(b"FPDF_RenderPageBitmap\0")
                .map_err(|e| e.to_string())?;

            let cpath =
                CString::new(pdf.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
            let doc = load_doc(cpath.as_ptr(), std::ptr::null());
            if doc.is_null() {
                return Err("PDFium could not open the document".into());
            }
            let page = load_page(doc, index);
            if page.is_null() {
                close_doc(doc);
                return Err(format!("PDFium could not load page {index}"));
            }
            let w = (page_w(page) as f64 * scale).ceil().max(1.0) as i32;
            let h = (page_h(page) as f64 * scale).ceil().max(1.0) as i32;

            // Format 4 = FPDFBitmap_BGRA.
            let bitmap = bmp_create(w, h, 1);
            if bitmap.is_null() {
                close_page(page);
                close_doc(doc);
                return Err("FPDFBitmap_Create failed".into());
            }
            bmp_fill(bitmap, 0, 0, w, h, 0xFFFF_FFFF); // white, like our Background::White
                                                       // FPDF_ANNOT (0x01): draw annotation static appearances — our side
                                                       // compiles with `with_annotations(true)` to match. Keep in sync
                                                       // with the CSV header's `flags=annot` stamp in main.rs.
            render(bitmap, page, 0, 0, w, h, 0, FPDF_ANNOT);

            let stride = bmp_stride(bitmap) as usize;
            let buf = bmp_buf(bitmap) as *const u8;
            let mut bgra = vec![0u8; w as usize * h as usize * 4];
            for y in 0..h as usize {
                let row = std::slice::from_raw_parts(buf.add(y * stride), w as usize * 4);
                bgra[y * w as usize * 4..(y + 1) * w as usize * 4].copy_from_slice(row);
            }
            bmp_destroy(bitmap);
            close_page(page);
            close_doc(doc);
            Ok(Bitmap {
                width: w as u32,
                height: h as u32,
                bgra,
            })
        }
    }

    /// Open `pdf` once and render each page in `indices` at `scale`, returning
    /// the one-time document-open cost and a per-page render duration (load +
    /// rasterize + copy out, matching what `render` does per page). For a fair
    /// speed comparison the document is *not* re-opened per page.
    pub fn render_doc_timed(
        &self,
        pdf: &Path,
        indices: &[i32],
        scale: f64,
    ) -> Result<(Duration, Vec<Duration>), String> {
        unsafe {
            let load_doc: Symbol<unsafe extern "C" fn(*const c_char, *const c_char) -> Handle> =
                self.lib
                    .get(b"FPDF_LoadDocument\0")
                    .map_err(|e| e.to_string())?;
            let close_doc: Symbol<unsafe extern "C" fn(Handle)> = self
                .lib
                .get(b"FPDF_CloseDocument\0")
                .map_err(|e| e.to_string())?;
            let load_page: Symbol<unsafe extern "C" fn(Handle, c_int) -> Handle> = self
                .lib
                .get(b"FPDF_LoadPage\0")
                .map_err(|e| e.to_string())?;
            let close_page: Symbol<unsafe extern "C" fn(Handle)> = self
                .lib
                .get(b"FPDF_ClosePage\0")
                .map_err(|e| e.to_string())?;
            let page_w: Symbol<unsafe extern "C" fn(Handle) -> f32> = self
                .lib
                .get(b"FPDF_GetPageWidthF\0")
                .map_err(|e| e.to_string())?;
            let page_h: Symbol<unsafe extern "C" fn(Handle) -> f32> = self
                .lib
                .get(b"FPDF_GetPageHeightF\0")
                .map_err(|e| e.to_string())?;
            let bmp_create: Symbol<unsafe extern "C" fn(c_int, c_int, c_int) -> Handle> = self
                .lib
                .get(b"FPDFBitmap_Create\0")
                .map_err(|e| e.to_string())?;
            let bmp_fill: Symbol<
                unsafe extern "C" fn(Handle, c_int, c_int, c_int, c_int, c_ulong),
            > = self
                .lib
                .get(b"FPDFBitmap_FillRect\0")
                .map_err(|e| e.to_string())?;
            let bmp_buf: Symbol<unsafe extern "C" fn(Handle) -> *mut c_void> = self
                .lib
                .get(b"FPDFBitmap_GetBuffer\0")
                .map_err(|e| e.to_string())?;
            let bmp_stride: Symbol<unsafe extern "C" fn(Handle) -> c_int> = self
                .lib
                .get(b"FPDFBitmap_GetStride\0")
                .map_err(|e| e.to_string())?;
            let bmp_destroy: Symbol<unsafe extern "C" fn(Handle)> = self
                .lib
                .get(b"FPDFBitmap_Destroy\0")
                .map_err(|e| e.to_string())?;
            let render: Symbol<
                unsafe extern "C" fn(Handle, Handle, c_int, c_int, c_int, c_int, c_int, c_int),
            > = self
                .lib
                .get(b"FPDF_RenderPageBitmap\0")
                .map_err(|e| e.to_string())?;

            let cpath =
                CString::new(pdf.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
            let t_open = Instant::now();
            let doc = load_doc(cpath.as_ptr(), std::ptr::null());
            if doc.is_null() {
                return Err("PDFium could not open the document".into());
            }
            let open = t_open.elapsed();

            let mut times = Vec::with_capacity(indices.len());
            for &index in indices {
                let t = Instant::now();
                let page = load_page(doc, index);
                if page.is_null() {
                    times.push(Duration::ZERO);
                    continue;
                }
                let w = (page_w(page) as f64 * scale).ceil().max(1.0) as i32;
                let h = (page_h(page) as f64 * scale).ceil().max(1.0) as i32;
                let bitmap = bmp_create(w, h, 1);
                if bitmap.is_null() {
                    close_page(page);
                    times.push(Duration::ZERO);
                    continue;
                }
                bmp_fill(bitmap, 0, 0, w, h, 0xFFFF_FFFF);
                // Same FPDF_ANNOT flag as the diff path so timing compares
                // like-for-like work.
                render(bitmap, page, 0, 0, w, h, 0, FPDF_ANNOT);
                // Copy the buffer out, mirroring `render`'s work and our own
                // `pixels.to_vec()`, so neither side gets a free pass.
                let stride = bmp_stride(bitmap) as usize;
                let buf = bmp_buf(bitmap) as *const u8;
                let mut sink = vec![0u8; w as usize * h as usize * 4];
                for y in 0..h as usize {
                    let row = std::slice::from_raw_parts(buf.add(y * stride), w as usize * 4);
                    sink[y * w as usize * 4..(y + 1) * w as usize * 4].copy_from_slice(row);
                }
                std::hint::black_box(&sink);
                bmp_destroy(bitmap);
                close_page(page);
                times.push(t.elapsed());
            }
            close_doc(doc);
            Ok((open, times))
        }
    }

    pub fn page_count(&self, pdf: &Path) -> Result<i32, String> {
        unsafe {
            let load_doc: Symbol<unsafe extern "C" fn(*const c_char, *const c_char) -> Handle> =
                self.lib
                    .get(b"FPDF_LoadDocument\0")
                    .map_err(|e| e.to_string())?;
            let close_doc: Symbol<unsafe extern "C" fn(Handle)> = self
                .lib
                .get(b"FPDF_CloseDocument\0")
                .map_err(|e| e.to_string())?;
            let count: Symbol<unsafe extern "C" fn(Handle) -> c_int> = self
                .lib
                .get(b"FPDF_GetPageCount\0")
                .map_err(|e| e.to_string())?;
            let cpath =
                CString::new(pdf.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
            let doc = load_doc(cpath.as_ptr(), std::ptr::null());
            if doc.is_null() {
                return Err("open failed".into());
            }
            let n = count(doc);
            close_doc(doc);
            Ok(n)
        }
    }
}

impl Drop for Pdfium {
    fn drop(&mut self) {
        unsafe {
            if let Ok(destroy) = self
                .lib
                .get::<unsafe extern "C" fn()>(b"FPDF_DestroyLibrary\0")
            {
                destroy();
            }
        }
    }
}
