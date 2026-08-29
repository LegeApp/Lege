//! C ABI over the Lege PDF renderer.
//!
//! This is the binding every non-Rust consumer goes through. C and C++ use it
//! directly; C#, Go, Swift and Java 22+ (`jextract`) all generate their own
//! bindings from the header with no further Rust work. Only Python, Node and
//! Wasm justify a separate Rust crate, because their idioms need more than a
//! C ABI can express.
//!
//! It binds [`lege_pdf_read`] and nothing deeper. That crate is already the
//! narrow, owned facade over the twenty-four renderer crates -- renderer
//! types stay private to it -- so this layer never has to decide how much of
//! the renderer to expose.
//!
//! # The rules this file follows
//!
//! * **Nothing crosses the boundary but plain data and opaque handles.** No
//!   Rust type is ever `#[repr(C)]`-exposed if it might grow a field;
//!   accessors are cheaper to keep ABI-stable than structs.
//! * **Every entry point catches unwinds.** A panic crossing an `extern "C"`
//!   frame is undefined behaviour. These guards only work because the release
//!   profile is `panic = "unwind"`; if that is ever set back to `abort`, every
//!   binding turns a malformed PDF into a killed host process. There is a test
//!   below that asserts a panic is caught.
//! * **Errors are a status code plus a thread-local message.** Never a Rust
//!   `String` handed over the wall.
//! * **Every buffer is freed by the allocator that made it.** Calling C
//!   `free()` on a Rust allocation is undefined behaviour, so callers get
//!   [`lege_buffer_free`] and nothing else.
//!
//! # Threading
//!
//! A [`LegeDocument`] is `Send + Sync`; several threads may render from one
//! document concurrently. Handles are not reference counted, so the *caller*
//! must not close one while another thread is still using it.
//! [`LegeCancellation`] is likewise shareable, and is the point: a slow render
//! can be cancelled from another thread.

// The workspace warns on `unsafe_code`, which is right everywhere else and
// meaningless here: a C ABI is `no_mangle` + `unsafe extern "C"` by
// definition, and warning on all forty-odd exports would bury real findings.
// Every `unsafe` block below still carries its own safety comment, which is
// the invariant that actually matters.
#![allow(unsafe_code)]

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_int, c_uint};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use lege_pdf_read::{ExportColor, ExportOptions, ImageFormat, ReadError, RenderSession, page_text};

// ── Status codes ────────────────────────────────────────────────────────────

/// Call succeeded.
pub const LEGE_OK: c_int = 0;
/// A required pointer argument was null.
pub const LEGE_ERR_NULL_ARGUMENT: c_int = -1;
/// An argument was outside its valid range (a bad page index, a bad DPI).
pub const LEGE_ERR_INVALID_ARGUMENT: c_int = -2;
/// The document could not be opened or decrypted.
pub const LEGE_ERR_OPEN: c_int = -3;
/// The page could not be parsed or rendered.
pub const LEGE_ERR_RENDER: c_int = -4;
/// The image could not be encoded.
pub const LEGE_ERR_ENCODE: c_int = -5;
/// The requested raster exceeds the pixel budget.
pub const LEGE_ERR_TOO_LARGE: c_int = -6;
/// The renderer panicked. The library remains usable; the document may not be.
pub const LEGE_ERR_PANIC: c_int = -7;

/// Output container for [`lege_document_render_page`].
pub const LEGE_FORMAT_PNG: c_int = 0;
/// Baseline JPEG.
pub const LEGE_FORMAT_JPEG: c_int = 1;

/// 8-bit RGB output.
pub const LEGE_COLOR_RGB: c_int = 0;
/// 8-bit grayscale output.
pub const LEGE_COLOR_GRAY: c_int = 1;

// ── Error reporting ─────────────────────────────────────────────────────────

