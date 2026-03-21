use dioxus::prelude::*;
use std::collections::{HashSet, VecDeque};
use std::fmt::Display;
use std::fs;

// Import version from the crate root
use crate::version::display_version;

#[cfg(target_os = "windows")]
fn is_msix_packaged() -> bool {
    // Detect packaged status using Windows AppModel. Calling with None yields
    // ERROR_INSUFFICIENT_BUFFER when packaged, and a distinct error when unpackaged.
    use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS};
    use windows::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;

    let mut len: u32 = 0;
    let rc = unsafe { GetCurrentPackageFullName(&mut len, None) };
    rc == ERROR_SUCCESS || rc == ERROR_INSUFFICIENT_BUFFER
}

#[cfg(feature = "debug-logging")]
use lege::dbglog;

// Assuming these are in your project
use crate::backend;
use crate::models::DocumentStatus;
use crate::models::{
    CompressionType, DocumentItem, ImageProcessingType, LogEntry, OutputFormat, ProcessingOptions,
    ProcessingResult,
};

use crate::logging;

// Import text loader for tooltips and messages
use crate::gui_text::GUI_TEXT;
use lege::target_profiles;
use lege::text_loader::CLI_TEXT;

fn format_eta_label(seconds: u32) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let secs = seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes:02}:{secs:02}")
    }
}

#[cfg(feature = "debug-logging")]
fn load_debug_log_file() -> Vec<String> {
    Legencode::get_debug_log_messages()
}

#[cfg(not(feature = "debug-logging"))]
#[allow(dead_code)]
fn load_debug_log_file() -> Vec<String> {
    Vec::new()
}

#[component]
fn PopupRail(state: Signal<AppState>) -> Element {
    use tokio::time::{Duration, sleep};

    let (
        show_hardware,
        hardware_msg,
        show_output,
        output_msg,
        show_no_layout,
        no_layout_msg,
        show_footnotes,
        footnotes_msg,
        show_ocr,
        ocr_msg,
        show_queue_operation,
        queue_operation_msg,
        show_processing,
        processing_result,
        center_active,
        crop_active,
    ) = {
        let read = state.read();
        (
            read.show_hardware_popup,
            read.hardware_acceleration_popup.clone(),
            read.show_output_popup,
            read.output_folder_popup.clone(),
            read.show_no_layout_popup,
            read.no_layout_detection_popup.clone(),
            read.show_footnotes_popup,
            read.footnotes_popup.clone(),
            read.show_ocr_detection_popup,
            read.ocr_detection_popup.clone(),
            read.show_queue_operation_popup,
            read.queue_operation_popup.clone(),
            read.show_post_processing_popup,
            read.processing_result.as_ref().cloned(),
            read.options.center_margins,
            read.options.crop_margins,
        )
    };

    let hardware_card = if show_hardware {
        hardware_msg.map(|msg| (msg, state.clone()))
    } else {
        None
    };

    let output_card = if show_output {
        output_msg.map(|msg| (msg, state.clone()))
    } else {
        None
    };

    let no_layout_card = if show_no_layout {
        no_layout_msg.map(|msg| (msg, state.clone()))
    } else {
        None
    };

    let footnotes_card = if show_footnotes && (center_active || crop_active) {
        footnotes_msg.map(|msg| (msg, state.clone()))
    } else {
        None
    };

    let ocr_card = if show_ocr {
        ocr_msg.map(|msg| (msg, state.clone()))
    } else {
        None
    };

    let queue_operation_card = if show_queue_operation {
        queue_operation_msg.map(|msg| (msg, state.clone()))
    } else {
        None
    };

    let processing_card = if show_processing {
        if let Some(result) = processing_result {
            let mut state_for_timeout = state.clone();
            let output_token = result.output_path.clone();
            spawn(async move {
                sleep(Duration::from_secs(5)).await;
                let mut s = state_for_timeout.write();
                if let Some(current) = s.processing_result.as_ref() {
                    if current.output_path == output_token {
                        s.show_post_processing_popup = false;
                        s.processing_result = None;
                    }
                }
            });

            let input_path_display = result.input_path.display().to_string();
            let output_path_display = result.output_path.display().to_string();
            let input_name = result
                .input_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&input_path_display);
            let output_name = result
                .output_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&output_path_display);
            let original_size = result.original_size;
            let compressed_size = result.compressed_size;
            let reduction_percent = if original_size > 0 {
                (1.0 - (compressed_size as f64 / original_size as f64)) * 100.0
            } else {
                0.0
            };
            let formatted_original = LogEntry::format_size(original_size);
            let formatted_compressed = LogEntry::format_size(compressed_size);
            let show_reduction = !result.page_range_used;
            let message = if show_reduction {
                if reduction_percent >= 0.0 {
                    format!(
                        "Finished {} → {} ({:.1}% smaller, {} → {})",
                        input_name,
                        output_name,
                        reduction_percent,
                        formatted_original,
                        formatted_compressed
                    )
                } else {
                    format!(
                        "Finished {} → {} ({:.1}% larger, {} → {})",
                        input_name,
                        output_name,
                        reduction_percent.abs(),
                        formatted_original,
                        formatted_compressed
                    )
                }
            } else {
                format!(
                    "Finished {} → {} (processed selected pages, {} output)",
                    input_name, output_name, formatted_compressed
                )
            };

            Some((state.clone(), message))
        } else {
            None
        }
    } else {
        None
    };

    let has_cards = hardware_card.is_some()
        || output_card.is_some()
        || no_layout_card.is_some()
        || footnotes_card.is_some()
        || ocr_card.is_some()
        || queue_operation_card.is_some()
        || processing_card.is_some();

    if !has_cards {
        return VNode::empty();
    }

    // Helper function to truncate text to max 255 characters
    let truncate_text = |text: &str| -> String {
        if text.len() > 255 {
            format!("{}...", &text[..252])
        } else {
            text.to_string()
        }
    };

    rsx! {
        div { class: "popup-rail",
            if let Some((msg, mut state_close)) = hardware_card {
                div { class: "popup-chip",
                    span { class: "popup-chip__text", "{truncate_text(&msg)}" }
                    button {
                        class: "popup-chip__close",
                        onclick: move |_| {
                            let mut s = state_close.write();
                            s.show_hardware_popup = false;
                            s.hardware_acceleration_popup = None;
                        },
                        "×"
                    }
                }
            }

            if let Some((msg, mut state_close)) = output_card {
                div { class: "popup-chip",
                    span { class: "popup-chip__text", "{truncate_text(&msg)}" }
                    button {
                        class: "popup-chip__close",
                        onclick: move |_| {
                            let mut s = state_close.write();
                            s.show_output_popup = false;
                            s.output_folder_popup = None;
                        },
                        "×"
                    }
                }
            }

            if let Some((msg, mut state_close)) = no_layout_card {
                div { class: "popup-chip",
                    span { class: "popup-chip__text", "{truncate_text(&msg)}" }
                    button {
                        class: "popup-chip__close",
                        onclick: move |_| {
                            let mut s = state_close.write();
                            s.show_no_layout_popup = false;
                            s.no_layout_detection_popup = None;
                        },
                        "×"
                    }
                }
            }

            if let Some((msg, mut state_close)) = footnotes_card {
                div { class: "popup-chip",
                    span { class: "popup-chip__text", "{truncate_text(&msg)}" }
                    button {
                        class: "popup-chip__close",
                        onclick: move |_| {
                            let mut s = state_close.write();
                            s.show_footnotes_popup = false;
                            s.footnotes_popup = None;
                        },
                        "×"
                    }
                }
            }

            if let Some((msg, mut state_close)) = ocr_card {
                div { class: "popup-chip",
                    span { class: "popup-chip__text", "{truncate_text(&msg)}" }
                    button {
                        class: "popup-chip__close",
                        onclick: move |_| {
                            let mut s = state_close.write();
                            s.show_ocr_detection_popup = false;
                            s.ocr_detection_popup = None;
                        },
                        "×"
                    }
                }
            }

            if let Some((msg, mut state_close)) = queue_operation_card {
                div { class: "popup-chip",
                    span { class: "popup-chip__text", "{truncate_text(&msg)}" }
                    button {
                        class: "popup-chip__close",
                        onclick: move |_| {
                            let mut s = state_close.write();
                            s.show_queue_operation_popup = false;
                            s.queue_operation_popup = None;
                        },
                        "×"
                    }
                }
            }

            if let Some((mut state_close, message)) = processing_card {
                div { class: "popup-chip",
                    span { class: "popup-chip__text", "{truncate_text(&message)}" }
                    button {
                        class: "popup-chip__close",
                        onclick: move |_| {
                            let mut s = state_close.write();
                            s.show_post_processing_popup = false;
                            s.processing_result = None;
                        },
                        "×"
                    }
                }
            }
        }
    }
}

// AppState with added fields for editing
#[derive(Clone, Debug, PartialEq)]
pub struct AppState {
    // Queue and processing
    pub queue: VecDeque<DocumentItem>,
    pub options: ProcessingOptions,
    pub is_processing: bool,
    pub current_processing_index: Option<usize>,
    pub status_message: String,
    // Always-present 3-line status for the status bar (4th line for multi-file processing)
    pub status_lines: (String, String, String, String),
    // Rolling log of recent single-line status updates (for 3-line window)
    pub status_log: VecDeque<String>,
    pub active_eta: Option<String>,

    // Track currently running job task IDs for cancellation
    pub active_task_ids: Vec<u64>,

    // UI state
    pub show_settings: bool,
    pub should_cancel: bool,
    pub output_is_editing: bool,
    pub next_output_click_opens_picker: bool,
    // Single vs double-click handling for Output Directory button
    pub output_click_pending: bool,

    // Input fields (for validation)
    pub target_height_input: String,
    pub target_resolution_selection: String,
    pub k_factor_input: String,
    pub threshold_input: String,
    pub page_range_input: String,

    // Bottom left popups
    pub hardware_acceleration_popup: Option<String>,
    pub show_hardware_popup: bool,
    pub output_folder_popup: Option<String>,
    pub show_output_popup: bool,
    // No Layout Detection popup (bottom-left)
    pub no_layout_detection_popup: Option<String>,
    pub show_no_layout_popup: bool,
    // Footnotes detected popup (bottom-left)
    pub footnotes_popup: Option<String>,
    pub show_footnotes_popup: bool,
    // OCR layer detection popup (bottom-left)
    pub ocr_detection_popup: Option<String>,
    pub show_ocr_detection_popup: bool,
    // Queue operation popup (bottom-left)
    pub queue_operation_popup: Option<String>,
    pub show_queue_operation_popup: bool,

    // Post-processing popup
    pub show_post_processing_popup: bool,
    pub processing_result: Option<ProcessingResult>,

    // Logging
    pub show_log_viewer: bool,
    pub processing_log: Vec<LogEntry>,

    // Licenses window
    pub show_licenses: bool,
    pub licenses_content: String,
    // Documentation window
    pub show_documentation: bool,
    pub documentation_content: String,
    // About window
    pub show_about: bool,

    // Debug logging (only when debug-logging feature is enabled)
    #[cfg(feature = "debug-logging")]
    pub show_debug_log: bool,
    #[cfg(feature = "debug-logging")]
    pub debug_log_messages: Vec<String>,

    // Queue viewer popover
    pub show_queue_viewer: bool,
}

