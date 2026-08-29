//! Python bindings for `jbig2enc-rust`.
//!
//! The Rust API is already FFI-shaped — bytes in, bytes out — so this module
//! is mostly about meeting Python where it is:
//!
//! * **Any buffer works as input.** `bytes`, `bytearray`, `memoryview`, a
//!   NumPy array, a `PIL` image's `tobytes()` — anything implementing the
//!   buffer protocol. One copy is made while normalising pixel values,
//!   which the encoder needs regardless.
//! * **Polarity is explicit.** JBIG2 stores 1 = black, while every
//!   grayscale image format in Python stores 0 = black. Conflating the two
//!   produces a perfectly valid, perfectly inverted JBIG2 file, so
//!   [`pack_grayscale`] exists to do the thresholding and the inversion in
//!   one obvious place rather than leaving it to each caller.
//! * **The GIL is released** around every encode. These calls run for
//!   hundreds of milliseconds on a real page; holding the GIL through that
//!   would serialise any caller trying to encode pages in a thread pool.
//! * **Errors are Python exceptions**, not status codes.
//!
//! Symbol substitution is **on by default**, matching the encoder. It is
//! what makes JBIG2 worth choosing over CCITT G4 on scanned text, but it is
//! a *lossy* transform: near-identical glyphs are replaced by a shared
//! bitmap, so a round trip is not guaranteed bit-exact. Pass
//! `symbol_mode=False` for a generic region, which is bit-exact and is the
//! right choice for line art, halftones, or anything where a substituted
//! glyph would be unacceptable.
//!
//! `Jbig2Config` has more than fifty fields, nearly all of them
//! symbol-unification tuning knobs that only make sense with the T.88 spec
//! open. This module exposes only the ones that were *measured* to change
//! the encoder's output — `symbol_mode`, `refine`, and the `lossless`
//! preset. Everything else keeps the Rust defaults.
//!
//! Three plausible-looking fields are deliberately **not** exposed, because
//! they are inert in the encoder as it currently stands:
//!
//! * `dpi` — recorded in the config but never written to the stream.
//! * `duplicate_line_removal` — defaults on; toggling it changed no output
//!   in testing.
//! * `match_tolerance` — only read when `text_refine` is set
//!   (`encode/document.rs:451`), and even then produced identical bytes.
//!
//! Exposing a knob that silently does nothing is worse than omitting it, so
//! these stay out until the encoder honours them.

use jbig2enc_rust::decode::{DecodeOptions, decode_embedded, decode_file};
use jbig2enc_rust::shared::bitmap::MonoBitmap;
use jbig2enc_rust::{Jbig2Config, Jbig2Context, Jbig2Error};
use ndarray::Array2;
use pyo3::exceptions::{PyBufferError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};

/// Translate an encoder error into the closest Python exception.
///
/// `PackedDataDetected` gets the long-form message on purpose: passing
/// 1-bit-packed data where one byte per pixel is expected is *the* mistake
/// this API invites, and the fix is not guessable from "buffer size
/// mismatch".
fn to_py_error(error: Jbig2Error) -> PyErr {
    match error {
        Jbig2Error::PackedDataDetected => PyValueError::new_err(
            "input looks like 1-bit-packed data (8 pixels per byte), but this \
             function expects one byte per pixel. Unpack it first, or use \
             jbig2enc.pack_grayscale() to build the buffer from an 8-bit image.",
        ),
        other => PyValueError::new_err(other.to_string()),
    }
}

/// Access a Python object through the buffer protocol.
fn buffer_bytes(object: &Bound<'_, PyAny>) -> PyResult<pyo3::buffer::PyBuffer<u8>> {
    pyo3::buffer::PyBuffer::<u8>::get(object).map_err(|_| {
        PyBufferError::new_err(
            "expected a bytes-like object of 8-bit values (bytes, bytearray, \
             memoryview, or a contiguous uint8 array)",
        )
    })
}

