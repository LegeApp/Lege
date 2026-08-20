//! Rust values to Java objects.
//!
//! All construction runs on the host's own thread (see the module docs in
//! `lib.rs`), so `find_class` resolves against the application ClassLoader and
//! no global references or thread attachment are needed.

use jni::JNIEnv;
use jni::objects::{JObject, JValue};

use lege::progress::{ProcessingStatus, ProgressMetrics, ProgressMode, ProgressUpdate};

/// JVM path of the host package. The `Java_…` symbol names in `api` encode the
/// same package and must be changed alongside this.
pub(crate) const JAVA_PACKAGE_PATH: &str = "com/legeapp/lege";

/// `LegeProgress.kind` discriminants. Mirrored by the Kotlin side.
pub(crate) mod kind {
    pub(crate) const STATUS: i32 = 0;
    pub(crate) const COMPLETED: i32 = 1;
    pub(crate) const ERROR: i32 = 2;
}

/// Sentinel for `ProgressMetrics::eta_seconds == None`, chosen over a boxed
/// `Integer` so `LegeMetrics` stays a flat primitive carrier.
const NO_ETA: i32 = -1;

fn class_path(simple_name: &str) -> String {
    format!("{JAVA_PACKAGE_PATH}/{simple_name}")
}

/// Stable integer for `ProgressMode`. Hand-assigned rather than derived from
/// declaration order, so reordering the Rust enum cannot silently renumber the
/// values the Kotlin side matches on.
fn mode_ordinal(mode: ProgressMode) -> i32 {
    match mode {
        ProgressMode::Unknown => 0,
        ProgressMode::NoLayout => 1,
        ProgressMode::Layout => 2,
        ProgressMode::Margin => 3,
        ProgressMode::HeavySequential => 4,
        ProgressMode::Reflow => 5,
        ProgressMode::Epub => 6,
    }
}

/// Build a `LegeMetrics`, or the null reference when there is no snapshot.
fn new_metrics<'local>(
    env: &mut JNIEnv<'local>,
    metrics: Option<ProgressMetrics>,
) -> jni::errors::Result<JObject<'local>> {
    let Some(metrics) = metrics else {
        return Ok(JObject::null());
    };

    env.new_object(
        class_path("LegeMetrics"),
        "(IIIIIZZI)V",
        &[
            JValue::Int(metrics.pages_total as i32),
            JValue::Int(metrics.rendered as i32),
            JValue::Int(metrics.detected as i32),
            JValue::Int(metrics.encoded as i32),
            JValue::Int(mode_ordinal(metrics.mode)),
            JValue::Bool(u8::from(metrics.is_djvu)),
            JValue::Bool(u8::from(metrics.enable_layout_detection)),
            JValue::Int(metrics.eta_seconds.map_or(NO_ETA, |eta| eta as i32)),
        ],
    )
}

/// Build a `LegeProgress` from one pipeline update.
///
/// The three display strings come from
/// [`ProcessingStatus::to_gui_display_lines`], which the desktop GUI already
/// uses. That is why the ~18 `ProcessingStatus` variants need no per-variant
/// marshalling here: the numeric payload is already unified in
/// `ProgressMetrics`, and the prose is already rendered. A new variant flows
/// through this bridge unchanged.
pub(crate) fn new_progress<'local>(
    env: &mut JNIEnv<'local>,
    update: &ProgressUpdate,
) -> jni::errors::Result<JObject<'local>> {
    let (kind, task_id, lines, metrics) = match update {
        ProgressUpdate::Status {
            task_id,
            status,
            metrics,
        } => (
            kind::STATUS,
            *task_id,
            status.to_gui_display_lines(),
            *metrics,
        ),
        ProgressUpdate::Completed {
            task_id,
            message,
            metrics,
        } => (
            kind::COMPLETED,
            *task_id,
            ProcessingStatus::Complete {
                message: message.clone(),
            }
            .to_gui_display_lines(),
            *metrics,
        ),
        ProgressUpdate::Error {
            task_id,
            error,
            metrics,
        } => (
            kind::ERROR,
            *task_id,
            ProcessingStatus::Error {
                error: error.clone(),
            }
            .to_gui_display_lines(),
            *metrics,
        ),
    };

    let (headline, detail, hint) = lines;
    let headline = env.new_string(headline)?;
    let detail = env.new_string(detail)?;
    let hint = env.new_string(hint)?;
    let metrics = new_metrics(env, metrics)?;

    // Built rather than written out so the package appears in exactly one
    // place; a hardcoded `Lcom/legeapp/lege/LegeMetrics;` here would silently
    // survive a change to JAVA_PACKAGE_PATH and fail at runtime.
    let signature = format!(
        "(IJLjava/lang/String;Ljava/lang/String;Ljava/lang/String;L{JAVA_PACKAGE_PATH}/LegeMetrics;)V"
    );

    env.new_object(
        class_path("LegeProgress"),
        signature,
        &[
            JValue::Int(kind),
            JValue::Long(task_id as i64),
            JValue::Object(&headline),
            JValue::Object(&detail),
            JValue::Object(&hint),
            JValue::Object(&metrics),
        ],
    )
}

/// Throw a Java `IllegalStateException` carrying `message`.
///
/// Used where an entry point cannot return a meaningful value. Failing to
/// throw is itself only loggable — there is nowhere left to report it.
pub(crate) fn throw(env: &mut JNIEnv<'_>, message: &str) {
    if env
        .throw_new("java/lang/IllegalStateException", message)
        .is_err()
    {
        log::error!("lege: failed to raise Java exception: {message}");
    }
}
