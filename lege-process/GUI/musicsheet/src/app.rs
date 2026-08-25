use freya::prelude::*;
use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;

use crate::backend;
use crate::gui_text::GUI_TEXT;
use crate::logging;
use crate::logging::{LogEntry, ProcessingResult};
use crate::models::{
    DEFAULT_MUSIC_TARGET_HEIGHT, DocumentItem, OutputFormat, ProcessingOptions, ResolutionPreset,
};
use crate::version::display_version;
use crate::widgets;
use crate::worker_process::{WorkerProcessingStatus, WorkerProgressUpdate};

use crate::appearance::{self, Rgb};
use crate::colors::{
    active_bg, app_bg, border, border_focus, card_bg, control_bg, focus_bg, hover_bg, info_bg,
    inverse_surface, inverse_surface_secondary, inverse_surface_tertiary, muted_fg, panel_bg,
    progress_track_bg, selected_bg, text_fg,
};

fn rgb(color: Rgb) -> Color {
    Color::from_rgb(color.0, color.1, color.2)
}

fn rgb_shade(color: Rgb, scale: f32) -> Color {
    Color::from_rgb(
        (color.0 as f32 * scale).round().clamp(0.0, 255.0) as u8,
        (color.1 as f32 * scale).round().clamp(0.0, 255.0) as u8,
        (color.2 as f32 * scale).round().clamp(0.0, 255.0) as u8,
    )
}

fn retro_theme() -> Theme {
    let mut theme = light_theme();
    theme.name = "lege-retro";
    theme.colors = ColorsSheet {
        primary: rgb(selected_bg()),
        secondary: rgb(focus_bg()),
        tertiary: rgb_shade(selected_bg(), 0.88),
        info: rgb(info_bg()),
        background: rgb(app_bg()),
        surface_primary: rgb(app_bg()),
        surface_secondary: rgb(panel_bg()),
        surface_tertiary: rgb(control_bg()),
        surface_inverse: rgb(inverse_surface()),
        surface_inverse_secondary: rgb(inverse_surface_secondary()),
        surface_inverse_tertiary: rgb(inverse_surface_tertiary()),
        border: rgb(border()),
        border_focus: rgb(border_focus()),
        text_primary: rgb(text_fg()),
        text_secondary: rgb(muted_fg()),
        text_placeholder: rgb(muted_fg()),
        text_inverse: rgb(text_fg()),
        text_highlight: rgb(text_fg()),
        hover: rgb(hover_bg()),
        focus: rgb(focus_bg()),
        active: rgb(active_bg()),
        disabled: rgb(active_bg()),
        ..LIGHT_COLORS
    };
    theme
}

fn refresh_appearance_theme(mut theme: State<Theme>) {
    // The theme is fixed by the user's committed choice; re-apply it so any external
    // change (settings load / reset) is reflected in the live Freya theme.
    theme.set(retro_theme());
}

// ---- Theme picker -----------------------------------------------------------
// Opened from the About popup ("Theme" button, which leaves About open). Selection is a
// single wrapped index over two rows so browsing never flips mode mid-row:
//   0                 = Default
//   1 ..= N           = the light row  (Sanzo Light 1..N)
//   N+1 ..= 2N        = the dark row   (Sanzo Dark 1..N)
// ←/→ walk this ring (all lights, then all darks; the seam wraps last-dark -> Default ->
// first-light and first-light <- Default <- last-dark). ↑/↓ jump a combo between its
// light and dark counterpart (same colors, different usage). Enter accepts (persists) and
// closes; Esc cancels (reverts) and closes. Every selection is previewed live.

fn theme_total() -> usize {
    2 * crate::sanzowada::THEME_COUNT + 1
}

/// The persisted choice for selection index `i`.
fn theme_choice_at(i: usize) -> appearance::ThemeChoice {
    let n = crate::sanzowada::THEME_COUNT;
    if i == 0 {
        appearance::ThemeChoice::Default
    } else if i <= n {
        appearance::ThemeChoice::Sanzo {
            index: i - 1,
            dark: false,
        }
    } else {
        appearance::ThemeChoice::Sanzo {
            index: i - n - 1,
            dark: true,
        }
    }
}

/// The curated-combo index for selection `i` (None for Default).
fn theme_combo_index(i: usize) -> Option<usize> {
    let n = crate::sanzowada::THEME_COUNT;
    if i == 0 {
        None
    } else if i <= n {
        Some(i - 1)
    } else {
        Some(i - n - 1)
    }
}

fn theme_name(i: usize) -> String {
    let n = crate::sanzowada::THEME_COUNT;
    if i == 0 {
        "Default".to_string()
    } else if i <= n {
        format!("Sanzo Light {}", i)
    } else {
        format!("Sanzo Dark {}", i - n)
    }
}

/// The selection index for a committed choice (used to open the picker where it left off).
fn index_for_choice(choice: appearance::ThemeChoice) -> usize {
    let n = crate::sanzowada::THEME_COUNT;
    match choice {
        appearance::ThemeChoice::Default => 0,
        appearance::ThemeChoice::Sanzo { index, dark } => {
            if dark {
                n + 1 + index
            } else {
                index + 1
            }
        }
    }
}

/// Preview selection `i` live (not persisted) and repaint.
fn preview_theme_at(mut theme: State<Theme>, i: usize) {
    appearance::set_theme_preview(Some(theme_choice_at(i)));
    theme.set(retro_theme());
}

/// Wrapped move by `delta` along the selection ring.
fn ring_step(i: usize, delta: isize) -> usize {
    let total = theme_total() as isize;
    (i as isize + delta).rem_euclid(total) as usize
}

/// The light (`dark=false`) or dark (`dark=true`) counterpart of selection `i`. Returns
/// `i` unchanged on Default (no dark) or when already in that row.
fn row_index(i: usize, dark: bool) -> usize {
    let n = crate::sanzowada::THEME_COUNT;
    if dark {
        if (1..=n).contains(&i) { i + n } else { i } // light -> dark
    } else if i > n {
        i - n // dark -> light
    } else {
        i
    }
}

/// Move by `delta` along the wrapped selection ring.
fn step_theme(mut sel: State<usize>, theme: State<Theme>, delta: isize) {
    let ni = ring_step(sel(), delta);
    sel.set(ni);
    preview_theme_at(theme, ni);
}

/// Jump to the light/dark counterpart of the current combo (no-op on Default).
fn set_theme_row(mut sel: State<usize>, theme: State<Theme>, dark: bool) {
    let i = sel();
    let ni = row_index(i, dark);
    if ni != i {
        sel.set(ni);
        preview_theme_at(theme, ni);
    }
}

/// Accept selection `i`: persist it, repaint and close the chooser. A failed
/// settings write still applies the theme for this session but is surfaced,
/// since it would silently revert on the next launch.
fn accept_theme_choice(mut state: State<AppState>, mut theme: State<Theme>, i: usize) {
    if let Err(err) = appearance::commit_theme_choice(theme_choice_at(i)) {
        schedule_popup(
            state,
            PopupKind::Warning,
            format!("Could not save the theme choice: {err}"),
            6,
        );
    }
    theme.set(retro_theme());
    state.write().show_theme_chooser = false;
}

/// Cancel any live preview, revert to the committed theme and close the chooser.
fn cancel_theme_preview(mut state: State<AppState>, mut theme: State<Theme>) {
    appearance::set_theme_preview(None);
    theme.set(retro_theme());
    state.write().show_theme_chooser = false;
}

fn theme_chooser(state: State<AppState>, theme: State<Theme>) -> Element {
    ThemeChooserInner {
        state,
        theme,
        init: index_for_choice(appearance::committed_theme_choice()),
    }
    .into()
}

#[derive(Clone, Copy, PartialEq)]
struct ThemeChooserInner {
    state: State<AppState>,
    theme: State<Theme>,
    init: usize,
}

impl Component for ThemeChooserInner {
    fn render(&self) -> impl IntoElement {
        let state = self.state;
        let theme = self.theme;
        let n = crate::sanzowada::THEME_COUNT;
        let sel = use_state(|| self.init.min(theme_total() - 1));

        let i = sel();
        let is_light = (1..=n).contains(&i);
        let is_dark = i > n;
        let colors = match theme_combo_index(i) {
            Some(c) => crate::sanzowada::theme_at(c)
                .map(|t| t.colors.join("  ·  "))
                .unwrap_or_default(),
            None => "The original Lege daytime theme.".to_string(),
        };

        let accept = move |_: Event<PressEventData>| accept_theme_choice(state, theme, sel());
        let cancel = move |_: Event<PressEventData>| cancel_theme_preview(state, theme);

        // A compact button-styled clickable cell (plain rect so Space/Enter don't also
        // trigger it via focus).
        let cell = |text: &'static str, width: f32, active: bool| {
            rect()
                .width(Size::px(width))
                .height(Size::px(28.0_f32))
                .background(if active { selected_bg() } else { control_bg() })
                .border(
                    Border::new()
                        .fill(border())
                        .width(1.)
                        .alignment(BorderAlignment::Inner),
                )
                .corner_radius(4.)
                .main_align(Alignment::Center)
                .cross_align(Alignment::Center)
                .child(label().text(text).font_size(13.).color(text_fg()))
        };

        rect()
            .width(Size::fill())
            .spacing(8.0_f32)
            .on_global_key_down(move |e: Event<KeyboardEventData>| match &e.key {
                Key::Named(NamedKey::ArrowLeft) => step_theme(sel, theme, -1),
                Key::Named(NamedKey::ArrowRight) => step_theme(sel, theme, 1),
                Key::Named(NamedKey::ArrowUp) => set_theme_row(sel, theme, false),
                Key::Named(NamedKey::ArrowDown) => set_theme_row(sel, theme, true),
                Key::Named(NamedKey::Enter) => accept_theme_choice(state, theme, sel()),
                Key::Named(NamedKey::Escape) => cancel_theme_preview(state, theme),
                _ => {}
            })
            .child(
                label()
                    .text(theme_name(i))
                    .font_size(18.)
                    .font_weight(700)
                    .color(text_fg()),
            )
            .child(label().text(colors).font_size(12.).color(muted_fg()))
            .child(
                rect()
                    .direction(Direction::Horizontal)
                    .spacing(6.0_f32)
                    .cross_align(Alignment::Center)
                    .child(
                        rect()
                            .on_press(move |_: Event<PressEventData>| step_theme(sel, theme, -1))
                            .child(cell("◀", 40., false)),
                    )
                    .child(
                        rect()
                            .on_press(move |_: Event<PressEventData>| step_theme(sel, theme, 1))
                            .child(cell("▶", 40., false)),
                    )
                    .child(
                        rect()
                            .on_press(move |_: Event<PressEventData>| {
                                set_theme_row(sel, theme, false)
                            })
                            .child(cell("Light", 58., is_light)),
                    )
                    .child(
                        rect()
                            .on_press(move |_: Event<PressEventData>| {
                                set_theme_row(sel, theme, true)
                            })
                            .child(cell("Dark", 58., is_dark)),
                    )
                    .child(rect().on_press(accept).child(cell("Accept", 72., false)))
                    .child(rect().on_press(cancel).child(cell("Cancel", 66., false))),
            )
            .child(
                label()
                    .text("←/→ theme   ·   ↑/↓ light-dark   ·   Enter accept   ·   Esc cancel")
                    .font_size(11.)
                    .color(muted_fg()),
            )
    }
}

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