/// Validate dimensions and return the expected buffer length.
fn expected_len(width: u32, height: u32) -> PyResult<usize> {
    if width == 0 || height == 0 {
        return Err(PyValueError::new_err(
            "width and height must both be greater than zero",
        ));
    }
    (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| PyValueError::new_err("width * height overflows the address space"))
}

/// Copy a Python buffer into the one-byte-per-pixel form the encoder wants,
/// normalising any non-zero value to 1 so that a caller who passes 0/255
/// (as NumPy and PIL do) gets the same result as one who passes 0/1.
fn normalized_pixels(object: &Bound<'_, PyAny>, width: u32, height: u32) -> PyResult<Vec<u8>> {
    let expected = expected_len(width, height)?;
    let buffer = buffer_bytes(object)?;
    let raw = buffer.to_vec(object.py())?;
    if raw.len() != expected {
        // Let the encoder's own richer diagnostic handle the packed-data case.
        if raw.len() == expected.div_ceil(8) {
            return Err(to_py_error(Jbig2Error::PackedDataDetected));
        }
        return Err(PyValueError::new_err(format!(
            "buffer has {} bytes but {width}x{height} needs {expected} \
             (one byte per pixel)",
            raw.len()
        )));
    }
    Ok(raw.into_iter().map(|value| u8::from(value != 0)).collect())
}

/// Build a [`Jbig2Config`] from the keyword arguments this module exposes.
fn config_from_kwargs(kwargs: Option<&Bound<'_, PyDict>>) -> PyResult<Jbig2Config> {
    let mut config = Jbig2Config::default();
    let Some(kwargs) = kwargs else {
        return Ok(config);
    };

    // `lossless` is a preset rather than a field: it turns off symbol
    // substitution and refinement together, which is the combination that
    // makes a stream safe for viewers that mishandle shared dictionaries.
    if let Some(value) = kwargs.get_item("lossless")?
        && value.extract::<bool>()?
    {
        config = Jbig2Config::lossless();
    }

    for (key, value) in kwargs.iter() {
        let key: String = key.extract()?;
        match key.as_str() {
            "lossless" => {}
            "symbol_mode" => {
                // `Jbig2Config::generic()` is the explicit off switch, not
                // merely `default()` with the flag cleared: it also clears the
                // refinement flags that only mean anything under symbol mode.
                if value.extract::<bool>()? {
                    config.symbol_mode = true;
                } else {
                    let refine = config.refine;
                    config = Jbig2Config::generic();
                    config.refine = refine;
                }
            }
            "refine" => config.refine = value.extract()?,
            other => {
                return Err(PyValueError::new_err(format!(
                    "unknown option '{other}'. Supported: symbol_mode, refine, lossless"
                )));
            }
        }
    }

    // Refinement without symbol substitution is not a meaningful combination;
    // the Rust `lossless()` preset clears both together, so mirror that rather
    // than emitting a stream whose refine flag can never fire.
    if !config.symbol_mode {
        config.refine = false;
    }
    Ok(config)
}

/// Encode one bilevel image as a standalone JBIG2 file.
///
/// `pixels` is one byte per pixel in row-major order; any non-zero value is
/// black. Returns the complete `.jb2` file bytes.
#[pyfunction]
#[pyo3(signature = (pixels, width, height, **options))]
fn encode(
    py: Python<'_>,
    pixels: &Bound<'_, PyAny>,
    width: u32,
    height: u32,
    options: Option<&Bound<'_, PyDict>>,
) -> PyResult<Py<PyBytes>> {
    let normalized = normalized_pixels(pixels, width, height)?;
    let config = config_from_kwargs(options)?;
    let result = py
        .detach(|| {
            jbig2enc_rust::encode_single_image_with_config(
                &normalized,
                width,
                height,
                Jbig2Context::with_config(config, false),
            )
        })
        .map_err(to_py_error)?;

    // Standalone mode returns everything in `page_data`; a global dictionary
    // is only split out for PDF embedding.
    Ok(PyBytes::new(py, &result.page_data).unbind())
}