thread_local! {
    /// Last error on *this* thread. Thread-local rather than global so two
    /// threads rendering concurrently cannot overwrite each other's message.
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

fn set_last_error(message: impl Into<Vec<u8>>) {
    // A message containing an interior NUL cannot be a C string; replace it
    // rather than lose the error entirely.
    let value =
        CString::new(message).unwrap_or_else(|_| c"error message contained a NUL byte".to_owned());
    LAST_ERROR.with(|slot| *slot.borrow_mut() = Some(value));
}

fn clear_last_error() {
    LAST_ERROR.with(|slot| *slot.borrow_mut() = None);
}

/// Message for the most recent failure **on the calling thread**, or `NULL`
/// if the last call succeeded.
///
/// The pointer is owned by the library and stays valid until the next failing
/// call on this thread. Copy it if you need to keep it; do not free it.
///
/// # Safety
/// The returned pointer must not be used after another library call on this
/// thread.
#[unsafe(no_mangle)]
pub extern "C" fn lege_last_error_message() -> *const c_char {
    LAST_ERROR.with(|slot| {
        slot.borrow()
            .as_ref()
            .map_or(std::ptr::null(), |message| message.as_ptr())
    })
}

fn status_for(error: &ReadError) -> c_int {
    match error {
        ReadError::Open(_) => LEGE_ERR_OPEN,
        ReadError::PageOutOfRange { .. } | ReadError::InvalidExportOptions(_) => {
            LEGE_ERR_INVALID_ARGUMENT
        }
        ReadError::InvalidRasterProduct(_) => LEGE_ERR_INVALID_ARGUMENT,
        ReadError::ExportTooLarge { .. } => LEGE_ERR_TOO_LARGE,
        ReadError::Encode(_) => LEGE_ERR_ENCODE,
        ReadError::Compile { .. } | ReadError::Render { .. } => LEGE_ERR_RENDER,
    }
}

fn report(error: ReadError) -> c_int {
    let status = status_for(&error);
    set_last_error(error.to_string());
    status
}

/// Run `body` with an unwind guard, mapping a panic to [`LEGE_ERR_PANIC`].
///
/// Every `extern "C"` function in this file goes through here. A panic that
/// escapes into C is undefined behaviour, and the renderer parses untrusted
/// input, so this is load-bearing rather than defensive.
fn guarded(body: impl FnOnce() -> c_int) -> c_int {
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(status) => status,
        Err(payload) => {
            // Recover the panic message where the payload carries one. The
            // renderer's own guards already stringify their panics before
            // they reach us, so this is mostly for panics raised in this
            // crate's own frames.
            let detail = payload
                .downcast_ref::<&'static str>()
                .map(|text| (*text).to_owned())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown payload".to_owned());
            set_last_error(format!("renderer panicked: {detail}"));
            LEGE_ERR_PANIC
        }
    }
}

// ── Buffers ─────────────────────────────────────────────────────────────────

/// An owned byte buffer produced by this library.
///
/// Free it with [`lege_buffer_free`] and nothing else: it was allocated by
/// Rust, and passing it to C `free()` is undefined behaviour. `capacity` is
/// carried because Rust's deallocator needs the original allocation size.
#[repr(C)]
#[derive(Debug)]
pub struct LegeBuffer {
    pub data: *mut u8,
    pub len: usize,
    pub capacity: usize,
}

impl LegeBuffer {
    fn empty() -> Self {
        Self {
            data: std::ptr::null_mut(),
            len: 0,
            capacity: 0,
        }
    }

    fn from_vec(mut bytes: Vec<u8>) -> Self {
        let buffer = Self {
            data: bytes.as_mut_ptr(),
            len: bytes.len(),
            capacity: bytes.capacity(),
        };
        std::mem::forget(bytes);
        buffer
    }
}

/// Release a buffer produced by this library. Safe to call on a zeroed or
/// already-emptied buffer; the fields are cleared so a double free is a no-op.
///
/// # Safety
/// `buffer` must be null, or point to a [`LegeBuffer`] this library filled in
/// and that has not already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lege_buffer_free(buffer: *mut LegeBuffer) {
    if buffer.is_null() {
        return;
    }
    // SAFETY: the caller guarantees `buffer` points to a LegeBuffer we
    // produced; the fields are cleared afterwards so a second call is inert.
    unsafe {
        let slot = &mut *buffer;
        if !slot.data.is_null() {
            drop(Vec::from_raw_parts(slot.data, slot.len, slot.capacity));
        }
        *slot = LegeBuffer::empty();
    }
}