impl Default for AppState {
    fn default() -> Self {
        // Get default options - single source of truth for all defaults
        let defaults = ProcessingOptions::new();

        // Pre-computed defaults for faster initialization
        Self {
            queue: VecDeque::new(),
            options: defaults.clone(),
            is_processing: false,
            current_processing_index: None,
            status_message: GUI_TEXT.interactive.status.ready.clone(),
            status_lines: (
                GUI_TEXT.interactive.status.ready.clone(),
                String::new(),
                String::new(),
                String::new(),
            ),
            status_log: VecDeque::new(),
            active_eta: None,
            active_task_ids: Vec::new(),
            show_settings: false,
            should_cancel: false,
            output_is_editing: false,
            next_output_click_opens_picker: false,
            output_click_pending: false,
            // Convert defaults from ProcessingOptions to strings (single source of truth)
            target_height_input: defaults
                .target_height
                .map(|h| h.to_string())
                .unwrap_or_default(),
            target_resolution_selection: target_profiles::PROPORTIONAL_OPTION_LABEL.to_string(),
            k_factor_input: defaults.k_factor.to_string(),
            threshold_input: defaults.threshold_value.to_string(),
            page_range_input: String::new(),
            hardware_acceleration_popup: None,
            show_hardware_popup: false,
            output_folder_popup: None,
            show_output_popup: false,
            no_layout_detection_popup: None,
            show_no_layout_popup: false,
            footnotes_popup: None,
            show_footnotes_popup: false,
            ocr_detection_popup: None,
            show_ocr_detection_popup: false,
            queue_operation_popup: None,
            show_queue_operation_popup: false,
            show_post_processing_popup: false,
            processing_result: None,
            show_queue_viewer: false,
            show_log_viewer: false,
            processing_log: Vec::new(),
            show_licenses: false,
            licenses_content: String::new(),
            show_documentation: false,
            documentation_content: String::new(),
            show_about: false,

            // Debug logging defaults (only when debug-logging feature is enabled)
            #[cfg(feature = "debug-logging")]
            show_debug_log: false,
            #[cfg(feature = "debug-logging")]
            debug_log_messages: Vec::new(),
        }
    }
}

impl AppState {
    /// Convenience: set a single-line status and mirror into 4-line form
    fn set_status_message(&mut self, msg: impl Into<String>) {
        let m = msg.into();
        self.status_message = m.clone();
        self.status_lines = (m, String::new(), String::new(), String::new());
    }

    /// Set three status lines at once (4th line cleared)
    fn set_status_lines(
        &mut self,
        l1: impl Into<String>,
        l2: impl Into<String>,
        l3: impl Into<String>,
    ) {
        let new_lines = (l1.into(), l2.into(), l3.into(), String::new());
        // Only update if the lines have actually changed to prevent unnecessary re-renders
        if self.status_lines != new_lines {
            self.status_lines = new_lines;
        }
    }

    /// Push a single-line log entry and map to 3 visible lines (4th line cleared).
    fn push_status_log(&mut self, line: impl Into<String>) {
        let line = line.into();
        // Deduplicate trivial repeats to reduce flicker
        if self.status_log.back().map(|s| s == &line).unwrap_or(false) {
            // still update the 3-line window to keep it fresh
        } else {
            self.status_log.push_back(line);
            if self.status_log.len() > 100 {
                self.status_log.pop_front();
            }
        }
        // Map last three entries to the status_lines window (top = newest)
        let n = self.status_log.len();
        let l1 = self
            .status_log
            .get(n.saturating_sub(1))
            .cloned()
            .unwrap_or_default();
        let l2 = self
            .status_log
            .get(n.saturating_sub(2))
            .cloned()
            .unwrap_or_default();
        let l3 = self
            .status_log
            .get(n.saturating_sub(3))
            .cloned()
            .unwrap_or_default();
        self.status_lines = (l1, l2, l3, String::new());
    }
}

#[component]
fn CenterSettingsPanel(state: Signal<AppState>) -> Element {
    let (selection, is_proportional, height_input, current_device) = {
        let read = state.read();
        (
            read.target_resolution_selection.clone(),
            read.options.target_device.is_none(),
            read.target_height_input.clone(),
            read.options.target_device.clone(),
        )
    };
    let select_title = if is_proportional {
        if height_input.is_empty() {
            GUI_TEXT.interactive.tooltips.target_height.clone()
        } else {
            format!("{} px (proportional width)", height_input)
        }
    } else if let Some(ref device) = current_device {
        if let Some(profile) = target_profiles::find_profile(device) {
            format!("{} × {} px", profile.width, profile.height)
        } else {
            GUI_TEXT.interactive.tooltips.target_height.clone()
        }
    } else {
        GUI_TEXT.interactive.tooltips.target_height.clone()
    };
    rsx! {
        div {
            class: "settings-card settings-card-center",

            div { class: "setting-item",
                label {
                    class: "setting-label",
                    title: "{GUI_TEXT.interactive.tooltips.page_range}",
                    "{GUI_TEXT.interactive.labels.page_range}"
                }
                input {
                    class: "setting-input",
                    r#type: "text",
                    placeholder: "e.g., 1-10",
                    value: "{state.read().page_range_input}",
                    oninput: move |evt| {
                        let mut s = state.write();
                        s.page_range_input = evt.value();
                        s.options.page_range = if evt.value().trim().is_empty() {
                            None
                        } else {
                            Some(evt.value().trim().to_string())
                        };
                    },
                }
            }

            div { class: "setting-item",
                div { class: "setting-label" }
                ThemedDropdown {
                    width: "260px".to_string(),
                    offset_left: -20,
                    title: select_title.clone(),
                    selected: selection.clone(),
                    options: {
                        let mut opts: Vec<(String, String)> = Vec::new();
                        opts.push((target_profiles::PROPORTIONAL_OPTION_LABEL.to_string(), GUI_TEXT.interactive.tooltips.target_height.clone()));
                        for profile in target_profiles::TARGET_DEVICE_PROFILES.iter() {
                            opts.push((profile.name.to_string(), format!("{} × {} px", profile.width, profile.height)));
                        }
                        opts
                    },
                    on_change: move |selected: String| {
                        let mut s = state.write();
                        s.target_resolution_selection = selected.clone();
                        if selected == target_profiles::PROPORTIONAL_OPTION_LABEL {
                            s.options.target_device = None;
                        } else if let Some(profile) = target_profiles::find_profile(&selected) {
                            s.options.target_device = Some(profile.name.to_string());
                            s.options.target_height = Some(profile.height);
                            s.target_height_input = profile.height.to_string();
                        } else {
                            s.options.target_device = None;
                        }
                    }
                }
            }

            div { class: "setting-item",
                div { class: "setting-label" }
                if is_proportional {
                    input {
                        class: "setting-input",
                        r#type: "number",
                        min: "100", max: "5000",
                        value: "{height_input}",
                        oninput: move |evt| {
                            let mut s = state.write();
                            s.target_resolution_selection = target_profiles::PROPORTIONAL_OPTION_LABEL.to_string();
                            s.options.target_device = None;
                            s.target_height_input = evt.value();
                            if let Ok(height) = evt.value().parse::<u32>() {
                                s.options.target_height = Some(height);
                            }
                        },
                    }
                } else if let Some(device_name) = current_device.clone() {
                    if let Some(profile) = target_profiles::find_profile(&device_name) {
                        input {
                            class: "setting-input",
                            r#type: "text",
                            value: "{profile.width} × {profile.height} px",
                            readonly: true,
                        }
                    }
                }
            }

            // Margin and deskew options
            div {
                style: "margin-top: 15px; margin-bottom: 15px; padding-top: 10px; border-top: 1px solid #e0e0e0;",
                div {
                    label {
                        class: "checkbox-label",
                        title: "Center content on a standard-sized canvas with uniform margins",
                        input {
                            r#type: "checkbox",
                            checked: state.read().options.center_margins,
                            onchange: move |evt| {
                                let mut s = state.write();
                                s.options.center_margins = evt.checked();
                                if s.options.center_margins {
                                    s.options.crop_margins = false;
                                }
                            }
                        }
                        span { style: "margin-left: 8px;", "Center margins" }
                    }
                }

                div {
                    label {
                        class: "checkbox-label",
                        title: "Crop to content area and resize to standard height",
                        input {
                            r#type: "checkbox",
                            checked: state.read().options.crop_margins,
                            onchange: move |evt| {
                                let mut s = state.write();
                                s.options.crop_margins = evt.checked();
                                if s.options.crop_margins {
                                    s.options.center_margins = false;
                                }
                            }
                        }
                        span { style: "margin-left: 8px;", "Crop margins" }
                    }
                }

                div {
                    label {
                        class: "checkbox-label",
                        title: "{GUI_TEXT.interactive.tooltips.deskew_documents}",
                        input {
                            r#type: "checkbox",
                            checked: state.read().options.deskew_documents,
                            onchange: move |evt| {
                                let mut s = state.write();
                                s.options.deskew_documents = evt.checked();
                            }
                        }
                        span { style: "margin-left: 8px;", "{GUI_TEXT.interactive.labels.deskew_documents}" }
                    }
                }

                // When Crop margins is selected, show a 'Crop footnotes' override
                if state.read().options.crop_margins {
                    div {
                        style: "margin-left: 24px; margin-top: 6px;",
                        label {
                            class: "checkbox-label",
                            title: "Allow cropping footnotes (disables auto-centering override)",
                            input {
                                r#type: "checkbox",
                                checked: state.read().options.crop_footnotes,
                                onchange: move |evt| {
                                    let enable = evt.checked();
                                    {
                                        let mut s = state.write();
                                        s.options.crop_footnotes = enable;
                                        if enable {
                                            s.footnotes_popup = Some("Crop footnotes enabled — will not switch to centering when footnotes are detected.".to_string());
                                            s.show_footnotes_popup = true;
                                        }
                                    }
                                    if enable {
                                        let mut state_for_popup = state.clone();
                                        spawn(async move {
                                            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                                            state_for_popup.write().show_footnotes_popup = false;
                                        });
                                    }
                                }
                            }
                            span { style: "margin-left: 8px;", "Crop footnotes" }
                        }
                    }
                }
            }
        }
    }
}

// Debug toggle button - only compiled when debug-logging feature is enabled
#[cfg(feature = "debug-logging")]
#[component]
fn DebugToggleButton(state: Signal<AppState>) -> Element {
    rsx! {
        button {
            class: "raised-btn",
            style: "padding: 6px 12px; font-size: 12px; border-radius: 4px;",
            onclick: move |_| {
                let mut s = state.write();
                s.show_debug_log = !s.show_debug_log;
                if s.show_debug_log {
                    // Load from persistent debug.log when opening
                    s.debug_log_messages = load_debug_log_file();
                }
            },
            "Debug"
        }
    }
}

// Empty debug toggle button when debug-logging feature is disabled
#[cfg(not(feature = "debug-logging"))]
#[component]
fn DebugToggleButton(state: Signal<AppState>) -> Element {
    let _ = state; // Consume the parameter to avoid warnings
    rsx! { div {} }
}

// Debug log viewer - only compiled when debug-logging feature is enabled
#[cfg(feature = "debug-logging")]
#[component]
fn DebugLogViewer(state: Signal<AppState>) -> Element {
    rsx! {
        if state.read().show_debug_log {
            DebugLogViewerPopup { state }
        }
    }
}

// Empty debug log viewer when debug-logging feature is disabled
#[cfg(not(feature = "debug-logging"))]
#[component]
fn DebugLogViewer(state: Signal<AppState>) -> Element {
    let _ = state; // Consume the parameter to avoid warnings
    rsx! { div {} }
}