fn fmt1(template: &str, a: impl std::fmt::Display) -> String {
    template.replacen("{}", &a.to_string(), 1)
}

fn fmt2(template: &str, a: impl std::fmt::Display, b: impl std::fmt::Display) -> String {
    fmt1(&fmt1(template, a), b)
}

fn fmt3(
    template: &str,
    a: impl std::fmt::Display,
    b: impl std::fmt::Display,
    c: impl std::fmt::Display,
) -> String {
    fmt1(&fmt2(template, a, b), c)
}

#[derive(Clone, Copy)]
enum PopupKind {
    Hardware,
    Output,
    NoLayout,
    Footnotes,
    Ocr,
    Queue,
    Complete,
    AddFileHelp,
    Warning,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum TooltipArea {
    FileActions,
    PagesDeviceCard,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum TooltipSide {
    Left,
    Right,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProcessingQueueDisplayItem {
    pub task_id: u64,
    pub path: PathBuf,
    pub status: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppState {
    pub queue: VecDeque<DocumentItem>,
    pub options: ProcessingOptions,
    pub is_processing: bool,
    pub current_processing_index: Option<usize>,
    pub status_message: String,
    pub status_lines: (String, String, String, String),
    pub status_log: VecDeque<String>,
    pub active_eta: Option<String>,
    pub progress_metrics: Option<crate::worker_process::WorkerProgressMetrics>,
    pub active_task_ids: Vec<u64>,
    pub processing_items: Vec<ProcessingQueueDisplayItem>,
    pub should_cancel: bool,

    pub target_height_input: String,
    pub k_factor_input: String,
    pub threshold_input: String,
    pub page_range_input: String,
    pub active_tooltip: Option<(TooltipSide, String)>,

    pub hardware_acceleration_popup: Option<String>,
    pub show_hardware_popup: bool,
    pub output_folder_popup: Option<String>,
    pub show_output_popup: bool,
    pub no_layout_detection_popup: Option<String>,
    pub show_no_layout_popup: bool,
    pub footnotes_popup: Option<String>,
    pub show_footnotes_popup: bool,
    pub ocr_detection_popup: Option<String>,
    pub show_ocr_detection_popup: bool,
    pub queue_operation_popup: Option<String>,
    pub show_queue_operation_popup: bool,
    pub completion_popup: Option<String>,
    pub show_completion_popup: bool,
    pub add_file_help_popup: Option<String>,
    pub show_add_file_help_popup: bool,
    pub warning_popup: Option<String>,
    pub show_warning_popup: bool,
    pub show_log_viewer: bool,
    pub processing_log: Vec<LogEntry>,
    pub show_about: bool,
    pub show_theme_chooser: bool,
    pub show_queue_viewer: bool,

    #[cfg(feature = "debug-logging")]
    pub show_debug_log: bool,
    #[cfg(feature = "debug-logging")]
    pub debug_log_messages: Vec<String>,
}

impl Default for AppState {
    fn default() -> Self {
        let defaults = ProcessingOptions::new();
        let saved = backend::load_saved_settings().ok().flatten();
        let mut options = saved.clone().unwrap_or_else(|| defaults.clone());
        // Sheet Music Edition: only PDF/DjVu output exists in the UI, so a
        // stale settings.json (e.g. written by the main edition) must not
        // smuggle EPUB in (the --music-sheet preset ignores the other stripped
        // options at the argv seam; output format also picks the output file
        // extension, so it has to be sanitized here).
        if matches!(options.output_format, OutputFormat::Epub) {
            options.output_format = OutputFormat::Pdf;
        }
        // Grayscale/MRC rendering is out of the Sheet Music Edition (bilevel
        // binarization only). A stale settings.json (e.g. written by the main
        // edition) must not re-enable it — the binarization controls would have
        // no Grayscale toggle to turn it back off, and the worker never emits
        // --grayscale.
        options.grayscale_mode = false;
        // OCR is out of this edition too (no purpose for sheet music): there
        // is no OCR control and the worker emits no OCR flags, so a stale
        // use_ocr must not survive the load.
        options.use_ocr = false;
        // Restore the resolution preset saved by the last processing run (the
        // resolution field persists itself; there are no save/load buttons).
        if let Ok(Some(preset)) = backend::load_resolution_preset() {
            options.target_device = None;
            options.target_height = Some(preset.height);
            options.target_width = if options.crop_free_aspect {
                None
            } else {
                preset.width
            };
        }
        let _ = logging::reconcile_started_entries();
        let mut state = Self {
            queue: VecDeque::new(),
            options: options.clone(),
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
            progress_metrics: None,
            active_task_ids: Vec::new(),
            processing_items: Vec::new(),
            should_cancel: false,
            target_height_input: resolution_input_from_options(&options),
            k_factor_input: options.k_factor.to_string(),
            threshold_input: options.threshold_value.to_string(),
            page_range_input: options.page_range.clone().unwrap_or_default(),
            active_tooltip: None,
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
            completion_popup: None,
            show_completion_popup: false,
            add_file_help_popup: None,
            show_add_file_help_popup: false,
            warning_popup: None,
            show_warning_popup: false,
            show_log_viewer: false,
            processing_log: logging::load_log_entries().unwrap_or_default(),
            show_about: false,
            show_theme_chooser: false,
            show_queue_viewer: false,
            #[cfg(feature = "debug-logging")]
            show_debug_log: false,
            #[cfg(feature = "debug-logging")]
            debug_log_messages: Vec::new(),
        };

        if let Some(saved) = saved {
            sync_state_from_options(&mut state, &saved);
        }

        state
    }
}

impl AppState {
    fn set_status_message(&mut self, msg: impl Into<String>) {
        let m = msg.into();
        self.status_message = m.clone();
        self.status_lines = (m, String::new(), String::new(), String::new());
        self.progress_metrics = None;
    }

    fn set_status_lines(
        &mut self,
        l1: impl Into<String>,
        l2: impl Into<String>,
        l3: impl Into<String>,
    ) {
        let new_lines = (l1.into(), l2.into(), l3.into(), String::new());
        if self.status_lines != new_lines {
            self.status_lines = new_lines;
        }
    }

    fn push_status_log(&mut self, line: impl Into<String>) {
        let line = line.into();
        if !self.status_log.back().map(|s| s == &line).unwrap_or(false) {
            self.status_log.push_back(line);
            if self.status_log.len() > 100 {
                self.status_log.pop_front();
            }
        }

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
        self.status_lines = (l1, l2, l3, self.status_lines.3.clone());
    }

    fn set_processing_item_status(&mut self, task_id: u64, status: impl Into<String>) {
        if let Some(item) = self
            .processing_items
            .iter_mut()
            .find(|item| item.task_id == task_id)
        {
            item.status = status.into();
        }
    }
}

fn sync_state_from_options(state: &mut AppState, options: &ProcessingOptions) {
    state.options = options.clone();
    if !state.options.use_fixed_threshold && !state.options.use_heavy_binarization {
        // Legacy all-off settings still mean adaptive processing. Reflect the
        // effective mode explicitly when restoring the GUI.
        state.options.use_custom_adaptive = true;
    }
    if matches!(state.options.output_format, OutputFormat::Epub) {
        state.options.output_format = OutputFormat::Pdf;
    }
    // Grayscale and OCR are out of this edition; never let stale values
    // survive a load.
    state.options.grayscale_mode = false;
    state.options.use_ocr = false;
    state.options.target_device = None;
    state.target_height_input = resolution_input_from_options(options);
    state.k_factor_input = options.k_factor.to_string();
    state.threshold_input = options.threshold_value.to_string();
    state.page_range_input = options.page_range.clone().unwrap_or_default();
}

fn panel_card(title: impl Into<String>, fill_height: bool, children: Vec<Element>) -> Element {
    widgets::lege_panel_card(title, fill_height, children)
}

fn tooltip_wrap_side(
    mut state: State<AppState>,
    side: TooltipSide,
    text: impl Into<String>,
    child: Element,
) -> Element {
    let text = text.into();
    let show_text = text.clone();
    let clear_text = text.clone();
    let mut leave_state = state;

    rect()
        .on_pointer_over(move |_| {
            let mut s = state.write();
            if !s.is_processing {
                s.active_tooltip = Some((side, show_text.clone()));
            }
        })
        .on_pointer_out(move |_| {
            let mut s = leave_state.write();
            if s.active_tooltip.as_ref().map(|(_, text)| text.as_str()) == Some(clear_text.as_str())
            {
                s.active_tooltip = None;
            }
        })
        .child(child)
        .into()
}

fn tooltip_wrap_at(
    state: State<AppState>,
    area: TooltipArea,
    text: impl Into<String>,
    position: AttachedPosition,
    child: Element,
) -> Element {
    let side = match position {
        AttachedPosition::Left => TooltipSide::Left,
        AttachedPosition::Right => TooltipSide::Right,
        AttachedPosition::Top | AttachedPosition::Bottom => match area {
            TooltipArea::FileActions => TooltipSide::Right,
            TooltipArea::PagesDeviceCard => TooltipSide::Left,
        },
    };
    tooltip_wrap_side(state, side, text, child)
}

fn settings_row(label_text: impl Into<String>, control: Element) -> Element {
    widgets::lege_field(label_text, control)
}

fn compact_checkbox_row(
    text: impl Into<String>,
    selected: bool,
    mut on_select: impl FnMut(()) + 'static,
) -> Element {
    widgets::lege_checkbox_row(text, selected, move |_| on_select(()))
}

fn popup_message_entries(state: State<AppState>) -> Vec<(String, PopupKind)> {
    let read = state.read();
    let mut entries_out = Vec::new();

    let entries = [
        (
            read.show_hardware_popup,
            read.hardware_acceleration_popup.clone(),
            PopupKind::Hardware,
        ),
        (
            read.show_output_popup,
            read.output_folder_popup.clone(),
            PopupKind::Output,
        ),
        (
            read.show_no_layout_popup,
            read.no_layout_detection_popup.clone(),
            PopupKind::NoLayout,
        ),
        (
            read.show_footnotes_popup,
            read.footnotes_popup.clone(),
            PopupKind::Footnotes,
        ),
        (
            read.show_ocr_detection_popup,
            read.ocr_detection_popup.clone(),
            PopupKind::Ocr,
        ),
        (
            read.show_queue_operation_popup,
            read.queue_operation_popup.clone(),
            PopupKind::Queue,
        ),
        (
            read.show_completion_popup,
            read.completion_popup.clone(),
            PopupKind::Complete,
        ),
        (
            read.show_add_file_help_popup,
            read.add_file_help_popup.clone(),
            PopupKind::AddFileHelp,
        ),
        (
            read.show_warning_popup,
            read.warning_popup.clone(),
            PopupKind::Warning,
        ),
    ];

    for (show, msg, kind) in entries {
        if show {
            if let Some(msg) = msg {
                entries_out.push((msg, kind));
            }
        }
    }

    entries_out.into_iter().take(2).collect()
}

fn rail_note_card(text: impl Into<String>, font_size: f32, background: (u8, u8, u8)) -> Element {
    let text = text.into();
    rect()
        .background(background)
        .border(
            Border::new()
                .fill(border())
                .width(1.)
                .alignment(BorderAlignment::Inner),
        )
        .corner_radius(6.)
        .padding((8., 10., 8., 10.))
        .width(Size::fill())
        .child(
            rect().width(Size::fill()).child(
                paragraph()
                    .width(Size::fill())
                    .span(Span::new(text).font_size(font_size).color(text_fg()))
                    .line_height(1.25)
                    .max_lines(5),
            ),
        )
        .into()
}

fn tooltip_note_card(text: impl Into<String>) -> Element {
    let text = text.into();
    rect()
        .background(panel_bg())
        .border(
            Border::new()
                .fill(border())
                .width(1.)
                .alignment(BorderAlignment::Inner),
        )
        .corner_radius(4.)
        .padding((6., 8., 6., 8.))
        .width(Size::fill())
        .child(
            paragraph()
                .width(Size::fill())
                .span(Span::new(text).font_size(13.).color(text_fg()))
                .line_height(1.3)
                .max_lines(6),
        )
        .into()
}

fn popup_rail(state: State<AppState>) -> Element {
    let entries = popup_message_entries(state);
    if entries.is_empty() {
        return rect().into();
    }

    let cards: Vec<Element> = entries
        .into_iter()
        .map(|(msg, kind)| {
            rect()
                .on_pointer_down(move |_: Event<PointerEventData>| {
                    hide_popup(state, kind);
                })
                .child(rail_note_card(msg, 13., info_bg()))
                .into()
        })
        .collect();

    rect()
        .width(Size::px(220.0_f32))
        .vertical()
        .spacing(6.0_f32)
        .children(cards)
        .into()
}

fn status_tooltip_panel(state: State<AppState>, side: TooltipSide) -> Element {
    let read = state.read();
    if read.is_processing {
        return rect().into();
    }
    let tooltip = read.active_tooltip.clone();
    drop(read);

    let Some((active_side, text)) = tooltip else {
        return rect().into();
    };
    if active_side != side {
        return rect().into();
    }

    rect()
        .width(Size::px(250.0_f32))
        .child(tooltip_note_card(text))
        .into()
}

fn hide_popup(mut state: State<AppState>, kind: PopupKind) {
    let mut s = state.write();
    match kind {
        PopupKind::Hardware => s.show_hardware_popup = false,
        PopupKind::Output => s.show_output_popup = false,
        PopupKind::NoLayout => s.show_no_layout_popup = false,
        PopupKind::Footnotes => s.show_footnotes_popup = false,
        PopupKind::Ocr => s.show_ocr_detection_popup = false,
        PopupKind::Queue => s.show_queue_operation_popup = false,
        PopupKind::Complete => s.show_completion_popup = false,
        PopupKind::AddFileHelp => s.show_add_file_help_popup = false,
        PopupKind::Warning => s.show_warning_popup = false,
    }
}

fn schedule_popup(mut state: State<AppState>, kind: PopupKind, message: String, seconds: u64) {
    spawn(async move {
        tokio::task::yield_now().await;
        {
            let mut s = state.write();
            match kind {
                PopupKind::Hardware => {
                    s.hardware_acceleration_popup = Some(message);
                    s.show_hardware_popup = true;
                }
                PopupKind::Output => {
                    s.output_folder_popup = Some(message);
                    s.show_output_popup = true;
                }
                PopupKind::NoLayout => {
                    s.no_layout_detection_popup = Some(message);
                    s.show_no_layout_popup = true;
                }
                PopupKind::Footnotes => {
                    s.footnotes_popup = Some(message);
                    s.show_footnotes_popup = true;
                }
                PopupKind::Ocr => {
                    s.ocr_detection_popup = Some(message);
                    s.show_ocr_detection_popup = true;
                }
                PopupKind::Queue => {
                    s.queue_operation_popup = Some(message);
                    s.show_queue_operation_popup = true;
                }
                PopupKind::Complete => {
                    s.completion_popup = Some(message);
                    s.show_completion_popup = true;
                }
                PopupKind::AddFileHelp => {
                    s.add_file_help_popup = Some(message);
                    s.show_add_file_help_popup = true;
                }
                PopupKind::Warning => {
                    s.warning_popup = Some(message);
                    s.show_warning_popup = true;
                }
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(seconds)).await;
        hide_popup(state, kind);
    });
}

fn sync_text_inputs(
    mut state: State<AppState>,
    target_height_input: State<String>,
    page_range_input: State<String>,
    k_factor_input: State<String>,
    threshold_input: State<String>,
) {
    let target_height_val = target_height_input.read().clone();
    let page_range_val = page_range_input.read().clone();
    let k_factor_val = k_factor_input.read().clone();
    let threshold_val = threshold_input.read().clone();

    use_side_effect(move || {
        let mut s = state.write();

        if s.target_height_input != target_height_val {
            s.target_height_input = target_height_val.clone();
            if let Some((height, width)) = parse_resolution_field(&target_height_val) {
                s.options.target_device = None;
                s.options.target_height = Some(height);
                s.options.target_width = width;
            }
        }

        if s.page_range_input != page_range_val {
            s.page_range_input = page_range_val.clone();
            s.options.page_range = if page_range_val.trim().is_empty() {
                None
            } else {
                Some(page_range_val.clone())
            };
        }

        if s.k_factor_input != k_factor_val {
            s.k_factor_input = k_factor_val.clone();
            if let Ok(value) = k_factor_val.trim().parse::<f32>() {
                s.options.k_factor = value;
            }
        }

        if s.threshold_input != threshold_val {
            s.threshold_input = threshold_val.clone();
            if let Ok(value) = threshold_val.trim().parse::<u16>() {
                s.options.threshold_value = value.min(255) as u8;
            }
        }
    });
}

pub fn app() -> impl IntoElement {
    use_hook(|| {
        appearance::initialize_runtime_appearance();
    });
    let theme = use_init_theme(retro_theme);
    // Subscribe the root to theme changes. The main shell paints its background from
    // the global palette (`app_bg()`), not the theme context, so without this read the
    // root would not re-render on a theme swap and the window background would stay
    // stale while the themed widgets (which consume the theme context) updated. Reading
    // the signal here makes a theme change repaint the whole tree, background included.
    let _ = theme.read();
    let state = use_state(AppState::default);

    let target_height_input = use_state({
        let read = state.read();
        let initial = read.target_height_input.clone();
        move || initial
    });
    let page_range_input = use_state({
        let read = state.read();
        let initial = read.page_range_input.clone();
        move || initial
    });
    let k_factor_input = use_state({
        let read = state.read();
        let initial = read.k_factor_input.clone();
        move || initial
    });
    let threshold_input = use_state({
        let read = state.read();
        let initial = read.threshold_input.clone();
        move || initial
    });

    sync_text_inputs(
        state,
        target_height_input,
        page_range_input,
        k_factor_input,
        threshold_input,
    );

    app_root(
        rect()
            .width(Size::fill())
            .height(Size::fill())
            .on_file_drop({
                move |e: Event<FileEventData>| {
                    if let Some(path) = e.file_path.clone() {
                        add_dropped_paths(state, vec![path]);
                    }
                }
            })
            .child(widgets::lege_main_shell(
                file_action_row(state),
                settings_dashboard(
                    state,
                    target_height_input,
                    page_range_input,
                    k_factor_input,
                    threshold_input,
                ),
                process_dashboard_row(state, page_range_input, theme),
                status_bar(state),
            ))
            // Transient notifications float above ALL window content (high
            // relative layer at the root → well above normal nesting depth, but
            // still below the `Overlay`-layer modal dialogs below).
            .child(
                rect()
                    .layer(200i16)
                    .position(Position::new_absolute().right(16.).bottom(16.))
                    .child(popup_rail(state)),
            )
            .child(queue_viewer_popup(state))
            .child(log_viewer_popup(state))
            .child(about_popup(state, theme))
            .child(debug_log_viewer_popup(state)),
    )
}

#[cfg(target_os = "windows")]
fn app_root(content: Rect) -> Rect {
    content.font_family("Segoe UI")
}

#[cfg(not(target_os = "windows"))]
fn app_root(content: Rect) -> Rect {
    content
}

fn process_utility_buttons(mut state: State<AppState>) -> Element {
    let queue_len = state.read().queue.len();

    rect()
        .direction(Direction::Horizontal)
        .spacing(6.0_f32)
        .cross_align(Alignment::Center)
        .child(
            Button::new()
                .compact()
                .on_press(move |_| state.write().show_about = true)
                .child(GUI_TEXT.interactive.queue.about_button.clone()),
        )
        .child(
            Button::new()
                .compact()
                .on_press(move |_| state.write().show_queue_viewer = true)
                .child(fmt1(&GUI_TEXT.interactive.queue.queue_button, queue_len)),
        )
        .child(
            Button::new()
                .compact()
                .on_press(move |_| {
                    let refreshed_entries = logging::load_log_entries().ok();
                    let mut s = state.write();
                    if let Some(entries) = refreshed_entries {
                        s.processing_log = entries;
                    }
                    s.show_log_viewer = true;
                })
                .child(GUI_TEXT.interactive.queue.log_button.clone()),
        )
        .into()
}

fn file_action_row(state: State<AppState>) -> Element {
    let output_text = state
        .read()
        .options
        .output_path
        .as_ref()
        .map(|path| format!("Output: {}", backend::truncate_path(path, 34)))
        .unwrap_or_else(|| GUI_TEXT.interactive.buttons.output_directory.clone());

    widgets::lege_file_action_row(
        tooltip_wrap_at(
            state,
            TooltipArea::FileActions,
            music_add_file_tooltip(),
            AttachedPosition::Left,
            Button::new()
                .width(Size::fill())
                .height(Size::fill())
                .on_press(move |_| add_files(state))
                .child(GUI_TEXT.interactive.buttons.add_file.clone())
                .into(),
        ),
        tooltip_wrap_at(
            state,
            TooltipArea::FileActions,
            music_add_folder_tooltip(),
            AttachedPosition::Left,
            Button::new()
                .width(Size::fill())
                .height(Size::fill())
                .on_press(move |_| add_folder(state))
                .child(GUI_TEXT.interactive.buttons.add_folder.clone())
                .into(),
        ),
        tooltip_wrap_at(
            state,
            TooltipArea::FileActions,
            GUI_TEXT.interactive.tooltips.output_directory.clone(),
            AttachedPosition::Right,
            Button::new()
                .width(Size::fill())
                .height(Size::fill())
                .on_press(move |_| choose_output_folder(state))
                .child(output_text)
                .into(),
        ),
    )
}

fn settings_dashboard(
    state: State<AppState>,
    target_height_input: State<String>,
    page_range_input: State<String>,
    k_factor_input: State<String>,
    threshold_input: State<String>,
) -> Element {
    // Sheet Music Edition: the CLI's --music-sheet preset owns layout
    // detection, rendering mode and image handling, so the dashboard is one
    // compact card with the four remaining controls plus the user-selectable
    // binarization mode (grayscale is out of this edition — bilevel only).
    rect()
        .width(Size::fill())
        .height(Size::fill())
        .child(music_settings_card(
            state,
            target_height_input,
            page_range_input,
            k_factor_input,
            threshold_input,
        ))
        .into()
}

fn process_dashboard_row(
    state: State<AppState>,
    page_range_input: State<String>,
    theme: State<Theme>,
) -> Element {
    let read = state.read();
    let is_processing = read.is_processing;
    let processing_items = read.processing_items.clone();
    let queued_items = read.queue.iter().cloned().collect::<Vec<_>>();
    drop(read);

    let button_text = if is_processing {
        GUI_TEXT.interactive.buttons.cancel.clone()
    } else {
        GUI_TEXT.interactive.buttons.start_processing.clone()
    };

    let item_rows: Vec<Element> = if is_processing && !processing_items.is_empty() {
        processing_items
            .iter()
            .take(3)
            .map(|item| process_queue_row(&item.status, &backend::truncate_path(&item.path, 54)))
            .collect()
    } else {
        queued_items
            .iter()
            .take(3)
            .map(|item| process_queue_row("Queued", &backend::truncate_path(&item.file_path, 54)))
            .collect()
    };

    let overflow_count = if is_processing && !processing_items.is_empty() {
        processing_items.len().saturating_sub(3)
    } else {
        queued_items.len().saturating_sub(3)
    };

    rect()
        .width(Size::fill())
        .height(Size::fill())
        .main_align(Alignment::Start)
        .cross_align(Alignment::Center)
        .child(
            rect()
                .width(Size::fill())
                .height(Size::px(44.0_f32))
                .main_align(Alignment::Center)
                .cross_align(Alignment::Center)
                .child(
                    Button::new()
                        .width(Size::px(240.0_f32))
                        .height(Size::px(36.0_f32))
                        .on_press(move |_| {
                            start_or_cancel_processing(state, page_range_input, theme)
                        })
                        .child(button_text),
                ),
        )
        .child(
            rect()
                .width(Size::fill())
                .height(Size::px(34.0_f32))
                .main_align(Alignment::Center)
                .cross_align(Alignment::Center)
                .child(process_utility_buttons(state)),
        )
        .maybe_child(if item_rows.is_empty() {
            None::<Element>
        } else {
            Some(
                rect()
                    .width(Size::fill())
                    .height(Size::px(42.0_f32))
                    .vertical()
                    .spacing(2.0_f32)
                    .children(item_rows)
                    .maybe_child(if overflow_count > 0 {
                        Some(
                            label()
                                .text(format!("+{overflow_count} more"))
                                .font_size(10.)
                                .color(muted_fg())
                                .into(),
                        )
                    } else {
                        None::<Element>
                    })
                    .into(),
            )
        })
        .into()
}

fn process_queue_row(status: &str, path: &str) -> Element {
    rect()
        .width(Size::fill())
        .direction(Direction::Horizontal)
        .spacing(8.0_f32)
        .cross_align(Alignment::Center)
        .child(
            label()
                .text(status.to_string())
                .font_size(10.)
                .font_weight(700)
                .color(muted_fg()),
        )
        .child(
            rect().width(Size::fill()).child(
                label()
                    .text(path.to_string())
                    .font_size(10.)
                    .color(text_fg()),
            ),
        )
        .into()
}

fn square_input(input: Input) -> Input {
    input.corner_radius(0.)
}

fn parse_resolution_field(value: &str) -> Option<(u32, Option<u32>)> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Some((DEFAULT_MUSIC_TARGET_HEIGHT, None));
    }

    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.len() == 2 {
        let height = parts[0].parse::<u32>().ok()?;
        let width = parts[1].parse::<u32>().ok()?;
        return (height > 0 && width > 0).then_some((height, Some(width)));
    }

    let normalized = trimmed.replace(['X', 'x', '×'], " ");
    let parts: Vec<&str> = normalized.split_whitespace().collect();
    if parts.len() == 2 {
        let height = parts[0].parse::<u32>().ok()?;
        let width = parts[1].parse::<u32>().ok()?;
        return (height > 0 && width > 0).then_some((height, Some(width)));
    }

    trimmed
        .parse::<u32>()
        .ok()
        .filter(|height| *height > 0)
        .map(|height| (height, None))
}

fn resolution_input_from_options(options: &ProcessingOptions) -> String {
    let height = options.target_height.unwrap_or(DEFAULT_MUSIC_TARGET_HEIGHT);
    match options.target_width {
        Some(width) => format!("{height}x{width}"),
        None => height.to_string(),
    }
}

fn music_target_height_tooltip() -> String {
    GUI_TEXT
        .interactive
        .tooltips
        .target_height
        .replace("1200", &DEFAULT_MUSIC_TARGET_HEIGHT.to_string())
}

fn music_add_file_tooltip() -> String {
    GUI_TEXT.interactive.tooltips.add_file.clone()
}

fn music_add_folder_tooltip() -> String {
    GUI_TEXT.interactive.tooltips.add_folder.clone()
}

#[derive(Clone, PartialEq)]
struct CompactChoiceButton {
    text: String,
    selected: bool,
    on_press: EventHandler<Event<PressEventData>>,
}

impl CompactChoiceButton {
    fn new(
        text: impl Into<String>,
        selected: bool,
        on_press: impl FnMut(Event<PressEventData>) + 'static,
    ) -> Self {
        Self {
            text: text.into(),
            selected,
            on_press: on_press.into(),
        }
    }
}

impl Component for CompactChoiceButton {
    fn render(&self) -> impl IntoElement {
        let mut hovering = use_state(|| false);
        let selected = self.selected;
        let on_press = self.on_press.clone();
        let text = self.text.clone();

        let background = if selected {
            rgb(selected_bg())
        } else if hovering() {
            rgb(hover_bg())
        } else {
            rgb(control_bg())
        };

        rect()
            .width(Size::fill())
            .height(Size::px(20.0_f32))
            .background(background)
            .border(
                Border::new()
                    .fill(border())
                    .width(1.)
                    .alignment(BorderAlignment::Inner),
            )
            .corner_radius(3.)
            .center()
            .on_pointer_enter(move |_| {
                hovering.set(true);
                Cursor::set(CursorIcon::Pointer);
            })
            .on_pointer_leave(move |_| {
                hovering.set(false);
                Cursor::set(CursorIcon::default());
            })
            .on_press(move |e| on_press.call(e))
            .child(label().text(text).font_size(12.).color(text_fg()))
    }
}

fn compact_choice_button(
    text: impl Into<String>,
    selected: bool,
    on_press: impl FnMut(Event<PressEventData>) + 'static,
) -> Element {
    CompactChoiceButton::new(text, selected, on_press).into()
}

fn compact_choice_row(children: Vec<Element>) -> Element {
    let cells: Vec<Element> = children
        .into_iter()
        .map(|child| {
            rect()
                .width(Size::flex(1.0_f32))
                .height(Size::fill())
                .child(child)
                .into()
        })
        .collect();

    rect()
        .width(Size::fill())
        .height(Size::px(20.0_f32))
        .direction(Direction::Horizontal)
        .content(Content::Flex)
        .spacing(3.0_f32)
        .children(cells)
        .into()
}

fn music_settings_card(
    state: State<AppState>,
    target_height_input: State<String>,
    page_range_input: State<String>,
    k_factor_input: State<String>,
    threshold_input: State<String>,
) -> Element {
    let options = state.read().options.clone();

    // Page range input.
    let page_range_control = settings_row(
        GUI_TEXT.interactive.labels.page_range.clone(),
        square_input(Input::new(page_range_input))
            .placeholder(GUI_TEXT.interactive.labels.page_range_placeholder.clone())
            .expanded()
            .into(),
    );

    // The resolution field persists itself: it is saved as the resolution
    // preset when a job runs with it and restored on startup. Clicking the
    // field clears it — no explicit save/load buttons.
    let resolution_control = tooltip_wrap_at(
        state,
        TooltipArea::PagesDeviceCard,
        music_target_height_tooltip(),
        AttachedPosition::Bottom,
        settings_row(
            GUI_TEXT.interactive.labels.target_height.clone(),
            square_input(Input::new(target_height_input))
                .placeholder(DEFAULT_MUSIC_TARGET_HEIGHT.to_string())
                .replace_on_focus(true)
                .on_submit({
                    let mut state = state;
                    let mut target_height_input = target_height_input;
                    move |text: String| {
                        if let Some((height, width)) = parse_resolution_field(&text) {
                            let normalized;
                            {
                                let mut s = state.write();
                                // crop_free_aspect has no UI in this edition but can
                                // arrive via a stale settings.json; it means
                                // "height only", so a fixed width is dropped.
                                let crop_free_aspect = s.options.crop_free_aspect;
                                if crop_free_aspect && width.is_some() {
                                    schedule_popup(
                                        state,
                                        PopupKind::Warning,
                                        "Free-aspect crop uses height only; fixed width was ignored."
                                            .to_string(),
                                        5,
                                    );
                                }
                                let width = if crop_free_aspect { None } else { width };
                                normalized = match width {
                                    Some(width) => format!("{height}x{width}"),
                                    None => height.to_string(),
                                };
                                s.options.target_device = None;
                                s.options.target_height = Some(height);
                                s.options.target_width = width;
                                s.target_height_input = normalized.clone();
                            }
                            target_height_input.set(normalized);
                        }
                    }
                })
                .expanded()
                .into(),
        ),
    );

    // Output format: PDF and DjVu are the only outputs in this edition (the
    // --music-sheet preset owns every other rendering decision).
    let format_control = settings_row(
        GUI_TEXT.interactive.labels.output_format.clone(),
        compact_choice_row(vec![
            compact_choice_button("PDF", matches!(options.output_format, OutputFormat::Pdf), {
                let mut state = state;
                move |_| {
                    state.write().options.output_format = OutputFormat::Pdf;
                }
            }),
            compact_choice_button(
                "DjVu",
                matches!(options.output_format, OutputFormat::Djvu),
                {
                    let mut state = state;
                    move |_| {
                        state.write().options.output_format = OutputFormat::Djvu;
                    }
                },
            ),
        ]),
    );

    // One compact strip: the three remaining controls side by side (OCR is out
    // of the music edition — the --music-sheet preset defaults it off and the
    // GUI emits no OCR flags), above the user-selectable binarization mode row.
    // 48px fits a 13px field label + 24px control with a little slack (the old
    // 68px existed only for the two-row OCR stack).
    rect()
        .width(Size::fill())
        .height(Size::fill())
        .child(panel_card(
            "",
            true,
            vec![
                widgets::lege_grid_row(
                    vec![page_range_control, resolution_control, format_control],
                    48.,
                    5.,
                ),
                // Keep the heading clear of the compact control strip above it.
                // The settings shell reserves the matching 15px so every
                // section below this one moves down with it.
                binarization_subcard(state, k_factor_input, threshold_input),
            ],
        ))
        .into()
}

fn binarization_subcard(
    state: State<AppState>,
    k_factor_input: State<String>,
    threshold_input: State<String>,
) -> Element {
    let options = state.read().options.clone();

    // Column width shared by the checkbox row and the input row below it, so each
    // input lines up under its own checkbox.
    const COL_W: f32 = 150.;
    // Fixed height reserved for the input row. It is always present (empty for Heavy
    // model / default adaptive) so the section never changes height between modes.
    const INPUT_ROW_H: f32 = 30.;

    // Exactly one mode is active at a time. "Custom adaptive" only counts when
    // neither other mode is selected; with all three off the pipeline uses default
    // adaptive (hidden K factor = DEFAULT_K_FACTOR). Grayscale is out of this
    // edition, so there is no Binarized/Grayscale toggle.
    let custom_adaptive = options.use_custom_adaptive
        && !options.use_fixed_threshold
        && !options.use_heavy_binarization;
    let fixed_enabled = options.use_fixed_threshold;
    let heavy_enabled = options.use_heavy_binarization;

    let checkbox_cell =
        |child: Element| -> Element { rect().width(Size::px(COL_W)).child(child).into() };

    // Row 1 — the three mutually-exclusive mode checkboxes.
    let checkbox_row = rect()
        .width(Size::fill())
        .direction(Direction::Horizontal)
        .main_align(Alignment::Center)
        .cross_align(Alignment::Center)
        .spacing(4.0_f32)
        .child(checkbox_cell(tooltip_wrap_at(
            state,
            TooltipArea::PagesDeviceCard,
            GUI_TEXT.interactive.tooltips.sauvola_k_factor.clone(),
            AttachedPosition::Left,
            compact_checkbox_row(
                GUI_TEXT.interactive.controls.custom_adaptive.clone(),
                custom_adaptive,
                {
                    let mut state = state;
                    let mut k_factor_input = k_factor_input;
                    move |_| {
                        let mut s = state.write();
                        let enable = !(s.options.use_custom_adaptive
                            && !s.options.use_fixed_threshold
                            && !s.options.use_heavy_binarization);
                        if enable {
                            s.options.use_custom_adaptive = true;
                            s.options.use_fixed_threshold = false;
                            s.options.use_heavy_binarization = false;
                        } else {
                            // Back to default adaptive: restore the built-in K factor.
                            s.options.use_custom_adaptive = false;
                            s.options.k_factor = lege_ipc::DEFAULT_K_FACTOR;
                            s.k_factor_input = lege_ipc::DEFAULT_K_FACTOR.to_string();
                            k_factor_input.set(lege_ipc::DEFAULT_K_FACTOR.to_string());
                        }
                    }
                },
            ),
        )))
        .child(checkbox_cell(tooltip_wrap_at(
            state,
            TooltipArea::PagesDeviceCard,
            GUI_TEXT.interactive.tooltips.threshold_value.clone(),
            AttachedPosition::Left,
            compact_checkbox_row(
                GUI_TEXT.interactive.controls.fixed_threshold.clone(),
                fixed_enabled,
                {
                    let mut state = state;
                    move |_| {
                        let mut s = state.write();
                        let enable = !s.options.use_fixed_threshold;
                        s.options.use_fixed_threshold = enable;
                        if enable {
                            s.options.use_custom_adaptive = false;
                            s.options.use_heavy_binarization = false;
                        }
                    }
                },
            ),
        )))
        .child(checkbox_cell(tooltip_wrap_at(
            state,
            TooltipArea::PagesDeviceCard,
            GUI_TEXT.interactive.tooltips.heavy_model.clone(),
            AttachedPosition::Left,
            compact_checkbox_row(
                GUI_TEXT.interactive.controls.heavy_model.clone(),
                heavy_enabled,
                {
                    let mut state = state;
                    move |_| {
                        let mut s = state.write();
                        let enable = !s.options.use_heavy_binarization;
                        s.options.use_heavy_binarization = enable;
                        if enable {
                            s.options.use_custom_adaptive = false;
                            s.options.use_fixed_threshold = false;
                        }
                    }
                },
            ),
        )));

    // Row 2 — numeric inputs under their respective checkboxes. Fixed height so the
    // section keeps the same size regardless of which mode is selected.
    let input_row = rect()
        .width(Size::fill())
        .height(Size::px(INPUT_ROW_H))
        .direction(Direction::Horizontal)
        .main_align(Alignment::Center)
        // Top-align with a small offset so the revealed input sits just below the
        // checkbox label instead of almost touching it.
        .cross_align(Alignment::Start)
        .padding((4., 0., 0., 0.))
        .spacing(4.0_f32)
        .child(
            rect()
                .width(Size::px(COL_W))
                // Smaller font than the other numeric fields; the Input's text inherits
                // this font size (it sets no font size of its own).
                .font_size(10.)
                .maybe_child(if custom_adaptive {
                    Some(
                        square_input(Input::new(k_factor_input))
                            .compact()
                            .placeholder("0.05")
                            // Clear on click for fresh entry, like the
                            // target-resolution field — no prepending.
                            .replace_on_focus(true)
                            .width(Size::px(64.0_f32))
                            .into(),
                    )
                } else {
                    None::<Element>
                }),
        )
        .child(
            rect()
                .width(Size::px(COL_W))
                .font_size(10.)
                .maybe_child(if fixed_enabled {
                    Some(
                        square_input(Input::new(threshold_input))
                            .compact()
                            .placeholder("180")
                            // Clear on click for fresh entry, like the
                            // target-resolution field — no prepending.
                            .replace_on_focus(true)
                            .width(Size::px(64.0_f32))
                            .into(),
                    )
                } else {
                    None::<Element>
                }),
        )
        .child(rect().width(Size::px(COL_W)));

    // Plain titled section — NOT a bordered sub-card.
    rect()
        .width(Size::fill())
        .padding((15., 0., 0., 0.))
        .spacing(4.0_f32)
        .vertical()
        .child(
            label()
                .text(GUI_TEXT.interactive.controls.binarization.clone())
                .font_size(13.)
                .font_weight(700)
                .color(text_fg()),
        )
        .child(checkbox_row)
        .child(input_row)
        .into()
}

fn progress_stage_card(
    title: impl Into<String>,
    current: u32,
    total: u32,
    accent: (u8, u8, u8),
) -> Element {
    let title = title.into();
    let safe_total = total.max(1);
    let clamped = current.min(total);
    let percent = ((clamped as f32 / safe_total as f32) * 100.0).clamp(0.0, 100.0);

    rect()
        .width(Size::fill())
        .height(Size::px(48.0_f32))
        .background(card_bg())
        .border(
            Border::new()
                .fill(border())
                .width(1.)
                .alignment(BorderAlignment::Inner),
        )
        .corner_radius(4.)
        .padding((6., 8., 6., 8.))
        .vertical()
        .spacing(4.0_f32)
        .child(
            rect()
                .width(Size::fill())
                .direction(Direction::Horizontal)
                .main_align(Alignment::SpaceBetween)
                .cross_align(Alignment::Center)
                .child(
                    label()
                        .text(title)
                        .font_size(11.)
                        .font_weight(700)
                        .color(text_fg()),
                )
                .child(
                    label()
                        .text(format!("{clamped}/{total}"))
                        .font_size(10.)
                        .color(muted_fg()),
                ),
        )
        .child(
            rect()
                .width(Size::fill())
                .height(Size::px(10.0_f32))
                .background(rgb(progress_track_bg()))
                .border(
                    Border::new()
                        .fill(border())
                        .width(1.)
                        .alignment(BorderAlignment::Inner),
                )
                .corner_radius(3.)
                .child(
                    rect()
                        .width(Size::percent(percent))
                        .height(Size::fill())
                        .background(Color::from_rgb(accent.0, accent.1, accent.2))
                        .corner_radius(2.),
                ),
        )
        .into()
}

fn progress_single_bar(
    metrics: crate::worker_process::WorkerProgressMetrics,
    color: (u8, u8, u8),
) -> Option<Element> {
    if metrics.pages_total == 0 {
        return None;
    }
    let current = match metrics.mode {
        crate::worker_process::WorkerProgressMode::Layout
        | crate::worker_process::WorkerProgressMode::Margin => {
            metrics.rendered.max(metrics.detected).max(metrics.encoded)
        }
        crate::worker_process::WorkerProgressMode::NoLayout
        | crate::worker_process::WorkerProgressMode::HeavySequential
        | crate::worker_process::WorkerProgressMode::Reflow
        | crate::worker_process::WorkerProgressMode::Unknown => {
            metrics.rendered.max(metrics.encoded)
        }
        crate::worker_process::WorkerProgressMode::Epub => {
            return Some(progress_stage_card(
                "Progress".to_string(),
                metrics.rendered + metrics.detected + metrics.encoded,
                metrics.pages_total.saturating_mul(3),
                color,
            ));
        }
    };
    Some(progress_stage_card(
        "Progress".to_string(),
        current,
        metrics.pages_total,
        color,
    ))
}

fn status_bar(state: State<AppState>) -> Element {
    let read = state.read();
    let is_processing = read.is_processing;
    let eta_text = read.active_eta.clone();
    let queue_items = read.queue.iter().cloned().collect::<Vec<_>>();
    let queue_len = queue_items.len();
    let status = read.status_lines.clone();
    let progress_metrics = read.progress_metrics;
    drop(read);

    let top_left: Element = status_tooltip_panel(state, TooltipSide::Left);

    // Top-right: tooltip slot only. The Queue/Log buttons now live in the header
    // bar so they never collide with the progress bar.
    let top_right: Element = status_tooltip_panel(state, TooltipSide::Right);

    // Notifications are rendered as a root-level overlay (see `app()`), so the
    // status panel's bottom-left slot is now empty.
    let bottom_left: Element = rect().into();

    // Bottom-right debug controls; About lives in the header utility bar.
    let bottom_right: Element = {
        #[cfg(feature = "debug-logging")]
        {
            rect()
                .vertical()
                .spacing(3.0_f32)
                .cross_align(Alignment::End)
                .child(
                    Button::new()
                        .compact()
                        .on_press({
                            let mut state = state;
                            move |_| state.write().show_debug_log = true
                        })
                        .child(GUI_TEXT.interactive.queue.debug_button.clone()),
                )
                .into()
        }

        #[cfg(not(feature = "debug-logging"))]
        {
            rect().into()
        }
    };

    // Main content: dual-mode.
    let main_content: Element = if is_processing {
        // Processing mode: existing progress display. One theme-derived color
        // (reddest pastel on the Default theme, the theme's hover accent on
        // Sanzo themes) instead of per-job pastel cycling.
        let bar_color = crate::colors::progress_bar_color();
        let progress_section = progress_metrics.and_then(|m| progress_single_bar(m, bar_color));
        let secondary_text = if progress_section.is_some() {
            let mut parts = Vec::new();
            if !status.1.is_empty() {
                parts.push(status.1.clone());
            }
            if !status.3.is_empty() {
                parts.push(status.3.clone());
            }
            if let Some(eta) = &eta_text {
                parts.push(fmt1(&GUI_TEXT.interactive.progress.eta, eta));
            }
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("  |  "))
            }
        } else {
            let mut parts = Vec::new();
            if !status.1.is_empty() {
                parts.push(status.1.clone());
            }
            if !status.2.is_empty() {
                parts.push(status.2.clone());
            }
            if !status.3.is_empty() {
                parts.push(status.3.clone());
            }
            if let Some(eta) = &eta_text {
                parts.push(fmt1(&GUI_TEXT.interactive.progress.eta, eta));
            }
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("  |  "))
            }
        };