// ── Documents ───────────────────────────────────────────────────────────────

/// An opened PDF document. Opaque: its layout is not part of the ABI.
#[derive(Debug)]
pub struct LegeDocument {
    session: RenderSession,
}

/// Open a PDF from memory.
///
/// The bytes are copied, so the caller may free `data` immediately. Pass
/// `password` as `NULL` for an unencrypted document, or an empty string for
/// one encrypted with the empty user password.
///
/// Returns `NULL` on failure; call [`lege_last_error_message`] for why.
///
/// # Safety
/// `data` must point to at least `len` readable bytes, and `password`, when
/// non-null, must be a NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lege_document_open(
    data: *const u8,
    len: usize,
    password: *const c_char,
) -> *mut LegeDocument {
    let mut document = std::ptr::null_mut();
    guarded(|| {
        clear_last_error();
        if data.is_null() {
            set_last_error("data pointer is null");
            return LEGE_ERR_NULL_ARGUMENT;
        }
        // SAFETY: the caller guarantees `len` readable bytes at `data`.
        let bytes: Arc<[u8]> = Arc::from(unsafe { std::slice::from_raw_parts(data, len) });

        let password = if password.is_null() {
            None
        } else {
            // SAFETY: the caller guarantees a NUL-terminated string.
            match unsafe { CStr::from_ptr(password) }.to_str() {
                Ok(text) => Some(text),
                Err(_) => {
                    set_last_error("password is not valid UTF-8");
                    return LEGE_ERR_INVALID_ARGUMENT;
                }
            }
        };

        match RenderSession::open(bytes, password) {
            Ok(session) => {
                document = Box::into_raw(Box::new(LegeDocument { session }));
                LEGE_OK
            }
            Err(error) => report(error),
        }
    });
    document
}

/// Close a document opened by [`lege_document_open`]. Null is ignored.
///
/// # Safety
/// `document` must be null or a handle from [`lege_document_open`] that has
/// not already been closed, and no other thread may be using it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lege_document_close(document: *mut LegeDocument) {
    if document.is_null() {
        return;
    }
    // SAFETY: the caller guarantees an unclosed handle from this library.
    let owned = unsafe { Box::from_raw(document) };
    // Dropping a document runs renderer teardown, which is caller code we do
    // not want unwinding into C.
    let _ = catch_unwind(AssertUnwindSafe(move || drop(owned)));
}

/// Number of pages, or 0 if `document` is null.
///
/// # Safety
/// `document` must be null or a valid open handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lege_document_page_count(document: *const LegeDocument) -> c_uint {
    // SAFETY: the caller guarantees a valid handle or null.
    let Some(document) = (unsafe { document.as_ref() }) else {
        return 0;
    };
    guarded_value(|| document.session.page_count(), 0)
}

fn guarded_value<T>(body: impl FnOnce() -> T, fallback: T) -> T {
    catch_unwind(AssertUnwindSafe(body)).unwrap_or(fallback)
}

/// Visible size of a page in PDF points (1/72 inch), with `/Rotate` applied so
/// the values match what a viewer displays.
///
/// # Safety
/// `document` must be a valid open handle; `width` and `height` must point to
/// writable `double`s.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lege_document_page_size(
    document: *const LegeDocument,
    page: c_uint,
    width: *mut f64,
    height: *mut f64,
) -> c_int {
    guarded(|| {
        clear_last_error();
        // SAFETY: the caller guarantees a valid handle or null.
        let Some(document) = (unsafe { document.as_ref() }) else {
            set_last_error("document handle is null");
            return LEGE_ERR_NULL_ARGUMENT;
        };
        if width.is_null() || height.is_null() {
            set_last_error("width and height out-parameters must not be null");
            return LEGE_ERR_NULL_ARGUMENT;
        }
        match document.session.page_geometry(page) {
            Ok(geometry) => {
                // SAFETY: both pointers were null-checked above.
                unsafe {
                    *width = geometry.display_width();
                    *height = geometry.display_height();
                }
                LEGE_OK
            }
            Err(error) => report(error),
        }
    })
}