/// Encode one bilevel image as a PDF-embeddable fragment.
///
/// Returns `(globals, page_data)`, where `globals` is the shared symbol
/// dictionary to store as a separate PDF object and reference from the
/// image stream's `/DecodeParms /JBIG2Globals`, or `None` when the encoder
/// produced no dictionary.
#[pyfunction]
#[pyo3(signature = (pixels, width, height, **options))]
fn encode_for_pdf(
    py: Python<'_>,
    pixels: &Bound<'_, PyAny>,
    width: u32,
    height: u32,
    options: Option<&Bound<'_, PyDict>>,
) -> PyResult<(Option<Py<PyBytes>>, Py<PyBytes>)> {
    let normalized = normalized_pixels(pixels, width, height)?;
    let config = config_from_kwargs(options)?;
    let result = py
        .detach(|| {
            jbig2enc_rust::encode_single_image_with_config(
                &normalized,
                width,
                height,
                Jbig2Context::with_config(config, true),
            )
        })
        .map_err(to_py_error)?;

    let globals = result
        .global_data
        .map(|data| PyBytes::new(py, &data).unbind());
    Ok((globals, PyBytes::new(py, &result.page_data).unbind()))
}

/// `(shared globals, one stream per page)`.
type DocumentStreams = (Option<Py<PyBytes>>, Vec<Py<PyBytes>>);

/// Encode several pages against one shared symbol dictionary.
///
/// `pages` is a sequence of `(pixels, width, height)` tuples. Returns
/// `(globals, [page_data, ...])`.
///
/// This is not the same as calling [`encode_for_pdf`] per page: PDF readers
/// expect a single `/JBIG2Globals` object referenced by every page stream,
/// and the symbol indices in those streams only line up if one encoder
/// instance planned all of them. Encoding pages independently and stitching
/// the results produces undecodable pages.
#[pyfunction]
#[pyo3(signature = (pages, **options))]
fn encode_document(
    py: Python<'_>,
    pages: Vec<(Py<PyAny>, u32, u32)>,
    options: Option<&Bound<'_, PyDict>>,
) -> PyResult<DocumentStreams> {
    if pages.is_empty() {
        return Err(PyValueError::new_err("pages must not be empty"));
    }
    let config = config_from_kwargs(options)?;

    let mut images = Vec::with_capacity(pages.len());
    for (index, (pixels, width, height)) in pages.iter().enumerate() {
        let normalized = normalized_pixels(pixels.bind(py), *width, *height)
            .map_err(|error| PyValueError::new_err(format!("page {index}: {error}")))?;
        let array = Array2::from_shape_vec((*height as usize, *width as usize), normalized)
            .map_err(|error| PyValueError::new_err(format!("page {index}: {error}")))?;
        images.push(array);
    }

    let output = py
        .detach(|| jbig2enc_rust::encode_document_pdf_split(&images, &config))
        .map_err(to_py_error)?;

    let globals = output
        .global_segments
        .map(|data| PyBytes::new(py, &data).unbind());
    let streams = output
        .page_streams
        .iter()
        .map(|stream| PyBytes::new(py, stream).unbind())
        .collect();
    Ok((globals, streams))
}

/// Threshold an 8-bit grayscale image into the buffer the encoders expect.
///
/// Grayscale images store 0 as black; JBIG2 stores 1 as black. This does the
/// comparison and the inversion together so callers do not have to
/// rediscover which way round it goes — a mistake that yields a valid but
/// fully inverted file.
///
/// A pixel is black when its value is **below** `threshold`.
#[pyfunction]
#[pyo3(signature = (pixels, width, height, threshold = 128))]
fn pack_grayscale(
    py: Python<'_>,
    pixels: &Bound<'_, PyAny>,
    width: u32,
    height: u32,
    threshold: u8,
) -> PyResult<Py<PyBytes>> {
    let expected = expected_len(width, height)?;
    let buffer = buffer_bytes(pixels)?;
    let raw = buffer.to_vec(py)?;
    if raw.len() != expected {
        return Err(PyValueError::new_err(format!(
            "buffer has {} bytes but {width}x{height} needs {expected}",
            raw.len()
        )));
    }
    let packed: Vec<u8> = raw
        .into_iter()
        .map(|value| u8::from(value < threshold))
        .collect();
    Ok(PyBytes::new(py, &packed).unbind())
}