        rect()
            .width(Size::fill())
            .height(Size::fill())
            .vertical()
            .spacing(6.0_f32)
            .child(label().text(status.0).font_size(12.).color(text_fg()))
            .maybe_child(secondary_text.map(|detail| -> Element {
                label().text(detail).font_size(11.).color(muted_fg()).into()
            }))
            .maybe_child(progress_section)
            .into()
    } else if queue_items.is_empty() {
        // Idle, empty: prompt to add files.
        rect()
            .width(Size::fill())
            .height(Size::fill())
            .main_align(Alignment::Center)
            .cross_align(Alignment::Center)
            .child(
                label()
                    .text(GUI_TEXT.interactive.queue.empty_message.clone())
                    .font_size(12.)
                    .color(muted_fg()),
            )
            .into()
    } else {
        // Idle with queue items: show only the aggregate summary. File paths are
        // shown below the Process button so they are not duplicated here.
        let total_pages: Option<u32> = {
            let all_known = queue_items.iter().all(|i| i.page_count.is_some());
            if all_known {
                Some(queue_items.iter().filter_map(|i| i.page_count).sum())
            } else {
                None
            }
        };

        let summary_line = match total_pages {
            Some(n) => fmt2(&GUI_TEXT.interactive.queue.item_ready_summary, queue_len, n),
            None => fmt1(&GUI_TEXT.interactive.queue.item_queued_summary, queue_len),
        };

        rect()
            .width(Size::fill())
            .height(Size::fill())
            .main_align(Alignment::Center)
            .cross_align(Alignment::Center)
            .child(label().text(summary_line).font_size(12.).color(muted_fg()))
            .into()
    };

    widgets::lege_status_panel(main_content, top_left, top_right, bottom_left, bottom_right)
}