/// Pixel size a page would render to at `dpi`, without rendering it.
///
/// # Safety
/// As [`lege_document_page_size`], with `uint32_t` out-parameters.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lege_document_page_pixel_size(
    document: *const LegeDocument,
    page: c_uint,
    dpi: f64,
    width: *mut c_uint,
    height: *mut c_uint,
) -> c_int {
    guarded(|| {
        clear_last_error();
        // SAFETY: the caller guarantees a valid handle or null.
        let Some(document) = (unsafe { document.as_ref() }) else {
            set_last_error("document handle is null");
            return LEGE_ERR_NULL_ARGUMENT;
        };
        if width.is_null() || height.is_null() {
            set_last_error("width and height out-parameters must not be null");
            return LEGE_ERR_NULL_ARGUMENT;
        }
        match document.session.page_pixel_size(page, dpi) {
            Ok((page_width, page_height)) => {
                // SAFETY: both pointers were null-checked above.
                unsafe {
                    *width = page_width;
                    *height = page_height;
                }
                LEGE_OK
            }
            Err(error) => report(error),
        }
    })
}

// ── Cancellation ────────────────────────────────────────────────────────────

/// A cancellation token. Opaque, shareable across threads, and the reason a
/// slow render does not have to be waited out.
#[derive(Debug)]
pub struct LegeCancellation {
    token: lege_pdf_read::CancellationToken,
}

/// Create a cancellation token. Returns `NULL` only on allocation failure.
#[unsafe(no_mangle)]
pub extern "C" fn lege_cancellation_new() -> *mut LegeCancellation {
    guarded_value(
        || {
            Box::into_raw(Box::new(LegeCancellation {
                token: lege_pdf_read::CancellationToken::new(),
            }))
        },
        std::ptr::null_mut(),
    )
}

/// Signal cancellation. Safe to call from any thread, including while a render
/// using this token is in flight -- that is the point of it.
///
/// # Safety
/// `token` must be null or a live handle from [`lege_cancellation_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lege_cancellation_cancel(token: *const LegeCancellation) {
    // SAFETY: the caller guarantees a live handle or null.
    if let Some(token) = unsafe { token.as_ref() } {
        token.token.cancel();
    }
}

/// Whether this token has been cancelled.
///
/// # Safety
/// `token` must be null or a live handle from [`lege_cancellation_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lege_cancellation_is_cancelled(token: *const LegeCancellation) -> c_int {
    // SAFETY: the caller guarantees a live handle or null.
    match unsafe { token.as_ref() } {
        Some(token) => c_int::from(token.token.is_cancelled()),
        None => 0,
    }
}

/// Free a cancellation token. Null is ignored.
///
/// # Safety
/// `token` must be null or a handle that has not already been freed, and no
/// render using it may still be running.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lege_cancellation_free(token: *mut LegeCancellation) {
    if token.is_null() {
        return;
    }
    // SAFETY: the caller guarantees an unfreed handle from this library.
    drop(unsafe { Box::from_raw(token) });
}

// ── Rendering ───────────────────────────────────────────────────────────────

/// How to render one page.
///
/// `#[repr(C)]` and deliberately frozen: adding a field here is an ABI break
/// for every already-compiled caller. New knobs get a new function.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LegeRenderOptions {
    /// Target resolution in dots per inch.
    pub dpi: f64,
    /// One of `LEGE_FORMAT_*`.
    pub format: c_int,
    /// One of `LEGE_COLOR_*`.
    pub color: c_int,
    /// JPEG quality, 1-100. Ignored for PNG.
    pub jpeg_quality: c_int,
    /// Reject renders whose pixel count exceeds this. 0 means the library
    /// default (about 100 megapixels).
    pub max_pixels: u64,
}

/// Options equivalent to a 150 DPI RGB PNG at the default pixel budget.
///
/// Provided so C callers get sane values without having to know them, and so
/// a zeroed struct is never mistaken for a valid request.
#[unsafe(no_mangle)]
pub extern "C" fn lege_render_options_default() -> LegeRenderOptions {
    LegeRenderOptions {
        dpi: lege_pdf_read::DEFAULT_EXPORT_DPI,
        format: LEGE_FORMAT_PNG,
        color: LEGE_COLOR_RGB,
        jpeg_quality: 85,
        max_pixels: 0,
    }
}