#[component]
pub fn App() -> Element {
    let state = use_signal(AppState::default);
    let _window = dioxus_desktop::use_window();

    {
        let mut state_for_effect = state.clone();
        use_effect(move || {
            if let Ok(logs) = logging::load_log_entries() {
                state_for_effect.write().processing_log = logs;
            }
        });
    }

    // Load saved settings on startup
    {
        let mut state_for_effect = state.clone();
        use_effect(move || {
            if let Ok(Some(saved)) = crate::backend::load_saved_settings() {
                let mut s = state_for_effect.write();
                s.options = saved.clone();
                // Sync input fields from options
                if let Some(device) = saved.target_device.clone() {
                    s.target_resolution_selection = device.clone();
                    if let Some(profile) = target_profiles::find_profile(&device) {
                        s.target_height_input = profile.height.to_string();
                        s.options.target_height = Some(profile.height);
                    } else {
                        s.target_resolution_selection =
                            target_profiles::PROPORTIONAL_OPTION_LABEL.to_string();
                        s.options.target_device = None;
                        s.target_height_input = saved.target_height.unwrap_or(1200).to_string();
                    }
                } else {
                    s.target_resolution_selection =
                        target_profiles::PROPORTIONAL_OPTION_LABEL.to_string();
                    s.target_height_input = saved.target_height.unwrap_or(1200).to_string();
                }
                s.k_factor_input = saved.k_factor.to_string();
                s.threshold_input = saved.threshold_value.to_string();
                s.page_range_input = saved.page_range.unwrap_or_default();
                // Only show message if settings were actually loaded from file
                s.set_status_message("Settings loaded");
            }
        });
    }

    // Defer heavy initialization until after GUI appears
    {
        use_effect(move || {
            spawn(async move {
                // Give the GUI time to appear first
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                // The new progress system is self-cleaning via broadcast channels
            });
        });
    }

    // Initial positioning handled in WindowBuilder (platform-specific) to avoid visible snapping

    rsx! {
        div { class: "app-shell",
            div { class: "app-content-shell",
                // Draggable header
                HeaderBar {}

                // Main container for controls and settings
                div { class: "app-content-inner",
                    // Row 1: Main Action Buttons with visual hierarchy
                    ActionBar { state }

                    // Row 2: Settings panel with central start button
                    MainContentArea { state }

                    // The queue can be added back here later if desired
                }

                // Status bar
                StatusBar { state }
            }

            // Log viewer popup
            if state.read().show_log_viewer {
                LogViewerPopup { state }
            }

        // Documentation window popup
        if state.read().show_documentation {
            DocumentationWindow { state }
        }

        // Licenses window popup
        if state.read().show_licenses {
            LicensesWindow { state }
        }

        // About window popup
        if state.read().show_about {
            AboutWindow { state }
        }

            // Debug log viewer popup (only when debug-logging feature is enabled)
            DebugLogViewer { state }
        }
    }
}

#[component]
fn HeaderBar() -> Element {
    let window = dioxus_desktop::use_window();
    let window_minimize = window.clone();
    let window_close = window.clone();
    let window_drag = window.clone();
    // Use hand-grab cursor on Windows; keep default move cursor elsewhere
    let cursor_style = if cfg!(target_os = "windows") {
        "cursor: grab; flex: 1;"
    } else {
        "cursor: move; flex: 1;"
    };

    rsx! {
        div {
            style: "background: var(--panel-bg, #ffffff); padding: 12px 16px; display: flex; justify-content: space-between; align-items: center; user-select: none; border-bottom: 1px solid var(--outline-color, #808080);",

            div {
                class: "header-draggable",
                style: "{cursor_style}",
                onmousedown: move |evt| {
                    // Only drag if not clicking on buttons
                    if evt.data().trigger_button() == Some(dioxus::html::input_data::MouseButton::Primary) {
                        window_drag.drag();
                    }
                },
                h1 {
                    class: "heading-on-paper",
                    style: "margin: 0; font-size: 16px; pointer-events: none;",
                    "Lege"
                }
            }

            div {
                class: "window-controls",
                style: "display: flex; gap: 4px; z-index: 1000;",
                button {
                    class: "window-control-btn minimize-btn",
                    style: "z-index: 1001;",
                    onmousedown: move |evt| evt.stop_propagation(),
                    onclick: move |evt| {
                        evt.stop_propagation();
                        window_minimize.set_minimized(true);
                    },
                    "−"
                }
                button {
                    class: "window-control-btn close-btn",
                    style: "z-index: 1001;",
                    onmousedown: move |evt| evt.stop_propagation(),
                    onclick: move |evt| {
                        evt.stop_propagation();
                        window_close.close();
                    },
                    "×"
                }
            }
        }
    }
}