fn render_log_rows(entries: &[LogEntry]) -> Vec<Element> {
    entries
        .iter()
        .map(|entry| {
            let text = format_log_entry_text(entry);
            rect()
                .width(Size::fill())
                .padding((6., 4., 6., 4.))
                .background(panel_bg())
                .corner_radius(4.)
                .child(
                    SelectableText::new(text)
                        .width(Size::fill())
                        .font_size(11.)
                        .color(text_fg())
                        .line_height(1.25),
                )
                .into()
        })
        .collect()
}

fn format_log_entry_text(entry: &LogEntry) -> String {
    let mut lines = vec![
        format!("{}  |  {}", entry.format_timestamp(), entry.status),
        fmt1(&GUI_TEXT.interactive.queue.input, &entry.input_path),
        fmt1(&GUI_TEXT.interactive.queue.output, &entry.output_path),
        format!(
            "{} -> {}",
            LogEntry::format_size(entry.original_size),
            LogEntry::format_size(entry.compressed_size)
        ),
    ];

    if let Some(message) = entry.error_message.as_ref() {
        let kind = entry.error_kind.as_deref().unwrap_or("processing_error");
        lines.push(format!("{kind}: {message}"));
    }

    lines.join("\n")
}

fn completion_message(result: &ProcessingResult, estimated_original_size: u64) -> String {
    let basis_size = if estimated_original_size > 0 {
        estimated_original_size
    } else {
        result.original_size
    };

    let reduction_percent = if basis_size > 0 {
        (1.0 - (result.compressed_size as f64 / basis_size as f64)) * 100.0
    } else {
        0.0
    };

    let compression_line = if reduction_percent >= 0.0 {
        fmt3(
            &GUI_TEXT.interactive.progress.reduced,
            format!("{reduction_percent:.1}"),
            LogEntry::format_size(basis_size),
            LogEntry::format_size(result.compressed_size),
        )
    } else {
        fmt3(
            &GUI_TEXT.interactive.progress.increased,
            format!("{:.1}", reduction_percent.abs()),
            LogEntry::format_size(basis_size),
            LogEntry::format_size(result.compressed_size),
        )
    };

    if result.page_range_used {
        fmt2(
            &GUI_TEXT.interactive.progress.completed_with_estimate,
            &result.output_filename,
            compression_line,
        )
    } else {
        fmt2(
            &GUI_TEXT.interactive.progress.completed,
            &result.output_filename,
            compression_line,
        )
    }
}