fn export_options_from(options: &LegeRenderOptions) -> Result<ExportOptions, c_int> {
    let format = match options.format {
        LEGE_FORMAT_PNG => ImageFormat::Png,
        LEGE_FORMAT_JPEG => {
            let quality = options.jpeg_quality;
            if !(1..=100).contains(&quality) {
                set_last_error(format!("jpeg_quality must be 1-100, got {quality}"));
                return Err(LEGE_ERR_INVALID_ARGUMENT);
            }
            ImageFormat::Jpeg {
                quality: quality as u8,
            }
        }
        other => {
            set_last_error(format!(
                "unknown format {other}; expected 0 (PNG) or 1 (JPEG)"
            ));
            return Err(LEGE_ERR_INVALID_ARGUMENT);
        }
    };
    let color = match options.color {
        LEGE_COLOR_RGB => ExportColor::Rgb8,
        LEGE_COLOR_GRAY => ExportColor::Gray8,
        other => {
            set_last_error(format!(
                "unknown color {other}; expected 0 (RGB) or 1 (gray)"
            ));
            return Err(LEGE_ERR_INVALID_ARGUMENT);
        }
    };
    let max_pixels = if options.max_pixels == 0 {
        lege_pdf_read::DEFAULT_MAX_EXPORT_PIXELS
    } else {
        options.max_pixels
    };
    Ok(ExportOptions {
        dpi: options.dpi,
        format,
        color,
        max_pixels,
    })
}

/// Render one page and encode it, writing the file bytes into `out`.
///
/// `cancellation` may be `NULL`. When non-null and cancelled, the call returns
/// [`LEGE_ERR_RENDER`] promptly rather than finishing the render.
///
/// On success `out` owns a buffer the caller must release with
/// [`lege_buffer_free`]. On failure `out` is left empty.
///
/// # Safety
/// `document` must be a valid open handle, `options` and `out` must be
/// non-null and writable, and `cancellation` must be null or a live token.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lege_document_render_page(
    document: *const LegeDocument,
    page: c_uint,
    options: *const LegeRenderOptions,
    cancellation: *const LegeCancellation,
    out: *mut LegeBuffer,
) -> c_int {
    guarded(|| {
        clear_last_error();
        if out.is_null() {
            set_last_error("output buffer pointer is null");
            return LEGE_ERR_NULL_ARGUMENT;
        }
        // SAFETY: null-checked immediately above. Emptied first so a failing
        // call never leaves the caller a stale pointer to free.
        unsafe { *out = LegeBuffer::empty() };

        // SAFETY: the caller guarantees a valid handle or null.
        let Some(document) = (unsafe { document.as_ref() }) else {
            set_last_error("document handle is null");
            return LEGE_ERR_NULL_ARGUMENT;
        };
        // SAFETY: the caller guarantees a readable options struct or null.
        let Some(options) = (unsafe { options.as_ref() }) else {
            set_last_error("options pointer is null");
            return LEGE_ERR_NULL_ARGUMENT;
        };
        let options = match export_options_from(options) {
            Ok(options) => options,
            Err(status) => return status,
        };
        // SAFETY: the caller guarantees a live token or null.
        let token = unsafe { cancellation.as_ref() }.map(|handle| &handle.token);

        let compiled = match document.session.compile(page) {
            Ok(compiled) => compiled,
            Err(error) => return report(error),
        };
        match document
            .session
            .export_compiled_page(&compiled, &options, token)
        {
            Ok(bytes) => {
                // SAFETY: `out` was null-checked at the top of this call.
                unsafe { *out = LegeBuffer::from_vec(bytes) };
                LEGE_OK
            }
            Err(error) => report(error),
        }
    })
}

