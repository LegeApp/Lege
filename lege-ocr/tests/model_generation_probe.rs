//! Opt-in probe: can the lege-gpu wgpu runtime actually run a given PP-OCR
//! model generation?
//!
//! Every model generation has to clear the same three bars before it can be
//! embedded: the graphs must compile on the wgpu runtime, the recognition head's
//! class count must agree with the dictionary, and a real page must come back
//! with text on it. Answering that by swapping the embedded assets and running
//! the whole CLI is slow and destructive, so this test takes the model files
//! directly.
//!
//! Point `LEGE_OCR_PROBE_MODELS` at a directory holding `ppocr-det.onnx`,
//! `ppocr-rec.onnx` and `ppocr-dict.txt`, and optionally point
//! `LEGE_OCR_PROBE_PAGE` at a page image to read:
//!
//! ```sh
//! LEGE_OCR_PROBE_MODELS=.agent/scratch/ocr-v6-probe/tiny \
//! LEGE_OCR_PROBE_PAGE=lege-process/page_0002-original.png \
//!   cargo test -p lege-ocr --features paddle-ocr --test model_generation_probe -- --nocapture
//! ```
//!
//! Without `LEGE_OCR_PROBE_MODELS` the test skips: it needs a real GPU adapter,
//! so it must not run as part of the default suite.
#![cfg(feature = "paddle-ocr")]

use std::path::PathBuf;

use lege_ocr::engine::OcrEngine;
use lege_ocr::engine_paddle::PaddleOcrEngine;

/// The three files a model generation is made of, using the same names as the
/// embedded assets in `lege-ocr/assets/`.
struct ProbeModels {
    detector: Vec<u8>,
    recognizer: Vec<u8>,
    dictionary: String,
}

fn load_probe_models() -> Option<ProbeModels> {
    let Some(directory) = std::env::var_os("LEGE_OCR_PROBE_MODELS") else {
        eprintln!(
            "skipping model-generation probe; set LEGE_OCR_PROBE_MODELS to a model directory"
        );
        return None;
    };
    let directory = PathBuf::from(directory);
    let read = |name: &str| {
        std::fs::read(directory.join(name))
            .unwrap_or_else(|error| panic!("read {}/{name}: {error}", directory.display()))
    };
    let dictionary = String::from_utf8(read("ppocr-dict.txt")).expect("dictionary is not UTF-8");
    Some(ProbeModels {
        detector: read("ppocr-det.onnx"),
        recognizer: read("ppocr-rec.onnx"),
        dictionary,
    })
}

/// Both graphs compile, and the recognition head agrees with the dictionary.
///
/// PP-OCR's class layout is `blank + dictionary lines + space`, so a head with
/// `N + 2` classes pairs with an `N`-line dictionary. A mismatch here means the
/// dictionary and the recognizer came from different model generations, which
/// would otherwise surface as silently shifted characters rather than an error.
#[test]
fn probe_models_compile_and_match_their_dictionary() {
    let Some(models) = load_probe_models() else {
        return;
    };
    let dictionary_lines = models.dictionary.lines().count();
    let engine = PaddleOcrEngine::new(&models.detector, &models.recognizer, &models.dictionary)
        .expect("probe models must build on the wgpu runtime");
    eprintln!(
        "probe: {dictionary_lines} dictionary lines, {} recognizer classes",
        engine.recognizer_classes()
    );
    assert_eq!(
        engine.recognizer_classes(),
        dictionary_lines + 2,
        "recognizer class count does not match the dictionary"
    );
}

/// A real page goes through detection and recognition and comes back with text.
///
/// Graph compilation alone does not prove a generation works — an unsupported
/// op can lower to a shader that runs and returns nothing useful. Reading an
/// actual page is the check that catches that.
#[test]
fn probe_models_read_a_page() {
    let Some(models) = load_probe_models() else {
        return;
    };
    let Some(page) = std::env::var_os("LEGE_OCR_PROBE_PAGE") else {
        eprintln!("skipping page read; set LEGE_OCR_PROBE_PAGE to a page image");
        return;
    };
    let engine = PaddleOcrEngine::new(&models.detector, &models.recognizer, &models.dictionary)
        .expect("probe models must build on the wgpu runtime");
    let gray = image::open(PathBuf::from(&page))
        .expect("open probe page")
        .to_luma8();

    let started = std::time::Instant::now();
    let lines = engine.ocr_page(&gray, "eng").expect("ocr_page");
    let elapsed = started.elapsed();

    eprintln!(
        "probe: {} lines from {}x{} in {elapsed:.2?}",
        lines.len(),
        gray.width(),
        gray.height()
    );
    for line in lines.iter().take(10) {
        eprintln!("  {:?}", line.text);
    }

    // A full transcript is what `score_ocr_accuracy.py` needs; the ten lines
    // above are only for reading the run as it happens.
    if let Some(destination) = std::env::var_os("LEGE_OCR_PROBE_OUT") {
        let transcript = lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(PathBuf::from(&destination), transcript).expect("write probe transcript");
        eprintln!("probe: transcript written to {:?}", destination);
    }

    assert!(
        !lines.is_empty(),
        "probe page produced no text lines at all"
    );
}