fn popup_shell(
    title: impl Into<String>,
    content: Element,
    on_close: impl FnMut() + 'static,
) -> Element {
    let on_close = Rc::new(RefCell::new(on_close));
    let on_close_request = on_close.clone();
    let on_close_button = on_close.clone();
    Popup::new()
        .show(true)
        .on_close_request(move |_| (on_close_request.borrow_mut())())
        .child(PopupTitle::new(title.into()))
        .child(PopupContent::new().child(content))
        .child(
            PopupButtons::new().child(
                Button::new()
                    .filled()
                    .on_press(move |_| (on_close_button.borrow_mut())())
                    .child(
                        label()
                            .text(GUI_TEXT.interactive.popups.close.clone())
                            .font_size(13.)
                            .color(text_fg()),
                    ),
            ),
        )
        .into()
}

fn document_popup_shell(
    title: impl Into<String>,
    content: Element,
    on_close: impl FnMut() + 'static,
) -> Element {
    let title = title.into();
    let on_close = Rc::new(RefCell::new(on_close));
    let on_close_request = on_close.clone();

    LegeModal::new(
        title,
        ScrollView::new()
            .width(Size::fill())
            .height(Size::fill())
            .child(
                rect()
                    .width(Size::fill())
                    .padding(12.)
                    .spacing(8.0_f32)
                    .child(content),
            )
            .into(),
    )
    .show(true)
    .width(Size::percent(50.0_f32))
    .height(Size::percent(50.0_f32))
    .min_width(Size::px(520.0_f32))
    .min_height(Size::px(360.0_f32))
    .on_close_request(move |_| (on_close_request.borrow_mut())())
    .into()
}