/// Extract a page's text layer as UTF-8 into `out`, without a trailing NUL.
///
/// A page with no text layer yields an empty buffer and [`LEGE_OK`]; that is
/// not an error, it is a scanned page.
///
/// # Safety
/// `document` must be a valid open handle and `out` non-null and writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lege_document_page_text(
    document: *const LegeDocument,
    page: c_uint,
    out: *mut LegeBuffer,
) -> c_int {
    guarded(|| {
        clear_last_error();
        if out.is_null() {
            set_last_error("output buffer pointer is null");
            return LEGE_ERR_NULL_ARGUMENT;
        }
        // SAFETY: null-checked immediately above.
        unsafe { *out = LegeBuffer::empty() };

        // SAFETY: the caller guarantees a valid handle or null.
        let Some(document) = (unsafe { document.as_ref() }) else {
            set_last_error("document handle is null");
            return LEGE_ERR_NULL_ARGUMENT;
        };
        match page_text(&document.session, page) {
            Ok(text) => {
                // SAFETY: `out` was null-checked at the top of this call.
                unsafe { *out = LegeBuffer::from_vec(text.into_bytes()) };
                LEGE_OK
            }
            Err(error) => report(error),
        }
    })
}

/// This library's version, as a static NUL-terminated string. Never null, and
/// must not be freed.
#[unsafe(no_mangle)]
pub extern "C" fn lege_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