/// Unpack a decoded bitmap into one byte per pixel, 1 = black, matching what
/// the encode functions take as input so a round trip is byte-comparable.
fn unpack_bitmap(py: Python<'_>, bitmap: &MonoBitmap) -> (Py<PyBytes>, u32, u32) {
    let (width, height) = (bitmap.width(), bitmap.height());
    let mut pixels = Vec::with_capacity((width as usize) * (height as usize));
    for y in 0..height {
        for x in 0..width {
            pixels.push(u8::from(bitmap.get(x, y)));
        }
    }
    (PyBytes::new(py, &pixels).unbind(), width, height)
}

/// Decode a standalone JBIG2 file.
///
/// Returns `(pixels, width, height)` with one byte per pixel, 1 = black —
/// the same layout [`encode`] accepts, so a round trip compares equal.
///
/// Only the first page is returned; use [`decode_document`] for a
/// multi-page file.
#[pyfunction]
fn decode(py: Python<'_>, data: &[u8]) -> PyResult<(Py<PyBytes>, u32, u32)> {
    let document = py
        .detach(|| decode_file(data, &DecodeOptions::default()))
        .map_err(|error| PyValueError::new_err(format!("decoding JBIG2: {error}")))?;
    let bitmap = document
        .first_page()
        .ok_or_else(|| PyValueError::new_err("JBIG2 stream contains no pages"))?;
    Ok(unpack_bitmap(py, bitmap))
}

/// Decode every page of a standalone JBIG2 file.
///
/// Returns a list of `(pixels, width, height)` tuples in stream order.
#[pyfunction]
fn decode_document(py: Python<'_>, data: &[u8]) -> PyResult<Vec<(Py<PyBytes>, u32, u32)>> {
    let document = py
        .detach(|| decode_file(data, &DecodeOptions::default()))
        .map_err(|error| PyValueError::new_err(format!("decoding JBIG2: {error}")))?;
    Ok(document
        .pages
        .iter()
        .map(|page| unpack_bitmap(py, &page.bitmap))
        .collect())
}

/// Decode a PDF-embedded JBIG2 page stream.
///
/// `globals` is the `/JBIG2Globals` stream when the page references one, as
/// returned by [`encode_for_pdf`] or [`encode_document`]. Pass `None` for a
/// page encoded without a shared dictionary.
#[pyfunction]
#[pyo3(signature = (page_data, globals = None))]
fn decode_pdf_stream(
    py: Python<'_>,
    page_data: &[u8],
    globals: Option<&[u8]>,
) -> PyResult<(Py<PyBytes>, u32, u32)> {
    let bitmap = py
        .detach(|| decode_embedded(globals, page_data, &DecodeOptions::default()))
        .map_err(|error| PyValueError::new_err(format!("decoding JBIG2 page: {error}")))?;
    Ok(unpack_bitmap(py, &bitmap))
}

/// Encoder version string.
#[pyfunction]
fn version() -> String {
    jbig2enc_rust::get_version()
}

/// Build configuration of the linked encoder.
#[pyfunction]
fn build_info() -> String {
    jbig2enc_rust::get_build_info()
}

#[pymodule]
fn jbig2enc(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add("__version__", env!("CARGO_PKG_VERSION"))?;
    module.add_function(wrap_pyfunction!(encode, module)?)?;
    module.add_function(wrap_pyfunction!(encode_for_pdf, module)?)?;
    module.add_function(wrap_pyfunction!(encode_document, module)?)?;
    module.add_function(wrap_pyfunction!(decode, module)?)?;
    module.add_function(wrap_pyfunction!(decode_document, module)?)?;
    module.add_function(wrap_pyfunction!(decode_pdf_stream, module)?)?;
    module.add_function(wrap_pyfunction!(pack_grayscale, module)?)?;
    module.add_function(wrap_pyfunction!(version, module)?)?;
    module.add_function(wrap_pyfunction!(build_info, module)?)?;
    Ok(())
}