fn queue_viewer_popup(mut state: State<AppState>) -> Element {
    if !state.read().show_queue_viewer {
        return rect().into();
    }

    let queue_items = state.read().queue.iter().cloned().collect::<Vec<_>>();
    let is_processing = state.read().is_processing;
    let queue_empty = queue_items.is_empty();
    popup_shell(
        GUI_TEXT.interactive.popups.queue_items_title.clone(),
        rect()
            .width(Size::px(520.0_f32))
            .spacing(8.0_f32)
            .child(
                Button::new()
                    .on_press(move |_| {
                        // Don't clear while a job is running; the active queue snapshot
                        // is what's being processed.
                        let mut s = state.write();
                        if !s.is_processing {
                            s.queue.clear();
                        }
                    })
                    .child(
                        label()
                            .text(GUI_TEXT.interactive.buttons.clear_queue.clone())
                            .font_size(13.)
                            .color(if is_processing || queue_empty {
                                muted_fg()
                            } else {
                                text_fg()
                            }),
                    ),
            )
            .child(
                ScrollView::new()
                    .width(Size::px(520.0_f32))
                    .height(Size::px(320.0_f32))
                    .child(rect().spacing(8.0_f32).children(if queue_items.is_empty() {
                        vec![
                            label()
                                .text(GUI_TEXT.interactive.queue.empty_short.clone())
                                .font_size(12.)
                                .color(muted_fg())
                                .into(),
                        ]
                    } else {
                        queue_items
                            .iter()
                            .enumerate()
                            .map(|(idx, item)| {
                                rect()
                                    .background(panel_bg())
                                    .corner_radius(4.)
                                    .padding(8.)
                                    .spacing(4.0_f32)
                                    .child(
                                        label()
                                            .text(format!("{}. {}", idx + 1, item.file_name))
                                            .font_size(13.)
                                            .color(text_fg()),
                                    )
                                    .child(
                                        label()
                                            .text(item.file_path.display().to_string())
                                            .font_size(11.)
                                            .color(muted_fg()),
                                    )
                                    .into()
                            })
                            .collect()
                    })),
            )
            .into(),
        move || state.write().show_queue_viewer = false,
    )
}

fn log_viewer_popup(mut state: State<AppState>) -> Element {
    if !state.read().show_log_viewer {
        return rect().into();
    }

    let (recent_entries, session_messages) = {
        let read = state.read();
        (
            read.processing_log
                .iter()
                .rev()
                .take(20)
                .cloned()
                .collect::<Vec<_>>(),
            read.status_log
                .iter()
                .rev()
                .take(100)
                .cloned()
                .collect::<Vec<_>>(),
        )
    };
    let mut log_rows = session_messages
        .into_iter()
        .map(|message| {
            rect()
                .background(panel_bg())
                .corner_radius(4.)
                .padding(6.)
                .child(label().text(message).font_size(11.).color(text_fg()))
                .into()
        })
        .collect::<Vec<Element>>();
    log_rows.extend(render_log_rows(&recent_entries));

    document_popup_shell(
        GUI_TEXT.interactive.popups.processing_log_title.clone(),
        rect()
            .width(Size::fill())
            .height(Size::fill())
            .spacing(8.0_f32)
            .child(
                Button::new()
                    .on_press(move |_| {
                        if logging::clear_log_entries().is_ok() {
                            state.write().processing_log.clear();
                        }
                    })
                    .child(
                        label()
                            .text(GUI_TEXT.interactive.popups.clear_log.clone())
                            .font_size(13.)
                            .color(text_fg()),
                    ),
            )
            .child(
                ScrollView::new()
                    .width(Size::fill())
                    .height(Size::fill())
                    .child(
                        rect()
                            .width(Size::fill())
                            .spacing(8.0_f32)
                            .children(log_rows),
                    ),
            )
            .into(),
        move || state.write().show_log_viewer = false,
    )
}

#[cfg(feature = "debug-logging")]
fn debug_log_viewer_popup(mut state: State<AppState>) -> Element {
    if !state.read().show_debug_log {
        return rect().into();
    }

    let messages = state
        .read()
        .debug_log_messages
        .iter()
        .rev()
        .take(100)
        .cloned()
        .collect::<Vec<_>>();

    popup_shell(
        GUI_TEXT.interactive.popups.debug_log_title.clone(),
        ScrollView::new()
            .width(Size::px(760.0_f32))
            .height(Size::px(420.0_f32))
            .child(
                rect().spacing(6.0_f32).children(
                    messages
                        .iter()
                        .map(|msg| {
                            rect()
                                .background(panel_bg())
                                .corner_radius(4.)
                                .padding(6.)
                                .child(label().text(msg.clone()).font_size(11.).color(text_fg()))
                                .into()
                        })
                        .collect::<Vec<_>>(),
                ),
            )
            .into(),
        move || state.write().show_debug_log = false,
    )
}

#[cfg(not(feature = "debug-logging"))]
fn debug_log_viewer_popup(_state: State<AppState>) -> Element {
    rect().into()
}