#[component]
fn ActionBar(state: Signal<AppState>) -> Element {
    let s = state.read();
    let output_path = s.options.output_path.clone();
    let is_editing = s.output_is_editing;

    rsx! {
        div {
            style: "background: var(--panel-bg, #ffffff); padding: 8px; border-radius: 4px; border: 1px solid var(--app-bg, #f5f5f5); display: flex; align-items: center; gap: 8px;",

            // --- Primary Actions (Evenly Spaced Buttons) ---
            button {
                class: "raised-btn primary-action-btn",
                style: "font-size: 18px; flex: 1;",
                onclick: move |_| {
                    spawn(async move {
                        if let Some(files) = rfd::AsyncFileDialog::new()
                            .add_filter("PDF", &["pdf"])
                            .pick_files()
                            .await {
                            let first_pdf_path = files.first().map(|f| f.path().to_owned());
                            let (_added_count, should_show_popup) = {
                                let mut s = state.write();
                                let added_count = files.len();
                                for file in files {
                                    s.queue.push_back(DocumentItem::new(file.path().to_owned()));
                                }
                                let should_show_popup = if s.options.output_path.is_none() {
                                    if let Some(first) = s.queue.front() {
                                        if let Some(parent) = first.file_path.parent() {
                                            s.options.output_path = Some(parent.to_path_buf());
                                            s.output_folder_popup = Some(GUI_TEXT.interactive.messages.defaulted_output_dir.clone());
                                            // Don't set show_output_popup here - let the delayed spawn do it
                                            true
                                        } else { false }
                                    } else { false }
                                } else { false };
                                (added_count, should_show_popup)
                            };

                            // Check OCR layer on the first PDF file added
                            if let Some(pdf_path) = first_pdf_path {
                                if let Ok(Some(has_ocr)) = backend::check_pdf_has_ocr(&pdf_path).await {
                                    let message = if has_ocr {
                                        "OCR layer detected — Leave OCR option off to preserve existing text layer.".to_string()
                                    } else {
                                        "No OCR layer found — Enable OCR if you want to add text recognition.".to_string()
                                    };
                                    let ocr_timeout_secs = if has_ocr { 5 } else { 5 };

                                    // Show OCR popup immediately - no delay
                                    let mut state_for_ocr = state.clone();
                                    spawn(async move {
                                        state_for_ocr.write().ocr_detection_popup = Some(message);
                                        state_for_ocr.write().show_ocr_detection_popup = true;

                                        tokio::time::sleep(tokio::time::Duration::from_secs(ocr_timeout_secs)).await;
                                        state_for_ocr.write().show_ocr_detection_popup = false;
                                    });

                                    // Show output folder popup after OCR popup finishes (wait OCR_timeout + 0.5s)
                                    if should_show_popup {
                                        let mut state_clone = state.clone();
                                        let delay = ocr_timeout_secs;
                                        spawn(async move {
                                            tokio::time::sleep(tokio::time::Duration::from_millis(delay * 1000 + 500)).await;
                                            state_clone.write().show_output_popup = true;

                                            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                                            state_clone.write().show_output_popup = false;
                                        });
                                    }
                                } else {
                                    // No OCR check result, just show output popup if needed
                                    if should_show_popup {
                                        let mut state_clone = state.clone();
                                        spawn(async move {
                                            state_clone.write().show_output_popup = true;

                                            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                                            state_clone.write().show_output_popup = false;
                                        });
                                    }
                                }
                            } else {
                                // No PDF to check, just show output popup immediately if needed
                                if should_show_popup {
                                    let mut state_clone = state.clone();
                                    spawn(async move {
                                        state_clone.write().show_output_popup = true;

                                        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                                        state_clone.write().show_output_popup = false;
                                    });
                                }
                            }
                        }
                    });
                },
                "{GUI_TEXT.interactive.buttons.add_files}"
            }
            button {
                class: "raised-btn primary-action-btn",
                style: "font-size: 18px; flex: 1;",
                onclick: move |_| {
                     spawn(async move {
                        if let Some(folder) = rfd::AsyncFileDialog::new().pick_folder().await {
                            let path = folder.path().to_owned();

                            // First check if it's an image folder
                            if let Ok(image_files) = backend::get_image_files_in_directory(&path) {
                                if !image_files.is_empty() {
                                    // Process as image folder
                                    let (_added_count, should_show_popup) = {
                                        let mut s = state.write();
                                        // For image folders, we create a single DocumentItem representing the folder
                                        let folder_item = DocumentItem {
                                            id: uuid::Uuid::new_v4().to_string(),
                                            file_path: path.clone(),
                                            file_name: path.file_name().unwrap_or_default().to_string_lossy().to_string(),
                                            status: DocumentStatus::Queued,
                                            output_path: None,
                                            progress: 0.0,
                                            error_message: None,
                                        };
                                        s.queue.push_back(folder_item);

                                        let should_show_popup = if s.options.output_path.is_none() {
                                            s.options.output_path = Some(path.clone());
                                            s.output_folder_popup = Some(GUI_TEXT.interactive.messages.defaulted_output_dir.clone());
                                            s.show_output_popup = true;
                                            true
                                        } else { false };

                                        let queue_len = s.queue.len();
                                        s.set_status_message(format!(
                                            "Added image folder with {} images (total: {})",
                                            image_files.len(),
                                            queue_len
                                        ));
                                        (image_files.len(), should_show_popup)
                                    };

                                    if should_show_popup {
                                        let mut state_clone = state.clone();
                                        spawn(async move {
                                            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                                            state_clone.write().show_output_popup = false;
                                        });
                                    }
                                    return;
                                }
                            }

                            // If not an image folder, try PDF folder
                            if let Ok(pdf_files) = backend::get_pdf_files_in_directory(&path) {
                                let (_added_count, should_show_popup) = {
                                    let mut s = state.write();
                                    let added_count = pdf_files.len();
                                    for file_path in pdf_files {
                                        s.queue.push_back(DocumentItem::new(file_path));
                                    }
                                    let should_show_popup = if s.options.output_path.is_none() {
                                        s.options.output_path = Some(path.clone());
                                        s.output_folder_popup = Some(GUI_TEXT.interactive.messages.defaulted_output_dir.clone());
                                        s.show_output_popup = true;
                                        true
                                    } else { false };
                                    // Precompute to avoid immutable borrow during mutable method call
                                    let queue_len = s.queue.len();
                                    s.set_status_message(format!(
                                        "Added {} PDF file{} from folder (total: {})",
                                        added_count,
                                        if added_count == 1 { "" } else { "s" },
                                        queue_len
                                    ));
                                    (added_count, should_show_popup)
                                };

                                // Now we can safely clone state outside the mutable borrow
                                if should_show_popup {
                                    let mut state_clone = state.clone();
                                    spawn(async move {
                                        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                                        state_clone.write().show_output_popup = false;
                                    });
                                }
                            }
                        }
                    });
                },
                "{GUI_TEXT.interactive.buttons.add_folder}"
            }


            // --- Output Directory (dynamic button/input) ---
            if let Some(path) = output_path {
                if is_editing {
                    input {
                        class: "setting-input",
                        style: "padding: 10px 14px; font-size: 18px; flex: 0.6;",
                        r#type: "text",
                        autofocus: true,
                        value: "{path.display()}",
                        oninput: move |evt| {
                            state.write().options.output_path = Some(std::path::PathBuf::from(evt.value()));
                        },
                        onblur: move |_| {
                            let mut s = state.write();
                            if s.options.output_path.as_ref().map_or(true, |p| p.to_string_lossy().is_empty()) {
                                s.options.output_path = None;
                            }
                            s.output_is_editing = false;
                            // Clear pending flags to avoid accidental picker
                            s.output_click_pending = false;
                            s.next_output_click_opens_picker = false;
                        }
                    }
                } else {
                    button {
                        class: "raised-btn",
                        style: "padding: 10px 14px; font-size: 18px; flex: 0.6; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;",
                        title: "single-click: choose folder. double-click: edit path.",
                        onclick: move |evt| {
                            evt.prevent_default();
                            // Robust single vs double-click logic (reversed):
                            // - First click: mark pending and after a short delay, open folder picker if no second click
                            // - Second click within window: enter edit mode instead of opening picker
                            let second_click = {
                                let mut s = state.write();
                                if s.output_click_pending {
                                    s.output_click_pending = false;
                                    true
                                } else {
                                    s.output_click_pending = true;
                                    false
                                }
                            };

                            if second_click {
                                // Double-click: go to edit mode
                                let mut s = state.write();
                                s.output_is_editing = true;
                            } else {
                                let mut state_clone = state.clone();
                                spawn(async move {
                                    tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
                                    let mut s2 = state_clone.write();
                                    if s2.output_click_pending {
                                        s2.output_click_pending = false;
                                        // Single-click: open folder picker
                                        drop(s2); // release before awaiting
                                        if let Some(folder) = rfd::AsyncFileDialog::new().pick_folder().await {
                                            let mut s3 = state_clone.write();
                                            s3.options.output_path = Some(folder.path().to_owned());
                                        }
                                    }
                                });
                            }
                        },
                        "{backend::truncate_path(&path, 35)}"
                    }
                }
            } else {
                button {
                    class: "raised-btn",
                    style: "padding: 10px 14px; font-size: 18px; flex: 1;",
                    onclick: move |_| {
                        spawn(async move {
                            if let Some(folder) = rfd::AsyncFileDialog::new().pick_folder().await {
                                state.write().options.output_path = Some(folder.path().to_owned());
                            }
                        });
                    },
                    "Output Directory"
                }
            }

            // --- Tertiary Actions (Smaller Buttons) ---
            div {
                style: "display: flex; flex-direction: column; gap: 4px;",
                button {
                    class: "raised-btn",
                    style: "padding: 2px 8px; font-size: 12px;",
                    onclick: move |_| {
                         // Persist current settings to settings.json
                         let options = state.read().options.clone();
                         let mut s = state.write();
                         match crate::backend::persist_settings(&options) {
                             Ok(_) => {
                                 s.set_status_message(GUI_TEXT.interactive.messages.settings_saved.clone());
                             }
                             Err(e) => {
                                 s.set_status_message(format!("Failed to save settings: {}", e));
                             }
                         }
                    },
                    "Save"
                }
                button {
                    class: "raised-btn",
                    style: "padding: 2px 8px; font-size: 12px;",
                    onclick: move |_| {
                        // Reset settings to defaults and remove saved settings file; DO NOT clear queue
                        let mut s = state.write();
                        s.options = ProcessingOptions::new();

                        // Also clear persisted settings on disk
                        if let Err(e) = crate::backend::remove_saved_settings() {
                            s.set_status_message(format!("Failed to clear saved settings: {}", e));
                        } else {
                            // Reset input fields to match defaults
                            s.target_height_input = s.options.target_height.unwrap_or(1200).to_string();
                            s.target_resolution_selection =
                                target_profiles::PROPORTIONAL_OPTION_LABEL.to_string();
                            s.k_factor_input = s.options.k_factor.to_string();
                            s.threshold_input = s.options.threshold_value.to_string();
                            s.page_range_input = String::new();
                            s.set_status_message(GUI_TEXT.interactive.messages.settings_reset.clone());
                        }
                    },
                    "Reset"
                }
            }

            // QoL: Clear Queue button
            if !state.read().queue.is_empty() {
                button {
                    class: "raised-btn danger",
                    style: "padding: 2px 8px; font-size: 12px;",
                    onclick: move |_| {
                        let initial_count = state.read().queue.len();
                        let active_count = state.read().active_task_ids.len();

                        // If there are active jobs, we can't determine which queue items they correspond to
                        // So we clear the entire queue and let processing continue
                        // The active jobs will try to remove themselves but won't find their entries (harmless)
                        if active_count > 0 {
                            // Clear all queued items - active jobs are already running
                            {
                                let mut s = state.write();
                                s.queue.clear();
                                s.show_queue_viewer = false;

                                // Don't reset is_processing or active_task_ids - let jobs finish
                                let message = format!(
                                    "Cleared {} queued file{} - {} active job{} will complete",
                                    initial_count,
                                    if initial_count == 1 { "" } else { "s" },
                                    active_count,
                                    if active_count == 1 { "" } else { "s" }
                                );
                                s.queue_operation_popup = Some(message);
                                s.show_queue_operation_popup = true;
                            } // Drop mutable borrow

                            // Auto-hide after 4 seconds
                            let mut state_for_popup = state.clone();
                            spawn(async move {
                                tokio::time::sleep(tokio::time::Duration::from_secs(4)).await;
                                state_for_popup.write().show_queue_operation_popup = false;
                            });
                        } else {
                            // No active jobs - safe to clear everything and reset state
                            {
                                let mut s = state.write();
                                let cleared_count = s.queue.len();
                                s.queue.clear();
                                s.show_queue_viewer = false;
                                s.is_processing = false;
                                s.should_cancel = false;
                                s.active_task_ids.clear();
                                s.processing_result = None;
                                s.show_post_processing_popup = false;
                                s.active_eta = None;

                                let message = format!("Cleared {} file{} from queue",
                                    cleared_count,
                                    if cleared_count == 1 { "" } else { "s" }
                                );
                                s.queue_operation_popup = Some(message);
                                s.show_queue_operation_popup = true;
                            } // Drop mutable borrow

                            // Auto-hide after 3 seconds
                            let mut state_for_popup = state.clone();
                            spawn(async move {
                                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                                state_for_popup.write().show_queue_operation_popup = false;
                            });
                        }
                    },
                    "Clear Queue"
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct ToggleButtonProps<T: Clone + PartialEq + Display + 'static> {
    left_option: T,
    right_option: T,
    current_selection: T,
    on_toggle: EventHandler<T>,
    #[props(default)]
    tooltip: Option<String>,
    #[props(default = "raised-btn toggle-btn".to_string())]
    button_class: String,
}

#[component]
fn ToggleButton<T: Clone + PartialEq + Display + 'static>(props: ToggleButtonProps<T>) -> Element {
    let is_left = props.current_selection == props.left_option;
    let (display_text, next_value) = if is_left {
        (props.left_option.to_string(), props.right_option.clone())
    } else {
        (props.right_option.to_string(), props.left_option.clone())
    };

    rsx! {
        button {
            class: "{props.button_class}",
            onclick: move |_| props.on_toggle.call(next_value.clone()),
            "{display_text}"
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
enum LayoutToggle {
    Off,
    On,
}

impl std::fmt::Display for LayoutToggle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LayoutToggle::Off => write!(f, "Off"),
            LayoutToggle::On => write!(f, "On"),
        }
    }
}

#[component]
fn MainContentArea(state: Signal<AppState>) -> Element {
    let popup_state = state.clone();
    let (is_processing, action_button_disabled) = {
        let s = state.read();
        (
            s.is_processing,
            s.queue.is_empty() || s.options.output_path.is_none(),
        )
    };

    rsx! {
        div {
            style: "position: relative; flex: 1; display: flex; flex-direction: column;",

            // This new container will manage the layout and CSS variables for the cutout effect.
            div {
                class: "settings-container",
                style: "flex: 1;", // Take up available space

                // Left panel - direct placement for proper flex behavior
                div {
                    class: "settings-panel-left",
                    style: "position: relative;",
                    LeftSettingsPanel { state }
                }

                // Center panel - page and margin controls
                div {
                    class: "settings-panel-center",
                    CenterSettingsPanel { state }
                }

                // Right panel - binarization options
                div {
                    class: "settings-panel-right",
                    RightSettingsPanel { state }
                }
            }

            // Central start processing button with debug toggle (positioned via theme CSS)
            div {
                class: "start-button-container",
                style: "position: relative; display: flex; align-items: center; justify-content: center; gap: 8px; padding: 12px 0; width: 100%;",

                PopupRail { state: popup_state.clone() }

                button {
                    class: if is_processing { "raised-btn danger" } else { "raised-btn success" },
                    id: "start-processing-btn", // Use an ID for easy selection in CSS
                    style: "width: 180px; height: 45px; font-size: 18px; font-weight: bold; border-radius: 10px;",
                    disabled: action_button_disabled,
                    onclick: move |_| {
                        if state.read().is_processing {
                            let mut s = state.write();
                            s.should_cancel = true;
                            s.set_status_message(GUI_TEXT.interactive.status.cancelling.clone());
                        } else {
                            // Clear any stale processing state before starting
                            {
                                let mut s = state.write();
                                s.show_hardware_popup = false;
                                s.show_output_popup = false;
                                s.processing_result = None;
                                s.show_post_processing_popup = false;
                                s.active_eta = None;
                            }
                            // Start processing and get actual provider info from backend
                            {
                                let mut s = state.write();
                                s.is_processing = true;
                                s.set_status_lines("[Starting]".to_string(), CLI_TEXT.interactive.messages.starting_processing.clone(), String::new());
                            }

                            let mut state_clone = state.clone();
                            let (queue, options) = {
                                let state_ref = state.read();
                                (state_ref.queue.clone(), state_ref.options.clone())
                            };
                            spawn(async move {
                                // Start async processing without upfront provider detection
                                match backend::start_async_processing(queue, &options).await {
                                    Ok(tracker_infos) => {
                                        // Store active task IDs for cancellation support
                                        {
                                            let mut s = state_clone.write();
                                            s.active_task_ids = tracker_infos.iter().map(|info| info.id).collect();
                                        }

                                        // Check if hardware acceleration is being used and show popup
                                        if options.layout_analysis || options.use_heavy_binarization {
                                            state_clone.write().hardware_acceleration_popup = Some("Using hardware acceleration".to_string());
                                            state_clone.write().show_hardware_popup = true;
                                            // Auto-hide after 3 seconds
                                            let mut state_for_popup = state_clone.clone();
                                            spawn(async move {
                                                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                                                state_for_popup.write().show_hardware_popup = false;
                                            });
                                        }
                                        state_clone.write().set_status_message(GUI_TEXT.interactive.status.processing.clone());

                                        // Set up simple file-based progress monitoring system
                                        let files_total = tracker_infos.len();
                                        let mut files_completed = 0usize;
                                        let mut handled: HashSet<u64> = HashSet::new();
                                        let mut active_files: HashSet<u64> = HashSet::new(); // Track which files are actively processing

                                        // Subscribe to progress updates from enhanced system
                                        let progress_manager = lege::progress::get_progress_manager();
                                        let receiver = progress_manager.subscribe();

                                        // Monitor progress via broadcast receiver
                                        loop {
                                            // Check for cancellation
                        if state_clone.read().should_cancel {
                            // Issue cancellation to all active tracker IDs
                            for info in tracker_infos.iter() {
                                let _ = lege::progress::cancel_task(info.id);
                            }

                            // Reset all processing state to clean slate
                            {
                                let mut state_write = state_clone.write();
                                state_write.is_processing = false;
                                state_write.should_cancel = false;
                                state_write.active_task_ids.clear();
                                state_write.set_status_message(GUI_TEXT.interactive.status.ready.clone());
                                state_write.show_hardware_popup = false;
                                state_write.show_output_popup = false;
                                state_write.processing_result = None;
                                state_write.show_post_processing_popup = false;
                            }
                            break;
                        }

                        // Wait for progress updates with timeout
                        match tokio::time::timeout(
                            tokio::time::Duration::from_millis(50), // Reduced from 100ms for more responsive updates
                            receiver.recv_async()
                        ).await {
                            Ok(Ok(update)) => {
                                                    match update {
                                                        lege::progress::ProgressUpdate::Status { task_id, status, metrics } => {
                                                            // Track active files for multi-file progress display
                                                            active_files.insert(task_id);

                                                            // GUI uses only the three core progress lanes.
                                                            // Do not surface separate DJVU bundling/finalizing steps.
                                                            match &status {
                                                                lege::progress::ProcessingStatus::AssemblingOutput => {
                                                                    // Suppress final bundling phase in GUI.
                                                                }
                                                                lege::progress::ProcessingStatus::NoLayoutProgress { .. } |
                                                                lege::progress::ProcessingStatus::LayoutProgress { .. } |
                                                                lege::progress::ProcessingStatus::MarginProgress { .. } => {
                                                                    // These statuses return coherent 3-line displays that should be updated atomically
                                                                    // to prevent flickering from multiple sequential updates
                                                                    let (l1, l2, l3) = status.to_gui_display_lines();
                                                                    state_clone.write().set_status_lines(l1, l2, l3);
                                                                }
                                                                lege::progress::ProcessingStatus::PdfAppend { .. } |
                                                                lege::progress::ProcessingStatus::PdfAppendMargin { .. } => {
                                                                    // These "page completed" messages are for CLI only.
                                                                    // In GUI, they would cause flickering with per-page progress updates.
                                                                    // Suppress them entirely - don't update status lines.
                                                                }
                                                                _ => {
                                                                    // Other statuses: add to rolling log
                                                                    let (l1, l2, l3) = status.to_gui_display_lines();
                                                                    state_clone.write().push_status_log(l1);
                                                                    if !l2.is_empty() { state_clone.write().push_status_log(l2); }
                                                                    if !l3.is_empty() { state_clone.write().push_status_log(l3); }
                                                                }
                                                            }

                                                            let eta_update = metrics
                                                                .and_then(|m| m.eta_seconds.map(format_eta_label));
                                                            state_clone.write().active_eta = eta_update;

                                                            // Update 4th status line for multi-file processing
                                                            if files_total > 1 {
                                                                let current_file = files_completed + 1;
                                                                let line4 = format!("Processing file {} of {}", current_file, files_total);
                                                                state_clone.write().status_lines.3 = line4;
                                                            }

                                                            // Show a bottom-left popup when footnotes are detected and centering is used
                                                            if let lege::progress::ProcessingStatus::FootnotesDetected { message } = &status {
                                                                state_clone.write().footnotes_popup = Some(message.clone());
                                                                state_clone.write().show_footnotes_popup = true;
                                                                let mut state_for_popup = state_clone.clone();
                                                                spawn(async move {
                                                                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                                                                    state_for_popup.write().show_footnotes_popup = false;
                                                                });
                                                            }

                                                            // Show a bottom-left popup when OCR layer is detected (or not detected)
                                                            if let lege::progress::ProcessingStatus::OcrLayerDetected { has_ocr } = &status {
                                                                let message = if *has_ocr {
                                                                    "OCR layer detected — Leave OCR option off to preserve existing text layer.".to_string()
                                                                } else {
                                                                    "No OCR layer found — Enable OCR if you want to add text recognition.".to_string()
                                                                };
                                                                let timeout_secs = if *has_ocr { 5 } else { 3 };
                                                                state_clone.write().ocr_detection_popup = Some(message);
                                                                state_clone.write().show_ocr_detection_popup = true;
                                                                let mut state_for_popup = state_clone.clone();
                                                                spawn(async move {
                                                                    tokio::time::sleep(tokio::time::Duration::from_secs(timeout_secs)).await;
                                                                    state_for_popup.write().show_ocr_detection_popup = false;
                                                                });
                                                            }
                                                        },
                                                        lege::progress::ProgressUpdate::Completed { task_id, message: _, metrics: _ } => {
                                                            if handled.insert(task_id) {
                                                                files_completed += 1;
                                                                active_files.remove(&task_id);
                                                                // Find the corresponding file info
                                                                if let Some(info) = tracker_infos.iter().find(|i| i.id == task_id) {
                                                                    // Use helper to calculate size - handles both files and image folders
                                                                    let original_size = backend::calculate_path_size(&info.input_path);
                                                                    let compressed_size = fs::metadata(&info.output_path).map(|m| m.len()).unwrap_or(0);
                                                                    let processing_result = ProcessingResult::new(
                                                                        info.input_path.clone(),
                                                                        info.output_path.clone(),
                                                                        original_size,
                                                                        compressed_size,
                                                                        options.page_range.is_some(),
                                                                    );

                                                                    // Add to persistent log + in-memory list
                                                                    if let Ok(log_entry) = logging::add_log_entry(&processing_result, &options) {
                                                                        state_clone.write().processing_log.push(log_entry);
                                                                    }

                                                                    // Show popup for this file
                                                                    state_clone.write().processing_result = Some(processing_result);
                                                                    state_clone.write().show_post_processing_popup = true;

                                                                    // Remove the completed file from the queue
                                                                    let mut s = state_clone.write();
                                                                    s.queue.retain(|item| item.file_path != info.input_path);
                                                                }

                                                                let files_remaining = files_total.saturating_sub(files_completed);
                                                                let summary_line = format!(
                                                                    "{} file{} completed, {} remaining",
                                                                    files_completed,
                                                                    if files_completed == 1 { "" } else { "s" },
                                                                    files_remaining
                                                                );
                                                                state_clone.write().push_status_log(summary_line);
                                                            }
                                                        },
                                                        lege::progress::ProgressUpdate::Error { task_id, error, metrics: _ } => {
                                                            if handled.insert(task_id) {
                                                                files_completed += 1;
                                                                active_files.remove(&task_id);

                                                                // Find the corresponding file info and remove from queue
                                                                if let Some(info) = tracker_infos.iter().find(|i| i.id == task_id) {
                                                                    let mut s = state_clone.write();
                                                                    s.queue.retain(|item| item.file_path != info.input_path);
                                                                }

                                                                // If this is a unified missing dependency error, surface friendly message
                                                                let dep_prefix = "Missing required component(s): ";
                                                                if let Some(idx) = error.find(dep_prefix) {
                                                                    let after = &error[idx + dep_prefix.len()..];
                                                                    // Trim trailing explanatory parentheses if present
                                                                    let list_part = if let Some(paren_idx) = after.find(" (ensure") { &after[..paren_idx] } else { after };
                                                                    let formatted = GUI_TEXT.interactive.status.missing_dependency.replace("{item}", list_part.trim());
                                                                    state_clone.write().set_status_message(formatted);
                                                                } else {
                                                                    state_clone.write().set_status_message(format!("Error: {}", error));
                                                                }
                                                            }
                                                        }
                                                    }
                                                },
                                                _ => {
                                                    // On timeout, just yield - don't break
                                                    tokio::task::yield_now().await;
                                                }
                                            }

                                            // Check if all files are completed
                                            if files_completed >= files_total {
                                                break;
                                            }
                                        }

                                        // Processing complete - processed files have been removed from queue
                                        {
                                            let mut s = state_clone.write();
                                            s.is_processing = false;
                                            s.active_task_ids.clear();
                                        }
                                        let queue_remaining = state_clone.read().queue.len();
                                        let status_line3 = if queue_remaining > 0 {
                                            format!("{} file(s) remaining in queue.", queue_remaining)
                                        } else {
                                            "Queue is now empty.".to_string()
                                        };
                                        state_clone.write().set_status_lines(
                                            "[Complete]",
                                            format!("{} file(s) processed successfully.", files_total),
                                            status_line3
                                        );
                                        state_clone.write().active_eta = None;

                                        // Per-file popups already shown during monitoring above

                                        // Hide provider status after a few seconds
                                        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                                        state_clone.write().show_hardware_popup = false;
                                        state_clone.write().show_output_popup = false;
                                    }
                                    Err(_e) => {
                                        {
                                            let mut state_write = state_clone.write();
                                            // Errors are handled in the status message area, no popup needed
                                            state_write.is_processing = false;
                                            state_write.set_status_message(GUI_TEXT.interactive.status.processing_failed.clone());
                                            state_write.active_eta = None;
                                        }

                                        // Keep error visible for longer
                                        let mut state_for_timeout = state_clone.clone();
                                        spawn(async move {
                                            tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
                                            state_for_timeout.write().show_hardware_popup = false;
                                            state_for_timeout.write().show_output_popup = false;
                                        });
                                    }
                                }
                            });
                        }
                    },
                    if is_processing { "Cancel" } else { "Process" }
                }


                // Debug toggle button (only when debug-logging feature is enabled)
                DebugToggleButton { state }
            }
        }
    }
}

#[component]
fn LeftSettingsPanel(state: Signal<AppState>) -> Element {
    rsx! {
        div {
            class: "settings-card settings-card-left",

            // Output format toggle (PDF vs DJVU)
            div { class: "setting-item",
                title: "{GUI_TEXT.interactive.tooltips.output_format}",
                label { class: "setting-label", "{GUI_TEXT.interactive.labels.output_format}" }
                ToggleButton {
                    left_option: OutputFormat::Pdf,
                    right_option: OutputFormat::Djvu,
                    current_selection: state.read().options.output_format.clone(),
                    on_toggle: move |val| state.write().options.output_format = val,
                    button_class: "raised-btn toggle-btn left-panel-btn",
                }
            }

            // Base Layer line removed (format inferred from image processing selection)

            // Keep image processing visible for both PDF and DJVU.
            // Base format remains PDF-only when layout detection is off.
            div { class: "setting-item",
                title: "{GUI_TEXT.interactive.tooltips.image_output_type}",
                label { class: "setting-label", "{GUI_TEXT.interactive.labels.image_output_type}" }
                ToggleButton {
                    left_option: ImageProcessingType::Original,
                    right_option: ImageProcessingType::Dithered,
                    current_selection: state.read().options.image_processing_type.clone(),
                    on_toggle: move |val| state.write().options.image_processing_type = val,
                    button_class: "raised-btn toggle-btn left-panel-btn",
                }
            }

            if matches!(state.read().options.output_format, OutputFormat::Pdf)
                && !state.read().options.layout_analysis
            {
                div { class: "setting-item",
                    title: "{GUI_TEXT.interactive.tooltips.base_format}",
                    label { class: "setting-label", "{GUI_TEXT.interactive.labels.base_format}" }
                    ToggleButton {
                        left_option: CompressionType::Ccitt4,
                        right_option: CompressionType::Jbig2,
                        current_selection: state.read().options.compression_type.clone(),
                        on_toggle: move |val| state.write().options.compression_type = val,
                        button_class: "raised-btn toggle-btn left-panel-btn",
                    }
                }
            }


            // 2) Layout Detection toggle
            div { class: "setting-item",
                title: "{GUI_TEXT.interactive.tooltips.layout_detection}",
                label { class: "setting-label", "{GUI_TEXT.interactive.labels.layout_detection}" }
                ToggleButton {
                    left_option: LayoutToggle::Off,
                    right_option: LayoutToggle::On,
                    current_selection: if state.read().options.layout_analysis {
                        LayoutToggle::On
                    } else {
                        LayoutToggle::Off
                    },
                    on_toggle: move |selection| {
                        let enable = matches!(selection, LayoutToggle::On);
                        let should_show_popup = {
                            let mut s = state.write();
                            s.options.layout_analysis = enable;
                            if !s.options.layout_analysis {
                                s.no_layout_detection_popup = Some("Image options only apply to the cover page when layout detection is off.".to_string());
                                s.show_no_layout_popup = true;
                                true
                            } else {
                                s.show_no_layout_popup = false;
                                false
                            }
                        };

                        if should_show_popup {
                            let mut state_clone = state.clone();
                            spawn(async move {
                                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                                state_clone.write().show_no_layout_popup = false;
                            });
                        }
                    },
                    button_class: "raised-btn toggle-btn left-panel-btn",
                }
            }

            // 3) Inverted colors (always available)
            label { class: "radio-label", title: "{GUI_TEXT.interactive.tooltips.inverted_colors}",
                input {
                    r#type: "checkbox",
                    checked: state.read().options.invert_input,
                    onchange: move |evt| {
                        state.write().options.invert_input = evt.checked();
                    }
                }
                span { "{GUI_TEXT.interactive.labels.inverted_colors}" }
            }

            // (moved to end) CCITT4 text with dithered images toggle


            // 5) OCR Text Layer (always allowed, including with quantize)
            label { class: "radio-label", title: "{GUI_TEXT.interactive.tooltips.ocr_text_layer}",
                input {
                    r#type: "checkbox",
                    checked: state.read().options.use_ocr,
                    onchange: move |evt| {
                        state.write().options.use_ocr = evt.checked();
                    }
                }
                span { "{GUI_TEXT.interactive.labels.ocr_text_layer}" }
            }

            label { class: "radio-label", title: "{GUI_TEXT.interactive.tooltips.high_quality_output}",
                input {
                    r#type: "checkbox",
                    checked: state.read().options.high_quality_output,
                    onchange: move |evt| {
                        state.write().options.high_quality_output = evt.checked();
                    }
                }
                span { "{GUI_TEXT.interactive.labels.high_quality_output}" }
            }

            // (removed) CCITT4 text with dithered images toggle - now controlled by base format toggle in no-layout mode
        }
    }
}

#[component]
fn RightSettingsPanel(state: Signal<AppState>) -> Element {
    let layout_on = state.read().options.layout_analysis;

    let (use_threshold, numeric_value) = {
        let read = state.read();
        // When layout is off, always behave as if fixed threshold is selected
        let flag = !layout_on || read.options.use_fixed_threshold;
        let value = if flag {
            read.threshold_input.clone()
        } else {
            read.k_factor_input.clone()
        };
        (flag, value)
    };
    let label_text = if use_threshold {
        "Threshold:"
    } else {
        "K-factor:"
    };
    let title_text = if use_threshold {
        GUI_TEXT.interactive.tooltips.threshold_value.clone()
    } else {
        GUI_TEXT.interactive.tooltips.sauvola_k_factor.clone()
    };
    let (step_value, min_value, max_value) = if use_threshold {
        ("1", "0", "255")
    } else {
        ("0.01", "0.0", "1.0")
    };

    rsx! {
        div {
            class: "settings-card settings-card-right",

            if layout_on {
                // Full binarization controls when layout detection is on
                div {
                    style: "font-weight: 600; margin-bottom: 8px;",
                    title: "Sauvola/Otsu fusion",
                    "Adaptive binarization"
                }

                // K-factor / threshold input
                div { class: "setting-item",
                    label {
                        class: "setting-label",
                        title: "{title_text}",
                        "{label_text}"
                    }
                    input {
                        class: "setting-input",
                        r#type: "number",
                        step: "{step_value}",
                        min: "{min_value}",
                        max: "{max_value}",
                        value: "{numeric_value}",
                        oninput: move |evt| {
                            let mut s = state.write();
                            if use_threshold {
                                s.threshold_input = evt.value();
                                if let Ok(val) = evt.value().parse::<u16>() {
                                    let clamped = val.min(255) as u8;
                                    s.options.threshold_value = clamped;
                                }
                            } else {
                                s.k_factor_input = evt.value();
                                if let Ok(val) = evt.value().parse::<f32>() {
                                    s.options.k_factor = val;
                                }
                            }
                        },
                    }
                }

                // Fixed threshold + Heavy model checkboxes
                div {
                    style: "margin-top: 8px; display: flex; gap: 12px; align-items: center;",
                    label {
                        class: "radio-label",
                        title: "Use a fixed 0-255 threshold after linearization.",
                        input {
                            r#type: "checkbox",
                            checked: use_threshold,
                            onchange: move |evt| {
                                let mut s = state.write();
                                let enabled = evt.checked();
                                s.options.use_fixed_threshold = enabled;
                                if enabled {
                                    s.options.use_heavy_binarization = false;
                                }
                            }
                        }
                        span { "Fixed threshold" }
                    }
                    label {
                        class: "radio-label",
                        title: "Neural model trained for degraded and historical documents. Slow.",
                        input {
                            r#type: "checkbox",
                            checked: state.read().options.use_heavy_binarization,
                            disabled: use_threshold,
                            onchange: move |evt| {
                                state.write().options.use_heavy_binarization = evt.checked();
                            }
                        }
                        span { "Heavy model" }
                    }
                }
            } else {
                // Layout detection is off — only fixed threshold is meaningful
                div {
                    style: "font-weight: 600; margin-bottom: 6px;",
                    title: "Adaptive binarization is unavailable without layout detection",
                    "Fixed threshold binarization"
                }
                div {
                    style: "font-size: 11px; color: var(--muted-text, #888); margin-bottom: 8px; line-height: 1.4;",
                    "Adaptive and heavy binarization are disabled \u{2014} image regions on the full page skew adaptive algorithms."
                }

                // Threshold value input (always shown, always fixed mode)
                div { class: "setting-item",
                    label {
                        class: "setting-label",
                        title: "{GUI_TEXT.interactive.tooltips.threshold_value}",
                        "Threshold:"
                    }
                    input {
                        class: "setting-input",
                        r#type: "number",
                        step: "1",
                        min: "0",
                        max: "255",
                        value: "{state.read().threshold_input}",
                        oninput: move |evt| {
                            let mut s = state.write();
                            s.threshold_input = evt.value();
                            if let Ok(val) = evt.value().parse::<u16>() {
                                s.options.threshold_value = val.min(255) as u8;
                            }
                        },
                    }
                }
            }
        }
    }
}

#[component]
fn StatusBar(state: Signal<AppState>) -> Element {
    // Access the desktop window context for platform-specific operations
    let _window = dioxus_desktop::use_window();
    let eta_text = state.read().active_eta.clone();

    // Extract queue and viewer state from signal
    let (queue_len, show_queue_viewer, queue_preview) = {
        let s = state.read();
        (
            s.queue.len(),
            s.show_queue_viewer,
            s.queue.iter().cloned().collect::<Vec<_>>(),
        )
    };

    rsx! {
        div { class: "status-bar-root",
            style: "background: var(--panel-bg, #ffffff); border-top: 1px solid var(--outline-color, #808080); padding: 10px 16px; min-height: 68px;",
            div {
                style: "display: flex; justify-content: space-between; align-items: flex-start; height: 100%;",

                // Left side: Status message and progress (fixed layout to prevent shifting)
                div {
                    style: "display: flex; flex-direction: column; gap: 4px; flex: 1; min-height: 42px;",
                    // Three-line status; first line shows spinner when processing
                    div {
                        class: "text-on-paper",
                        style: "font-size: 13px; min-height: 18px; display: flex; align-items: center; gap: 8px;",
                        if state.read().is_processing {
                            svg {
                                width: "14",
                                height: "14",
                                view_box: "0 0 50 50",
                                style: "display: inline-block;",
                                circle {
                                    cx: "25",
                                    cy: "25",
                                    r: "20",
                                    stroke: "#e0e0e0",
                                    stroke_width: "5",
                                    fill: "none"
                                }
                                path {
                                    d: "M25 5 A 20 20 0 0 1 45 25",
                                    stroke: "#2c214a",
                                    stroke_width: "5",
                                    fill: "none",
                                    stroke_linecap: "round",
                                    style: "transform-origin: 25px 25px; animation: spin 2.5s linear infinite;"
                                }
                            }
                        }
                        // Line 1
                        span { "{state.read().status_lines.0}" }
                    }
            div { class: "text-on-paper", style: "font-size: 14px; min-height: 20px;", "{state.read().status_lines.1}" }
            div { class: "text-on-paper", style: "font-size: 14px; min-height: 20px;", "{state.read().status_lines.2}" }
                    // Line 4: Multi-file progress (only shown when not empty)
                    if !state.read().status_lines.3.is_empty() {
                        div { class: "text-on-paper", style: "font-size: 13px; min-height: 18px; opacity: 0.85;", "{state.read().status_lines.3}" }
                    }
                    if eta_text.is_some() {
                        div {
                            class: "text-on-paper",
                            style: "font-size: 11px; min-height: 14px; opacity: 0.75;",
                            "ETA - {eta_text.clone().unwrap()}"
                        }
                    }
                    // Progress bar removed: percent-based progress is no longer displayed
                }

                // Right side: Queue info, Log button, and Licenses/Donate buttons (fixed position)
                div {
                    style: "display: flex; flex-direction: column; align-items: flex-end; gap: 4px; min-height: 60px; justify-content: space-between;",
                    div {
                        style: "position: relative; width: 100%; display: flex; justify-content: flex-end;",
                        button {
                            class: "raised-btn",
                            style: "padding: 4px 10px; font-size: 11px;",
                            onclick: move |_| {
                                let mut s = state.write();
                                s.show_queue_viewer = !s.show_queue_viewer;
                            },
                            "Queue: {queue_len} file(s)"
                        }
                        if show_queue_viewer {
                            div {
                                style: "position: fixed; top: 50%; left: 50%; transform: translate(-50%, -50%); background: rgba(255,255,255,0.96); border: 1px solid #d9d3f2; border-radius: 8px; box-shadow: 0 10px 24px rgba(0,0,0,0.15); padding: 12px; min-width: 320px; max-width: 420px; max-height: 400px; overflow-y: auto; z-index: 200;",
                                div {
                                    style: "display: flex; justify-content: space-between; align-items: center; margin-bottom: 8px;",
                                    span { style: "font-weight: 600; font-size: 12px; text-transform: uppercase; letter-spacing: 0.04em;", "Queue Items" }
                                    button {
                                        class: "raised-btn danger",
                                        style: "padding: 2px 6px; font-size: 11px;",
                                        onclick: move |_| {
                                            state.write().show_queue_viewer = false;
                                        },
                                        "Close"
                                    }
                                }
                                if queue_preview.is_empty() {
                                    div { style: "font-size: 12px; color: #555;", "Queue is empty." }
                                } else {
                                    for (idx, item) in queue_preview.iter().enumerate() {
                                        div {
                                            key: "{item.id}",
                                            style: "display: flex; flex-direction: column; gap: 2px; padding: 6px 0; border-bottom: 1px solid rgba(0,0,0,0.07);",
                                            span { style: "font-size: 12px; font-weight: 600;", "{idx + 1}. {item.file_name}" }
                                            span { style: "font-size: 11px; color: #555; word-break: break-word;", "{item.file_path.display()}" }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Log button row (right-aligned); version text removed
                    div {
                        style: "width: 100%; display: flex; justify-content: flex-end; align-items: center; gap: 8px; margin-top: 4px;",
                        button {
                            class: "raised-btn",
                            style: "padding: 4px 8px; font-size: 11px; white-space: nowrap;",
                            onclick: move |_| {
                                state.write().show_log_viewer = true;
                            },
                            "Log"
                        }
                    }

                    // Third row: Licenses and Donate buttons (with gap above for separation)
                    div {
                        style: "display: flex; gap: 4px; align-items: center;",

                        // Custom style for outline buttons
                        style { "
                            .outline-btn {{
                                background: transparent;
                                border: 1px solid #333;
                                color: #333;
                                padding: 4px 8px;
                                font-size: 11px;
                                border-radius: 4px;
                                cursor: pointer;
                                transition: background-color 0.15s ease;
                            }}
                            .outline-btn:active {{
                                background-color: rgba(44, 33, 74, 0.1);
                            }}
                            "
                        }

                        button {
                            class: "outline-btn",
                            onclick: move |_| {
                                let mut s = state.write();
                                s.show_documentation = true;
                                s.show_licenses = false;
                                if s.documentation_content.is_empty() {
                                    s.documentation_content =
                                        include_str!("../../../docs/Documentation.md").to_string();
                                }
                            },
                            "Documentation"
                        }

                        button {
                            class: "outline-btn",
                            onclick: move |_| {
                                let mut s = state.write();
                                s.show_licenses = true;
                                s.show_documentation = false;
                                // Load licenses content if not already loaded
                                if s.licenses_content.is_empty() {
                                    s.licenses_content =
                                        include_str!("../../../docs/THIRD-PARTY-LICENSES.md").to_string();
                                }
                            },
                            "Licenses"
                        }

                        button {
                            class: "outline-btn",
                            onclick: move |_| {
                                let mut s = state.write();
                                s.show_about = true;
                                s.show_documentation = false;
                                s.show_licenses = false;
                            },
                            "About"
                        }
                    }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct ThemedDropdownProps {
    options: Vec<(String, String)>, // (value, title)
    selected: String,
    #[props(default)]
    width: String,
    #[props(default)]
    offset_left: i32,
    #[props(default)]
    title: String,
    on_change: EventHandler<String>,
}

fn sanitize_dropdown_label(value: &str) -> String {
    const STRIP_CHARS: [char; 6] = [
        '\u{200B}', // zero width space
        '\u{200C}', // zero width non-joiner
        '\u{200D}', // zero width joiner
        '\u{200E}', // left-to-right mark
        '\u{200F}', // right-to-left mark
        '\u{202A}', // left-to-right embedding
    ];

    value
        .chars()
        .filter(|c| !c.is_control() && !STRIP_CHARS.contains(c))
        .collect::<String>()
        .trim()
        .to_string()
}

#[component]
fn ThemedDropdown(props: ThemedDropdownProps) -> Element {
    let mut is_open = use_signal(|| false);

    let raw_selected = props.selected.clone();
    let selected_display = sanitize_dropdown_label(&raw_selected);
    let current_label = props
        .options
        .iter()
        .find(|(v, _)| *v == props.selected)
        .map(|(v, _)| sanitize_dropdown_label(v))
        .unwrap_or_else(|| selected_display.clone());

    let options = props.options.clone();
    let selected_value = selected_display.clone();
    let change_handler = props.on_change.clone();
    let is_open_signal = is_open.clone();

    let option_nodes = options
        .into_iter()
        .enumerate()
        .map(|(idx, (value, opt_title))| {
            let display_value = sanitize_dropdown_label(&value);
            let callback_value = value.clone();
            let title_text = opt_title.clone();
            let is_selected = display_value == selected_value;
            let change_handler = change_handler.clone();
            let mut is_open_signal = is_open_signal.clone();
            rsx! {
                div {
                    key: "opt-{idx}",
                    class: "themed-select-option",
                    aria_selected: "{is_selected}",
                    title: "{title_text}",
                    onclick: move |_| {
                        change_handler.call(callback_value.clone());
                        is_open_signal.set(false);
                    },
                    "{display_value}"
                }
            }
        })
        .collect::<Vec<_>>();

    rsx! {
        div { class: "themed-select",
            style: "min-width: {props.width}; margin-left: {props.offset_left}px;",
            title: "{props.title}",
            div { class: "themed-select-display",
                onclick: move |_| { is_open.set(!is_open()); },
                "{current_label}"
            }
            if is_open() {
                div { class: "themed-select-list",
                    { option_nodes.into_iter() }
                }
            }
        }
    }
}

#[component]
fn PostProcessingPopup(state: Signal<AppState>) -> Element {
    let should_show = state.read().show_post_processing_popup;
    let result = state.read().processing_result.as_ref().cloned();

    if should_show {
        if let Some(result) = result {
            // Auto-hide after 5 seconds and clear cached result to avoid stale popups
            {
                let mut state_clone = state.clone();
                let output_token = result.output_path.clone();
                spawn(async move {
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    let mut s = state_clone.write();
                    if let Some(current) = s.processing_result.as_ref() {
                        if current.output_path == output_token {
                            s.show_post_processing_popup = false;
                            s.processing_result = None;
                        }
                    }
                });
            }

            let input_path_display = result.input_path.display().to_string();
            let output_path_display = result.output_path.display().to_string();
            let original_size = result.original_size;
            let compressed_size = result.compressed_size;
            let reduction_percent = if original_size > 0 {
                (1.0 - (compressed_size as f64 / original_size as f64)) * 100.0
            } else {
                0.0
            };
            let formatted_original = LogEntry::format_size(original_size);
            let formatted_compressed = LogEntry::format_size(compressed_size);
            let show_reduction = !result.page_range_used;
            let reduction_text = if show_reduction {
                if reduction_percent >= 0.0 {
                    format!(
                        "File size reduced by {:.1}% ({} → {})",
                        reduction_percent, formatted_original, formatted_compressed
                    )
                } else {
                    format!(
                        "File size increased by {:.1}% ({} → {})",
                        reduction_percent.abs(),
                        formatted_original,
                        formatted_compressed
                    )
                }
            } else {
                String::new()
            };

            rsx! {
                div {
                    style: "position: fixed; bottom: 20px; right: 20px; width: 480px; background: var(--info-bg, #ffffe1); color: var(--info-text, #333); border: 1px solid var(--outline-color, #808080); border-radius: 8px; z-index: 1000; display: flex; flex-direction: column; animation: slideIn 0.3s ease-out;",

                    // Header
                    div {
                        style: "padding: 12px 16px; border-bottom: 1px solid var(--outline-color, #808080); display: flex; justify-content: space-between; align-items: center; border-radius: 8px 8px 0 0;",

                        div {
                            style: "font-weight: 600; font-size: 14px;",
                            "Processing Complete"
                        }

                        button {
                            style: "background: transparent; border: none; color: inherit; font-size: 18px; cursor: pointer; width: 24px; height: 24px; display: flex; align-items: center; justify-content: center; padding: 0;",
                            onclick: move |_| {
                                let mut s = state.write();
                                s.show_post_processing_popup = false;
                                s.processing_result = None;
                            },
                            "×"
                        }
                    }

                    // Content
                    div {
                        style: "padding: 16px; display: flex; flex-direction: column; gap: 16px;",

                        // Input file section
                        div {
                            div {
                                style: "font-size: 12px; opacity: 0.85; margin-bottom: 4px;",
                                "Input File"
                            }

                            div {
                                style: "font-size: 13px; font-weight: 500; word-break: break-all; margin-bottom: 4px;",
                                "{result.input_filename}"
                            }

                            div {
                                style: "font-size: 11px; opacity: 0.7; word-break: break-all;",
                                "{input_path_display}"
                            }
                        }

                        // Output file section
                        div {
                            div {
                                style: "font-size: 12px; opacity: 0.85; margin-bottom: 4px;",
                                "Output File"
                            }

                            div {
                                style: "font-size: 13px; font-weight: 500; word-break: break-all; margin-bottom: 4px;",
                                "{result.output_filename}"
                            }

                            div {
                                style: "font-size: 11px; opacity: 0.7; word-break: break-all;",
                                "{output_path_display}"
                            }
                        }

                        if show_reduction {
                            div {
                                style: "padding: 10px 12px; background: var(--panel-bg, #ffffff); border-radius: 4px; color: var(--text-color, #333);",
                                div {
                                    style: "font-weight: 600; font-size: 13px;",
                                    "{reduction_text}"
                                }
                            }
                        } else {
                            div {
                                style: "padding: 10px 12px; background: var(--panel-bg, #ffffff); border-radius: 4px; color: var(--text-color, #333);",
                                div {
                                    style: "font-size: 12px;",
                                    "Page range output – compression metrics omitted."
                                }
                            }
                        }

                        // View in folder button
                        div {
                            style: "text-align: right;",
                            button {
                                class: "raised-btn",
                                style: "padding: 6px 12px; font-size: 12px;",
                                onclick: move |_| {
                                    let output_path = result.output_path.clone();
                                    spawn(async move {
                                        if let Err(e) = backend::open_folder_in_explorer(&output_path) {
                                            eprintln!("Failed to open folder: {}", e);
                                        }
                                    });
                                },
                                "View in Folder"
                            }
                        }
                    }
                }
            }
        } else {
            rsx! { div {} }
        }
    } else {
        rsx! { div {} }
    }
}

#[component]
fn LogViewerPopup(state: Signal<AppState>) -> Element {
    let recent_entries: Vec<_> = {
        let state_ref = state.read();
        state_ref
            .processing_log
            .iter()
            .rev()
            .take(20)
            .cloned()
            .collect()
    };

    rsx! {
        div {
            style: "position: fixed; bottom: 12px; right: 12px; width: 80%; max-width: 800px; height: 60%; background: white; border: 1px solid #e0e0e0; border-radius: 8px; box-shadow: 0 6px 18px rgba(0,0,0,0.15); z-index: 1000; display: flex; flex-direction: column;",

            div {
                style: "flex: 1; overflow-y: auto; padding: 16px;",

                if recent_entries.is_empty() {
                    div {
                        style: "text-align: center; color: #666; font-style: italic; margin-top: 50px;",
                        "No processing history available"
                    }
                } else {
                    div {
                        style: "display: grid; grid-template-columns: 140px 2fr 2fr 1fr 1fr; gap: 8px; font-size: 12px; padding: 8px; border-bottom: 2px solid #e0e0e0; background: #f8f9fa;",
                        div { "Date/Time" }
                        div { "Input File" }
                        div { "Output File" }
                        div { "Input Size" }
                        div { "Output Size" }
                    }

                    for entry in recent_entries.iter() {
                        div {
                            style: "display: grid; grid-template-columns: 140px 2fr 2fr 1fr 1fr; gap: 8px; font-size: 11px; padding: 8px; border-bottom: 1px solid #e0e0e0; hover:background-color: #f8f9fa;",

                            div {
                                style: "color: #666;",
                                "{entry.format_timestamp()}"
                            }
                            div {
                                style: "word-break: break-all;",
                                title: "{entry.input_path}",
                                "{entry.input_filename}"
                            }
                            div {
                                style: "word-break: break-all;",
                                title: "{entry.output_path}",
                                "{entry.output_filename}"
                            }
                            div {
                                style: "font-family: monospace; text-align: right; padding-right: 8px;",
                                "{LogEntry::format_size(entry.original_size)}"
                            }
                            div {
                                style: "font-family: monospace; text-align: right; padding-right: 8px; font-weight: 500;",
                                "{LogEntry::format_size(entry.compressed_size)}"
                            }
                        }
                    }
                }
            }

            div {
                style: "padding: 12px; border-top: 1px solid #e0e0e0; background: #f8f9fa; text-align: center; font-size: 12px; color: #666;",
                "Showing last {recent_entries.len()} entries (total: {state.read().processing_log.len()})"
            }

            // Close button (no title bar)
            button {
                style: "position: absolute; top: 8px; right: 8px; background: transparent; border: none; color: #666; font-size: 18px; cursor: pointer; width: 24px; height: 24px; display: flex; align-items: center; justify-content: center;",
                onclick: move |_| {
                    state.write().show_log_viewer = false;
                },
                "×"
            }
        }
    }
}

#[cfg(feature = "debug-logging")]
#[component]
fn DebugLogViewerPopup(state: Signal<AppState>) -> Element {
    let recent_messages: Vec<_> = {
        let state_ref = state.read();
        state_ref
            .debug_log_messages
            .iter()
            .rev()
            .take(100)
            .cloned()
            .collect()
    };

    rsx! {
        div {
            style: "position: fixed; bottom: 12px; left: 12px; width: 70%; max-width: 900px; height: 70%; background: white; border: 1px solid #e0e0e0; border-radius: 8px; box-shadow: 0 6px 18px rgba(0,0,0,0.15); z-index: 1000; display: flex; flex-direction: column;",

            // Header
            div {
                style: "padding: 12px 16px; border-bottom: 1px solid #e0e0e0; background: #f8f9fa; display: flex; justify-content: space-between; align-items: center; border-radius: 8px 8px 0 0;",

                div {
                    style: "font-weight: 600; font-size: 14px; color: #333;",
                    "Debug Log (Encoding Operations)"
                }

                div {
                    style: "display: flex; gap: 8px; align-items: center;",

                    button {
                        style: "background: #f0f0f0; border: 1px solid #d0d0d0; border-radius: 4px; padding: 4px 8px; font-size: 11px; cursor: pointer; color: #333;",
                        onclick: move |_| {
                            // Refresh from file each time
                            state.write().debug_log_messages = load_debug_log_file();
                        },
                        "Refresh"
                    }

                    button {
                        style: "background: #f0f0f0; border: 1px solid #d0d0d0; border-radius: 4px; padding: 4px 8px; font-size: 11px; cursor: pointer; color: #333;",
                        onclick: move |_| {
                            // Clear both in-memory (legacy) and file for consistency
                            Legencode::clear_debug_log();
                            if let Err(e) = std::fs::write("debug.log", "") {
                                dbglog!("Failed to truncate debug.log: {}", e);
                            }
                            state.write().debug_log_messages.clear();
                        },
                        "Clear"
                    }

                    button {
                        style: "background: transparent; border: none; color: #666; font-size: 18px; cursor: pointer; width: 24px; height: 24px; display: flex; align-items: center; justify-content: center; padding: 0;",
                        onclick: move |_| {
                            state.write().show_debug_log = false;
                        },
                        "×"
                    }
                }
            }

            // Content
            div {
                style: "flex: 1; overflow-y: auto; padding: 16px; font-family: 'Consolas', 'Monaco', monospace;",

                if recent_messages.is_empty() {
                    div {
                        style: "text-align: center; color: #666; font-style: italic; margin-top: 50px;",
                        "No debug messages available\nDebug logging is enabled. Messages will appear here during encoding operations."
                    }
                } else {
                    for message in recent_messages.iter() {
                        div {
                            style: "font-size: 11px; line-height: 1.4; margin-bottom: 4px; padding: 2px 4px; border-radius: 2px; background: #f8f9fa;",
                            "{message}"
                        }
                    }
                }
            }

            // Footer
            div {
                style: "padding: 8px 12px; border-top: 1px solid #e0e0e0; background: #f8f9fa; text-align: center; font-size: 11px; color: #666;",
                "Showing last {recent_messages.len()} messages • Auto-refreshes on toggle"
            }
        }
    }
}

#[component]
fn DocumentationWindow(state: Signal<AppState>) -> Element {
    let content = state.read().documentation_content.clone();

    let html_content = convert_markdown_to_html(&content);

    rsx! {
        div {
            style: "position: fixed; top: 50%; left: 50%; transform: translate(-50%, -50%); width: 600px; height: 500px; background: white; border: 1px solid #e0e0e0; border-radius: 8px; box-shadow: 0 6px 18px rgba(0,0,0,0.15); z-index: 1000; display: flex; flex-direction: column;",

            // Header
            div {
                style: "padding: 12px 16px; border-bottom: 1px solid #e0e0e0; background: #f8f9fa; display: flex; justify-content: space-between; align-items: center; border-radius: 8px 8px 0 0;",

                div {
                    style: "font-weight: 600; font-size: 14px; color: #333;",
                    "Documentation"
                }

                button {
                    style: "background: transparent; border: none; color: #666; font-size: 18px; cursor: pointer; width: 24px; height: 24px; display: flex; align-items: center; justify-content: center; padding: 0;",
                    onclick: move |_| {
                        let mut s = state.write();
                        s.show_documentation = false;
                    },
                    "x"
                }
            }

            div {
                style: "flex: 1; overflow-y: auto; padding: 16px; line-height: 1.6;",
                dangerous_inner_html: "{html_content}"
            }
        }
    }
}

#[component]
fn LicensesWindow(state: Signal<AppState>) -> Element {
    let content = state.read().licenses_content.clone();

    // Convert markdown to structured HTML
    let html_content = convert_markdown_to_html(&content);

    rsx! {
        div {
            style: "position: fixed; top: 50%; left: 50%; transform: translate(-50%, -50%); width: 600px; height: 500px; background: white; border: 1px solid #e0e0e0; border-radius: 8px; box-shadow: 0 6px 18px rgba(0,0,0,0.15); z-index: 1000; display: flex; flex-direction: column;",

            // Header
            div {
                style: "padding: 12px 16px; border-bottom: 1px solid #e0e0e0; background: #f8f9fa; display: flex; justify-content: space-between; align-items: center; border-radius: 8px 8px 0 0;",

                div {
                    style: "font-weight: 600; font-size: 14px; color: #333;",
                    "Third-Party Licenses"
                }

                button {
                    style: "background: transparent; border: none; color: #666; font-size: 18px; cursor: pointer; width: 24px; height: 24px; display: flex; align-items: center; justify-content: center; padding: 0;",
                    onclick: move |_| {
                        state.write().show_licenses = false;
                    },
                    "×"
                }
            }

            // Content - scrollable with proper HTML rendering
            div {
                style: "flex: 1; overflow-y: auto; padding: 16px; line-height: 1.6;",
                dangerous_inner_html: "{html_content}"
            }
        }
    }
}

#[component]
fn AboutWindow(state: Signal<AppState>) -> Element {
    rsx! {
        div {
            style: "position: fixed; top: 50%; left: 50%; transform: translate(-50%, -50%); width: 420px; background: white; border: 1px solid #e0e0e0; border-radius: 8px; box-shadow: 0 6px 18px rgba(0,0,0,0.15); z-index: 1000; display: flex; flex-direction: column;",

            // Header
            div {
                style: "padding: 12px 16px; border-bottom: 1px solid #e0e0e0; background: #f8f9fa; display: flex; justify-content: space-between; align-items: center; border-radius: 8px 8px 0 0;",

                div {
                    style: "font-weight: 600; font-size: 14px; color: #333;",
                    "About Lege"
                }

                button {
                    style: "background: transparent; border: none; color: #666; font-size: 18px; cursor: pointer; width: 24px; height: 24px; display: flex; align-items: center; justify-content: center; padding: 0;",
                    onclick: move |_| {
                        state.write().show_about = false;
                    },
                    "×"
                }
            }

            // Content
            div {
                style: "padding: 16px; display: flex; flex-direction: column; gap: 10px;",

                div { class: "text-on-paper", style: "font-size: 13px;", "Version: v{display_version()}" }

                div { class: "text-on-paper", style: "font-size: 13px;",
                    span { "Contact: " }
                    a { href: "mailto:read@legeapp.com", style: "color: #1a5fb4;", "read@legeapp.com" }
                }
            }
        }
    }
}

fn convert_markdown_to_html(markdown: &str) -> String {
    let mut html = String::new();
    html.push_str(
        "<style>\
            .md-root { font-family: 'Segoe UI', Tahoma, sans-serif; font-size: 13px; line-height: 1.6; color: #222; }\
            .md-root h1 { font-size: 20px; font-weight: 600; margin: 18px 0 10px; }\
            .md-root h2 { font-size: 17px; font-weight: 600; margin: 16px 0 8px; }\
            .md-root h3 { font-size: 15px; font-weight: 600; margin: 14px 0 6px; }\
            .md-root p { margin: 8px 0; }\
            .md-root ul, .md-root ol { margin: 8px 0 8px 22px; padding: 0; }\
            .md-root li { margin: 4px 0; }\
            .md-root code { background: #f3f4f6; border-radius: 3px; padding: 0 4px; font-family: 'Consolas','Monaco',monospace; font-size: 12px; }\
            .md-root pre { background: #f3f4f6; border-radius: 6px; padding: 12px; margin: 12px 0; overflow-x: auto; font-family: 'Consolas','Monaco',monospace; font-size: 12px; }\
            .md-root a { color: #1a5fb4; text-decoration: none; }\
            .md-root a:hover { text-decoration: underline; }\
        </style><div class='md-root'>",
    );

    let mut in_code_block = false;
    let mut in_unordered = false;
    let mut in_ordered = false;

    for line in markdown.lines() {
        let trimmed_line = line.trim_end();
        let collapsed = trimmed_line.trim();

        if trimmed_line.starts_with("```") {
            close_lists(&mut html, &mut in_unordered, &mut in_ordered);
            if in_code_block {
                html.push_str("</code></pre>");
                in_code_block = false;
            } else {
                html.push_str("<pre><code>");
                in_code_block = true;
            }
            continue;
        }

        if in_code_block {
            html.push_str(&escape_html(trimmed_line));
            html.push('\n');
            continue;
        }

        if collapsed.is_empty() {
            close_lists(&mut html, &mut in_unordered, &mut in_ordered);
            html.push_str("<div style='height: 8px;'></div>");
            continue;
        }

        if let Some(rest) = collapsed.strip_prefix("# ") {
            close_lists(&mut html, &mut in_unordered, &mut in_ordered);
            html.push_str(&format!("<h1>{}</h1>", render_inline_text(rest)));
        } else if let Some(rest) = collapsed.strip_prefix("## ") {
            close_lists(&mut html, &mut in_unordered, &mut in_ordered);
            html.push_str(&format!("<h2>{}</h2>", render_inline_text(rest)));
        } else if let Some(rest) = collapsed.strip_prefix("### ") {
            close_lists(&mut html, &mut in_unordered, &mut in_ordered);
            html.push_str(&format!("<h3>{}</h3>", render_inline_text(rest)));
        } else if let Some(rest) = collapsed
            .strip_prefix("- ")
            .or_else(|| collapsed.strip_prefix("* "))
            .or_else(|| collapsed.strip_prefix("+ "))
        {
            if !in_unordered {
                close_lists(&mut html, &mut in_unordered, &mut in_ordered);
                html.push_str("<ul>");
                in_unordered = true;
            }
            html.push_str(&format!("<li>{}</li>", render_inline_text(rest)));
        } else if let Some((number_part, rest)) = parse_ordered_list_item(collapsed) {
            if !in_ordered {
                close_lists(&mut html, &mut in_unordered, &mut in_ordered);
                html.push_str("<ol>");
                in_ordered = true;
            }
            html.push_str(&format!(
                "<li value='{}'>{}</li>",
                number_part,
                render_inline_text(rest)
            ));
        } else {
            close_lists(&mut html, &mut in_unordered, &mut in_ordered);
            html.push_str(&format!("<p>{}</p>", render_inline_text(collapsed)));
        }
    }

    if in_code_block {
        html.push_str("</code></pre>");
    }
    close_lists(&mut html, &mut in_unordered, &mut in_ordered);
    html.push_str("</div>");
    html
}

fn close_lists(html: &mut String, in_unordered: &mut bool, in_ordered: &mut bool) {
    if *in_unordered {
        html.push_str("</ul>");
        *in_unordered = false;
    }
    if *in_ordered {
        html.push_str("</ol>");
        *in_ordered = false;
    }
}

fn parse_ordered_list_item(line: &str) -> Option<(String, &str)> {
    let mut parts = line.splitn(2, ". ");
    let number = parts.next()?;
    let rest = parts.next()?;
    if !number.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((number.to_string(), rest))
}

fn render_inline_text(text: &str) -> String {
    let mut sanitized = text.replace("**", "");
    sanitized = sanitized.replace("__", "");
    sanitized = sanitized.replace('*', "");
    sanitized = sanitized.replace('_', "");

    let mut result = String::new();
    let mut in_code = false;
    for segment in sanitized.split('`') {
        if in_code {
            result.push_str("<code>");
            result.push_str(&escape_html(segment));
            result.push_str("</code>");
        } else {
            result.push_str(&linkify_text(segment));
        }
        in_code = !in_code;
    }

    result
}

fn linkify_text(text: &str) -> String {
    let mut output = String::new();
    let mut remaining = text;

    while let Some(start) = remaining.find('[') {
        output.push_str(&escape_html(&remaining[..start]));
        let after_bracket = &remaining[start + 1..];
        if let Some(end_label) = after_bracket.find(']') {
            let label = &after_bracket[..end_label];
            let after_label = &after_bracket[end_label + 1..];
            if let Some(url) = after_label.strip_prefix('(') {
                if let Some(end_url) = url.find(')') {
                    let href = &url[..end_url];
                    output.push_str(&format!(
                        "<a href='{0}'>{1}</a>",
                        escape_html(href),
                        escape_html(label)
                    ));
                    remaining = &url[end_url + 1..];
                    continue;
                }
            }
        }
        // No proper link structure; emit the remainder escaped.
        output.push_str(&escape_html(&remaining[start..]));
        return output;
    }

    output.push_str(&escape_html(remaining));
    output
}

fn escape_html(input: &str) -> String {
    input
        .chars()
        .map(|c| match c {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '"' => "&quot;".to_string(),
            '\'' => "&#39;".to_string(),
            _ => c.to_string(),
        })
        .collect()
}