#[cfg(test)]
mod tests {
    // Every call here is deliberately exercising the C ABI, so the unsafe
    // blocks are the subject of the tests rather than incidental to them; a
    // safety comment on each of the twenty-five would be noise.
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::undocumented_unsafe_blocks
    )]

    use super::*;

    /// The smallest PDF the renderer will open: one blank 200x100 page.
    fn minimal_pdf() -> Vec<u8> {
        let body = b"%PDF-1.4\n\
1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n\
2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n\
3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 100]/Contents 4 0 R>>endobj\n\
4 0 obj<</Length 8>>stream\n1 0 0 RG\nendstream endobj\n";
        let mut pdf = body.to_vec();
        pdf.extend_from_slice(b"trailer<</Root 1 0 R/Size 5>>\n%%EOF\n");
        pdf
    }

    fn open(pdf: &[u8]) -> *mut LegeDocument {
        unsafe { lege_document_open(pdf.as_ptr(), pdf.len(), std::ptr::null()) }
    }

    #[test]
    fn open_render_and_free_round_trips_through_the_c_abi() {
        let pdf = minimal_pdf();
        let document = open(&pdf);
        assert!(!document.is_null(), "fixture should open");
        assert_eq!(unsafe { lege_document_page_count(document) }, 1);

        let (mut width, mut height) = (0.0, 0.0);
        assert_eq!(
            unsafe { lege_document_page_size(document, 0, &mut width, &mut height) },
            LEGE_OK
        );
        assert_eq!((width, height), (200.0, 100.0));

        let (mut pixel_width, mut pixel_height) = (0, 0);
        assert_eq!(
            unsafe {
                lege_document_page_pixel_size(
                    document,
                    0,
                    144.0,
                    &mut pixel_width,
                    &mut pixel_height,
                )
            },
            LEGE_OK
        );
        assert_eq!((pixel_width, pixel_height), (400, 200));

        let options = lege_render_options_default();
        let mut buffer = LegeBuffer::empty();
        assert_eq!(
            unsafe {
                lege_document_render_page(document, 0, &options, std::ptr::null(), &mut buffer)
            },
            LEGE_OK
        );
        assert!(buffer.len > 8);
        let bytes = unsafe { std::slice::from_raw_parts(buffer.data, buffer.len) };
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");

        unsafe { lege_buffer_free(&mut buffer) };
        assert!(buffer.data.is_null(), "freeing must clear the buffer");
        // Freeing twice must be inert, not a double free.
        unsafe { lege_buffer_free(&mut buffer) };

        unsafe { lege_document_close(document) };
    }

    #[test]
    fn null_and_invalid_arguments_are_reported_not_dereferenced() {
        assert!(open(b"not a pdf at all").is_null());
        assert!(
            !lege_last_error_message().is_null(),
            "failure sets a message"
        );

        let mut buffer = LegeBuffer::empty();
        let options = lege_render_options_default();
        assert_eq!(
            unsafe {
                lege_document_render_page(
                    std::ptr::null(),
                    0,
                    &options,
                    std::ptr::null(),
                    &mut buffer,
                )
            },
            LEGE_ERR_NULL_ARGUMENT
        );
        assert_eq!(unsafe { lege_document_page_count(std::ptr::null()) }, 0);
        unsafe { lege_document_close(std::ptr::null_mut()) };
        unsafe { lege_buffer_free(std::ptr::null_mut()) };

        let pdf = minimal_pdf();
        let document = open(&pdf);
        assert!(!document.is_null());

        // Out-of-range page.
        let mut width = 0.0;
        let mut height = 0.0;
        assert_eq!(
            unsafe { lege_document_page_size(document, 99, &mut width, &mut height) },
            LEGE_ERR_INVALID_ARGUMENT
        );

        // Bad enum values and a bad quality must be rejected, not coerced.
        for (format, color, quality, expected) in [
            (7, LEGE_COLOR_RGB, 85, LEGE_ERR_INVALID_ARGUMENT),
            (LEGE_FORMAT_PNG, 9, 85, LEGE_ERR_INVALID_ARGUMENT),
            (
                LEGE_FORMAT_JPEG,
                LEGE_COLOR_RGB,
                0,
                LEGE_ERR_INVALID_ARGUMENT,
            ),
            (
                LEGE_FORMAT_JPEG,
                LEGE_COLOR_RGB,
                101,
                LEGE_ERR_INVALID_ARGUMENT,
            ),
        ] {
            let options = LegeRenderOptions {
                format,
                color,
                jpeg_quality: quality,
                ..lege_render_options_default()
            };
            assert_eq!(
                unsafe {
                    lege_document_render_page(document, 0, &options, std::ptr::null(), &mut buffer)
                },
                expected,
                "format={format} color={color} quality={quality}"
            );
            assert!(
                buffer.data.is_null(),
                "a failed render must leave out empty"
            );
        }

        // A DPI that would blow the pixel budget reports it as such.
        let options = LegeRenderOptions {
            dpi: 4000.0,
            max_pixels: 1000,
            ..lege_render_options_default()
        };
        assert_eq!(
            unsafe {
                lege_document_render_page(document, 0, &options, std::ptr::null(), &mut buffer)
            },
            LEGE_ERR_TOO_LARGE
        );

        unsafe { lege_document_close(document) };
    }

    #[test]
    fn a_cancelled_token_stops_a_render() {
        let pdf = minimal_pdf();
        let document = open(&pdf);
        assert!(!document.is_null());

        let token = lege_cancellation_new();
        assert!(!token.is_null());
        assert_eq!(unsafe { lege_cancellation_is_cancelled(token) }, 0);
        unsafe { lege_cancellation_cancel(token) };
        assert_eq!(unsafe { lege_cancellation_is_cancelled(token) }, 1);

        let options = LegeRenderOptions {
            dpi: 600.0,
            ..lege_render_options_default()
        };
        let mut buffer = LegeBuffer::empty();
        assert_eq!(
            unsafe { lege_document_render_page(document, 0, &options, token, &mut buffer) },
            LEGE_ERR_RENDER
        );
        assert!(buffer.data.is_null());

        unsafe { lege_cancellation_free(token) };
        unsafe { lege_document_close(document) };
    }

    #[test]
    fn a_panic_is_caught_at_the_boundary_rather_than_crossing_it() {
        // The guard is only real if unwinding actually reaches it. If the
        // release profile is ever set back to panic="abort" this test still
        // passes under `cargo test` (which uses the test profile), so the
        // profile itself is asserted separately in the workspace manifest.
        assert_eq!(
            guarded(|| panic!("simulated renderer panic")),
            LEGE_ERR_PANIC
        );
        let message = lege_last_error_message();
        assert!(!message.is_null());
        let message = unsafe { CStr::from_ptr(message) }.to_string_lossy();
        assert!(message.contains("panicked"), "{message}");
    }

    #[test]
    fn error_messages_are_thread_local() {
        // Two threads failing concurrently must not see each other's message.
        let failing = std::thread::spawn(|| {
            assert!(open(b"garbage").is_null());
            unsafe { CStr::from_ptr(lege_last_error_message()) }
                .to_string_lossy()
                .into_owned()
        });
        let message = failing.join().unwrap();
        assert!(!message.is_empty());
        // This thread never failed, so it has no message.
        assert!(lege_last_error_message().is_null());
    }
}