fn about_popup(mut state: State<AppState>, theme: State<Theme>) -> Element {
    let show_about = state.read().show_about;

    if !show_about {
        return rect().into();
    }

    let show_chooser = state.read().show_theme_chooser;

    let on_close = Rc::new(RefCell::new(move || {
        // Closing the About window also cancels any in-progress theme preview so an
        // un-accepted browse doesn't leave the app on a theme that isn't persisted.
        appearance::set_theme_preview(None);
        let mut theme = theme;
        theme.set(retro_theme());
        let mut state = state;
        let mut s = state.write();
        s.show_about = false;
        s.show_theme_chooser = false;
    }));
    let on_close_request = on_close.clone();
    let on_close_button = on_close.clone();
    let documentation_button = on_close.clone();
    let licenses_button = on_close.clone();
    let state_for_docs = state;
    let state_for_licenses = state;

    let content = rect()
        .width(Size::px(if show_chooser { 480.0_f32 } else { 300.0_f32 }))
        .padding(10.)
        .spacing(6.0_f32)
        .child(
            rect()
                .direction(Direction::Horizontal)
                .spacing(8.0_f32)
                .cross_align(Alignment::Center)
                .child(
                    label()
                        .text("Lege")
                        .font_size(18.)
                        .font_weight(700)
                        .color(text_fg()),
                )
                .child(
                    label()
                        .text("Sheet Music Edition")
                        .font_size(13.)
                        .color(muted_fg()),
                )
                .child(
                    label()
                        .text(format!("v{}", display_version()))
                        .font_size(12.)
                        .color(muted_fg()),
                ),
        );
    let content = if show_chooser {
        content.child(theme_chooser(state, theme))
    } else {
        content
    };

    Popup::new()
        .show(true)
        .on_close_request(move |_| (on_close_request.borrow_mut())())
        .child(PopupContent::new().child(content))
        .child(
            PopupButtons::new()
                .child(
                    Button::new()
                        .on_press(move |_| {
                            // Opens the theme chooser in place; About stays open.
                            state.write().show_theme_chooser = true;
                        })
                        .child("Theme"),
                )
                .child(
                    Button::new()
                        .on_press(move |_| {
                            match backend::bundled_docs_path_if_exists(
                                "documentation-sheetmusic.html",
                            ) {
                                Some(path) => {
                                    match backend::open_with_system(&path.to_string_lossy()) {
                                        Ok(()) => (documentation_button.borrow_mut())(),
                                        Err(error) => schedule_popup(
                                            state_for_docs,
                                            PopupKind::Warning,
                                            format!("Could not open documentation: {error}"),
                                            8,
                                        ),
                                    }
                                }
                                None => {
                                    schedule_popup(
                                        state_for_docs,
                                        PopupKind::Warning,
                                        GUI_TEXT.interactive.popups.docs_not_found.clone(),
                                        5,
                                    );
                                }
                            }
                        })
                        .child(GUI_TEXT.interactive.popups.documentation.clone()),
                )
                .child(
                    Button::new()
                        .on_press(move |_| {
                            match backend::bundled_docs_path_if_exists("licenses.html") {
                                Some(path) => {
                                    match backend::open_with_system(&path.to_string_lossy()) {
                                        Ok(()) => (licenses_button.borrow_mut())(),
                                        Err(error) => schedule_popup(
                                            state_for_licenses,
                                            PopupKind::Warning,
                                            format!("Could not open licenses: {error}"),
                                            8,
                                        ),
                                    }
                                }
                                None => {
                                    schedule_popup(
                                        state_for_licenses,
                                        PopupKind::Warning,
                                        GUI_TEXT.interactive.popups.docs_not_found.clone(),
                                        5,
                                    );
                                }
                            }
                        })
                        .child(GUI_TEXT.interactive.popups.licenses.clone()),
                )
                .child(
                    Button::new()
                        .filled()
                        .on_press(move |_| (on_close_button.borrow_mut())())
                        .child(
                            label()
                                .text(GUI_TEXT.interactive.popups.close.clone())
                                .font_size(13.)
                                .color(text_fg()),
                        ),
                ),
        )
        .into()
}

fn add_files(state: State<AppState>) {
    spawn(async move {
        let paths = rfd::AsyncFileDialog::new()
            .add_filter(
                &GUI_TEXT.interactive.popups.supported_inputs_filter,
                &["pdf", "zip"],
            )
            .set_title("Select PDF/ZIP files")
            .pick_files()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|file| file.path().to_owned())
            .collect();

        enqueue_paths(state, paths);
    });
}

fn enqueue_paths(mut state: State<AppState>, paths: Vec<std::path::PathBuf>) {
    if paths.is_empty() {
        return;
    }

    let first_pdf_path = paths.iter().find(|p| backend::is_pdf_file(p)).cloned();
    let mut should_show_output_popup = false;
    let mut new_ids: Vec<(String, std::path::PathBuf)> = Vec::new();

    {
        let mut s = state.write();
        for path in paths.iter().cloned() {
            let item = DocumentItem::new(path);
            new_ids.push((item.id.clone(), item.file_path.clone()));
            s.queue.push_back(item);
        }
        if s.options.output_path.is_none() || s.options.output_path_is_auto {
            if let Some(first) = paths.first() {
                let output_base = if first.is_dir() {
                    Some(first.clone())
                } else {
                    first.parent().map(|p| p.to_path_buf())
                };
                if let Some(output_base) = output_base {
                    should_show_output_popup = s.options.output_path.is_none();
                    s.options.output_path = Some(output_base);
                    s.options.output_path_is_auto = true;
                }
            }
        }
    }

    for (id, path) in new_ids {
        let mut s = state;
        spawn(async move {
            if let Some(count) = backend::precheck_page_count(path).await {
                if let Some(item) = s.write().queue.iter_mut().find(|i| i.id == id) {
                    item.page_count = Some(count);
                }
            }
        });
    }

    if should_show_output_popup {
        schedule_popup(
            state,
            PopupKind::Output,
            GUI_TEXT.interactive.messages.defaulted_output_dir.clone(),
            5,
        );
    }

    if let Some(pdf_path) = first_pdf_path {
        spawn(async move {
            if let Ok(Some(has_ocr)) = backend::check_pdf_has_ocr(&pdf_path).await {
                let message = if has_ocr {
                    GUI_TEXT.interactive.popups.ocr_detected.as_str()
                } else {
                    GUI_TEXT.interactive.popups.no_ocr_detected.as_str()
                };
                schedule_popup(state, PopupKind::Ocr, message.to_string(), 5);
            }
        });
    }
}

fn add_dropped_paths(state: State<AppState>, paths: Vec<std::path::PathBuf>) {
    let mut accepted = Vec::new();
    for path in paths {
        match backend::expand_queue_input(path.clone()) {
            Ok(expanded) if !expanded.is_empty() => accepted.extend(expanded),
            _ => {
                schedule_popup(
                    state,
                    PopupKind::Warning,
                    fmt1(
                        "{}: folder contains no PDF files or supported page images",
                        path.display(),
                    ),
                    5,
                );
            }
        }
    }

    enqueue_paths(state, accepted);
}

fn add_folder(mut state: State<AppState>) {
    spawn(async move {
        if let Some(folder) = rfd::AsyncFileDialog::new().pick_folder().await {
            let path = folder.path().to_owned();

            match backend::get_image_files_in_directory(&path) {
                Ok(image_files) if !image_files.is_empty() => {
                    let item = DocumentItem::new(path.clone());
                    let id = item.id.clone();
                    {
                        let mut s = state.write();
                        s.queue.push_back(item);
                        if s.options.output_path.is_none() || s.options.output_path_is_auto {
                            s.options.output_path = Some(path.clone());
                            s.options.output_path_is_auto = true;
                        }
                    }
                    let mut s = state;
                    spawn(async move {
                        if let Some(count) = backend::precheck_page_count(path).await {
                            if let Some(item) = s.write().queue.iter_mut().find(|i| i.id == id) {
                                item.page_count = Some(count);
                            }
                        }
                    });
                    schedule_popup(
                        state,
                        PopupKind::Output,
                        GUI_TEXT.interactive.messages.defaulted_output_dir.clone(),
                        5,
                    );
                }
                _ => {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    schedule_popup(
                        state,
                        PopupKind::Queue,
                        fmt1(
                            &GUI_TEXT.interactive.popups.folder_no_supported_images,
                            name,
                        ),
                        6,
                    );
                }
            }
        }
    });
}

fn choose_output_folder(mut state: State<AppState>) {
    spawn(async move {
        if let Some(folder) = rfd::AsyncFileDialog::new().pick_folder().await {
            let mut s = state.write();
            s.options.output_path = Some(folder.path().to_owned());
            s.options.output_path_is_auto = false;
        }
    });
}

fn start_or_cancel_processing(
    mut state: State<AppState>,
    page_range_input: State<String>,
    theme: State<Theme>,
) {
    if state.read().is_processing {
        state.write().should_cancel = true;
        state
            .write()
            .set_status_message(GUI_TEXT.interactive.status.cancelling.clone());
        refresh_appearance_theme(theme);
        return;
    }

    refresh_appearance_theme(theme);

    let queue = state.read().queue.clone();
    let mut options = state.read().options.clone();
    let live_page_range = page_range_input.read().trim().to_string();
    options.page_range = if live_page_range.is_empty() {
        None
    } else {
        Some(live_page_range.clone())
    };
    {
        let mut s = state.write();
        s.page_range_input = live_page_range.clone();
        s.options.page_range = options.page_range.clone();
    }

    if queue.is_empty() {
        state
            .write()
            .set_status_message(GUI_TEXT.interactive.status.no_files_in_queue.clone());
        return;
    }

    if let Some(missing) = queue.iter().find(|item| !item.file_path.exists()) {
        let message = format!(
            "The queued input could not be found: {}",
            missing.file_path.display()
        );
        state.write().set_status_message(message.clone());
        schedule_popup(state, PopupKind::Warning, message, 8);
        return;
    }

    if options.output_path.is_none() {
        state
            .write()
            .set_status_message(GUI_TEXT.interactive.status.choose_output_directory.clone());
        return;
    }

    if let Some(range) = options.page_range.as_deref() {
        let known_page_counts: Vec<u32> = queue.iter().filter_map(|item| item.page_count).collect();
        if backend::validate_page_range_for_totals(range, &known_page_counts).is_err() {
            let message = "Invalid page range".to_string();
            state.write().set_status_message(message.clone());
            schedule_popup(state, PopupKind::Warning, message, 5);
            return;
        }
    }

    // Persist the target resolution as the preset: running a job with a
    // resolution saves it (restored on next launch); running without one
    // clears the stored preset. Replaces the old save/load preset buttons.
    if let Some(height) = options.target_height {
        let preset = ResolutionPreset {
            height,
            width: options.target_width,
        };
        if let Err(e) = backend::save_resolution_preset(&preset) {
            eprintln!("Failed to save resolution preset: {e}");
        }
    } else if let Err(e) = backend::clear_resolution_preset() {
        eprintln!("Failed to clear resolution preset: {e}");
    }

    {
        let mut s = state.write();
        s.is_processing = true;
        s.should_cancel = false;
        s.active_eta = None;
        s.progress_metrics = None;
        s.processing_items = queue
            .iter()
            .enumerate()
            .map(|(index, item)| ProcessingQueueDisplayItem {
                task_id: index as u64,
                path: item.file_path.clone(),
                status: "Queued".to_string(),
            })
            .collect();
        s.show_completion_popup = false;
        s.completion_popup = None;
        s.set_status_lines(
            GUI_TEXT.interactive.status.starting.clone(),
            fmt1(
                &GUI_TEXT.interactive.status.queued_files_for_processing,
                queue.len(),
            ),
            String::new(),
        );
    }

    spawn(async move {
        match backend::start_async_processing(queue, &options).await {
            Ok((tracker_infos, mut worker_handles, receiver)) => {
                {
                    let mut s = state.write();
                    s.active_task_ids = tracker_infos.iter().map(|info| info.id).collect();
                    s.processing_items = tracker_infos
                        .iter()
                        .map(|info| ProcessingQueueDisplayItem {
                            task_id: info.id,
                            path: info.input_path.clone(),
                            status: "Queued".to_string(),
                        })
                        .collect();
                }

                if options.layout_analysis || options.use_heavy_binarization {
                    schedule_popup(
                        state,
                        PopupKind::Hardware,
                        GUI_TEXT
                            .interactive
                            .status
                            .using_hardware_acceleration
                            .clone(),
                        3,
                    );
                }

                let files_total = tracker_infos.len();
                let mut files_completed = 0usize;
                let mut files_errored = 0usize;
                let mut handled: HashSet<u64> = HashSet::new();

                // Guard against empty queue (all spawns failed before we got here).
                if files_total == 0 {
                    let mut s = state.write();
                    s.is_processing = false;
                    s.active_task_ids.clear();
                    s.processing_items.clear();
                }

                loop {
                    // select! suspends the task — either a worker event wakes it or
                    // a 200 ms timer fires for the cancel check.  This avoids the
                    // tight yield_now() spin that starved the renderer.
                    tokio::select! {
                        // A worker event arrived.
                        result = receiver.recv_async() => {
                            let update = match result {
                                Ok(u) => u,
                                Err(_) => break, // all senders dropped — done
                            };
                            match update {
                            WorkerProgressUpdate::Status {
                                task_id,
                                status,
                                metrics,
                            } => {
                                {
                                    let mut s = state.write();
                                    s.progress_metrics = metrics;
                                    s.set_processing_item_status(task_id, "Processing");
                                }
                                match &status {
                                    WorkerProcessingStatus::AssemblingOutput => {}
                                    WorkerProcessingStatus::NoLayoutProgress { .. }
                                    | WorkerProcessingStatus::LayoutProgress { .. }
                                    | WorkerProcessingStatus::MarginProgress { .. }
                                    | WorkerProcessingStatus::EpubProgress { .. }
                                    | WorkerProcessingStatus::ReflowProgress { .. } => {
                                        let (l1, l2, l3) = status.to_gui_display_lines();
                                        state.write().set_status_lines(l1, l2, l3);
                                    }
                                    WorkerProcessingStatus::PdfAppend { .. }
                                    | WorkerProcessingStatus::PdfAppendMargin { .. } => {}
                                    _ => {
                                        let (l1, l2, l3) = status.to_gui_display_lines();
                                        state.write().push_status_log(l1);
                                        if !l2.is_empty() {
                                            state.write().push_status_log(l2);
                                        }
                                        if !l3.is_empty() {
                                            state.write().push_status_log(l3);
                                        }
                                    }
                                }

                                state.write().active_eta =
                                    metrics.and_then(|m| m.eta_seconds.map(format_eta_label));

                                if let WorkerProcessingStatus::FootnotesDetected { message } =
                                    &status
                                {
                                    schedule_popup(state, PopupKind::Footnotes, message.clone(), 5);
                                }
                                if let WorkerProcessingStatus::PipelineMessage { stage, message } =
                                    &status
                                {
                                    if stage == "GPU Warning" {
                                        schedule_popup(
                                            state,
                                            PopupKind::Warning,
                                            message.clone(),
                                            8,
                                        );
                                    }
                                }
                            }
                            WorkerProgressUpdate::Completed {
                                task_id,
                                message: _,
                                metrics: _,
                            } => {
                                if handled.insert(task_id) {
                                    refresh_appearance_theme(theme);
                                    files_completed += 1;
                                    state.write().set_processing_item_status(task_id, "Completed");
                                    if let Some(info) =
                                        tracker_infos.iter().find(|info| info.id == task_id)
                                    {
                                        let original_size =
                                            backend::calculate_path_size(&info.input_path);
                                        let compressed_size = fs::metadata(&info.output_path)
                                            .map(|m| m.len())
                                            .unwrap_or(0);
                                        let processing_result = ProcessingResult::new(
                                            info.input_path.clone(),
                                            info.output_path.clone(),
                                            original_size,
                                            compressed_size,
                                            options.page_range.is_some(),
                                        );

                                        if let Ok(entries) = logging::load_log_entries() {
                                            state.write().processing_log = entries;
                                        }

                                        let estimated_original_size =
                                            backend::estimate_original_size_for_page_range(
                                                &info.input_path,
                                                &options.page_range,
                                                original_size,
                                            )
                                            .await;
                                        let completion_text = completion_message(
                                            &processing_result,
                                            estimated_original_size,
                                        );
                                        schedule_popup(
                                            state,
                                            PopupKind::Complete,
                                            completion_text,
                                            5,
                                        );

                                        state
                                            .write()
                                            .queue
                                            .retain(|item| item.file_path != info.input_path);
                                    }

                                    let files_remaining =
                                        files_total.saturating_sub(files_completed);
                                    state.write().push_status_log(fmt2(
                                        &GUI_TEXT.interactive.status.file_completed_log,
                                        files_completed,
                                        files_remaining,
                                    ));
                                }
                            }
                            WorkerProgressUpdate::Error {
                                task_id,
                                error,
                                metrics: _,
                            } => {
                                if handled.insert(task_id) {
                                    refresh_appearance_theme(theme);
                                    files_completed += 1;
                                    files_errored += 1;
                                    state.write().set_processing_item_status(task_id, "Failed");
                                    if let Some(info) =
                                        tracker_infos.iter().find(|info| info.id == task_id)
                                    {
                                        // Persist all terminal failures. Previously
                                        // Renderer, Metal, encoder and permission errors
                                        // disappeared from the Log viewer.
                                        let original_size =
                                            backend::calculate_path_size(&info.input_path);
                                        let _ = logging::add_failed_log_entry(
                                            info.input_path.clone(),
                                            info.output_path.clone(),
                                            original_size,
                                            &error,
                                            &options,
                                        );
                                        if let Ok(entries) = logging::load_log_entries() {
                                            state.write().processing_log = entries;
                                        }

                                        state
                                            .write()
                                            .queue
                                            .retain(|item| item.file_path != info.input_path);
                                    }

                                    let dep_prefix = "Missing required component(s): ";
                                    if let Some(idx) = error.find(dep_prefix) {
                                        let after = &error[idx + dep_prefix.len()..];
                                        let list_part =
                                            if let Some(paren_idx) = after.find(" (ensure") {
                                                &after[..paren_idx]
                                            } else {
                                                after
                                            };
                                        let formatted = GUI_TEXT
                                            .interactive
                                            .status
                                            .missing_dependency
                                            .replace("{item}", list_part.trim());
                                        state.write().set_status_message(formatted);
                                    } else {
                                        state.write().set_status_message(fmt1(
                                            &GUI_TEXT.interactive.status.error,
                                            error.clone(),
                                        ));
                                    }

                                    // The status bar is a single line that later
                                    // events overwrite, and a Finder-launched
                                    // macOS app has no terminal — without a
                                    // popup a failed run can look like nothing
                                    // happened at all.
                                    let mut popup_text = error.clone();
                                    if popup_text.len() > 600 {
                                        let cut = popup_text
                                            .char_indices()
                                            .take_while(|(i, _)| *i < 600)
                                            .last()
                                            .map(|(i, c)| i + c.len_utf8())
                                            .unwrap_or(0);
                                        popup_text.truncate(cut);
                                        popup_text.push('…');
                                    }
                                    popup_text.push_str(&format!(
                                        "\n\nFull log: {}",
                                        crate::settings::get_worker_log_path().display()
                                    ));
                                    schedule_popup(state, PopupKind::Warning, popup_text, 12);
                                }
                            }
                        } // end match update
                        } // end recv_async arm

                        // Periodic cancel check — fires every 200ms when no events arrive.
                        _ = tokio::time::sleep(tokio::time::Duration::from_millis(200)) => {
                            if state.read().should_cancel {
                                for handle in worker_handles.iter_mut() {
                                    handle.kill();
                                }
                                let mut s = state.write();
                                s.is_processing = false;
                                s.should_cancel = false;
                                s.active_task_ids.clear();
                                s.processing_items.clear();
                                s.active_eta = None;
                                s.progress_metrics = None;
                                s.set_status_message(GUI_TEXT.interactive.status.ready.clone());
                                drop(s);
                                refresh_appearance_theme(theme);
                                break;
                            }
                        }
                    } // end select!

                    if files_completed >= files_total {
                        break;
                    }
                }

                {
                    refresh_appearance_theme(theme);
                    let mut s = state.write();
                    s.is_processing = false;
                    s.active_task_ids.clear();
                    s.processing_items.clear();
                    s.active_eta = None;
                    s.progress_metrics = None;
                    // Only overwrite the status with a success summary when every file
                    // actually succeeded. If any job reported an error, leave the error
                    // status message in place instead of masking it as "complete".
                    if files_errored == 0 {
                        let queue_remaining = s.queue.len();
                        let line3 = if queue_remaining > 0 {
                            fmt1(
                                &GUI_TEXT.interactive.status.files_remaining_in_queue,
                                queue_remaining,
                            )
                        } else {
                            GUI_TEXT.interactive.status.queue_empty.clone()
                        };
                        s.set_status_lines(
                            GUI_TEXT.interactive.status.complete.clone(),
                            fmt1(
                                &GUI_TEXT.interactive.status.files_processed_successfully,
                                files_total.saturating_sub(files_errored),
                            ),
                            line3,
                        );
                    }
                }
            }
            Err(e) => {
                refresh_appearance_theme(theme);
                let mut s = state.write();
                s.is_processing = false;
                s.active_task_ids.clear();
                s.processing_items.clear();
                s.active_eta = None;
                s.progress_metrics = None;
                s.set_status_message(fmt1(
                    &GUI_TEXT.interactive.status.failed_start_processing,
                    e,
                ));
            }
        }
    });
}

#[cfg(test)]
mod theme_picker_tests {
    use super::*;
    use crate::appearance::ThemeChoice;

    // The selection ring is [Default, Light 1..N, Dark 1..N]; these lock down the
    // exact adjacencies the two-row model promises.
    #[test]
    fn ring_walks_lights_then_darks() {
        let n = crate::sanzowada::THEME_COUNT;
        // Right from the last light lands on the first dark.
        assert_eq!(ring_step(n, 1), n + 1);
        // Left from the first light lands on Default.
        assert_eq!(ring_step(1, -1), 0);
        // Left from Default wraps to the last dark (furthest-right dark).
        assert_eq!(ring_step(0, -1), 2 * n);
        // Right from the last dark wraps back to Default.
        assert_eq!(ring_step(2 * n, 1), 0);
    }

    #[test]
    fn up_down_toggle_same_combo() {
        let n = crate::sanzowada::THEME_COUNT;
        // Up from the first dark -> the first light (same combo).
        assert_eq!(row_index(n + 1, false), 1);
        // Down from the first light -> the first dark.
        assert_eq!(row_index(1, true), n + 1);
        // Default has no dark; up/down are no-ops.
        assert_eq!(row_index(0, true), 0);
        assert_eq!(row_index(0, false), 0);
        // Already-light stays light on up; already-dark stays dark on down.
        assert_eq!(row_index(1, false), 1);
        assert_eq!(row_index(n + 1, true), n + 1);
    }

    #[test]
    fn choice_index_names_are_consistent() {
        let n = crate::sanzowada::THEME_COUNT;
        for i in 0..theme_total() {
            // Index -> choice -> index round-trips.
            assert_eq!(index_for_choice(theme_choice_at(i)), i, "roundtrip at {i}");
        }
        assert_eq!(theme_choice_at(0), ThemeChoice::Default);
        assert_eq!(
            theme_choice_at(1),
            ThemeChoice::Sanzo {
                index: 0,
                dark: false
            }
        );
        assert_eq!(
            theme_choice_at(n + 1),
            ThemeChoice::Sanzo {
                index: 0,
                dark: true
            }
        );
        assert_eq!(theme_name(0), "Default");
        assert_eq!(theme_name(1), "Sanzo Light 1");
        assert_eq!(theme_name(n + 1), "Sanzo Dark 1");
        assert_eq!(theme_name(2 * n), format!("Sanzo Dark {n}"));
    }

    #[test]
    fn music_resolution_tooltip_uses_music_default() {
        let tooltip = music_target_height_tooltip();
        assert!(!tooltip.contains("1200"));
        assert!(tooltip.contains(&DEFAULT_MUSIC_TARGET_HEIGHT.to_string()));
    }
}
