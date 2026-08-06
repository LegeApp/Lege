use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};
use winit::window::{CursorIcon, Fullscreen, Window, WindowId};

use crate::chrome::{AppLayout, ScrollbarGeometry, ScrollbarState, StatusState};
use crate::damage::DamageRegion;
use crate::diagnostics::{FrameMetrics, SeekTrace};
use crate::document::engine::DocumentEngine;
use crate::document::layout::PageLayoutIndex;
use crate::document::session::{SessionUpdate, UpdateQueue};
use crate::document::synthetic::SyntheticEngine;
use crate::document::{
    CacheCategory, ColorMode, ConductorHandle, DocumentLink, LinkTarget, MemoryArbiter,
    MemoryLease, NavigationMode, OutlineNode, PageIndex, PagePreviewCache, PageStructure,
    TileCache, TileFrameSnapshot, TileTier, ViewportIntent, ViewportPlanner, WarmHint, WarmReason,
};
use crate::event::ViewerEvent;
use crate::frame::FrameScheduler;
use crate::geometry::{PointF, RectF, RectI, SizeF, Vec2d};
use crate::input::{HitTarget, InputState, PointerCapture, ScrollbarDragState, ScrollbarPart};
use crate::paint::scroll_exposed_regions;
use crate::present::{Presenter, PresenterBackend, PresenterPreference, ScrollReuse};
use crate::processing::{
    self, Binarization, CoverMode, ImageProcessing, MarginMode, OcrMode, OutputFormat,
    ProcessingOptions, ProcessingProfile, ProcessingRequest, ProcessingScope, ProcessingUpdate,
    TextCompression,
};
use crate::scene::{FrameScene, ImageSampling, SceneBuilder, SceneSurface};
use crate::scroll::{
    DocumentLocation, NavigationHistory, PagingDirection, ReadingAnchor, ScrollCommand, ScrollMode,
    ScrollModel, notional_page_lines, paging_target,
};
use crate::settings::ViewerSettings;
use crate::text::{
    OutlineSynthesizer, SearchHit, SearchIndex, SearchService, SelectionModel, TextSubstrate,
    hit_test,
};
use crate::theme::Theme;
use crate::ui::{RectPaint, SystemClipboard, TextPaint, UiTextRenderer};

#[derive(Debug)]
struct PageViewArtifacts {
    text: Arc<TextSubstrate>,
    structure: PageStructure,
    _operation_count: usize,
    _lowering_degraded: bool,
    _memory_lease: MemoryLease,
}

const LINK_PEEK_DELAY: Duration = Duration::from_millis(400);

#[derive(Debug, Clone)]
struct LinkHoverState {
    source_page: PageIndex,
    link: DocumentLink,
    started: Instant,
    peek_visible: bool,
}

impl LinkHoverState {
    fn same_link(&self, page: PageIndex, link: &DocumentLink) -> bool {
        self.source_page == page && self.link == *link
    }

    fn peek_deadline(&self) -> Option<Instant> {
        matches!(self.link.target, LinkTarget::Internal { .. })
            .then_some(self.started + LINK_PEEK_DELAY)
            .filter(|_| !self.peek_visible)
    }
}

#[derive(Debug, Clone, Copy)]
struct LinkPeekView {
    target_page: PageIndex,
    target_region: Option<RectF>,
    pointer: PointF,
}

#[derive(Debug, Default)]
struct SearchUiState {
    open: bool,
    query: String,
    cursor: usize,
    selection_anchor: Option<usize>,
    preedit: String,
    hits: Vec<SearchHit>,
    active: Option<usize>,
    capped: bool,
    pending: bool,
    indexed_pages: u32,
    total_pages: u32,
}

#[derive(Debug, Clone)]
struct ProcessingUiState {
    visible: bool,
    profile: ProcessingProfile,
    tab: ProcessingTab,
    open_option: Option<usize>,
    resolution_editing: bool,
    resolution_buffer: String,
    options: ProcessingOptions,
    scope: ProcessingScope,
    running: bool,
    title: String,
    detail: String,
    original: Option<std::path::PathBuf>,
    output: Option<std::path::PathBuf>,
    result_visible: bool,
    result_ready: bool,
    viewing_new: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessingTab {
    Output,
    Recognition,
    Page,
}

impl ProcessingTab {
    const ALL: [Self; 3] = [Self::Output, Self::Recognition, Self::Page];

    const fn label(self) -> &'static str {
        match self {
            Self::Output => "Output",
            Self::Recognition => "Recognition",
            Self::Page => "Page + geometry",
        }
    }
}

/// Page tab row holding the mutually exclusive Crop / Center / Reflow
/// segment. Mirrors the margin trio of the earlier Freya GUI.
const LAYOUT_ROW: usize = 1;
/// Page tab row holding the editable target-resolution field.
const RESOLUTION_ROW: usize = 5;
const LAYOUT_SEGMENTS: [&str; 3] = ["Crop", "Center", "Reflow"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessingPanelAction {
    Run,
    ToggleProfile,
    Tab(ProcessingTab),
    Option(usize),
    Choice { option: usize, choice: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HoverControl {
    Toolbar(ToolbarAction),
    Processing(ProcessingPanelAction),
    Appearance(ColorMode),
    Result(bool),
}

impl Default for ProcessingUiState {
    fn default() -> Self {
        Self {
            visible: false,
            profile: ProcessingProfile::Reading,
            tab: ProcessingTab::Output,
            open_option: None,
            resolution_editing: false,
            resolution_buffer: "1200".to_owned(),
            options: ProcessingOptions::default(),
            scope: ProcessingScope::Document,
            running: false,
            title: "Ready to process".to_owned(),
            detail: "Choose Process for the whole document, or select the current page first."
                .to_owned(),
            original: None,
            output: None,
            result_visible: false,
            result_ready: false,
            viewing_new: false,
        }
    }
}

struct ChromeSurfacePlacement {
    surface: Arc<SceneSurface>,
    destination: RectF,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolbarAction {
    OpenDocument,
    ZoomOut,
    ZoomIn,
    FitWidth,
    FitPage,
    ToggleSidebar,
    ToggleTrim,
    ToggleProcessing,
    ToggleOptions,
}

// Toolbar groups are laid out left to right at fixed widths. Hit testing and
// surface placement both derive from these origins so the two cannot drift.
const OPEN_GROUP_X: f64 = 0.0;
const OPEN_GROUP_WIDTH: f64 = 64.0;
const ZOOM_GROUP_X: f64 = OPEN_GROUP_X + OPEN_GROUP_WIDTH;
const ZOOM_GROUP_WIDTH: f64 = 214.0;
const DOCUMENT_GROUP_X: f64 = ZOOM_GROUP_X + ZOOM_GROUP_WIDTH;
const DOCUMENT_GROUP_WIDTH: f64 = 140.0;
const PROCESS_GROUP_X: f64 = DOCUMENT_GROUP_X + DOCUMENT_GROUP_WIDTH;
const PROCESS_GROUP_WIDTH: f64 = 84.0;
const OPTIONS_GROUP_X: f64 = PROCESS_GROUP_X + PROCESS_GROUP_WIDTH;
const OPTIONS_GROUP_WIDTH: f64 = 96.0;
const SEARCH_FIELD_X: f64 = OPTIONS_GROUP_X + OPTIONS_GROUP_WIDTH + 10.0;

fn toolbar_action_at(x: f64) -> Option<ToolbarAction> {
    match x {
        x if x < ZOOM_GROUP_X => Some(ToolbarAction::OpenDocument),
        x if x < ZOOM_GROUP_X + 34.0 => Some(ToolbarAction::ZoomOut),
        x if x < ZOOM_GROUP_X + 68.0 => Some(ToolbarAction::ZoomIn),
        x if x < ZOOM_GROUP_X + 146.0 => Some(ToolbarAction::FitWidth),
        x if x < ZOOM_GROUP_X + 214.0 => Some(ToolbarAction::FitPage),
        x if x < DOCUMENT_GROUP_X + 78.0 => Some(ToolbarAction::ToggleSidebar),
        x if x < DOCUMENT_GROUP_X + 140.0 => Some(ToolbarAction::ToggleTrim),
        x if x < PROCESS_GROUP_X + PROCESS_GROUP_WIDTH => Some(ToolbarAction::ToggleProcessing),
        x if x < OPTIONS_GROUP_X + OPTIONS_GROUP_WIDTH => Some(ToolbarAction::ToggleOptions),
        _ => None,
    }
}

fn theme_for_mode(mode: ColorMode) -> Theme {
    match mode {
        ColorMode::Original => Theme::light(),
        ColorMode::Night => Theme::night(),
        ColorMode::WarmPaper => Theme::warm(),
        ColorMode::SanzoEarth => Theme::sanzo_earth(),
        ColorMode::SanzoSea => Theme::sanzo_sea(),
    }
}

fn shade_color(color: u32, factor: f32) -> u32 {
    let channel = |shift: u32| {
        (((color >> shift) & 0xff) as f32 * factor)
            .round()
            .clamp(0.0, 255.0) as u32
    };
    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

fn mix_color(top: u32, bottom: u32, amount: f32) -> u32 {
    let channel = |shift: u32| {
        let a = ((top >> shift) & 0xff) as f32;
        let b = ((bottom >> shift) & 0xff) as f32;
        (a + (b - a) * amount).round().clamp(0.0, 255.0) as u32
    };
    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}

fn color_luminance(color: u32) -> f32 {
    let linear = |shift: u32| {
        let channel = ((color >> shift) & 0xff) as f32 / 255.0;
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(16) + 0.7152 * linear(8) + 0.0722 * linear(0)
}

fn contrast_text_color(background: u32) -> u32 {
    let luminance = color_luminance(background);
    let white_contrast = 1.05 / (luminance + 0.05);
    let black_contrast = (luminance + 0.05) / 0.05;
    if white_contrast >= black_contrast {
        0x00ff_ffff
    } else {
        0x0010_1010
    }
}

fn button_bounds(x: i32, y: i32, width: u32, toolbar_height: u32) -> RectI {
    let original_height = toolbar_height.saturating_sub(10).min(246);
    let height = ((original_height as f32 * 0.70).round() as u32).max(12);
    RectI {
        x,
        y: y + original_height.saturating_sub(height) as i32 / 2,
        width,
        height,
    }
}

fn hover_adjusted_color(color: u32, hovered: bool, dark_mode: bool) -> u32 {
    if !hovered {
        return color;
    }
    shade_color(color, if dark_mode { 1.08 } else { 0.92 })
}

fn centered_button_text(
    text: impl Into<String>,
    bounds: RectI,
    size: f32,
    color: u32,
    bold: bool,
) -> TextPaint {
    let text = text.into();
    let line_height = (size * 1.35).ceil();
    TextPaint {
        text,
        x: bounds.x + 4,
        y: bounds.y + ((bounds.height as f32 - line_height).max(0.0) * 0.5).round() as i32,
        max_width: bounds.width.saturating_sub(8),
        size,
        color,
        bold,
        centered: true,
    }
}

/// Raised gradient controls with the same lit lip, dark edge and offset shadow
/// as the sibling image-viewer.
fn button_paint(x: i32, y: i32, width: u32, toolbar_height: u32, color: u32) -> Vec<RectPaint> {
    let bounds = button_bounds(x, y, width, toolbar_height);
    let x = bounds.x;
    let y = bounds.y;
    let height = bounds.height;
    let mut paint = Vec::with_capacity(height as usize + 6);
    paint.push(RectPaint {
        rect: RectI {
            x: x + 2,
            y: y + 2,
            width,
            height,
        },
        color: 0x0030_3030,
    });
    let top = shade_color(color, 1.28);
    let bottom = shade_color(color, 0.78);
    for row in 0..height {
        paint.push(RectPaint {
            rect: RectI {
                x,
                y: y + row as i32,
                width,
                height: 1,
            },
            color: mix_color(
                top,
                bottom,
                row as f32 / height.saturating_sub(1).max(1) as f32,
            ),
        });
    }
    let light = shade_color(top, 1.2);
    let dark = shade_color(bottom, 0.45);
    paint.extend([
        RectPaint {
            rect: RectI {
                x,
                y,
                width,
                height: 1,
            },
            color: light,
        },
        RectPaint {
            rect: RectI {
                x,
                y,
                width: 1,
                height,
            },
            color: light,
        },
        RectPaint {
            rect: RectI {
                x,
                y: y + height.saturating_sub(1) as i32,
                width,
                height: 1,
            },
            color: dark,
        },
        RectPaint {
            rect: RectI {
                x: x + width.saturating_sub(1) as i32,
                y,
                width: 1,
                height,
            },
            color: dark,
        },
    ]);
    paint
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ZoomMode {
    Automatic,
    FitWidth,
    FitPage,
    Manual,
}

#[allow(missing_debug_implementations)]
pub struct ViewerApp {
    engine: Arc<dyn DocumentEngine>,
    layout: Arc<PageLayoutIndex>,
    planner: ViewportPlanner,
    conductor: ConductorHandle,
    updates: Arc<UpdateQueue>,
    memory: MemoryArbiter,
    tiles: Arc<TileCache>,
    previews: Arc<PagePreviewCache>,
    tile_snapshot: TileFrameSnapshot,
    tile_scratch: Vec<Arc<crate::document::TileSurface>>,
    painted_tiles: HashSet<crate::document::TileKey>,
    page_artifacts: HashMap<PageIndex, PageViewArtifacts>,
    page_errors: HashMap<PageIndex, String>,
    search: SearchIndex,
    search_service: SearchService,
    search_request: u64,
    search_index_revision: u64,
    search_ui: SearchUiState,
    processing_ui: ProcessingUiState,
    processing_panel_width: f64,
    processing_panel_height: f64,
    options_visible: bool,
    processing_proxy: Option<EventLoopProxy<ViewerEvent>>,
    processing_control: Option<processing::ProcessingControl>,
    selection: SelectionModel,
    outline_synthesizer: OutlineSynthesizer,
    outline: Arc<[OutlineNode]>,
    clipboard: SystemClipboard,
    ui_text: UiTextRenderer,
    last_click: Option<(Instant, PointF, u8)>,
    history: NavigationHistory,
    pointer_warm: Option<(PageIndex, WarmReason)>,
    hovered_link: Option<LinkHoverState>,

    window: Option<Arc<Window>>,
    presenter: Option<Box<dyn Presenter>>,
    presenter_preference: PresenterPreference,
    scene: FrameScene,
    pending_scroll_reuse: Option<ScrollReuse>,
    surface_suspended: bool,
    damage: DamageRegion,
    frame_damage: Vec<RectI>,
    frame: FrameScheduler,
    metrics: FrameMetrics,
    seek_trace: SeekTrace,
    _gpu_memory_lease: Option<MemoryLease>,
    gpu_memory_bytes: u64,
    gpu_diagnostics: bool,
    seek_diagnostics: bool,
    last_gpu_diagnostics: Instant,

    theme: Theme,
    app_layout: AppLayout,
    input: InputState,
    hovered_control: Option<HoverControl>,
    scrollbar: ScrollbarState,
    status: StatusState,
    scroll: ScrollModel,
    presented_scroll: Vec2d,
    zoom: f64,
    zoom_mode: ZoomMode,
    scale_factor: f64,
    sidebar_visible: bool,
    outline_scroll_start: Option<usize>,
    fullscreen: bool,
    generation: u64,
    intent: ViewportIntent,
    intent_dirty: bool,
    navigation_mode: NavigationMode,
    navigation_settle_deadline: Option<Instant>,
    trim_enabled: bool,
    color_mode: ColorMode,
    content_extents: Vec<Option<RectF>>,
    render_variant: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ViewerStartError {
    #[error("failed to start the document conductor: {0}")]
    Conductor(#[from] std::io::Error),
    #[error("failed to start the document search worker: {0}")]
    Search(std::io::Error),
}

fn create_presenter(
    preference: PresenterPreference,
    window: Arc<Window>,
    width: u32,
    height: u32,
) -> Result<Box<dyn Presenter>, String> {
    match preference {
        PresenterPreference::Auto => match create_gpu_presenter(window.clone(), width, height) {
            Ok(presenter) => {
                let stats = presenter.stats();
                eprintln!(
                    "Lege Viewer GPU presenter: {} ({})",
                    stats.adapter.as_deref().unwrap_or("unknown adapter"),
                    stats.backend.as_deref().unwrap_or("unknown backend")
                );
                Ok(presenter)
            }
            Err(gpu_error) => {
                eprintln!("GPU presenter unavailable; using softbuffer: {gpu_error}");
                create_software_presenter(window, width, height)
            }
        },
        PresenterPreference::Gpu => create_gpu_presenter(window, width, height),
        PresenterPreference::Software => create_software_presenter(window, width, height),
    }
}

#[cfg(feature = "wgpu-presenter")]
fn create_gpu_presenter(
    window: Arc<Window>,
    width: u32,
    height: u32,
) -> Result<Box<dyn Presenter>, String> {
    crate::present::wgpu::WgpuPresenter::new(window, width, height)
        .map(|presenter| Box::new(presenter) as Box<dyn Presenter>)
        .map_err(|error| format!("failed to create GPU presenter: {error}"))
}

#[cfg(not(feature = "wgpu-presenter"))]
fn create_gpu_presenter(
    _window: Arc<Window>,
    _width: u32,
    _height: u32,
) -> Result<Box<dyn Presenter>, String> {
    Err("this build does not include the GPU presenter".to_owned())
}

#[cfg(feature = "softbuffer-presenter")]
fn create_software_presenter(
    window: Arc<Window>,
    _width: u32,
    _height: u32,
) -> Result<Box<dyn Presenter>, String> {
    crate::present::softbuffer::SoftbufferPresenter::new(window)
        .map(|presenter| Box::new(presenter) as Box<dyn Presenter>)
        .map_err(|error| format!("failed to create software presenter: {error}"))
}

#[cfg(not(feature = "softbuffer-presenter"))]
fn create_software_presenter(
    _window: Arc<Window>,
    _width: u32,
    _height: u32,
) -> Result<Box<dyn Presenter>, String> {
    Err("this build does not include the software presenter".to_owned())
}

impl ViewerApp {
    pub fn new(
        engine: Arc<dyn DocumentEngine>,
        updates: Arc<UpdateQueue>,
    ) -> Result<Self, ViewerStartError> {
        Self::new_with_presenter(engine, updates, PresenterPreference::Auto)
    }

    pub fn new_with_presenter(
        engine: Arc<dyn DocumentEngine>,
        updates: Arc<UpdateQueue>,
        presenter_preference: PresenterPreference,
    ) -> Result<Self, ViewerStartError> {
        let settings = ViewerSettings::load();
        let theme = theme_for_mode(settings.color_mode);
        let content_extents = vec![None; engine.descriptor().page_count as usize];
        let layout = Arc::new(PageLayoutIndex::build_with_options(
            &engine.descriptor().page_geometries,
            &content_extents,
            settings.trim_enabled,
            settings.color_mode,
            1,
            &theme.metrics,
        ));
        let memory = MemoryArbiter::new(1024 * 1024 * 1024);
        let tiles = Arc::new(TileCache::new(engine.descriptor().id, memory.clone()));
        let tile_snapshot = tiles.frame_snapshot();
        let conductor = ConductorHandle::spawn(
            engine.clone(),
            layout.clone(),
            updates.clone(),
            memory.clone(),
            tiles.clone(),
        )?;
        let previews = conductor.previews();
        let search_service =
            SearchService::spawn(updates.clone()).map_err(ViewerStartError::Search)?;
        let zero_layout = AppLayout::calculate(SizeF::default(), 1.0, false, &theme.metrics);
        Ok(Self {
            engine: engine.clone(),
            layout,
            planner: ViewportPlanner::default(),
            conductor,
            updates,
            memory: memory.clone(),
            tiles,
            previews,
            tile_snapshot,
            tile_scratch: Vec::with_capacity(16),
            painted_tiles: HashSet::with_capacity(64),
            page_artifacts: HashMap::new(),
            page_errors: HashMap::new(),
            search: SearchIndex::with_memory(memory.clone()),
            search_service,
            search_request: 0,
            search_index_revision: 0,
            search_ui: SearchUiState {
                total_pages: engine.descriptor().page_count,
                ..SearchUiState::default()
            },
            processing_ui: ProcessingUiState {
                original: engine.source_path().map(std::path::Path::to_path_buf),
                // With nothing to read yet, the processing workspace is the
                // most useful thing to show, so it starts expanded.
                visible: engine.descriptor().page_count == 0,
                ..ProcessingUiState::default()
            },
            processing_panel_width: 540.0,
            processing_panel_height: 370.0,
            options_visible: false,
            processing_proxy: None,
            processing_control: None,
            selection: SelectionModel::default(),
            outline_synthesizer: OutlineSynthesizer::default(),
            outline: Arc::clone(&engine.descriptor().outline),
            clipboard: SystemClipboard::new(),
            ui_text: UiTextRenderer::new(),
            last_click: None,
            history: NavigationHistory::default(),
            pointer_warm: None,
            hovered_link: None,
            window: None,
            presenter: None,
            presenter_preference,
            scene: FrameScene::new(1, 1),
            pending_scroll_reuse: None,
            surface_suspended: false,
            damage: DamageRegion::new(1, 1),
            frame_damage: Vec::with_capacity(8),
            frame: FrameScheduler::new(),
            metrics: FrameMetrics::default(),
            seek_trace: SeekTrace::default(),
            _gpu_memory_lease: None,
            gpu_memory_bytes: 0,
            gpu_diagnostics: matches!(
                std::env::var("LEGE_VIEWER_GPU_DIAGNOSTICS").as_deref(),
                Ok("1")
            ),
            seek_diagnostics: matches!(
                std::env::var("LEGE_VIEWER_SEEK_DIAGNOSTICS").as_deref(),
                Ok("1")
            ),
            last_gpu_diagnostics: Instant::now() - Duration::from_secs(1),
            theme,
            app_layout: zero_layout,
            input: InputState::default(),
            hovered_control: None,
            scrollbar: ScrollbarState::default(),
            status: StatusState::default(),
            scroll: ScrollModel::new(),
            presented_scroll: Vec2d::ZERO,
            zoom: 1.0,
            zoom_mode: ZoomMode::Automatic,
            scale_factor: 1.0,
            sidebar_visible: false,
            outline_scroll_start: None,
            fullscreen: false,
            generation: 1,
            intent: ViewportIntent::empty(),
            intent_dirty: true,
            navigation_mode: NavigationMode::Idle,
            navigation_settle_deadline: None,
            trim_enabled: settings.trim_enabled,
            color_mode: settings.color_mode,
            content_extents,
            render_variant: 1,
        })
    }

    pub fn synthetic(updates: Arc<UpdateQueue>) -> Result<Self, ViewerStartError> {
        Self::new(Arc::new(SyntheticEngine::new(10_000)), updates)
    }

    /// Connect the desktop-only processing worker to this application's event
    /// loop. Keeping construction independent of winit makes viewer tests and
    /// headless renderer tools stay lightweight.
    pub fn set_event_proxy(&mut self, proxy: EventLoopProxy<ViewerEvent>) {
        self.processing_proxy = Some(proxy);
    }

    fn create_window(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
        if self.window.is_some() {
            return Ok(());
        }
        let attributes = Window::default_attributes()
            .with_title(format!("Lege — {}", self.engine.descriptor().display_name))
            .with_inner_size(PhysicalSize::new(1280, 900))
            .with_min_inner_size(PhysicalSize::new(640, 420));
        let window = Arc::new(
            event_loop
                .create_window(attributes)
                .map_err(|error| format!("failed to create Lege window: {error}"))?,
        );
        self.scale_factor = window.scale_factor();
        let size = window.inner_size();
        self.resize(size.width, size.height, true);

        self.presenter = Some(create_presenter(
            self.presenter_preference,
            window.clone(),
            size.width.max(1),
            size.height.max(1),
        )?);
        self.window = Some(window);
        self.request_redraw();
        Ok(())
    }

    fn resize(&mut self, width: u32, height: u32, initial: bool) {
        let anchor = self.reading_anchor();
        self.surface_suspended = width == 0 || height == 0;
        self.scene.resize(width.max(1), height.max(1));
        self.damage.resize(width.max(1), height.max(1));
        self.pending_scroll_reuse = None;
        if !self.surface_suspended
            && let Some(presenter) = self.presenter.as_mut()
            && let Err(error) = presenter.resize(width, height)
        {
            eprintln!("viewer presenter resize deferred after error: {error}");
        }
        self.app_layout = AppLayout::calculate(
            SizeF {
                width: f64::from(width),
                height: f64::from(height),
            },
            self.scale_factor,
            self.sidebar_visible,
            &self.theme.metrics,
        );
        if initial || self.zoom_mode != ZoomMode::Manual {
            self.zoom = self.zoom_for_mode(self.zoom_mode);
        }
        self.update_scroll_extents();
        self.restore_anchor(anchor);
        self.bump_generation();
    }

    fn update_scroll_extents(&mut self) {
        self.scroll.set_extents(
            SizeF {
                width: self.layout.total_width * self.zoom,
                height: self.layout.total_height * self.zoom,
            },
            SizeF {
                width: self.app_layout.canvas.width,
                height: self.app_layout.canvas.height,
            },
        );
    }

    fn fit_width_zoom(&self) -> f64 {
        if self.layout.total_width <= 0.0 {
            1.0
        } else {
            (self.app_layout.canvas.width / self.layout.total_width).clamp(0.05, 12.0)
        }
    }

    fn fit_page_zoom(&self) -> f64 {
        let page = self
            .status
            .current_page
            .and_then(|page| self.layout.placement(page))
            .or_else(|| self.layout.placements().first());
        page.map_or(1.0, |placement| {
            fit_page_scale(
                SizeF {
                    width: self.app_layout.canvas.width,
                    height: self.app_layout.canvas.height,
                },
                SizeF {
                    width: placement.bounds.width,
                    height: placement.bounds.height,
                },
                self.theme.metrics.canvas_margin,
            )
        })
    }

    fn automatic_zoom(&self) -> f64 {
        // One zoom bucket above full-page fit is a comfortable reading size
        // on a landscape monitor, while fit-width remains the hard upper
        // bound on narrow windows and landscape pages.
        automatic_zoom_scale(self.fit_page_zoom(), self.fit_width_zoom())
    }

    fn zoom_for_mode(&self, mode: ZoomMode) -> f64 {
        match mode {
            ZoomMode::Automatic => self.automatic_zoom(),
            ZoomMode::FitWidth => self.fit_width_zoom(),
            ZoomMode::FitPage => self.fit_page_zoom(),
            ZoomMode::Manual => self.zoom,
        }
    }

    fn document_origin_x(&self) -> f64 {
        centered_document_origin_x(self.app_layout.canvas, self.layout.total_width * self.zoom)
    }

    fn viewport_document(&self) -> RectF {
        RectF {
            x: self.scroll.position.x / self.zoom,
            y: self.scroll.position.y / self.zoom,
            width: self.app_layout.canvas.width / self.zoom,
            height: self.app_layout.canvas.height / self.zoom,
        }
    }

    fn reading_anchor(&self) -> Option<ReadingAnchor> {
        ReadingAnchor::capture(&self.layout, self.viewport_document())
    }

    fn restore_anchor(&mut self, anchor: Option<ReadingAnchor>) {
        if let Some(anchor) = anchor
            && let Some(document_y) = anchor.restore(&self.layout, self.viewport_document().height)
        {
            self.scroll.apply(ScrollCommand::SetAbsolute(Vec2d {
                x: self.scroll.position.x,
                y: document_y.max(0.0) * self.zoom,
            }));
        }
    }

    fn set_zoom(&mut self, zoom: f64, mode: ZoomMode) {
        let anchor = self.reading_anchor();
        self.zoom = zoom.clamp(0.05, 12.0);
        self.zoom_mode = mode;
        self.update_scroll_extents();
        self.restore_anchor(anchor);
        self.navigation_mode = NavigationMode::Idle;
        self.navigation_settle_deadline = None;
        self.bump_generation();
    }

    fn record_content_extent(&mut self, page: PageIndex, structure: &PageStructure) -> bool {
        let Some(slot) = self.content_extents.get_mut(page.0 as usize) else {
            return false;
        };
        let extent = (structure.content_extent.source
            == crate::document::ContentExtentSource::DisplayList)
            .then_some(structure.content_extent.rect);
        if *slot == extent {
            return false;
        }
        *slot = extent;
        true
    }

    fn rebuild_stage5_layout(&mut self) {
        let anchor = self.reading_anchor();
        self.render_variant = self.render_variant.wrapping_add(1);
        self.layout = Arc::new(PageLayoutIndex::build_with_options(
            &self.engine.descriptor().page_geometries,
            &self.content_extents,
            self.trim_enabled,
            self.color_mode,
            self.render_variant,
            &self.theme.metrics,
        ));
        self.conductor.publish_layout(self.layout.clone());
        if self.zoom_mode != ZoomMode::Manual {
            self.zoom = self.zoom_for_mode(self.zoom_mode).clamp(0.05, 12.0);
        }
        self.update_scroll_extents();
        self.restore_anchor(anchor);
        self.bump_generation();
    }

    fn save_stage5_settings(&self) {
        ViewerSettings {
            trim_enabled: self.trim_enabled,
            color_mode: self.color_mode,
        }
        .save_async();
    }

    fn toggle_trim(&mut self) {
        self.trim_enabled = !self.trim_enabled;
        self.save_stage5_settings();
        self.rebuild_stage5_layout();
    }

    fn cycle_color_mode(&mut self) {
        self.set_color_mode(self.color_mode.next());
    }

    fn set_color_mode(&mut self, color_mode: ColorMode) {
        if self.color_mode == color_mode {
            return;
        }
        self.color_mode = color_mode;
        self.theme = theme_for_mode(color_mode);
        self.save_stage5_settings();
        self.rebuild_stage5_layout();
    }

    fn bump_generation(&mut self) {
        self.clear_link_hover();
        self.generation = self.generation.wrapping_add(1);
        self.seek_trace.begin(
            self.generation,
            self.metrics.input_received.unwrap_or_else(Instant::now),
        );
        self.intent_dirty = true;
        self.damage.mark_full();
        self.frame.interactive();
        self.request_redraw();
    }

    fn request_redraw(&mut self) {
        if self.frame.request_redraw() {
            self.metrics.redraw_requested = Some(Instant::now());
        }
        // Always forward to the window: winit coalesces duplicate requests,
        // and gating on the scheduler flag can wedge the UI forever if the OS
        // drops a single RedrawRequested (observed during presenter fallback
        // and modal dialogs on Windows).
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn publish_intent_if_needed(&mut self) {
        if !self.intent_dirty {
            return;
        }
        let hover_page = self
            .scrollbar
            .hover_document_fraction
            .and_then(|fraction| self.page_at_document_fraction(fraction));
        self.intent = self.planner.build_with_navigation(
            self.generation,
            &self.layout,
            self.scroll.position,
            self.scroll.velocity,
            SizeF {
                width: self.app_layout.canvas.width,
                height: self.app_layout.canvas.height,
            },
            self.zoom,
            hover_page,
            self.navigation_mode,
        );
        self.conductor.publish_intent(self.intent.clone());
        self.refresh_tile_snapshot();
        self.seek_trace.mark_intent_published();
        self.intent_dirty = false;
    }

    fn refresh_tile_snapshot(&mut self) {
        let pages = self
            .intent
            .visible_tiles
            .iter()
            .chain(self.intent.overscan_tiles.iter())
            .chain(self.intent.final_prefetch_tiles.iter())
            .map(|demand| demand.page)
            .chain(self.intent.thumbnail_pages.iter().copied())
            .chain(self.intent.hover_page)
            .collect::<HashSet<_>>();
        self.tile_snapshot = self.tiles.frame_snapshot_for_pages(pages);
    }

    fn page_at_document_fraction(&self, fraction: f64) -> Option<PageIndex> {
        self.layout
            .page_at_y(self.layout.total_height * fraction.clamp(0.0, 1.0))
            .map(|placement| placement.page)
    }

    fn drain_updates(&mut self) {
        let mut any_visible_change = false;
        let mut tile_pages_changed = HashSet::new();
        let mut trim_extents_changed = false;
        for update in self.updates.drain() {
            match update {
                SessionUpdate::PageCompiled(update) => {
                    trim_extents_changed |=
                        self.record_content_extent(update.page, &update.structure);
                    self.ingest_index_page(
                        update.page,
                        Arc::clone(&update.text),
                        Some(update.memory_lease.clone()),
                    );
                    self.page_artifacts.insert(
                        update.page,
                        PageViewArtifacts {
                            text: update.text,
                            structure: update.structure,
                            _operation_count: update.operation_count,
                            _lowering_degraded: update.lowering_degraded,
                            _memory_lease: update.memory_lease,
                        },
                    );
                    any_visible_change = true;
                }
                SessionUpdate::PageIndexed {
                    page,
                    text,
                    structure,
                } => {
                    trim_extents_changed |= self.record_content_extent(page, &structure);
                    self.ingest_index_page(page, text, None);
                    any_visible_change |= self.search_ui.open || self.sidebar_visible;
                }
                SessionUpdate::TextIndexProgress {
                    indexed_pages,
                    total_pages,
                } => {
                    self.search_ui.indexed_pages = indexed_pages;
                    self.search_ui.total_pages = total_pages;
                    if indexed_pages >= total_pages && self.engine.descriptor().outline.is_empty() {
                        self.outline = self.outline_synthesizer.finish(total_pages);
                    }
                    any_visible_change |= self.search_ui.open || self.sidebar_visible;
                }
                SessionUpdate::SearchCompleted {
                    request,
                    index_revision,
                    hits,
                    capped,
                } => {
                    if request == self.search_request {
                        if index_revision < self.search_index_revision {
                            self.refresh_search_results();
                        } else {
                            self.apply_search_results(hits.iter().cloned().collect(), capped);
                            self.search_ui.pending = false;
                        }
                        any_visible_change = true;
                    }
                }
                SessionUpdate::TileReady { key, generation: _ } => {
                    if self
                        .intent
                        .raster_tile_is_relevant(key.page, key.bucket, key.coord, key.tier)
                    {
                        any_visible_change = true;
                        tile_pages_changed.insert(key.page);
                        self.seek_trace.mark_pixels_ready();
                    }
                }
                SessionUpdate::PreviewReady { page } => {
                    let relevant = self.intent.page_is_relevant(page)
                        || self.intent.thumbnail_page_is_relevant(page);
                    any_visible_change |= relevant;
                    if relevant {
                        self.seek_trace.mark_pixels_ready();
                    }
                }
                SessionUpdate::PageError {
                    page,
                    message,
                    quarantined: _,
                } => {
                    self.page_errors.insert(page, message);
                    any_visible_change = true;
                }
                SessionUpdate::QueueDepths {
                    compile_pending,
                    raster_pending,
                    in_flight,
                } => {
                    self.metrics.compile_pending = compile_pending;
                    self.metrics.raster_pending = raster_pending;
                    self.metrics.in_flight = in_flight;
                }
            }
        }
        if trim_extents_changed && self.trim_enabled {
            // One rebuild per drained update batch: visible pages refine
            // immediately while the background sweep naturally coalesces.
            self.rebuild_stage5_layout();
            any_visible_change = true;
        }
        if !tile_pages_changed.is_empty() {
            self.refresh_tile_snapshot();
        }
        self.evict_page_artifacts();
        if any_visible_change {
            self.damage.mark_full();
            self.request_redraw();
        }
    }

    fn ingest_index_page(
        &mut self,
        page: PageIndex,
        text: Arc<TextSubstrate>,
        existing_lease: Option<MemoryLease>,
    ) {
        let replacing = self.search.contains_page(page);
        self.outline_synthesizer.insert(page, &text);
        self.search.insert_with_lease(page, text, existing_lease);
        self.search_index_revision = self.search_index_revision.wrapping_add(1);
        if self.search_ui.open && !self.search_ui.query.is_empty() {
            if replacing {
                self.refresh_search_results();
            } else {
                self.extend_search_results(page);
            }
        }
    }

    fn extend_search_results(&mut self, page: PageIndex) {
        let old_active = self
            .search_ui
            .active
            .and_then(|index| self.search_ui.hits.get(index))
            .map(|hit| (hit.page, hit.text_range.clone()));
        let mut page_hits =
            self.search
                .search_page_case_insensitive(page, &self.search_ui.query, 10_001);
        let page_was_capped = page_hits.len() > 10_000;
        page_hits.truncate(10_000);
        self.search_ui.hits.append(&mut page_hits);
        self.search_ui.hits.sort_unstable_by(|left, right| {
            (left.page, left.text_range.start, left.text_range.end).cmp(&(
                right.page,
                right.text_range.start,
                right.text_range.end,
            ))
        });
        self.search_ui.capped |= page_was_capped || self.search_ui.hits.len() > 10_000;
        self.search_ui.hits.truncate(10_000);
        self.search_ui.active = old_active
            .and_then(|(active_page, range)| {
                self.search_ui
                    .hits
                    .iter()
                    .position(|hit| hit.page == active_page && hit.text_range == range)
            })
            .or_else(|| (!self.search_ui.hits.is_empty()).then_some(0));
        self.warm_search_neighborhood();
        self.damage.mark_full();
        self.request_redraw();
    }

    fn refresh_search_results(&mut self) {
        self.search_request = self.search_request.wrapping_add(1).max(1);
        if self.search_ui.query.is_empty() {
            self.search_service.cancel(self.search_request);
            self.search_ui.hits.clear();
            self.search_ui.active = None;
            self.search_ui.capped = false;
            self.search_ui.pending = false;
            self.damage.mark_full();
            self.request_redraw();
            return;
        }
        self.search_ui.pending = true;
        self.search_service.submit(
            self.search_request,
            self.search_index_revision,
            self.search.clone(),
            self.search_ui.query.clone(),
        );
        self.damage.mark_full();
        self.request_redraw();
    }

    fn apply_search_results(&mut self, hits: Vec<SearchHit>, capped: bool) {
        let old_active = self
            .search_ui
            .active
            .and_then(|index| self.search_ui.hits.get(index))
            .map(|hit| (hit.page, hit.text_range.clone()));
        self.search_ui.capped = capped;
        self.search_ui.active = old_active
            .and_then(|(page, range)| {
                hits.iter()
                    .position(|hit| hit.page == page && hit.text_range == range)
            })
            .or_else(|| (!hits.is_empty()).then_some(0));
        self.search_ui.hits = hits;
        self.warm_search_neighborhood();
        self.damage.mark_full();
        self.request_redraw();
    }

    fn navigate_search(&mut self, direction: i32) {
        if self.search_ui.hits.is_empty() {
            return;
        }
        let length = self.search_ui.hits.len() as i32;
        let current = self.search_ui.active.unwrap_or(0) as i32;
        let next = (current + direction).rem_euclid(length) as usize;
        self.search_ui.active = Some(next);
        self.warm_search_neighborhood();
        let page = self.search_ui.hits[next].page;
        self.navigate_to(
            DocumentLocation {
                page,
                target_region: None,
            },
            true,
        );
    }

    fn warm_search_neighborhood(&self) {
        let Some(active) = self.search_ui.active else {
            return;
        };
        let Some(hit) = self.search_ui.hits.get(active) else {
            return;
        };
        self.warm_page(
            hit.page,
            WarmReason::SearchActive,
            1.0,
            Duration::from_secs(2),
        );
        let length = self.search_ui.hits.len();
        if length <= 1 {
            return;
        }
        let previous = (active + length - 1) % length;
        let next = (active + 1) % length;
        for index in [previous, next] {
            if let Some(adjacent) = self.search_ui.hits.get(index)
                && adjacent.page != hit.page
            {
                self.warm_page(
                    adjacent.page,
                    WarmReason::SearchAdjacent,
                    0.75,
                    Duration::from_secs(2),
                );
            }
        }
    }

    fn warm_page(&self, page: PageIndex, reason: WarmReason, probability: f32, duration: Duration) {
        self.conductor
            .warm(WarmHint::for_duration(page, reason, probability, duration));
    }

    fn set_pointer_warm(&mut self, target: Option<(PageIndex, WarmReason, f32, Duration)>) {
        let identity = target.map(|(page, reason, _, _)| (page, reason));
        if identity != self.pointer_warm
            && let Some((page, reason, probability, duration)) = target
        {
            self.warm_page(page, reason, probability, duration);
        }
        self.pointer_warm = identity;
    }

    fn set_search_open(&mut self, open: bool) {
        if open {
            self.commit_resolution_edit();
        }
        self.search_ui.open = open;
        self.search_ui.preedit.clear();
        if open {
            self.search_ui.cursor = self.search_ui.query.len();
            self.search_ui.selection_anchor = None;
        }
        if let Some(window) = &self.window {
            window.set_ime_allowed(open);
        }
        self.damage.mark_full();
        self.request_redraw();
    }

    fn evict_page_artifacts(&mut self) {
        const SOFT_PAGE_LIMIT: usize = 128;
        while self.page_artifacts.len() > SOFT_PAGE_LIMIT || self.memory.over_budget() > 0 {
            let viewport_center = self.viewport_document().center().y;
            let candidate = self
                .page_artifacts
                .keys()
                .filter(|page| !self.intent.page_is_relevant(**page))
                .max_by(|left, right| {
                    let left_distance = self
                        .layout
                        .placement(**left)
                        .map_or(f64::INFINITY, |placement| {
                            (placement.bounds.center().y - viewport_center).abs()
                        });
                    let right_distance = self
                        .layout
                        .placement(**right)
                        .map_or(f64::INFINITY, |placement| {
                            (placement.bounds.center().y - viewport_center).abs()
                        });
                    left_distance
                        .partial_cmp(&right_distance)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .copied();
            let Some(candidate) = candidate else {
                break;
            };
            self.page_artifacts.remove(&candidate);
        }
    }

    fn scroll_by(&mut self, command: ScrollCommand) {
        let before = self.scroll.position;
        self.scroll.apply(command);
        if self.scroll.position != before {
            self.finish_direct_scroll();
        }
    }

    fn finish_direct_scroll(&mut self) {
        self.navigation_mode = match self.scroll.mode {
            ScrollMode::ThumbDrag => NavigationMode::Skimming,
            _ if self.scroll.velocity.y > 0.0 => NavigationMode::SequentialForward,
            _ if self.scroll.velocity.y < 0.0 => NavigationMode::SequentialBackward,
            _ => NavigationMode::Idle,
        };
        self.navigation_settle_deadline = Some(Instant::now() + Duration::from_millis(140));
        self.generation = self.generation.wrapping_add(1);
        self.seek_trace.begin(
            self.generation,
            self.metrics.input_received.unwrap_or_else(Instant::now),
        );
        self.intent_dirty = true;
        self.frame.interactive();

        let snapped = Vec2d {
            x: self.scroll.position.x.round(),
            y: self.scroll.position.y.round(),
        };
        let delta_x = (self.presented_scroll.x - snapped.x) as i32;
        let delta_y = (self.presented_scroll.y - snapped.y) as i32;
        let canvas = RectI::from(self.app_layout.canvas);
        let can_blit = !self.damage.is_full()
            && delta_x.unsigned_abs() < canvas.width
            && delta_y.unsigned_abs() < canvas.height;

        if can_blit && (delta_x != 0 || delta_y != 0) {
            self.pending_scroll_reuse = Some(ScrollReuse {
                canvas,
                delta_x,
                delta_y,
            });
            let exposed = scroll_exposed_regions(canvas, delta_x, delta_y);
            for rect in exposed.rects {
                self.damage.add(rect);
            }
        } else if !can_blit {
            self.pending_scroll_reuse = None;
            self.damage.mark_full();
        }
        self.damage
            .add(RectI::from(self.app_layout.vertical_scrollbar));
        self.damage.add(RectI::from(self.app_layout.status));
        self.presented_scroll = snapped;
        self.request_redraw();
    }

    fn page_step(&mut self, direction: PagingDirection) {
        let viewport = self.viewport_document();
        let visible_range = self.layout.visible_pages(viewport);
        let lines = self.layout.placements()[visible_range]
            .iter()
            .flat_map(|placement| {
                self.page_artifacts
                    .get(&placement.page)
                    .map(|artifacts| artifacts.text.lines.lines.as_ref())
                    .filter(|lines| !lines.is_empty())
                    .map_or_else(
                        || notional_page_lines(placement.page, placement.bounds),
                        <[crate::text::LineBox]>::to_vec,
                    )
            })
            .collect::<Vec<_>>();
        let mut target = paging_target(direction, viewport, lines, self.layout.total_height);
        // Text-line anchoring is ideal when a page is compiled, but a lone
        // line at the viewport edge can otherwise resolve to the current
        // position.  PageUp/PageDown must always advance when movement is
        // available, including during initial rendering.
        if (target - viewport.y).abs() < 1.0 {
            let overlap = (viewport.height * 0.08).clamp(24.0, 96.0);
            let delta = (viewport.height - overlap).max(1.0);
            target = match direction {
                PagingDirection::Down => viewport.y + delta,
                PagingDirection::Up => viewport.y - delta,
            }
            .clamp(0.0, (self.layout.total_height - viewport.height).max(0.0));
        }
        self.scroll.apply(ScrollCommand::SetAbsolute(Vec2d {
            x: self.scroll.position.x,
            y: target * self.zoom,
        }));
        self.navigation_mode = match direction {
            PagingDirection::Down => NavigationMode::SequentialForward,
            PagingDirection::Up => NavigationMode::SequentialBackward,
        };
        self.navigation_settle_deadline = Some(Instant::now() + Duration::from_millis(140));
        self.bump_generation();
    }

    fn fine_step(&mut self, direction: f64) {
        let median = self
            .status
            .current_page
            .and_then(|page| self.page_artifacts.get(&page))
            .and_then(|artifacts| artifacts.text.lines.median_height)
            .unwrap_or(42.0);
        self.scroll_by(ScrollCommand::FineStep(Vec2d {
            x: 0.0,
            y: direction * median * self.zoom,
        }));
    }

    fn scrollbar_geometry(&self) -> ScrollbarGeometry {
        ScrollbarGeometry::calculate(
            self.app_layout.vertical_scrollbar,
            self.layout.total_height * self.zoom,
            self.app_layout.canvas.height,
            self.scroll.position.y,
        )
    }

    fn handle_cursor_moved(&mut self, x: f64, y: f64) {
        self.input.pointer_position = PointF { x, y };
        let hovered_control = self.hover_control_at(self.input.pointer_position);
        if hovered_control != self.hovered_control {
            self.hovered_control = hovered_control;
            self.damage.mark_full();
            self.request_redraw();
        }
        if let Some(PointerCapture::ProcessingPanelResize {
            origin,
            initial_width,
            initial_height,
        }) = self.input.capture
        {
            self.processing_panel_width = (initial_width + x - origin.x).max(320.0);
            self.processing_panel_height = (initial_height + y - origin.y).max(350.0);
            self.damage.mark_full();
            self.request_redraw();
            return;
        }
        if matches!(self.input.capture, Some(PointerCapture::Selection { .. })) {
            const EDGE_ZONE: f64 = 28.0;
            let canvas = self.app_layout.canvas;
            if x >= canvas.x && x <= canvas.right() {
                let velocity = if y < canvas.y + EDGE_ZONE {
                    -((canvas.y + EDGE_ZONE - y) / EDGE_ZONE).clamp(0.2, 2.0) * 18.0
                } else if y > canvas.bottom() - EDGE_ZONE {
                    ((y - (canvas.bottom() - EDGE_ZONE)) / EDGE_ZONE).clamp(0.2, 2.0) * 18.0
                } else {
                    0.0
                };
                if velocity != 0.0 {
                    self.scroll_by(ScrollCommand::FineStep(Vec2d {
                        x: 0.0,
                        y: velocity,
                    }));
                }
            }
            let selection_point = PointF {
                x,
                y: y.clamp(canvas.y + 1.0, canvas.bottom() - 1.0),
            };
            if let Some((position, _)) = self.text_position_at(selection_point) {
                self.selection.extend(position);
                self.damage.mark_full();
                self.request_redraw();
            }
            return;
        }
        if let Some(PointerCapture::VerticalThumb(drag)) = self.input.capture {
            let geometry = self.scrollbar_geometry();
            self.scrollbar
                .enter_or_move(geometry.document_fraction_at(y), Instant::now());
            let thumb_top = y - drag.pointer_offset_in_thumb;
            let target = geometry.scroll_for_thumb_top(
                thumb_top,
                self.layout.total_height * self.zoom,
                self.app_layout.canvas.height,
            );
            let before = self.scroll.position;
            self.scroll.apply(ScrollCommand::SetAbsolute(Vec2d {
                x: self.scroll.position.x,
                y: target,
            }));
            if self.scroll.position != before {
                self.finish_direct_scroll();
            }
            return;
        }

        let geometry = self.scrollbar_geometry();
        if geometry.track.contains(self.input.pointer_position) {
            self.clear_link_hover();
            self.input.hover = HitTarget::VerticalScrollbar(
                if geometry.thumb.contains(self.input.pointer_position) {
                    ScrollbarPart::Thumb
                } else if y < geometry.thumb.y {
                    ScrollbarPart::DecrementTrack
                } else {
                    ScrollbarPart::IncrementTrack
                },
            );
            self.scrollbar
                .enter_or_move(geometry.document_fraction_at(y), Instant::now());
            let warm_target = self
                .scrollbar
                .hover_document_fraction
                .and_then(|fraction| self.page_at_document_fraction(fraction))
                .map(|page| {
                    (
                        page,
                        WarmReason::ScrollbarPrediction,
                        0.7,
                        Duration::from_millis(700),
                    )
                });
            self.set_pointer_warm(warm_target);
            self.intent_dirty = true;
        } else if self.app_layout.canvas.contains(self.input.pointer_position) {
            self.input.hover = HitTarget::Canvas;
            self.scrollbar.leave();
            self.update_link_hover();
            self.intent_dirty = true;
        } else if self
            .app_layout
            .sidebar
            .contains(self.input.pointer_position)
        {
            self.clear_link_hover();
            self.input.hover = HitTarget::Sidebar;
            self.scrollbar.leave();
            let warm_target = self
                .outline_row_at(self.input.pointer_position)
                .and_then(|index| self.outline.get(index))
                .map(|node| {
                    (
                        node.page,
                        WarmReason::OutlineHover,
                        0.85,
                        Duration::from_millis(900),
                    )
                });
            self.set_pointer_warm(warm_target);
        } else if self
            .app_layout
            .toolbar
            .contains(self.input.pointer_position)
        {
            self.clear_link_hover();
            self.input.hover = HitTarget::Popup;
            self.scrollbar.leave();
            self.set_pointer_warm(None);
        } else {
            self.clear_link_hover();
            self.input.hover = HitTarget::None;
            self.scrollbar.leave();
            self.set_pointer_warm(None);
            self.intent_dirty = true;
        }
        self.damage.mark_full();
        self.request_redraw();
    }

    fn link_at(&self, point: PointF) -> Option<(PageIndex, DocumentLink)> {
        if !self.app_layout.canvas.contains(point) || self.zoom <= 0.0 {
            return None;
        }
        let document = PointF {
            x: (point.x - self.document_origin_x() + self.scroll.position.x) / self.zoom,
            y: (point.y - self.app_layout.canvas.y + self.scroll.position.y) / self.zoom,
        };
        let placement = self.layout.page_at_y(document.y)?;
        if !placement.bounds.contains(document) {
            return None;
        }
        let page_local = PointF {
            x: document.x - placement.bounds.x + placement.view_box.x,
            y: document.y - placement.bounds.y + placement.view_box.y,
        };
        let links = self
            .page_artifacts
            .get(&placement.page)
            .map(|artifacts| artifacts.structure.links.as_ref())
            .or_else(|| {
                self.engine
                    .descriptor()
                    .page_links
                    .get(placement.page.0 as usize)
                    .map(AsRef::as_ref)
            })?;
        links
            .iter()
            .rev()
            .find(|link| link.source_region.contains(page_local))
            .cloned()
            .map(|link| (placement.page, link))
    }

    fn update_link_hover(&mut self) {
        let hovered = self.link_at(self.input.pointer_position);
        let unchanged = hovered.as_ref().is_some_and(|(page, link)| {
            self.hovered_link
                .as_ref()
                .is_some_and(|current| current.same_link(*page, link))
        });
        if unchanged {
            if let Some(window) = &self.window {
                window.set_cursor(CursorIcon::Pointer);
            }
            return;
        }

        self.hovered_link = hovered.map(|(source_page, link)| {
            if let LinkTarget::Internal {
                page,
                target_region,
            } = &link.target
            {
                let geometry = self
                    .engine
                    .descriptor()
                    .page_geometries
                    .get(page.0 as usize)
                    .copied();
                let region = (*target_region).or_else(|| {
                    geometry.map(|geometry| RectF {
                        x: 0.0,
                        y: 0.0,
                        width: geometry.display_width(),
                        height: geometry.display_height(),
                    })
                });
                if let Some(region) = region {
                    self.conductor.request_link_peek(*page, region);
                }
                self.set_pointer_warm(Some((
                    *page,
                    WarmReason::LinkHover,
                    0.95,
                    Duration::from_secs(2),
                )));
            } else {
                self.set_pointer_warm(None);
            }
            LinkHoverState {
                source_page,
                link,
                started: Instant::now(),
                peek_visible: false,
            }
        });
        if self.hovered_link.is_none() {
            self.set_pointer_warm(None);
        }
        if let Some(window) = &self.window {
            window.set_cursor(if self.hovered_link.is_some() {
                CursorIcon::Pointer
            } else {
                CursorIcon::Default
            });
        }
    }

    fn clear_link_hover(&mut self) {
        if self
            .hovered_link
            .take()
            .is_some_and(|hovered| hovered.peek_visible)
        {
            self.damage.mark_full();
        }
        if let Some(window) = &self.window {
            window.set_cursor(CursorIcon::Default);
        }
    }

    fn activate_link(&mut self, link: DocumentLink) {
        match link.target {
            LinkTarget::Internal {
                page,
                target_region,
            } => {
                self.warm_page(page, WarmReason::LinkHover, 1.0, Duration::from_secs(2));
                self.navigate_to(
                    DocumentLocation {
                        page,
                        target_region,
                    },
                    true,
                );
            }
            LinkTarget::External(uri) => {
                if external_uri_is_allowed(&uri) {
                    if let Err(error) = open::that(uri.as_ref()) {
                        eprintln!("failed to open external link: {error}");
                    }
                } else {
                    eprintln!("blocked unsupported external link scheme: {uri}");
                }
            }
        }
    }

    fn handle_mouse_input(&mut self, state: ElementState, button: MouseButton) {
        if std::env::var_os("LEGE_INPUT_TRACE").is_some() {
            eprintln!(
                "mouse {state:?} {button:?} at ({:.1}, {:.1}) toolbar={:?} scale={}",
                self.input.pointer_position.x,
                self.input.pointer_position.y,
                toolbar_action_at(self.input.pointer_position.x),
                self.scale_factor,
            );
        }
        if button != MouseButton::Left {
            return;
        }
        self.input.left_button_down = state == ElementState::Pressed;
        match state {
            ElementState::Pressed => {
                if let Some(show_new) = self.result_choice_at(self.input.pointer_position) {
                    self.switch_processing_result(show_new);
                    return;
                }
                if self.processing_resize_handle_contains(self.input.pointer_position) {
                    self.input.capture = Some(PointerCapture::ProcessingPanelResize {
                        origin: self.input.pointer_position,
                        initial_width: self.processing_panel_width,
                        initial_height: self.processing_panel_height,
                    });
                    return;
                }
                let processing_action =
                    self.processing_panel_action_at(self.input.pointer_position);
                if self.processing_ui.resolution_editing
                    && processing_action != Some(ProcessingPanelAction::Option(RESOLUTION_ROW))
                {
                    self.commit_resolution_edit();
                }
                if let Some(action) = processing_action {
                    match action {
                        ProcessingPanelAction::Run => self.start_processing(),
                        ProcessingPanelAction::ToggleProfile => self.toggle_processing_profile(),
                        ProcessingPanelAction::Tab(tab) => {
                            self.processing_ui.tab = tab;
                            self.processing_ui.open_option = None;
                            self.damage.mark_full();
                            self.request_redraw();
                        }
                        ProcessingPanelAction::Option(index) => {
                            self.activate_processing_option(index);
                        }
                        ProcessingPanelAction::Choice { option, choice } => {
                            self.select_processing_option_choice(option, choice);
                        }
                    }
                    return;
                }
                self.commit_resolution_edit();
                if self.processing_ui.open_option.take().is_some() {
                    self.damage.mark_full();
                    self.request_redraw();
                }
                if let Some(mode) = self.options_color_at(self.input.pointer_position) {
                    self.set_color_mode(mode);
                    self.options_visible = false;
                    return;
                }
                if self
                    .search_field_rect()
                    .contains(self.input.pointer_position)
                {
                    self.set_search_open(true);
                    return;
                }
                if self
                    .app_layout
                    .toolbar
                    .contains(self.input.pointer_position)
                {
                    match toolbar_action_at(self.input.pointer_position.x) {
                        Some(ToolbarAction::OpenDocument) => self.prompt_for_document(),
                        Some(ToolbarAction::ZoomOut) => {
                            self.set_zoom(self.zoom / std::f64::consts::SQRT_2, ZoomMode::Manual);
                        }
                        Some(ToolbarAction::ZoomIn) => {
                            self.set_zoom(self.zoom * std::f64::consts::SQRT_2, ZoomMode::Manual);
                        }
                        Some(ToolbarAction::FitWidth) => {
                            self.set_zoom(self.fit_width_zoom(), ZoomMode::FitWidth);
                        }
                        Some(ToolbarAction::FitPage) => {
                            self.set_zoom(self.fit_page_zoom(), ZoomMode::FitPage);
                        }
                        Some(ToolbarAction::ToggleSidebar) => self.toggle_sidebar(),
                        Some(ToolbarAction::ToggleTrim) => self.toggle_trim(),
                        Some(ToolbarAction::ToggleProcessing) => self.toggle_processing_workspace(),
                        Some(ToolbarAction::ToggleOptions) => self.toggle_options_popup(),
                        None => {}
                    }
                    return;
                }
                if self
                    .app_layout
                    .sidebar
                    .contains(self.input.pointer_position)
                {
                    if let Some(index) = self.outline_row_at(self.input.pointer_position)
                        && let Some(node) = self.outline.get(index)
                    {
                        let location = DocumentLocation {
                            page: node.page,
                            target_region: node.target_region,
                        };
                        self.warm_page(
                            location.page,
                            WarmReason::OutlineTarget,
                            1.0,
                            Duration::from_secs(2),
                        );
                        self.navigate_to(location, true);
                        self.outline_scroll_start = None;
                    } else {
                        self.outline_scroll_start = None;
                        self.damage.mark_full();
                        self.request_redraw();
                    }
                    return;
                }
                let geometry = self.scrollbar_geometry();
                if geometry.thumb.contains(self.input.pointer_position) {
                    self.input.capture = Some(PointerCapture::VerticalThumb(ScrollbarDragState {
                        pointer_offset_in_thumb: self.input.pointer_position.y - geometry.thumb.y,
                    }));
                    self.scrollbar.begin_drag();
                } else if geometry.track.contains(self.input.pointer_position) {
                    if self.input.modifiers.shift_key() {
                        let fraction = geometry.document_fraction_at(self.input.pointer_position.y);
                        self.scroll.apply(ScrollCommand::SetAbsolute(Vec2d {
                            x: self.scroll.position.x,
                            y: fraction
                                * (self.layout.total_height * self.zoom
                                    - self.app_layout.canvas.height)
                                    .max(0.0),
                        }));
                        self.navigation_mode = NavigationMode::JumpLikely;
                        self.navigation_settle_deadline =
                            Some(Instant::now() + Duration::from_millis(140));
                        self.bump_generation();
                    } else if self.input.pointer_position.y < geometry.thumb.y {
                        self.page_step(PagingDirection::Up);
                    } else {
                        self.page_step(PagingDirection::Down);
                    }
                } else if self.app_layout.canvas.contains(self.input.pointer_position)
                    && let Some(link) = self
                        .hovered_link
                        .as_ref()
                        .map(|hovered| hovered.link.clone())
                {
                    self.activate_link(link);
                } else if self.app_layout.canvas.contains(self.input.pointer_position)
                    && let Some((position, substrate)) =
                        self.text_position_at(self.input.pointer_position)
                {
                    let click_count = self.next_click_count(self.input.pointer_position);
                    match click_count {
                        2 => self.selection.select_word(position, &substrate),
                        3.. => self.selection.select_line(position, &substrate),
                        _ => self.selection.begin(position),
                    }
                    self.input.capture = Some(PointerCapture::Selection {
                        anchor: self.input.pointer_position,
                    });
                    self.damage.mark_full();
                    self.request_redraw();
                }
            }
            ElementState::Released => {
                self.input.capture = None;
                self.scrollbar.end_drag();
                self.scroll.settle();
                self.navigation_settle_deadline = None;
                if self.navigation_mode != NavigationMode::Idle {
                    self.navigation_mode = NavigationMode::Idle;
                    self.bump_generation();
                } else {
                    self.intent_dirty = true;
                }
            }
        }
    }

    fn text_position_at(
        &self,
        point: PointF,
    ) -> Option<(crate::text::TextPosition, Arc<TextSubstrate>)> {
        if !self.app_layout.canvas.contains(point) || self.zoom <= 0.0 {
            return None;
        }
        let document = PointF {
            x: (point.x - self.document_origin_x() + self.scroll.position.x) / self.zoom,
            y: (point.y - self.app_layout.canvas.y + self.scroll.position.y) / self.zoom,
        };
        let placement = self.layout.page_at_y(document.y)?;
        if document.x < placement.bounds.x || document.x > placement.bounds.right() {
            return None;
        }
        let substrate = Arc::clone(&self.page_artifacts.get(&placement.page)?.text);
        let position = hit_test(placement.page, &substrate, document)?;
        Some((position, substrate))
    }

    fn next_click_count(&mut self, point: PointF) -> u8 {
        let now = Instant::now();
        let count = self
            .last_click
            .filter(|(when, previous, _)| {
                now.duration_since(*when) <= Duration::from_millis(450)
                    && (previous.x - point.x).abs() <= 4.0
                    && (previous.y - point.y).abs() <= 4.0
            })
            .map_or(1, |(_, _, count)| count.saturating_add(1).min(3));
        self.last_click = Some((now, point, count));
        count
    }

    fn search_field_rect(&self) -> RectF {
        // Narrow windows pull the field back over the palette rather than off
        // the right edge, where it would be unreachable.
        let x = SEARCH_FIELD_X.min((self.app_layout.toolbar.width - 110.0).max(0.0));
        let width = (self.app_layout.toolbar.width - x - 30.0).clamp(80.0, 250.0);
        RectF {
            x,
            y: 7.0,
            width,
            height: (self.app_layout.toolbar.height - 14.0).max(24.0),
        }
    }

    fn processing_panel_rect(&self) -> Option<RectF> {
        let canvas = self.app_layout.canvas;
        if canvas.width < 344.0 || canvas.height < 360.0 {
            return None;
        }
        let width = self
            .processing_panel_width
            .clamp(320.0, (canvas.width - 24.0).max(320.0));
        let height = self
            .processing_panel_height
            .clamp(350.0, (canvas.height - 24.0).max(350.0));
        Some(RectF {
            x: canvas.x + 14.0,
            y: canvas.y + 14.0,
            width,
            height,
        })
    }

    fn options_panel_rect(&self) -> Option<RectF> {
        const WIDTH: f64 = 292.0;
        const HEIGHT: f64 = 206.0;
        let canvas = self.app_layout.canvas;
        (canvas.width >= WIDTH + 24.0 && canvas.height >= HEIGHT + 24.0).then_some(RectF {
            x: canvas.right() - WIDTH - 14.0,
            y: canvas.y + 14.0,
            width: WIDTH,
            height: HEIGHT,
        })
    }

    fn result_switch_rect(&self) -> Option<RectF> {
        if !self.processing_ui.result_visible {
            return None;
        }
        let canvas = self.app_layout.canvas;
        let width = 330.0_f64.min((canvas.width - 28.0).max(0.0));
        (width >= 220.0).then_some(RectF {
            x: canvas.x + (canvas.width - width) * 0.5,
            y: canvas.y + 8.0,
            width,
            height: 38.0,
        })
    }

    fn result_choice_at(&self, point: PointF) -> Option<bool> {
        let rect = self.result_switch_rect()?;
        rect.contains(point)
            .then_some(point.x >= rect.x + rect.width * 0.5)
    }

    fn hover_control_at(&self, point: PointF) -> Option<HoverControl> {
        if let Some(choice) = self.result_choice_at(point) {
            return Some(HoverControl::Result(choice));
        }
        if let Some(action) = self.processing_panel_action_at(point) {
            return Some(HoverControl::Processing(action));
        }
        if let Some(mode) = self.options_color_at(point) {
            return Some(HoverControl::Appearance(mode));
        }
        self.app_layout
            .toolbar
            .contains(point)
            .then(|| toolbar_action_at(point.x))
            .flatten()
            .map(HoverControl::Toolbar)
    }

    fn control_color(&self, color: u32, control: HoverControl) -> u32 {
        hover_adjusted_color(
            color,
            self.hovered_control == Some(control),
            color_luminance(self.theme.colors.chrome) < 0.35,
        )
    }

    fn processing_panel_action_at(&self, point: PointF) -> Option<ProcessingPanelAction> {
        if !self.processing_ui.visible {
            return None;
        }
        if let (Some(option), Some(dropdown)) = (
            self.processing_ui.open_option,
            self.processing_dropdown_rect(),
        ) && dropdown.contains(point)
        {
            let choice = ((point.y - dropdown.y - 4.0) / 26.0).floor() as usize;
            if choice < self.processing_option_choices(option).len() {
                return Some(ProcessingPanelAction::Choice { option, choice });
            }
        }
        let panel = self.processing_panel_rect()?;
        if !panel.contains(point) {
            return None;
        }
        let tab_y = panel.y + 40.0;
        if point.y >= tab_y && point.y < tab_y + 30.0 {
            let tab_width = (panel.width - 28.0) / 3.0;
            let index = ((point.x - panel.x - 14.0) / tab_width).floor() as usize;
            return ProcessingTab::ALL
                .get(index.min(ProcessingTab::ALL.len() - 1))
                .copied()
                .map(ProcessingPanelAction::Tab);
        }
        let options_y = panel.y + 80.0;
        let row_count = self.processing_option_rows().len();
        if point.y >= options_y && point.y < options_y + row_count as f64 * 26.0 {
            let row = (((point.y - options_y) / 26.0).floor() as usize).min(row_count - 1);
            if self.processing_ui.tab == ProcessingTab::Page && row == LAYOUT_ROW {
                let value_x = panel.x + panel.width * 0.5;
                if point.x >= value_x {
                    let field_width = panel.width * 0.5 - 20.0;
                    let segment_pitch = (field_width - 8.0).max(3.0) / 3.0 + 4.0;
                    let segment = (((point.x - value_x) / segment_pitch).floor() as usize).min(2);
                    return Some(ProcessingPanelAction::Choice {
                        option: LAYOUT_ROW,
                        choice: segment,
                    });
                }
            }
            return Some(ProcessingPanelAction::Option(row));
        }
        let y = panel.y + panel.height - 58.0;
        if point.y < y || point.y > y + 34.0 {
            return None;
        }
        let relative_x = point.x - panel.x;
        if relative_x < panel.width * 0.58 {
            Some(ProcessingPanelAction::Run)
        } else {
            Some(ProcessingPanelAction::ToggleProfile)
        }
    }

    fn options_color_at(&self, point: PointF) -> Option<ColorMode> {
        if !self.options_visible {
            return None;
        }
        let panel = self.options_panel_rect()?;
        if !panel.contains(point) {
            return None;
        }
        let row = ((point.y - panel.y - 35.0) / 28.0).floor() as i32;
        match row {
            0 => Some(ColorMode::Original),
            1 => Some(ColorMode::Night),
            2 => Some(ColorMode::WarmPaper),
            3 => Some(ColorMode::SanzoEarth),
            4 => Some(ColorMode::SanzoSea),
            _ => None,
        }
    }

    fn processing_resize_handle_contains(&self, point: PointF) -> bool {
        self.processing_ui.visible
            && self.processing_panel_rect().is_some_and(|panel| {
                point.x >= panel.right() - 20.0
                    && point.x <= panel.right()
                    && point.y >= panel.bottom() - 20.0
                    && point.y <= panel.bottom()
            })
    }

    fn outline_window_start(&self) -> usize {
        let visible_rows = ((self.app_layout.sidebar.height - 36.0) / 24.0)
            .floor()
            .max(1.0) as usize;
        if let Some(start) = self.outline_scroll_start {
            return start.min(self.outline.len().saturating_sub(visible_rows));
        }
        let current = self.status.current_page.unwrap_or(PageIndex(0));
        let selected = self
            .outline
            .iter()
            .rposition(|node| node.page <= current)
            .unwrap_or(0);
        selected
            .saturating_sub(visible_rows / 2)
            .min(self.outline.len().saturating_sub(visible_rows))
    }

    fn scroll_outline(&mut self, rows: i32) {
        let visible_rows = ((self.app_layout.sidebar.height - 36.0) / 24.0)
            .floor()
            .max(1.0) as usize;
        let maximum = self.outline.len().saturating_sub(visible_rows);
        let current = self.outline_window_start();
        self.outline_scroll_start = Some(
            if rows < 0 {
                current.saturating_sub(rows.unsigned_abs() as usize)
            } else {
                current.saturating_add(rows as usize)
            }
            .min(maximum),
        );
        self.damage.add(RectI::from(self.app_layout.sidebar));
        self.request_redraw();
    }

    fn outline_row_at(&self, point: PointF) -> Option<usize> {
        let relative_y = point.y - self.app_layout.sidebar.y - 34.0;
        if relative_y < 0.0 {
            return None;
        }
        let index = self.outline_window_start() + (relative_y / 24.0).floor() as usize;
        (index < self.outline.len()).then_some(index)
    }

    fn current_location(&self) -> Option<DocumentLocation> {
        let page = self.status.current_page?;
        Some(DocumentLocation {
            page,
            target_region: None,
        })
    }

    fn navigate_to(&mut self, location: DocumentLocation, record_jump: bool) {
        if record_jump {
            if let Some(current) = self.current_location() {
                self.history.push_jump(current);
            }
            self.history.push_jump(location);
        }
        let Some(placement) = self.layout.placement(location.page) else {
            return;
        };
        let target_y = location.target_region.map_or(placement.bounds.y, |region| {
            placement.bounds.y
                + (region.y - placement.view_box.y).clamp(0.0, placement.bounds.height)
        });
        self.scroll.apply(ScrollCommand::SetAbsolute(Vec2d {
            x: self.scroll.position.x,
            y: target_y * self.zoom,
        }));
        self.navigation_mode = NavigationMode::JumpLikely;
        self.navigation_settle_deadline = Some(Instant::now() + Duration::from_millis(140));
        self.bump_generation();
    }

    fn handle_key(&mut self, key: &Key, physical_key: PhysicalKey, state: ElementState) {
        if state != ElementState::Pressed {
            return;
        }
        if self.input.modifiers.control_key()
            && matches!(key, Key::Character(character) if character.as_str().eq_ignore_ascii_case("o"))
        {
            self.prompt_for_document();
            return;
        }
        if self.input.modifiers.control_key()
            && matches!(key, Key::Character(character) if character.as_str().eq_ignore_ascii_case("f"))
        {
            self.set_search_open(true);
            return;
        }
        if self.handle_resolution_key(key) {
            return;
        }
        // Some Linux keyboard stacks report PageUp/PageDown inconsistently as
        // logical named keys while a text/IME path is active.  Physical HID
        // codes are layout-independent, so handle them before search editing.
        match physical_key {
            PhysicalKey::Code(KeyCode::PageDown) => {
                self.page_step(PagingDirection::Down);
                return;
            }
            PhysicalKey::Code(KeyCode::PageUp) => {
                self.page_step(PagingDirection::Up);
                return;
            }
            _ => {}
        }
        if self.search_ui.open && self.handle_search_key(key) {
            return;
        }
        if matches!(key, Key::Named(NamedKey::F3)) && !self.search_ui.query.is_empty() {
            self.navigate_search(if self.input.modifiers.shift_key() {
                -1
            } else {
                1
            });
            return;
        }
        if self.input.modifiers.control_key()
            && matches!(key, Key::Character(character) if character.as_str().eq_ignore_ascii_case("c"))
        {
            self.copy_document_selection();
            return;
        }
        match key {
            Key::Named(NamedKey::PageDown) => self.page_step(PagingDirection::Down),
            Key::Character(character) if character.as_str() == " " => {
                self.page_step(PagingDirection::Down);
            }
            Key::Named(NamedKey::PageUp) => self.page_step(PagingDirection::Up),
            Key::Named(NamedKey::ArrowLeft) if self.input.modifiers.alt_key() => {
                if let Some(location) = self.history.back() {
                    self.warm_page(
                        location.page,
                        WarmReason::History,
                        1.0,
                        Duration::from_secs(2),
                    );
                    self.navigate_to(location, false);
                }
            }
            Key::Named(NamedKey::ArrowRight) if self.input.modifiers.alt_key() => {
                if let Some(location) = self.history.forward() {
                    self.warm_page(
                        location.page,
                        WarmReason::History,
                        1.0,
                        Duration::from_secs(2),
                    );
                    self.navigate_to(location, false);
                }
            }
            Key::Named(NamedKey::ArrowDown) => self.fine_step(1.0),
            Key::Named(NamedKey::ArrowUp) => self.fine_step(-1.0),
            Key::Named(NamedKey::Home) => {
                self.scroll.apply(ScrollCommand::SetAbsolute(Vec2d::ZERO));
                self.navigation_mode = NavigationMode::JumpLikely;
                self.navigation_settle_deadline = Some(Instant::now() + Duration::from_millis(140));
                self.bump_generation();
            }
            Key::Named(NamedKey::End) => {
                self.scroll.apply(ScrollCommand::SetAbsolute(Vec2d {
                    x: self.scroll.position.x,
                    y: self.scroll.max_position().y,
                }));
                self.navigation_mode = NavigationMode::JumpLikely;
                self.navigation_settle_deadline = Some(Instant::now() + Duration::from_millis(140));
                self.bump_generation();
            }
            Key::Named(NamedKey::F11) => self.toggle_fullscreen(),
            Key::Character(character) if character.as_str() == "+" || character.as_str() == "=" => {
                self.set_zoom(self.zoom * std::f64::consts::SQRT_2, ZoomMode::Manual);
            }
            Key::Character(character) if character.as_str() == "-" => {
                self.set_zoom(self.zoom / std::f64::consts::SQRT_2, ZoomMode::Manual);
            }
            Key::Character(character) if character.as_str().eq_ignore_ascii_case("w") => {
                self.set_zoom(self.fit_width_zoom(), ZoomMode::FitWidth);
            }
            Key::Character(character) if character.as_str().eq_ignore_ascii_case("p") => {
                self.set_zoom(self.fit_page_zoom(), ZoomMode::FitPage);
            }
            Key::Character(character) if character.as_str().eq_ignore_ascii_case("b") => {
                self.toggle_sidebar();
            }
            Key::Character(character) if character.as_str().eq_ignore_ascii_case("m") => {
                self.toggle_trim();
            }
            Key::Character(character) if character.as_str().eq_ignore_ascii_case("n") => {
                self.cycle_color_mode();
            }
            _ => {}
        }
    }

    fn handle_search_key(&mut self, key: &Key) -> bool {
        match key {
            Key::Named(NamedKey::Escape) => {
                self.set_search_open(false);
            }
            Key::Named(NamedKey::Enter) | Key::Named(NamedKey::F3) => {
                self.navigate_search(if self.input.modifiers.shift_key() {
                    -1
                } else {
                    1
                });
            }
            Key::Named(NamedKey::ArrowLeft) => {
                let cursor = previous_boundary(&self.search_ui.query, self.search_ui.cursor);
                self.move_search_cursor(cursor, self.input.modifiers.shift_key());
            }
            Key::Named(NamedKey::ArrowRight) => {
                let cursor = next_boundary(&self.search_ui.query, self.search_ui.cursor);
                self.move_search_cursor(cursor, self.input.modifiers.shift_key());
            }
            Key::Named(NamedKey::Home) => {
                self.move_search_cursor(0, self.input.modifiers.shift_key());
            }
            Key::Named(NamedKey::End) => {
                self.move_search_cursor(
                    self.search_ui.query.len(),
                    self.input.modifiers.shift_key(),
                );
            }
            Key::Named(NamedKey::Backspace) => {
                if self.delete_search_selection() {
                    self.refresh_search_results();
                } else {
                    let previous = previous_boundary(&self.search_ui.query, self.search_ui.cursor);
                    if previous < self.search_ui.cursor {
                        self.search_ui.query.drain(previous..self.search_ui.cursor);
                        self.search_ui.cursor = previous;
                        self.refresh_search_results();
                    }
                }
            }
            Key::Named(NamedKey::Delete) => {
                if self.delete_search_selection() {
                    self.refresh_search_results();
                } else {
                    let next = next_boundary(&self.search_ui.query, self.search_ui.cursor);
                    if next > self.search_ui.cursor {
                        self.search_ui.query.drain(self.search_ui.cursor..next);
                        self.refresh_search_results();
                    }
                }
            }
            Key::Character(character)
                if self.input.modifiers.control_key()
                    && character.as_str().eq_ignore_ascii_case("a") =>
            {
                self.search_ui.selection_anchor = Some(0);
                self.search_ui.cursor = self.search_ui.query.len();
            }
            Key::Character(character)
                if self.input.modifiers.control_key()
                    && character.as_str().eq_ignore_ascii_case("v") =>
            {
                if let Ok(text) = self.clipboard.get() {
                    self.insert_search_text(&text);
                }
            }
            Key::Character(character)
                if self.input.modifiers.control_key()
                    && character.as_str().eq_ignore_ascii_case("c") =>
            {
                if let Some(range) = self.search_selection() {
                    let _ = self.clipboard.set(self.search_ui.query[range].to_owned());
                }
            }
            Key::Character(character)
                if self.input.modifiers.control_key()
                    && character.as_str().eq_ignore_ascii_case("x") =>
            {
                if let Some(range) = self.search_selection() {
                    let _ = self
                        .clipboard
                        .set(self.search_ui.query[range.clone()].to_owned());
                    self.search_ui.query.drain(range.clone());
                    self.search_ui.cursor = range.start;
                    self.search_ui.selection_anchor = None;
                    self.refresh_search_results();
                }
            }
            Key::Character(character) if !self.input.modifiers.control_key() => {
                self.insert_search_text(character.as_str());
            }
            _ => return false,
        }
        self.damage.mark_full();
        self.request_redraw();
        true
    }

    fn insert_search_text(&mut self, text: &str) {
        let filtered: String = text
            .chars()
            .filter(|character| !character.is_control())
            .collect();
        if filtered.is_empty() {
            return;
        }
        self.delete_search_selection();
        self.search_ui
            .query
            .insert_str(self.search_ui.cursor, &filtered);
        self.search_ui.cursor += filtered.len();
        self.search_ui.selection_anchor = None;
        self.refresh_search_results();
    }

    fn move_search_cursor(&mut self, cursor: usize, extend_selection: bool) {
        if extend_selection {
            self.search_ui
                .selection_anchor
                .get_or_insert(self.search_ui.cursor);
        } else {
            self.search_ui.selection_anchor = None;
        }
        self.search_ui.cursor = cursor.min(self.search_ui.query.len());
    }

    fn search_selection(&self) -> Option<std::ops::Range<usize>> {
        let anchor = self.search_ui.selection_anchor?;
        (anchor != self.search_ui.cursor)
            .then(|| anchor.min(self.search_ui.cursor)..anchor.max(self.search_ui.cursor))
    }

    fn delete_search_selection(&mut self) -> bool {
        let Some(range) = self.search_selection() else {
            return false;
        };
        self.search_ui.query.drain(range.clone());
        self.search_ui.cursor = range.start;
        self.search_ui.selection_anchor = None;
        true
    }

    fn copy_document_selection(&mut self) {
        let (Some(anchor), Some(focus)) = (self.selection.anchor, self.selection.focus) else {
            return;
        };
        let (start, end) = if anchor <= focus {
            (anchor, focus)
        } else {
            (focus, anchor)
        };
        let mut pages = BTreeMap::new();
        for number in start.page.0..=end.page.0 {
            let page = PageIndex(number);
            if let Some(text) = self.search.page_text(page) {
                pages.insert(page, text);
            }
        }
        let text = self.selection.selected_text(&pages);
        if !text.is_empty()
            && let Err(error) = self.clipboard.set(text)
        {
            eprintln!("viewer clipboard: {error}");
        }
    }

    /// Ask the user for a document and open it. The dialog is modal on the
    /// viewer window, so the event loop simply resumes once it closes.
    fn prompt_for_document(&mut self) {
        let mut dialog = rfd::FileDialog::new()
            .set_title("Open a PDF document")
            .add_filter("PDF documents", &["pdf"]);
        if let Some(directory) = self
            .engine
            .source_path()
            .and_then(std::path::Path::parent)
            .map(std::path::Path::to_path_buf)
        {
            dialog = dialog.set_directory(directory);
        }
        if let Some(window) = &self.window {
            dialog = dialog.set_parent(window.as_ref());
        }
        let Some(path) = dialog.pick_file() else {
            return;
        };
        self.processing_ui.result_visible = false;
        self.processing_ui.result_ready = false;
        self.processing_ui.viewing_new = false;
        self.processing_ui.output = None;
        self.processing_ui.original = Some(path.clone());
        self.open_document(&path);
    }

    fn toggle_processing_workspace(&mut self) {
        self.processing_ui.visible = !self.processing_ui.visible;
        if !self.processing_ui.visible {
            self.processing_ui.open_option = None;
            self.commit_resolution_edit();
        }
        self.damage.mark_full();
        self.request_redraw();
    }

    fn toggle_options_popup(&mut self) {
        self.options_visible = !self.options_visible;
        self.damage.mark_full();
        self.request_redraw();
    }

    fn switch_processing_result(&mut self, show_new: bool) {
        if show_new && !self.processing_ui.result_ready {
            return;
        }
        let path = if show_new {
            self.processing_ui.output.clone()
        } else {
            self.processing_ui.original.clone()
        };
        let Some(path) = path else {
            return;
        };
        if path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
        {
            self.open_document(&path);
            self.processing_ui.viewing_new = show_new;
        } else if show_new && let Err(error) = open::that(&path) {
            self.processing_ui.title = "Could not open processed output".to_owned();
            self.processing_ui.detail = error.to_string();
        }
        self.damage.mark_full();
        self.request_redraw();
    }

    fn processing_option_rows(&self) -> Vec<(String, String)> {
        let options = &self.processing_ui.options;
        let enabled = |value: bool| if value { "On" } else { "Off" }.to_owned();
        match self.processing_ui.tab {
            ProcessingTab::Output => vec![
                (
                    "Container".to_owned(),
                    options.output_format.label().to_owned(),
                ),
                (
                    "Text compression".to_owned(),
                    options.compression.label().to_owned(),
                ),
                ("Cover".to_owned(), options.cover.label().to_owned()),
                (
                    "Image regions".to_owned(),
                    options.image_processing.label().to_owned(),
                ),
                (
                    "EPUB sidecar".to_owned(),
                    enabled(options.make_epub_sidecar),
                ),
                (
                    "JPEG compatibility".to_owned(),
                    enabled(options.jpeg_compat),
                ),
                ("High quality".to_owned(), enabled(options.high_quality)),
            ],
            ProcessingTab::Recognition => vec![
                (
                    "Layout analysis".to_owned(),
                    enabled(options.layout_analysis),
                ),
                (
                    "Exclude current page".to_owned(),
                    self.status.current_page.map_or_else(
                        || "No page".to_owned(),
                        |page| {
                            if options.layout_exclusion_pages.contains(&page.0) {
                                "Excluded".to_owned()
                            } else {
                                "Included".to_owned()
                            }
                        },
                    ),
                ),
                ("OCR text layer".to_owned(), enabled(options.use_ocr)),
                (
                    "OCR quality".to_owned(),
                    options.ocr_mode.label().to_owned(),
                ),
                (
                    "JBIG2 halftone".to_owned(),
                    enabled(options.use_jbig2_halftone),
                ),
                ("Grayscale / MRC".to_owned(), enabled(options.grayscale)),
                ("Invert input".to_owned(), enabled(options.invert)),
            ],
            ProcessingTab::Page => vec![
                ("Scope".to_owned(), self.processing_ui.scope.label()),
                (
                    "Page layout".to_owned(),
                    if options.reflow {
                        "Reflow".to_owned()
                    } else {
                        match options.margin_mode {
                            MarginMode::None => "Original".to_owned(),
                            MarginMode::Center => "Center".to_owned(),
                            MarginMode::Crop => "Crop".to_owned(),
                        }
                    },
                ),
                ("Binarization".to_owned(), options.binarization.label()),
                (
                    "Adaptive strength".to_owned(),
                    match options.binarization {
                        Binarization::Adaptive { sauvola_k } => format!("k={sauvola_k:.2}"),
                        _ => "Select adaptive first".to_owned(),
                    },
                ),
                (
                    "Threshold".to_owned(),
                    match options.binarization {
                        Binarization::Threshold { value } => value.to_string(),
                        _ => "Select threshold first".to_owned(),
                    },
                ),
                (
                    "Target resolution".to_owned(),
                    if self.processing_ui.resolution_editing {
                        format!("{}| px high", self.processing_ui.resolution_buffer)
                    } else {
                        options.target_width.map_or_else(
                            || format!("{} px high", options.target_height),
                            |width| format!("{width} × {}", options.target_height),
                        )
                    },
                ),
                (
                    "Preset".to_owned(),
                    self.processing_ui.profile.label().to_owned(),
                ),
            ],
        }
    }

    fn processing_option_choices(&self, index: usize) -> Vec<String> {
        let boolean = || vec!["Off".to_owned(), "On".to_owned()];
        match (self.processing_ui.tab, index) {
            (ProcessingTab::Output, 0) => ["PDF", "DjVu", "EPUB"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            (ProcessingTab::Output, 1) => {
                ["CCITT4", "JBIG2"].into_iter().map(str::to_owned).collect()
            }
            (ProcessingTab::Output, 2) => ["Preserve", "JPEG", "JPEG 2000", "Remove"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            (ProcessingTab::Output, 3) => ["Original", "Dithered"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            (ProcessingTab::Output, 4..=6) => boolean(),
            (ProcessingTab::Recognition, 0 | 2 | 4..=6) => boolean(),
            (ProcessingTab::Recognition, 1) => ["Included", "Excluded"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            (ProcessingTab::Recognition, 3) => {
                ["Fast", "Best"].into_iter().map(str::to_owned).collect()
            }
            (ProcessingTab::Page, 0) => {
                let mut choices = vec!["Whole document".to_owned()];
                if let Some(page) = self.status.current_page {
                    choices.push(format!("Current page ({})", page.0 + 1));
                }
                choices
            }
            (ProcessingTab::Page, LAYOUT_ROW) => {
                LAYOUT_SEGMENTS.into_iter().map(str::to_owned).collect()
            }
            (ProcessingTab::Page, 2) => ["Adaptive", "Threshold", "Heavy"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            (ProcessingTab::Page, 3) => ["k=0.05", "k=0.10", "k=0.15", "k=0.20", "k=0.25"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            (ProcessingTab::Page, 4) => ["120", "160", "180", "200", "220"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            (ProcessingTab::Page, 6) => ["Reading", "Bilevel"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        }
    }

    fn processing_dropdown_rect(&self) -> Option<RectF> {
        let option = self.processing_ui.open_option?;
        let choice_count = self.processing_option_choices(option).len();
        if choice_count == 0 {
            return None;
        }
        let panel = self.processing_panel_rect()?;
        let width = (panel.width * 0.5 - 20.0).max(150.0);
        let height = 8.0 + choice_count as f64 * 26.0;
        let row_top = panel.y + 80.0 + option as f64 * 26.0;
        let below = row_top + 24.0;
        let action_top = panel.bottom() - 64.0;
        let y = if below + height <= action_top {
            below
        } else {
            (row_top - height).max(panel.y + 74.0)
        };
        Some(RectF {
            x: panel.x + panel.width * 0.5,
            y,
            width,
            height,
        })
    }

    fn processing_choice_is_selected(&self, option: usize, choice: &str) -> bool {
        if self.processing_ui.tab == ProcessingTab::Page && option == LAYOUT_ROW {
            let options = &self.processing_ui.options;
            return match choice {
                "Crop" => !options.reflow && options.margin_mode == MarginMode::Crop,
                "Center" => !options.reflow && options.margin_mode == MarginMode::Center,
                "Reflow" => options.reflow,
                _ => false,
            };
        }
        let current_value = self
            .processing_option_rows()
            .get(option)
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        current_value == choice
            || current_value.starts_with(choice)
            || (choice == "Whole document"
                && matches!(self.processing_ui.scope, ProcessingScope::Document))
            || (choice.starts_with("Current page")
                && matches!(self.processing_ui.scope, ProcessingScope::Pages(_)))
    }

    fn begin_resolution_edit(&mut self) {
        self.set_search_open(false);
        self.processing_ui.open_option = None;
        self.processing_ui.resolution_buffer = self.processing_ui.options.target_height.to_string();
        self.processing_ui.resolution_editing = true;
        if let Some(window) = &self.window {
            window.set_ime_allowed(true);
        }
        self.damage.mark_full();
        self.request_redraw();
    }

    fn commit_resolution_edit(&mut self) {
        if !self.processing_ui.resolution_editing {
            return;
        }
        if let Ok(value) = self.processing_ui.resolution_buffer.parse::<u32>()
            && value > 0
        {
            self.processing_ui.options.target_height = value;
            self.processing_ui.options.target_width = None;
        }
        self.processing_ui.resolution_buffer = self.processing_ui.options.target_height.to_string();
        self.processing_ui.resolution_editing = false;
        if let Some(window) = &self.window {
            window.set_ime_allowed(self.search_ui.open);
        }
        self.damage.mark_full();
        self.request_redraw();
    }

    fn handle_resolution_key(&mut self, key: &Key) -> bool {
        if !self.processing_ui.resolution_editing {
            return false;
        }
        match key {
            Key::Named(NamedKey::Enter) => self.commit_resolution_edit(),
            Key::Named(NamedKey::Escape) => {
                self.processing_ui.resolution_buffer =
                    self.processing_ui.options.target_height.to_string();
                self.commit_resolution_edit();
            }
            Key::Named(NamedKey::Backspace) => {
                self.processing_ui.resolution_buffer.pop();
            }
            Key::Character(character)
                if self.input.modifiers.control_key()
                    && character.as_str().eq_ignore_ascii_case("v") =>
            {
                if let Ok(text) = self.clipboard.get() {
                    self.insert_resolution_text(&text);
                }
            }
            Key::Character(character) if !self.input.modifiers.control_key() => {
                self.insert_resolution_text(character.as_str());
            }
            _ => return false,
        }
        self.damage.mark_full();
        self.request_redraw();
        true
    }

    fn insert_resolution_text(&mut self, text: &str) {
        for character in text.chars().filter(char::is_ascii_digit) {
            if self.processing_ui.resolution_buffer.len() < 6 {
                self.processing_ui.resolution_buffer.push(character);
            }
        }
    }

    fn select_processing_option_choice(&mut self, index: usize, choice: usize) {
        let enabled = choice == 1;
        match (self.processing_ui.tab, index) {
            (ProcessingTab::Output, 0) => {
                self.processing_ui.options.output_format = match choice {
                    1 => OutputFormat::Djvu,
                    2 => OutputFormat::Epub,
                    _ => OutputFormat::Pdf,
                };
            }
            (ProcessingTab::Output, 1) => {
                self.processing_ui.options.compression = if choice == 0 {
                    TextCompression::Ccitt4
                } else {
                    TextCompression::Jbig2
                };
            }
            (ProcessingTab::Output, 2) => {
                self.processing_ui.options.cover = match choice {
                    1 => CoverMode::Jpeg,
                    2 => CoverMode::Jpeg2000,
                    3 => CoverMode::None,
                    _ => CoverMode::Preserve,
                };
            }
            (ProcessingTab::Output, 3) => {
                self.processing_ui.options.image_processing = if choice == 0 {
                    ImageProcessing::Original
                } else {
                    ImageProcessing::Dithered
                };
            }
            (ProcessingTab::Output, 4) => {
                self.processing_ui.options.make_epub_sidecar = enabled;
            }
            (ProcessingTab::Output, 5) => self.processing_ui.options.jpeg_compat = enabled,
            (ProcessingTab::Output, 6) => self.processing_ui.options.high_quality = enabled,
            (ProcessingTab::Recognition, 0) => {
                self.processing_ui.options.layout_analysis = enabled;
            }
            (ProcessingTab::Recognition, 1) => {
                if let Some(page) = self.status.current_page {
                    if enabled {
                        self.processing_ui
                            .options
                            .layout_exclusion_pages
                            .insert(page.0);
                    } else {
                        self.processing_ui
                            .options
                            .layout_exclusion_pages
                            .remove(&page.0);
                    }
                }
            }
            (ProcessingTab::Recognition, 2) => self.processing_ui.options.use_ocr = enabled,
            (ProcessingTab::Recognition, 3) => {
                self.processing_ui.options.ocr_mode = if choice == 0 {
                    OcrMode::Fast
                } else {
                    OcrMode::Best
                };
            }
            (ProcessingTab::Recognition, 4) => {
                self.processing_ui.options.use_jbig2_halftone = enabled;
            }
            (ProcessingTab::Recognition, 5) => self.processing_ui.options.grayscale = enabled,
            (ProcessingTab::Recognition, 6) => self.processing_ui.options.invert = enabled,
            (ProcessingTab::Page, 0) => {
                self.processing_ui.scope = if choice == 1 {
                    self.status
                        .current_page
                        .map_or(ProcessingScope::Document, |page| {
                            ProcessingScope::Pages([page.0].into_iter().collect())
                        })
                } else {
                    ProcessingScope::Document
                };
            }
            (ProcessingTab::Page, LAYOUT_ROW) => {
                // Crop, Center and Reflow are one mutually exclusive trio:
                // picking one clears the other two, picking the active one
                // returns the page to its original layout. Crop welds the
                // free-aspect flag on, exactly like the Freya GUI did.
                let options = &mut self.processing_ui.options;
                match choice {
                    0 => {
                        if !options.reflow && options.margin_mode == MarginMode::Crop {
                            options.margin_mode = MarginMode::None;
                            options.crop_free_aspect = false;
                        } else {
                            options.margin_mode = MarginMode::Crop;
                            options.crop_free_aspect = true;
                            options.reflow = false;
                        }
                    }
                    1 => {
                        if !options.reflow && options.margin_mode == MarginMode::Center {
                            options.margin_mode = MarginMode::None;
                        } else {
                            options.margin_mode = MarginMode::Center;
                            options.crop_free_aspect = false;
                            options.reflow = false;
                        }
                    }
                    _ => {
                        if options.reflow {
                            options.reflow = false;
                        } else {
                            options.reflow = true;
                            options.margin_mode = MarginMode::None;
                            options.crop_free_aspect = false;
                        }
                    }
                }
            }
            (ProcessingTab::Page, 2) => {
                self.processing_ui.options.binarization = match choice {
                    1 => Binarization::Threshold { value: 180 },
                    2 => Binarization::Heavy,
                    _ => Binarization::Adaptive { sauvola_k: 0.05 },
                };
            }
            (ProcessingTab::Page, 3) => {
                let values = [0.05, 0.10, 0.15, 0.20, 0.25];
                self.processing_ui.options.binarization = Binarization::Adaptive {
                    sauvola_k: values.get(choice).copied().unwrap_or(0.05),
                };
            }
            (ProcessingTab::Page, 4) => {
                let values = [120, 160, 180, 200, 220];
                self.processing_ui.options.binarization = Binarization::Threshold {
                    value: values.get(choice).copied().unwrap_or(180),
                };
            }
            (ProcessingTab::Page, 6) => {
                self.processing_ui.profile = if choice == 0 {
                    ProcessingProfile::Reading
                } else {
                    ProcessingProfile::Bilevel
                };
                self.processing_ui
                    .options
                    .apply_profile(self.processing_ui.profile);
            }
            _ => {}
        }
        self.processing_ui.options.normalize_dependencies();
        self.processing_ui.open_option = None;
        self.processing_ui.title = "Processing options updated".to_owned();
        let rows = self.processing_option_rows();
        self.processing_ui.detail = rows
            .get(index.min(rows.len().saturating_sub(1)))
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        self.damage.mark_full();
        self.request_redraw();
    }

    fn activate_processing_option(&mut self, index: usize) {
        if self.processing_ui.tab == ProcessingTab::Page && index == RESOLUTION_ROW {
            self.begin_resolution_edit();
            return;
        }
        self.commit_resolution_edit();
        if self.processing_ui.tab == ProcessingTab::Page && index == LAYOUT_ROW {
            // The layout trio renders as three inline segments; clicks are
            // dispatched per segment, so the row itself has no dropdown.
            return;
        }
        let choices = self.processing_option_choices(index);
        if choices.len() == 2 {
            let next = if self.processing_choice_is_selected(index, &choices[0]) {
                1
            } else {
                0
            };
            self.select_processing_option_choice(index, next);
            return;
        }
        self.processing_ui.open_option =
            (choices.len() > 2 && self.processing_ui.open_option != Some(index)).then_some(index);
        self.damage.mark_full();
        self.request_redraw();
    }

    fn toggle_processing_profile(&mut self) {
        self.processing_ui.profile = match self.processing_ui.profile {
            ProcessingProfile::Reading => ProcessingProfile::Bilevel,
            ProcessingProfile::Bilevel => ProcessingProfile::Reading,
        };
        self.processing_ui
            .options
            .apply_profile(self.processing_ui.profile);
        self.processing_ui.visible = true;
        self.processing_ui.title = format!("{} profile", self.processing_ui.profile.label());
        self.processing_ui.detail = match self.processing_ui.profile {
            ProcessingProfile::Reading => "Balanced raster PDF for comfortable reading.".to_owned(),
            ProcessingProfile::Bilevel => {
                "High-compression text-first output for clean scans.".to_owned()
            }
        };
        self.damage.mark_full();
        self.request_redraw();
    }

    fn start_processing(&mut self) {
        if self.processing_ui.running {
            if let Some(control) = &self.processing_control {
                control.cancel();
                self.processing_ui.title = "Cancelling processing…".to_owned();
                self.processing_ui.detail =
                    "The current page is allowed to stop safely.".to_owned();
                self.damage.mark_full();
                self.request_redraw();
            }
            return;
        }
        self.commit_resolution_edit();
        let Some(input) = self.engine.source_path().map(std::path::Path::to_path_buf) else {
            self.processing_ui.visible = true;
            self.processing_ui.title = "Open a PDF first".to_owned();
            self.processing_ui.detail =
                "Synthetic and empty documents cannot be processed.".to_owned();
            self.damage.mark_full();
            self.request_redraw();
            return;
        };
        let Some(proxy) = self.processing_proxy.clone() else {
            self.processing_ui.visible = true;
            self.processing_ui.title = "Processing unavailable".to_owned();
            self.processing_ui.detail =
                "The desktop event loop has not been initialized.".to_owned();
            self.damage.mark_full();
            self.request_redraw();
            return;
        };
        let output =
            default_processing_output(&input, self.processing_ui.options.output_format.extension());
        let request = ProcessingRequest {
            input: input.clone(),
            output,
            profile: self.processing_ui.profile,
            scope: self.processing_ui.scope.clone(),
            options: self.processing_ui.options.clone(),
        };
        self.processing_ui.visible = true;
        self.processing_ui.original = Some(input);
        self.processing_ui.output = Some(request.output.clone());
        self.processing_ui.result_visible = true;
        self.processing_ui.result_ready = false;
        self.processing_ui.viewing_new = false;
        self.processing_ui.title = "Starting processing…".to_owned();
        self.processing_ui.detail =
            format!("{} · {}", request.scope.label(), request.profile.label());
        match processing::start(request, proxy) {
            Ok(control) => {
                self.processing_control = Some(control);
                self.processing_ui.running = true;
            }
            Err(error) => {
                self.processing_ui.title = "Could not start processing".to_owned();
                self.processing_ui.detail = error;
            }
        }
        self.damage.mark_full();
        self.request_redraw();
    }

    fn apply_processing_update(&mut self, update: ProcessingUpdate) {
        self.processing_ui.visible = true;
        let mut completed_output = None;
        match update {
            ProcessingUpdate::Started { output } => {
                self.processing_ui.running = true;
                self.processing_ui.output = Some(output);
                self.processing_ui.result_visible = true;
                self.processing_ui.result_ready = false;
                self.processing_ui.title = "Processing".to_owned();
            }
            ProcessingUpdate::Progress { title, detail } => {
                self.processing_ui.title = title;
                self.processing_ui.detail = detail;
            }
            ProcessingUpdate::Completed { message, output } => {
                self.processing_ui.running = false;
                self.processing_control = None;
                self.processing_ui.output = Some(output);
                self.processing_ui.result_visible = true;
                self.processing_ui.result_ready = true;
                self.processing_ui.title = "Processing complete".to_owned();
                self.processing_ui.detail = message;
                completed_output = self.processing_ui.output.clone();
            }
            ProcessingUpdate::Cancelled => {
                self.processing_ui.running = false;
                self.processing_control = None;
                self.processing_ui.title = "Processing cancelled".to_owned();
                self.processing_ui.detail = "No further pages will be written.".to_owned();
                self.processing_ui.result_ready = false;
            }
            ProcessingUpdate::Failed { message } => {
                self.processing_ui.running = false;
                self.processing_control = None;
                self.processing_ui.title = "Processing failed".to_owned();
                self.processing_ui.detail = message;
                self.processing_ui.result_ready = false;
            }
        }
        if let Some(output) = completed_output
            && output
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
        {
            self.open_document(&output);
            self.processing_ui.viewing_new = true;
        }
        self.damage.mark_full();
        self.request_redraw();
    }

    /// Open `path`, reporting any failure where a user who launched the viewer
    /// by double-clicking it can actually see it.
    fn open_document(&mut self, path: &std::path::Path) {
        match open_pdf_engine(path) {
            Ok(engine) => {
                if let Err(error) = self.adopt_engine(engine) {
                    report_document_error(path, &error.to_string());
                }
            }
            Err(error) => report_document_error(path, &error),
        }
    }

    /// Replace the current document wholesale. Every per-document cache,
    /// index, and history is rebuilt; only the window, presenter, and user
    /// preferences (theme, trim, colour mode) survive the swap.
    fn adopt_engine(&mut self, engine: Arc<dyn DocumentEngine>) -> Result<(), ViewerStartError> {
        let page_count = engine.descriptor().page_count;
        let content_extents = vec![None; page_count as usize];
        self.render_variant = self.render_variant.wrapping_add(1);
        let layout = Arc::new(PageLayoutIndex::build_with_options(
            &engine.descriptor().page_geometries,
            &content_extents,
            self.trim_enabled,
            self.color_mode,
            self.render_variant,
            &self.theme.metrics,
        ));
        let tiles = Arc::new(TileCache::new(engine.descriptor().id, self.memory.clone()));
        let conductor = ConductorHandle::spawn(
            engine.clone(),
            layout.clone(),
            self.updates.clone(),
            self.memory.clone(),
            tiles.clone(),
        )?;
        // Joining the previous conductor first guarantees nothing else can
        // publish into the shared update queue while it is drained. The new
        // conductor stays idle until the first intent is published below.
        drop(std::mem::replace(&mut self.conductor, conductor));
        drop(self.updates.drain());

        self.previews = self.conductor.previews();
        self.engine = engine;
        self.layout = layout;
        self.tiles = tiles;
        self.content_extents = content_extents;
        self.tile_snapshot = self.tiles.frame_snapshot();
        self.tile_scratch.clear();
        self.painted_tiles.clear();
        self.page_artifacts.clear();
        self.page_errors.clear();
        self.search = SearchIndex::with_memory(self.memory.clone());
        self.search_request = self.search_request.wrapping_add(1).max(1);
        self.search_index_revision = 0;
        self.search_ui = SearchUiState {
            total_pages: page_count,
            ..SearchUiState::default()
        };
        self.selection = SelectionModel::default();
        self.outline_synthesizer = OutlineSynthesizer::default();
        self.outline = Arc::clone(&self.engine.descriptor().outline);
        self.history = NavigationHistory::default();
        self.last_click = None;
        self.pointer_warm = None;
        self.hovered_link = None;
        self.status = StatusState::default();
        self.scroll = ScrollModel::new();
        self.presented_scroll = Vec2d::ZERO;
        self.outline_scroll_start = None;
        self.navigation_mode = NavigationMode::Idle;
        self.navigation_settle_deadline = None;
        self.intent = ViewportIntent::empty();
        self.zoom_mode = ZoomMode::Automatic;
        self.update_scroll_extents();
        self.zoom = self.zoom_for_mode(ZoomMode::Automatic);
        self.update_scroll_extents();
        if let Some(window) = &self.window {
            window.set_ime_allowed(false);
            window.set_title(&format!(
                "Lege Viewer — {}",
                self.engine.descriptor().display_name
            ));
        }
        self.bump_generation();
        Ok(())
    }

    fn toggle_sidebar(&mut self) {
        let anchor = self.reading_anchor();
        self.sidebar_visible = !self.sidebar_visible;
        self.outline_scroll_start = None;
        if let Some(window) = &self.window {
            let size = window.inner_size();
            self.app_layout = AppLayout::calculate(
                SizeF {
                    width: f64::from(size.width),
                    height: f64::from(size.height),
                },
                self.scale_factor,
                self.sidebar_visible,
                &self.theme.metrics,
            );
        }
        if self.zoom_mode != ZoomMode::Manual {
            self.zoom = self.zoom_for_mode(self.zoom_mode);
        }
        self.update_scroll_extents();
        self.restore_anchor(anchor);
        self.bump_generation();
    }

    fn toggle_fullscreen(&mut self) {
        self.fullscreen = !self.fullscreen;
        if let Some(window) = &self.window {
            window.set_fullscreen(if self.fullscreen {
                Some(Fullscreen::Borderless(window.current_monitor()))
            } else {
                None
            });
        }
        self.bump_generation();
    }

    fn compose(&mut self) -> Result<(), crate::present::PresentError> {
        if self.surface_suspended {
            return Ok(());
        }
        self.metrics.begin_frame();
        self.publish_intent_if_needed();
        self.frame_damage.clear();
        self.frame_damage.extend_from_slice(self.damage.rects());
        if self.frame_damage.is_empty() {
            return Ok(());
        }
        let viewport_document = self.viewport_document();
        let scrollbar = self.scrollbar_geometry();
        let memory_bytes = self.memory.total_bytes();
        let chrome_surfaces = self.render_chrome_surfaces(memory_bytes);
        let hover_preview = self
            .scrollbar
            .preview_visible()
            .then(|| {
                let fraction = self.scrollbar.hover_document_fraction?;
                let page = self.page_at_document_fraction(fraction)?;
                Some((page, fraction))
            })
            .flatten();
        let link_peek = self.hovered_link.as_ref().and_then(|hovered| {
            if !hovered.peek_visible {
                return None;
            }
            let LinkTarget::Internal {
                page,
                target_region,
            } = hovered.link.target
            else {
                return None;
            };
            Some(LinkPeekView {
                target_page: page,
                target_region,
                pointer: self.input.pointer_position,
            })
        });
        {
            let mut scene = self.scene.begin(self.theme.colors.canvas);
            paint_scene(
                &mut scene,
                &self.theme,
                self.app_layout,
                &self.layout,
                viewport_document,
                self.zoom,
                self.scroll.position,
                &self.intent,
                &self.tile_snapshot,
                &self.previews,
                &mut self.tile_scratch,
                &mut self.painted_tiles,
                &self.page_errors,
                &self.page_artifacts,
                &self.search_ui.hits,
                self.search_ui.active,
                &self.selection,
                scrollbar,
                hover_preview,
                link_peek,
            );
            for placement in chrome_surfaces {
                scene.draw_surface(
                    placement.surface,
                    placement.destination,
                    ImageSampling::Nearest,
                );
            }
        }
        self.metrics.damaged_pixels = self
            .frame_damage
            .iter()
            .map(|rect| u64::from(rect.width) * u64::from(rect.height))
            .sum();
        self.metrics.finish_compose();

        self.present_scene()?;
        self.metrics.finish_present();
        let has_page_pixels = self.intent.visible_tiles.iter().any(|demand| {
            self.previews
                .contains_variant(demand.page, self.layout.render_variant)
                || [TileTier::TextFirst, TileTier::Draft, TileTier::Final]
                    .into_iter()
                    .any(|tier| {
                        self.tiles.contains(demand.key(
                            self.engine.descriptor().id,
                            self.intent.bucket,
                            tier,
                        ))
                    })
        });
        if has_page_pixels {
            self.seek_trace.mark_pixels_ready();
            let exact = !self.intent.visible_tiles.is_empty()
                && self.intent.visible_tiles.iter().all(|demand| {
                    self.tiles.contains(demand.key(
                        self.engine.descriptor().id,
                        self.intent.bucket,
                        TileTier::Final,
                    ))
                });
            let exact_just_presented = self.seek_trace.mark_presented(exact);
            if self.seek_diagnostics
                && exact_just_presented
                && let Some(report) = self.seek_trace.report_line(&self.metrics)
            {
                eprintln!("{report}");
            }
        }
        self.sync_presenter_stats();
        self.presented_scroll = Vec2d {
            x: self.scroll.position.x.round(),
            y: self.scroll.position.y.round(),
        };
        self.damage.clear();
        self.pending_scroll_reuse = None;
        Ok(())
    }

    fn present_scene(&mut self) -> Result<(), crate::present::PresentError> {
        let result = self.presenter.as_mut().map_or(Ok(()), |presenter| {
            presenter.present(&self.scene, &self.frame_damage, self.pending_scroll_reuse)
        });
        let Err(error) = result else {
            return Ok(());
        };
        let can_fallback = self.presenter_preference == PresenterPreference::Auto
            && self
                .presenter
                .as_ref()
                .is_some_and(|presenter| presenter.backend() == PresenterBackend::Gpu);
        if !can_fallback {
            return Err(error);
        }

        eprintln!("GPU presentation failed; switching to softbuffer: {error}");
        let window = self.window.clone().ok_or_else(|| {
            crate::present::PresentError::Backend(
                "cannot create software fallback without a window".to_owned(),
            )
        })?;
        let mut presenter =
            create_software_presenter(window, self.scene.width.max(1), self.scene.height.max(1))
                .map_err(crate::present::PresentError::Backend)?;
        let full_damage = [self.scene.bounds()];
        presenter.present(&self.scene, &full_damage, None)?;
        self.presenter = Some(presenter);
        Ok(())
    }

    fn sync_presenter_stats(&mut self) {
        let Some(stats) = self.presenter.as_ref().map(|presenter| presenter.stats()) else {
            return;
        };
        let atlas_changed = stats.atlas_bytes != self.gpu_memory_bytes;
        if atlas_changed {
            self._gpu_memory_lease = None;
            self.gpu_memory_bytes = stats.atlas_bytes;
            if stats.atlas_bytes > 0 {
                self._gpu_memory_lease = Some(
                    self.memory
                        .reserve(CacheCategory::GpuTiles, stats.atlas_bytes),
                );
            }
        }
        self.metrics.gpu_atlas_bytes = stats.atlas_bytes;
        self.metrics.gpu_atlas_uploads = stats.atlas_uploads;
        self.metrics.gpu_draw_calls = stats.draw_calls;
        self.metrics.gpu_vertices = stats.vertices;
        if self.gpu_diagnostics
            && (atlas_changed || self.last_gpu_diagnostics.elapsed().as_secs_f32() >= 1.0)
        {
            eprintln!(
                "viewer-gpu backend={} adapter={} atlas={}MiB resident={} uploads={} draws={} vertices={} compose_us={} present_us={} queues={}/{}/{}",
                stats.backend.as_deref().unwrap_or("software"),
                stats.adapter.as_deref().unwrap_or("n/a"),
                stats.atlas_bytes / (1024 * 1024),
                stats.atlas_resident_images,
                stats.atlas_uploads,
                stats.draw_calls,
                stats.vertices,
                self.metrics.compose_time.as_micros(),
                self.metrics.present_time.as_micros(),
                self.metrics.compile_pending,
                self.metrics.raster_pending,
                self.metrics.in_flight,
            );
            self.last_gpu_diagnostics = Instant::now();
        }
    }

    fn refresh_status(&mut self) {
        let viewport = self.viewport_document();
        self.status.update(&self.layout, viewport, self.zoom);
    }

    fn render_chrome_surfaces(&mut self, memory_bytes: u64) -> Vec<ChromeSurfacePlacement> {
        let mut surfaces = Vec::new();
        let toolbar_height = self.app_layout.toolbar.height.round().max(1.0) as u32;
        let base_control = self.theme.colors.scrollbar_thumb;
        let open_color = self.control_color(
            base_control,
            HoverControl::Toolbar(ToolbarAction::OpenDocument),
        );
        let open_bounds = button_bounds(4, 5, OPEN_GROUP_WIDTH as u32 - 10, toolbar_height);
        let open_button = self.ui_text.render(
            [0x4c45_4745, 9, 0, 0],
            OPEN_GROUP_WIDTH as u32,
            toolbar_height.min(256),
            self.theme.colors.chrome,
            &button_paint(
                4,
                5,
                OPEN_GROUP_WIDTH as u32 - 10,
                toolbar_height,
                open_color,
            ),
            &[centered_button_text(
                "Open…",
                open_bounds,
                14.0,
                contrast_text_color(open_color),
                true,
            )],
        );
        surfaces.push(ChromeSurfacePlacement {
            surface: open_button,
            destination: RectF {
                x: OPEN_GROUP_X,
                y: 0.0,
                width: OPEN_GROUP_WIDTH,
                height: f64::from(toolbar_height.min(256)),
            },
        });

        let zoom_out_color =
            self.control_color(base_control, HoverControl::Toolbar(ToolbarAction::ZoomOut));
        let zoom_in_color =
            self.control_color(base_control, HoverControl::Toolbar(ToolbarAction::ZoomIn));
        let fit_width_color =
            self.control_color(base_control, HoverControl::Toolbar(ToolbarAction::FitWidth));
        let fit_page_color =
            self.control_color(base_control, HoverControl::Toolbar(ToolbarAction::FitPage));
        let navigation = self.ui_text.render(
            [0x4c45_4745, 5, 0, 0],
            ZOOM_GROUP_WIDTH as u32,
            toolbar_height.min(256),
            self.theme.colors.chrome,
            &button_paint(2, 5, 30, toolbar_height, zoom_out_color)
                .into_iter()
                .chain(button_paint(36, 5, 30, toolbar_height, zoom_in_color))
                .chain(button_paint(70, 5, 74, toolbar_height, fit_width_color))
                .chain(button_paint(148, 5, 64, toolbar_height, fit_page_color))
                .collect::<Vec<_>>(),
            &[
                centered_button_text(
                    "−",
                    button_bounds(2, 5, 30, toolbar_height),
                    16.0,
                    contrast_text_color(zoom_out_color),
                    true,
                ),
                centered_button_text(
                    "+",
                    button_bounds(36, 5, 30, toolbar_height),
                    16.0,
                    contrast_text_color(zoom_in_color),
                    true,
                ),
                centered_button_text(
                    "Fit width",
                    button_bounds(70, 5, 74, toolbar_height),
                    13.0,
                    contrast_text_color(fit_width_color),
                    false,
                ),
                centered_button_text(
                    "Fit page",
                    button_bounds(148, 5, 64, toolbar_height),
                    13.0,
                    contrast_text_color(fit_page_color),
                    false,
                ),
            ],
        );
        surfaces.push(ChromeSurfacePlacement {
            surface: navigation,
            destination: RectF {
                x: ZOOM_GROUP_X,
                y: 0.0,
                width: ZOOM_GROUP_WIDTH,
                height: f64::from(toolbar_height.min(256)),
            },
        });

        let contents_color = self.control_color(
            base_control,
            HoverControl::Toolbar(ToolbarAction::ToggleSidebar),
        );
        let trim_base = if self.trim_enabled {
            self.theme.colors.selection
        } else {
            base_control
        };
        let trim_color =
            self.control_color(trim_base, HoverControl::Toolbar(ToolbarAction::ToggleTrim));
        let document_controls = self.ui_text.render(
            [0x4c45_4745, 7, self.trim_enabled as u64, 0],
            DOCUMENT_GROUP_WIDTH as u32,
            toolbar_height.min(256),
            self.theme.colors.chrome,
            &button_paint(2, 5, 74, toolbar_height, contents_color)
                .into_iter()
                .chain(button_paint(80, 5, 58, toolbar_height, trim_color))
                .collect::<Vec<_>>(),
            &[
                centered_button_text(
                    "Contents",
                    button_bounds(2, 5, 74, toolbar_height),
                    13.0,
                    contrast_text_color(contents_color),
                    false,
                ),
                centered_button_text(
                    if self.trim_enabled {
                        "Trim on"
                    } else {
                        "Trim off"
                    },
                    button_bounds(80, 5, 58, toolbar_height),
                    12.0,
                    contrast_text_color(trim_color),
                    self.trim_enabled,
                ),
            ],
        );
        surfaces.push(ChromeSurfacePlacement {
            surface: document_controls,
            destination: RectF {
                x: DOCUMENT_GROUP_X,
                y: 0.0,
                width: DOCUMENT_GROUP_WIDTH,
                height: f64::from(toolbar_height.min(256)),
            },
        });

        let processing_base = if self.processing_ui.running {
            self.theme.colors.error
        } else if self.processing_ui.visible {
            self.theme.colors.selection
        } else {
            base_control
        };
        let processing_color = self.control_color(
            processing_base,
            HoverControl::Toolbar(ToolbarAction::ToggleProcessing),
        );
        let process_surface = self.ui_text.render(
            [
                0x4c45_4745,
                11,
                self.processing_ui.running as u64,
                self.processing_ui.visible as u64,
            ],
            PROCESS_GROUP_WIDTH as u32,
            toolbar_height.min(256),
            self.theme.colors.chrome,
            &button_paint(
                4,
                5,
                PROCESS_GROUP_WIDTH as u32 - 10,
                toolbar_height,
                processing_color,
            ),
            &[centered_button_text(
                if self.processing_ui.running {
                    "Processing…"
                } else {
                    "Process"
                },
                button_bounds(4, 5, PROCESS_GROUP_WIDTH as u32 - 10, toolbar_height),
                12.5,
                contrast_text_color(processing_color),
                self.processing_ui.visible || self.processing_ui.running,
            )],
        );
        surfaces.push(ChromeSurfacePlacement {
            surface: process_surface,
            destination: RectF {
                x: PROCESS_GROUP_X,
                y: 0.0,
                width: PROCESS_GROUP_WIDTH,
                height: f64::from(toolbar_height.min(256)),
            },
        });

        let appearance_base = if self.options_visible {
            self.theme.colors.selection
        } else {
            base_control
        };
        let appearance_color = self.control_color(
            appearance_base,
            HoverControl::Toolbar(ToolbarAction::ToggleOptions),
        );
        let options = self.ui_text.render(
            [
                0x4c45_4745,
                8,
                self.options_visible as u64,
                self.color_mode as u64,
            ],
            OPTIONS_GROUP_WIDTH as u32,
            toolbar_height.min(256),
            self.theme.colors.chrome,
            &button_paint(
                4,
                5,
                OPTIONS_GROUP_WIDTH as u32 - 10,
                toolbar_height,
                appearance_color,
            ),
            &[centered_button_text(
                "Appearance",
                button_bounds(4, 5, OPTIONS_GROUP_WIDTH as u32 - 10, toolbar_height),
                12.0,
                contrast_text_color(appearance_color),
                self.options_visible,
            )],
        );
        surfaces.push(ChromeSurfacePlacement {
            surface: options,
            destination: RectF {
                x: OPTIONS_GROUP_X,
                y: 0.0,
                width: OPTIONS_GROUP_WIDTH,
                height: f64::from(toolbar_height.min(256)),
            },
        });

        let field = self.search_field_rect();
        let field_width = field.width.round().clamp(1.0, 256.0) as u32;
        let mut rectangles = vec![
            RectPaint {
                rect: RectI {
                    x: 0,
                    y: 0,
                    width: field_width,
                    height: field.height.round() as u32,
                },
                color: if self.search_ui.open {
                    0x00ff_ffff
                } else {
                    0x00df_dfdf
                },
            },
            RectPaint {
                rect: RectI {
                    x: 1,
                    y: 1,
                    width: field_width.saturating_sub(2),
                    height: 1,
                },
                color: if self.search_ui.open {
                    0x0056_8bd2
                } else {
                    0x0090_9090
                },
            },
        ];
        let placeholder = self.search_ui.query.is_empty() && !self.search_ui.open;
        let mut field_text = if placeholder {
            "Ctrl+F  Search document".to_owned()
        } else {
            self.search_ui.query.clone()
        };
        if !placeholder {
            field_text.insert_str(self.search_ui.cursor, &self.search_ui.preedit);
        }
        if self.search_ui.open {
            if let Some(range) = self.search_selection() {
                let selection_x = 8 + self.search_ui.query[..range.start]
                    .chars()
                    .count()
                    .saturating_mul(8) as i32;
                let selection_width = self.search_ui.query[range]
                    .chars()
                    .count()
                    .saturating_mul(8)
                    .max(1) as u32;
                rectangles.push(RectPaint {
                    rect: RectI {
                        x: selection_x,
                        y: 4,
                        width: selection_width.min(field_width.saturating_sub(9)),
                        height: (field.height.round() - 8.0).max(1.0) as u32,
                    },
                    color: 0x00bf_d7f4,
                });
            }
            let caret_x = 8
                + (self.search_ui.query[..self.search_ui.cursor]
                    .chars()
                    .count()
                    + self.search_ui.preedit.chars().count())
                .saturating_mul(8) as i32;
            rectangles.push(RectPaint {
                rect: RectI {
                    x: caret_x.min(field_width.saturating_sub(3) as i32),
                    y: 6,
                    width: 1,
                    height: (field.height.round() - 12.0).max(1.0) as u32,
                },
                color: self.theme.colors.text,
            });
        }
        let search_surface = self.ui_text.render(
            [0x4c45_4745, 2, 0, 0],
            field_width,
            field.height.round().max(1.0) as u32,
            self.theme.colors.chrome,
            &rectangles,
            &[TextPaint {
                text: field_text,
                x: 8,
                y: 5,
                max_width: field_width.saturating_sub(14),
                size: 14.0,
                color: if placeholder {
                    self.theme.colors.muted_text
                } else {
                    self.theme.colors.text
                },
                bold: false,
                centered: false,
            }],
        );
        surfaces.push(ChromeSurfacePlacement {
            surface: search_surface,
            destination: RectF {
                x: field.x,
                y: field.y,
                width: f64::from(field_width),
                height: field.height,
            },
        });

        // Match counts appear beside the field only while a search is active;
        // indexing progress lives in the status bar at the bottom instead of
        // cluttering the toolbar.
        if self.search_ui.open || !self.search_ui.query.is_empty() {
            let match_status = if self.search_ui.pending {
                "Searching…".to_owned()
            } else if self.search_ui.query.is_empty() {
                String::new()
            } else if self.search_ui.hits.is_empty() {
                "No matches".to_owned()
            } else {
                format!(
                    "{} of {}{}",
                    self.search_ui.active.map_or(0, |index| index + 1),
                    self.search_ui.hits.len(),
                    if self.search_ui.capped { "+" } else { "" },
                )
            };
            let status_surface = self.ui_text.render(
                [0x4c45_4745, 3, 0, 0],
                256,
                toolbar_height.min(256),
                self.theme.colors.chrome,
                &[],
                &[TextPaint {
                    text: match_status,
                    x: 5,
                    y: 10,
                    max_width: 246,
                    size: 13.0,
                    color: self.theme.colors.muted_text,
                    bold: false,
                    centered: false,
                }],
            );
            surfaces.push(ChromeSurfacePlacement {
                surface: status_surface,
                destination: RectF {
                    x: field.x + f64::from(field_width) + 8.0,
                    y: 0.0,
                    width: 256.0,
                    height: f64::from(toolbar_height.min(256)),
                },
            });
        }

        if self.sidebar_visible {
            self.render_outline_surfaces(&mut surfaces);
        }
        if self.processing_ui.visible {
            self.render_processing_surface(&mut surfaces);
            self.render_processing_dropdown_surface(&mut surfaces);
        }
        if self.options_visible {
            self.render_options_surface(&mut surfaces);
        }
        self.render_result_switch(&mut surfaces);

        let status_height = self.app_layout.status.height.round().max(1.0) as u32;
        let mut status_text = format!(
            "Page {} of {}   ·   {:.0}%   ·   {:.0} MiB",
            self.status.current_page.map_or(0, |page| page.0 + 1),
            self.status.page_count,
            self.zoom * 100.0,
            memory_bytes as f64 / (1024.0 * 1024.0)
        );
        // Transient indexing progress; disappears once the index is complete.
        if self.search_ui.total_pages > 0
            && self.search_ui.indexed_pages < self.search_ui.total_pages
        {
            let percent = (f64::from(self.search_ui.indexed_pages)
                / f64::from(self.search_ui.total_pages)
                * 100.0)
                .floor();
            status_text.push_str(&format!("   ·   Indexing {percent:.0}%"));
        }
        if self.processing_ui.running {
            status_text.push_str(&format!("   ·   {}", self.processing_ui.detail));
        }
        let footer_width = (self.app_layout.status.width.round() as u32).clamp(256, 1024);
        let footer = self.ui_text.render(
            [0x4c45_4745, 4, 0, 0],
            footer_width,
            status_height.min(256),
            self.theme.colors.chrome,
            &[],
            &[TextPaint {
                text: status_text,
                x: 8,
                y: 4,
                max_width: footer_width.saturating_sub(16),
                size: 12.0,
                color: self.theme.colors.text,
                bold: false,
                centered: false,
            }],
        );
        surfaces.push(ChromeSurfacePlacement {
            surface: footer,
            destination: RectF {
                x: self.app_layout.status.x,
                y: self.app_layout.status.y,
                width: f64::from(footer_width),
                height: f64::from(status_height.min(256)),
            },
        });

        if self.engine.descriptor().page_count == 0 {
            self.render_empty_state(&mut surfaces);
        }
        surfaces
    }

    /// The canvas stays blank until a document is chosen, so say why and point
    /// at the control that fixes it rather than showing an unexplained void.
    fn render_empty_state(&mut self, output: &mut Vec<ChromeSurfacePlacement>) {
        const WIDTH: u32 = 420;
        const HEIGHT: u32 = 62;
        let canvas = self.app_layout.canvas;
        if canvas.width < f64::from(WIDTH) || canvas.height < f64::from(HEIGHT) {
            return;
        }
        let surface = self.ui_text.render(
            [0x4c45_4745, 10, 0, 0],
            WIDTH,
            HEIGHT,
            self.theme.colors.canvas,
            &[],
            &[
                TextPaint {
                    text: "No document open".to_owned(),
                    x: 0,
                    y: 0,
                    max_width: WIDTH,
                    size: 20.0,
                    color: self.theme.colors.text,
                    bold: true,
                    centered: false,
                },
                TextPaint {
                    text: "Choose a PDF with the Open button above, or press Ctrl+O.".to_owned(),
                    x: 0,
                    y: 34,
                    max_width: WIDTH,
                    size: 14.0,
                    color: self.theme.colors.muted_text,
                    bold: false,
                    centered: false,
                },
            ],
        );
        output.push(ChromeSurfacePlacement {
            surface,
            destination: RectF {
                x: (canvas.x + (canvas.width - f64::from(WIDTH)) * 0.5).round(),
                y: (canvas.y + (canvas.height - f64::from(HEIGHT)) * 0.5).round(),
                width: f64::from(WIDTH),
                height: f64::from(HEIGHT),
            },
        });
    }

    fn render_result_switch(&mut self, output: &mut Vec<ChromeSurfacePlacement>) {
        let Some(destination) = self.result_switch_rect() else {
            return;
        };
        let width = destination.width.round() as u32;
        let height = destination.height.round() as u32;
        let half = width.saturating_sub(12) / 2;
        let control = self.theme.colors.scrollbar_thumb;
        let original_base = if self.processing_ui.viewing_new {
            control
        } else {
            self.theme.colors.selection
        };
        let new_base = if self.processing_ui.viewing_new
            || self.processing_ui.running
            || self.processing_ui.result_ready
        {
            self.theme.colors.selection
        } else {
            control
        };
        let original_color = self.control_color(original_base, HoverControl::Result(false));
        let new_color = self.control_color(new_base, HoverControl::Result(true));
        let new_label = if self.processing_ui.running {
            "New · processing"
        } else if self.processing_ui.result_ready && !self.processing_ui.viewing_new {
            "New · ready"
        } else {
            "New"
        };
        let surface = self.ui_text.render(
            [
                0x4c45_4745,
                20,
                self.processing_ui.running as u64,
                (self.processing_ui.result_ready as u64) << 1
                    | self.processing_ui.viewing_new as u64,
            ],
            width,
            height,
            self.theme.colors.canvas,
            &button_paint(4, 5, half, height, original_color)
                .into_iter()
                .chain(button_paint(8 + half as i32, 5, half, height, new_color))
                .collect::<Vec<_>>(),
            &[
                centered_button_text(
                    "Original",
                    button_bounds(4, 5, half, height),
                    12.0,
                    contrast_text_color(original_color),
                    !self.processing_ui.viewing_new,
                ),
                centered_button_text(
                    new_label,
                    button_bounds(8 + half as i32, 5, half, height),
                    12.0,
                    contrast_text_color(new_color),
                    self.processing_ui.viewing_new || self.processing_ui.running,
                ),
            ],
        );
        output.push(ChromeSurfacePlacement {
            surface,
            destination,
        });
    }

    fn render_processing_surface(&mut self, output: &mut Vec<ChromeSurfacePlacement>) {
        let Some(panel) = self.processing_panel_rect() else {
            return;
        };
        let width = panel.width.round() as u32;
        let height = panel.height.round() as u32;
        let rows = self.processing_option_rows();
        let base_control = self.theme.colors.scrollbar_thumb;
        let mut rectangles = vec![
            RectPaint {
                rect: RectI {
                    x: 5,
                    y: 5,
                    width: width.saturating_sub(5),
                    height: height.saturating_sub(5),
                },
                color: 0x0018_1818,
            },
            RectPaint {
                rect: RectI {
                    x: 0,
                    y: 0,
                    width: width.saturating_sub(6),
                    height: height.saturating_sub(6),
                },
                color: self.theme.colors.chrome,
            },
            RectPaint {
                rect: RectI {
                    x: 0,
                    y: 0,
                    width: width.saturating_sub(6),
                    height: 34,
                },
                color: self.theme.colors.selection,
            },
        ];
        let tab_width = width.saturating_sub(28) / 3;
        for (index, tab) in ProcessingTab::ALL.into_iter().enumerate() {
            let base = if tab == self.processing_ui.tab {
                self.theme.colors.selection
            } else {
                base_control
            };
            let color = self.control_color(
                base,
                HoverControl::Processing(ProcessingPanelAction::Tab(tab)),
            );
            rectangles.extend(button_paint(
                14 + index as i32 * tab_width as i32,
                40,
                tab_width.saturating_sub(4),
                36,
                color,
            ));
        }
        for row in 0..rows.len().min(8) {
            let y = 80 + row as i32 * 26;
            let color = if row % 2 == 0 {
                self.theme.colors.canvas
            } else {
                self.theme.colors.chrome
            };
            rectangles.push(RectPaint {
                rect: RectI {
                    x: 14,
                    y,
                    width: width.saturating_sub(34),
                    height: 23,
                },
                color,
            });
            let field_width = width.saturating_sub(width / 2 + 20);
            let is_resolution =
                self.processing_ui.tab == ProcessingTab::Page && row == RESOLUTION_ROW;
            let is_layout = self.processing_ui.tab == ProcessingTab::Page && row == LAYOUT_ROW;
            if is_layout {
                let segment_width = field_width.saturating_sub(8) / 3;
                for (segment, label) in LAYOUT_SEGMENTS.iter().enumerate() {
                    let x = (width / 2) as i32 + segment as i32 * (segment_width as i32 + 4);
                    let base = if self.processing_choice_is_selected(row, label) {
                        self.theme.colors.selection
                    } else {
                        base_control
                    };
                    let color = self.control_color(
                        base,
                        HoverControl::Processing(ProcessingPanelAction::Choice {
                            option: row,
                            choice: segment,
                        }),
                    );
                    rectangles.extend(button_paint(x, y, segment_width, 33, color));
                }
            } else if is_resolution {
                let field_color = self.control_color(
                    if self.processing_ui.resolution_editing {
                        self.theme.colors.paper
                    } else {
                        self.theme.colors.chrome
                    },
                    HoverControl::Processing(ProcessingPanelAction::Option(row)),
                );
                rectangles.push(RectPaint {
                    rect: RectI {
                        x: (width / 2) as i32,
                        y: y + 2,
                        width: field_width,
                        height: 19,
                    },
                    color: field_color,
                });
                rectangles.push(RectPaint {
                    rect: RectI {
                        x: (width / 2) as i32,
                        y: y + 2,
                        width: field_width,
                        height: 1,
                    },
                    color: if self.processing_ui.resolution_editing {
                        self.theme.colors.selection
                    } else {
                        self.theme.colors.page_border
                    },
                });
            } else {
                let base = if self.processing_ui.open_option == Some(row) {
                    self.theme.colors.selection
                } else {
                    base_control
                };
                let color = self.control_color(
                    base,
                    HoverControl::Processing(ProcessingPanelAction::Option(row)),
                );
                rectangles.extend(button_paint((width / 2) as i32, y, field_width, 33, color));
            }
        }
        let action_y = height.saturating_sub(58) as i32;
        let run_width = ((width.saturating_sub(34)) as f64 * 0.58) as u32;
        let run_base = if self.processing_ui.running {
            self.theme.colors.error
        } else {
            self.theme.colors.selection
        };
        let run_color = self.control_color(
            run_base,
            HoverControl::Processing(ProcessingPanelAction::Run),
        );
        let preset_color = self.control_color(
            base_control,
            HoverControl::Processing(ProcessingPanelAction::ToggleProfile),
        );
        rectangles.extend(button_paint(14, action_y, run_width, 44, run_color));
        rectangles.extend(button_paint(
            18 + run_width as i32,
            action_y,
            width.saturating_sub(run_width + 38),
            44,
            preset_color,
        ));

        let mut text = vec![TextPaint {
            text: if self.processing_ui.running {
                format!("Processing · {}", self.processing_ui.detail)
            } else {
                "Document processing".to_owned()
            },
            x: 14,
            y: 10,
            max_width: width.saturating_sub(28),
            size: 15.0,
            color: contrast_text_color(self.theme.colors.selection),
            bold: true,
            centered: false,
        }];
        for (index, tab) in ProcessingTab::ALL.into_iter().enumerate() {
            let base = if tab == self.processing_ui.tab {
                self.theme.colors.selection
            } else {
                base_control
            };
            let color = self.control_color(
                base,
                HoverControl::Processing(ProcessingPanelAction::Tab(tab)),
            );
            text.push(centered_button_text(
                tab.label(),
                button_bounds(
                    14 + index as i32 * tab_width as i32,
                    40,
                    tab_width.saturating_sub(4),
                    36,
                ),
                11.0,
                contrast_text_color(color),
                tab == self.processing_ui.tab,
            ));
        }
        for (row, (label, value)) in rows.into_iter().take(8).enumerate() {
            let y = 86 + row as i32 * 26;
            text.push(TextPaint {
                text: label,
                x: 22,
                y,
                max_width: width / 2 - 30,
                size: 12.0,
                color: self.theme.colors.text,
                bold: false,
                centered: false,
            });
            let is_resolution =
                self.processing_ui.tab == ProcessingTab::Page && row == RESOLUTION_ROW;
            let is_layout = self.processing_ui.tab == ProcessingTab::Page && row == LAYOUT_ROW;
            if is_layout {
                let field_width = width.saturating_sub(width / 2 + 20);
                let segment_width = field_width.saturating_sub(8) / 3;
                for (segment, label) in LAYOUT_SEGMENTS.iter().enumerate() {
                    let x = (width / 2) as i32 + segment as i32 * (segment_width as i32 + 4);
                    let selected = self.processing_choice_is_selected(row, label);
                    let base = if selected {
                        self.theme.colors.selection
                    } else {
                        base_control
                    };
                    let color = self.control_color(
                        base,
                        HoverControl::Processing(ProcessingPanelAction::Choice {
                            option: row,
                            choice: segment,
                        }),
                    );
                    text.push(centered_button_text(
                        *label,
                        button_bounds(x, y - 6, segment_width, 33),
                        11.0,
                        contrast_text_color(color),
                        selected,
                    ));
                }
                continue;
            }
            let choices = self.processing_option_choices(row);
            let base = if self.processing_ui.open_option == Some(row) {
                self.theme.colors.selection
            } else {
                base_control
            };
            let color = self.control_color(
                base,
                HoverControl::Processing(ProcessingPanelAction::Option(row)),
            );
            text.push(centered_button_text(
                if is_resolution {
                    value
                } else if choices.len() == 2 {
                    format!("{value}  ↔")
                } else {
                    format!("{value}  ▾")
                },
                button_bounds(
                    (width / 2) as i32,
                    y - 6,
                    width.saturating_sub(width / 2 + 20),
                    33,
                ),
                11.0,
                if is_resolution {
                    self.theme.colors.text
                } else {
                    contrast_text_color(color)
                },
                true,
            ));
        }
        text.extend([
            centered_button_text(
                if self.processing_ui.running {
                    "Stop safely"
                } else {
                    "Run with these options"
                },
                button_bounds(14, action_y, run_width, 44),
                12.0,
                contrast_text_color(run_color),
                true,
            ),
            centered_button_text(
                format!("Preset: {}", self.processing_ui.profile.label()),
                button_bounds(
                    18 + run_width as i32,
                    action_y,
                    width.saturating_sub(run_width + 38),
                    44,
                ),
                11.0,
                contrast_text_color(preset_color),
                true,
            ),
            TextPaint {
                text: "Choose values from the menus · drag lower-right corner to resize".to_owned(),
                x: 14,
                y: height.saturating_sub(16) as i32,
                max_width: width.saturating_sub(34),
                size: 9.0,
                color: self.theme.colors.muted_text,
                bold: false,
                centered: false,
            },
        ]);

        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        format!(
            "{:?}{:?}{:?}{:?}",
            self.processing_ui.options,
            self.processing_ui.scope,
            self.processing_ui.tab,
            self.processing_ui.open_option
        )
        .hash(&mut hasher);
        let surface = self.ui_text.render(
            [
                0x4c45_4745,
                hasher.finish(),
                self.processing_ui.running as u64,
                width as u64 | (u64::from(height) << 32),
            ],
            width,
            height,
            self.theme.colors.canvas,
            &rectangles,
            &text,
        );
        output.push(ChromeSurfacePlacement {
            surface,
            destination: panel,
        });
    }

    fn render_processing_dropdown_surface(&mut self, output: &mut Vec<ChromeSurfacePlacement>) {
        let Some(option) = self.processing_ui.open_option else {
            return;
        };
        let Some(panel) = self.processing_dropdown_rect() else {
            return;
        };
        let choices = self.processing_option_choices(option);
        let width = panel.width.round() as u32;
        let height = panel.height.round() as u32;
        let mut rectangles = vec![
            RectPaint {
                rect: RectI {
                    x: 3,
                    y: 3,
                    width: width.saturating_sub(3),
                    height: height.saturating_sub(3),
                },
                color: 0x0010_1010,
            },
            RectPaint {
                rect: RectI {
                    x: 0,
                    y: 0,
                    width: width.saturating_sub(3),
                    height: height.saturating_sub(3),
                },
                color: self.theme.colors.chrome,
            },
        ];
        let mut text = Vec::with_capacity(choices.len());
        for (index, choice) in choices.into_iter().enumerate() {
            let y = 4 + index as i32 * 26;
            let selected = self.processing_choice_is_selected(option, &choice);
            let hovered = self.hovered_control
                == Some(HoverControl::Processing(ProcessingPanelAction::Choice {
                    option,
                    choice: index,
                }));
            if selected || hovered {
                rectangles.push(RectPaint {
                    rect: RectI {
                        x: 4,
                        y,
                        width: width.saturating_sub(11),
                        height: 25,
                    },
                    color: if selected {
                        self.theme.colors.selection
                    } else {
                        hover_adjusted_color(
                            self.theme.colors.chrome,
                            true,
                            color_luminance(self.theme.colors.chrome) < 0.35,
                        )
                    },
                });
            }
            text.push(TextPaint {
                text: if selected {
                    format!("✓  {choice}")
                } else {
                    format!("   {choice}")
                },
                x: 10,
                y: y + 4,
                max_width: width.saturating_sub(20),
                size: 12.0,
                color: if selected {
                    contrast_text_color(self.theme.colors.selection)
                } else {
                    self.theme.colors.text
                },
                bold: selected,
                centered: false,
            });
        }
        output.push(ChromeSurfacePlacement {
            surface: self.ui_text.render(
                [0x4c45_4745, 19, option as u64, self.color_mode as u64],
                width,
                height,
                self.theme.colors.chrome,
                &rectangles,
                &text,
            ),
            destination: panel,
        });
    }

    fn render_options_surface(&mut self, output: &mut Vec<ChromeSurfacePlacement>) {
        const WIDTH: u32 = 292;
        const HEIGHT: u32 = 206;
        let Some(panel) = self.options_panel_rect() else {
            return;
        };
        let choices = [
            (ColorMode::Original, "Original"),
            (ColorMode::Night, "Night"),
            (ColorMode::WarmPaper, "Warm paper"),
            (ColorMode::SanzoEarth, "Sanzo earth"),
            (ColorMode::SanzoSea, "Sanzo sea"),
        ];
        let mut rectangles = vec![
            RectPaint {
                rect: RectI {
                    x: 4,
                    y: 4,
                    width: WIDTH - 4,
                    height: HEIGHT - 4,
                },
                color: 0x0018_1818,
            },
            RectPaint {
                rect: RectI {
                    x: 0,
                    y: 0,
                    width: WIDTH - 5,
                    height: HEIGHT - 5,
                },
                color: self.theme.colors.chrome,
            },
        ];
        let mut text = vec![TextPaint {
            text: "Appearance".to_owned(),
            x: 14,
            y: 10,
            max_width: WIDTH - 28,
            size: 16.0,
            color: self.theme.colors.text,
            bold: true,
            centered: false,
        }];
        for (index, (mode, label)) in choices.into_iter().enumerate() {
            let y = 38 + index as i32 * 28;
            let hovered = self.hovered_control == Some(HoverControl::Appearance(mode));
            if self.color_mode == mode || hovered {
                rectangles.push(RectPaint {
                    rect: RectI {
                        x: 10,
                        y: y - 3,
                        width: WIDTH - 26,
                        height: 24,
                    },
                    color: if self.color_mode == mode {
                        self.theme.colors.selection
                    } else {
                        hover_adjusted_color(
                            self.theme.colors.chrome,
                            true,
                            color_luminance(self.theme.colors.chrome) < 0.35,
                        )
                    },
                });
            }
            text.push(TextPaint {
                text: label.to_owned(),
                x: 18,
                y,
                max_width: WIDTH - 38,
                size: 13.0,
                color: if self.color_mode == mode {
                    contrast_text_color(self.theme.colors.selection)
                } else {
                    self.theme.colors.text
                },
                bold: self.color_mode == mode,
                centered: false,
            });
        }
        output.push(ChromeSurfacePlacement {
            surface: self.ui_text.render(
                [0x4c45_4745, 13, self.color_mode as u64, 0],
                WIDTH,
                HEIGHT,
                self.theme.colors.canvas,
                &rectangles,
                &text,
            ),
            destination: panel,
        });
    }

    fn render_outline_surfaces(&mut self, output: &mut Vec<ChromeSurfacePlacement>) {
        let width = self.app_layout.sidebar.width.round().clamp(1.0, 256.0) as u32;
        let source =
            self.outline
                .first()
                .map_or("Building contents…", |node| match node.source {
                    crate::document::OutlineSource::Embedded => "Contents",
                    crate::document::OutlineSource::Synthesized => "Generated contents",
                });
        let header = self.ui_text.render(
            [0x4c45_4745, 5, 0, 0],
            width,
            34,
            self.theme.colors.chrome,
            &[],
            &[TextPaint {
                text: source.to_owned(),
                x: 10,
                y: 8,
                max_width: width.saturating_sub(16),
                size: 14.0,
                color: self.theme.colors.text,
                bold: true,
                centered: false,
            }],
        );
        output.push(ChromeSurfacePlacement {
            surface: header,
            destination: RectF {
                x: self.app_layout.sidebar.x,
                y: self.app_layout.sidebar.y,
                width: f64::from(width),
                height: 34.0,
            },
        });
        let start = self.outline_window_start();
        let rows = ((self.app_layout.sidebar.height - 34.0) / 24.0)
            .ceil()
            .max(0.0) as usize;
        let current = self.status.current_page.unwrap_or(PageIndex(0));
        for (row, (index, node)) in self
            .outline
            .iter()
            .enumerate()
            .skip(start)
            .take(rows)
            .enumerate()
        {
            let selected = node.page <= current
                && self
                    .outline
                    .get(index + 1)
                    .is_none_or(|next| next.page > current);
            let row_surface = self.ui_text.render(
                [0x4c45_4745, 6, index as u64, 0],
                width,
                24,
                if selected {
                    0x00d8_e7f8
                } else {
                    self.theme.colors.chrome
                },
                &[],
                &[TextPaint {
                    text: node.title.to_string(),
                    x: 8 + i32::from(node.depth.min(8)) * 12,
                    y: 4,
                    max_width: width.saturating_sub(16),
                    size: 13.0,
                    color: self.theme.colors.text,
                    bold: node.depth == 0,
                    centered: false,
                }],
            );
            output.push(ChromeSurfacePlacement {
                surface: row_surface,
                destination: RectF {
                    x: self.app_layout.sidebar.x,
                    y: self.app_layout.sidebar.y + 34.0 + row as f64 * 24.0,
                    width: f64::from(width),
                    height: 24.0,
                },
            });
        }
    }
}

impl Drop for ViewerApp {
    fn drop(&mut self) {
        if self.processing_ui.running {
            if let Some(control) = &self.processing_control {
                control.cancel();
            }
        }
    }
}

impl ApplicationHandler<ViewerEvent> for ViewerApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if let Err(error) = self.create_window(event_loop) {
            eprintln!("viewer startup failure: {error}");
            event_loop.exit();
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: ViewerEvent) {
        match event {
            ViewerEvent::Wake => self.drain_updates(),
            ViewerEvent::FatalBackgroundError(message) => {
                eprintln!("viewer background failure: {message}");
            }
            ViewerEvent::Processing(update) => self.apply_processing_update(update),
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                self.resize(size.width, size.height, false);
                // Present a frame at the new size before returning to the
                // modal resize loop. Waiting for the next RedrawRequested
                // leaves the previous frame stretched across the new surface,
                // which reads as black gutters and rubber-banding pages while
                // the user drags the window border.
                if !self.surface_suspended {
                    self.frame.redraw_started();
                    self.refresh_status();
                    if let Err(error) = self.compose() {
                        eprintln!("viewer presentation failure: {error}");
                        event_loop.exit();
                    }
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale_factor = scale_factor;
                if let Some(window) = &self.window {
                    let size = window.inner_size();
                    self.resize(size.width, size.height, false);
                }
            }
            WindowEvent::RedrawRequested => {
                self.frame.redraw_started();
                self.refresh_status();
                if let Err(error) = self.compose() {
                    eprintln!("viewer presentation failure: {error}");
                    event_loop.exit();
                }
                self.frame.settle_if_quiet();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                self.metrics.input_received = Some(Instant::now());
                if self.sidebar_visible
                    && self
                        .app_layout
                        .sidebar
                        .contains(self.input.pointer_position)
                {
                    let rows = match delta {
                        MouseScrollDelta::PixelDelta(position) => {
                            if position.y > 0.0 {
                                -3
                            } else if position.y < 0.0 {
                                3
                            } else {
                                0
                            }
                        }
                        MouseScrollDelta::LineDelta(_, y) => (-y.signum() * 3.0) as i32,
                    };
                    if rows != 0 {
                        self.scroll_outline(rows);
                    }
                    return;
                }
                match delta {
                    MouseScrollDelta::PixelDelta(position) => {
                        self.scroll_by(ScrollCommand::WheelPixels(Vec2d {
                            x: -position.x,
                            y: -position.y,
                        }));
                    }
                    MouseScrollDelta::LineDelta(x, y) => {
                        self.scroll_by(ScrollCommand::WheelLines(Vec2d {
                            x: -f64::from(x),
                            y: -f64::from(y),
                        }));
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.handle_cursor_moved(position.x, position.y);
            }
            WindowEvent::MouseInput { state, button, .. } => {
                self.handle_mouse_input(state, button);
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.input.modifiers = modifiers.state();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                self.handle_key(&event.logical_key, event.physical_key, event.state);
            }
            WindowEvent::Ime(Ime::Preedit(text, _cursor)) => {
                if self.search_ui.open {
                    self.search_ui.preedit = text;
                    self.damage.mark_full();
                    self.request_redraw();
                }
            }
            WindowEvent::Ime(Ime::Commit(text)) => {
                if self.processing_ui.resolution_editing {
                    self.insert_resolution_text(&text);
                    self.damage.mark_full();
                    self.request_redraw();
                } else if self.search_ui.open {
                    self.search_ui.preedit.clear();
                    self.insert_search_text(&text);
                }
            }
            WindowEvent::Ime(Ime::Enabled | Ime::Disabled) => {}
            WindowEvent::Focused(false) => {
                self.input.capture = None;
                self.hovered_control = None;
                self.commit_resolution_edit();
                self.scrollbar.end_drag();
                self.pointer_warm = None;
                self.search_ui.preedit.clear();
                if self.navigation_mode == NavigationMode::Skimming {
                    self.navigation_mode = NavigationMode::Idle;
                    self.navigation_settle_deadline = None;
                    self.scroll.settle();
                    self.bump_generation();
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        if self.scrollbar.reveal_preview_if_due(now) {
            self.damage.mark_full();
            self.request_redraw();
        }
        if self
            .hovered_link
            .as_ref()
            .and_then(LinkHoverState::peek_deadline)
            .is_some_and(|deadline| now >= deadline)
            && let Some(hovered) = self.hovered_link.as_mut()
        {
            hovered.peek_visible = true;
            self.damage.mark_full();
            self.request_redraw();
        }
        if self
            .navigation_settle_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.navigation_settle_deadline = None;
            self.scroll.settle();
            if !matches!(self.input.capture, Some(PointerCapture::VerticalThumb(_))) {
                self.navigation_mode = NavigationMode::Idle;
                self.bump_generation();
            }
        }
        let deadline = [
            self.scrollbar.preview_deadline(),
            self.navigation_settle_deadline,
            self.hovered_link
                .as_ref()
                .and_then(LinkHoverState::peek_deadline),
        ]
        .into_iter()
        .flatten()
        .min();
        if let Some(deadline) = deadline {
            event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }
}

#[cfg(feature = "pdf-engine")]
fn open_pdf_engine(path: &std::path::Path) -> Result<Arc<dyn DocumentEngine>, String> {
    crate::document::pdf_engine::PdfEngine::open(path, None)
        .map(|engine| Arc::new(engine) as Arc<dyn DocumentEngine>)
        .map_err(|error| error.to_string())
}

#[cfg(not(feature = "pdf-engine"))]
fn open_pdf_engine(_path: &std::path::Path) -> Result<Arc<dyn DocumentEngine>, String> {
    Err("this build does not include the PDF engine".to_owned())
}

/// A Windows build has no console of its own, so a failed open has to be shown
/// on screen as well as written to stderr.
fn report_document_error(path: &std::path::Path, message: &str) {
    let text = format!("Could not open {}\n\n{message}", path.display());
    eprintln!("lege-gui: {text}");
    let _ = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title("Lege")
        .set_description(text)
        .show();
}

fn previous_boundary(text: &str, cursor: usize) -> usize {
    text[..cursor.min(text.len())]
        .char_indices()
        .next_back()
        .map_or(0, |(index, _)| index)
}

fn next_boundary(text: &str, cursor: usize) -> usize {
    let cursor = cursor.min(text.len());
    text[cursor..]
        .char_indices()
        .nth(1)
        .map_or(text.len(), |(offset, _)| cursor + offset)
}

fn external_uri_is_allowed(uri: &str) -> bool {
    let Some((scheme, _)) = uri.split_once(':') else {
        return false;
    };
    matches!(
        scheme.to_ascii_lowercase().as_str(),
        "http" | "https" | "mailto"
    )
}

fn default_processing_output(input: &std::path::Path, extension: &str) -> std::path::PathBuf {
    let stem = input
        .file_stem()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("document");
    input.with_file_name(format!("{stem}-lege.{extension}"))
}

fn fit_page_scale(canvas: SizeF, page: SizeF, margin: f64) -> f64 {
    if page.width <= 0.0 || page.height <= 0.0 {
        return 1.0;
    }
    let horizontal_extent = page.width + margin.max(0.0) * 2.0;
    let vertical_extent = page.height + margin.max(0.0) * 2.0;
    (canvas.width / horizontal_extent)
        .min(canvas.height / vertical_extent)
        .clamp(0.05, 12.0)
}

fn automatic_zoom_scale(fit_page: f64, fit_width: f64) -> f64 {
    (fit_page * std::f64::consts::SQRT_2)
        .min(fit_width)
        .clamp(0.05, 12.0)
}

fn centered_document_origin_x(canvas: RectF, scaled_document_width: f64) -> f64 {
    canvas.x + ((canvas.width - scaled_document_width.max(0.0)) * 0.5).max(0.0)
}

#[allow(clippy::too_many_arguments)]
fn paint_scene(
    painter: &mut SceneBuilder<'_>,
    theme: &Theme,
    app_layout: AppLayout,
    layout: &PageLayoutIndex,
    viewport_document: RectF,
    zoom: f64,
    scroll: Vec2d,
    intent: &ViewportIntent,
    tiles: &TileFrameSnapshot,
    previews: &PagePreviewCache,
    tile_scratch: &mut Vec<Arc<crate::document::TileSurface>>,
    painted_tiles: &mut HashSet<crate::document::TileKey>,
    page_errors: &HashMap<PageIndex, String>,
    page_artifacts: &HashMap<PageIndex, PageViewArtifacts>,
    search_hits: &[SearchHit],
    active_search_hit: Option<usize>,
    selection: &SelectionModel,
    scrollbar: ScrollbarGeometry,
    hover_preview: Option<(PageIndex, f64)>,
    link_peek: Option<LinkPeekView>,
) {
    painter.fill_rect(RectI::from(app_layout.toolbar), theme.colors.chrome);
    painter.fill_rect(RectI::from(app_layout.sidebar), theme.colors.chrome);
    painter.fill_rect(RectI::from(app_layout.status), theme.colors.chrome);
    painter.fill_rect(RectI::from(app_layout.canvas), theme.colors.canvas);

    painter.push_clip(RectI::from(app_layout.canvas));
    let document_origin_x =
        centered_document_origin_x(app_layout.canvas, layout.total_width * zoom);
    for placement in &layout.placements()[layout.visible_pages(viewport_document)] {
        let page_screen = RectF {
            x: document_origin_x + placement.bounds.x * zoom - scroll.x,
            y: app_layout.canvas.y + placement.bounds.y * zoom - scroll.y,
            width: placement.bounds.width * zoom,
            height: placement.bounds.height * zoom,
        };
        let page_rect = RectI::from(page_screen);
        painter.fill_rect(page_rect, theme.colors.paper);
        painter.stroke_rect(page_rect, 1, theme.colors.page_border);

        painter.push_clip(page_rect);
        if let Some(preview) = previews.get_variant(placement.page, layout.render_variant) {
            painter.draw_tile(preview, page_screen, ImageSampling::Linear);
        }
        painted_tiles.clear();
        for demand in intent
            .visible_tiles
            .iter()
            .filter(|demand| demand.page == placement.page)
        {
            tiles.best_covering_into(*demand, intent.bucket, tile_scratch);
            for tile in tile_scratch.iter() {
                if !painted_tiles.insert(tile.key) {
                    continue;
                }
                let destination = RectF {
                    x: document_origin_x + tile.page_document_rect.x * zoom - scroll.x,
                    y: app_layout.canvas.y + tile.page_document_rect.y * zoom - scroll.y,
                    width: tile.page_document_rect.width * zoom,
                    height: tile.page_document_rect.height * zoom,
                };
                let sampling = if tile.key.bucket == intent.bucket
                    && (destination.width - f64::from(tile.pixels.width)).abs() < 0.01
                    && (destination.height - f64::from(tile.pixels.height)).abs() < 0.01
                {
                    ImageSampling::Nearest
                } else {
                    ImageSampling::Linear
                };
                painter.draw_tile(Arc::clone(tile), destination, sampling);
                if tile.degraded {
                    painter.fill_rect(
                        RectI {
                            x: page_rect.right() - 10,
                            y: page_rect.y,
                            width: 10,
                            height: 10,
                        },
                        theme.colors.error,
                    );
                }
            }
        }

        if let Some(artifacts) = page_artifacts.get(&placement.page) {
            for (index, hit) in search_hits
                .iter()
                .enumerate()
                .filter(|(_, hit)| hit.page == placement.page)
            {
                let overlays = SearchIndex::overlays_for_hit(hit, &artifacts.text);
                let color = if Some(index) == active_search_hit {
                    0x90_ff_98_00
                } else {
                    0x68_ff_d5_4f
                };
                for overlay in overlays.iter().copied() {
                    painter.blend_rect(
                        RectI::from(RectF {
                            x: document_origin_x + overlay.x * zoom - scroll.x,
                            y: app_layout.canvas.y + overlay.y * zoom - scroll.y,
                            width: overlay.width * zoom,
                            height: overlay.height * zoom,
                        }),
                        color,
                    );
                }
            }
            for overlay in selection
                .overlays(placement.page, &artifacts.text)
                .iter()
                .copied()
            {
                painter.blend_rect(
                    RectI::from(RectF {
                        x: document_origin_x + overlay.x * zoom - scroll.x,
                        y: app_layout.canvas.y + overlay.y * zoom - scroll.y,
                        width: overlay.width * zoom,
                        height: overlay.height * zoom,
                    }),
                    0x70_56_8b_d2,
                );
            }
        }
        painter.pop_clip();

        if page_errors.contains_key(&placement.page) {
            painter.fill_rect(
                RectI {
                    x: page_rect.x + 6,
                    y: page_rect.y + 6,
                    width: 18,
                    height: 18,
                },
                theme.colors.error,
            );
        }
        draw_page_number_marker(
            painter,
            page_rect,
            placement.page.0 + 1,
            theme.colors.muted_text,
        );
    }
    painter.pop_clip();

    paint_scrollbar_document_map(painter, theme, layout, scrollbar, search_hits);
    if let Some((page, fraction)) = hover_preview {
        paint_scrollbar_preview(
            painter, theme, layout, tiles, previews, scrollbar, page, fraction,
        );
    }
    if let Some(peek) = link_peek {
        paint_link_peek(painter, theme, app_layout.canvas, layout, previews, peek);
    }
}

fn paint_scrollbar_document_map(
    painter: &mut SceneBuilder<'_>,
    theme: &Theme,
    layout: &PageLayoutIndex,
    scrollbar: ScrollbarGeometry,
    search_hits: &[SearchHit],
) {
    painter.fill_rect(RectI::from(scrollbar.track), theme.colors.scrollbar_track);
    if layout.placements().len() <= 200 && layout.total_height > 0.0 {
        for placement in layout.placements() {
            let fraction = (placement.bounds.y / layout.total_height).clamp(0.0, 1.0);
            let y = (scrollbar.track.y + fraction * scrollbar.track.height).round() as i32;
            painter.fill_rect(
                RectI {
                    x: scrollbar.track.x.round() as i32 + 2,
                    y,
                    width: (scrollbar.track.width - 4.0).max(1.0) as u32,
                    height: 1,
                },
                theme.colors.muted_text,
            );
        }
    }
    let mut marked_pages = HashSet::new();
    for hit in search_hits {
        if !marked_pages.insert(hit.page) {
            continue;
        }
        let Some(placement) = layout.placement(hit.page) else {
            continue;
        };
        let fraction = (placement.bounds.center().y / layout.total_height.max(1.0)).clamp(0.0, 1.0);
        let y = (scrollbar.track.y + fraction * scrollbar.track.height).round() as i32;
        painter.fill_rect(
            RectI {
                x: scrollbar.track.x.round() as i32,
                y,
                width: scrollbar.track.width.max(1.0).round() as u32,
                height: 2,
            },
            0x00e0_8a00,
        );
    }
    painter.fill_rect(RectI::from(scrollbar.thumb), theme.colors.scrollbar_thumb);
}

#[allow(clippy::too_many_arguments)]
fn paint_scrollbar_preview(
    painter: &mut SceneBuilder<'_>,
    theme: &Theme,
    layout: &PageLayoutIndex,
    tiles: &TileFrameSnapshot,
    previews: &PagePreviewCache,
    scrollbar: ScrollbarGeometry,
    page: PageIndex,
    document_fraction: f64,
) {
    let Some(placement) = layout.placement(page) else {
        return;
    };
    const POPUP_WIDTH: f64 = 184.0;
    const PADDING: f64 = 8.0;
    const LABEL_HEIGHT: f64 = 18.0;
    const MAX_IMAGE_HEIGHT: f64 = 210.0;

    let image_width = POPUP_WIDTH - PADDING * 2.0;
    let image_height = (image_width * placement.bounds.height / placement.bounds.width.max(1.0))
        .clamp(32.0, MAX_IMAGE_HEIGHT);
    let popup_height = image_height + LABEL_HEIGHT + PADDING * 2.0;
    let pointer_y = scrollbar.track.y + document_fraction.clamp(0.0, 1.0) * scrollbar.track.height;
    let popup_y = (pointer_y - popup_height * 0.5).clamp(
        scrollbar.track.y,
        (scrollbar.track.bottom() - popup_height).max(scrollbar.track.y),
    );
    let popup = RectF {
        x: scrollbar.track.x - POPUP_WIDTH - 8.0,
        y: popup_y,
        width: POPUP_WIDTH,
        height: popup_height,
    };
    painter.fill_rect(RectI::from(popup), theme.colors.chrome);
    painter.stroke_rect(RectI::from(popup), 1, theme.colors.page_border);

    let image = RectF {
        x: popup.x + PADDING,
        y: popup.y + PADDING,
        width: image_width,
        height: image_height,
    };
    painter.fill_rect(RectI::from(image), theme.colors.paper);
    painter.stroke_rect(RectI::from(image), 1, theme.colors.page_border);

    let scale = (image.width / placement.bounds.width.max(1.0))
        .min(image.height / placement.bounds.height.max(1.0));
    let fitted_width = placement.bounds.width * scale;
    let fitted_height = placement.bounds.height * scale;
    let page_origin_x = image.x + (image.width - fitted_width) * 0.5;
    let page_origin_y = image.y + (image.height - fitted_height) * 0.5;
    painter.push_clip(RectI::from(image));
    if let Some(preview) = previews.get_variant(page, layout.render_variant) {
        painter.draw_tile(
            preview,
            RectF {
                x: page_origin_x,
                y: page_origin_y,
                width: fitted_width,
                height: fitted_height,
            },
            ImageSampling::Linear,
        );
    } else {
        for tile in tiles
            .page_tiles_at_tier(page, TileTier::Thumbnail)
            .into_iter()
            .filter(|tile| tile.key.variant == layout.render_variant)
        {
            let destination = RectF {
                x: page_origin_x + (tile.page_document_rect.x - placement.bounds.x) * scale,
                y: page_origin_y + (tile.page_document_rect.y - placement.bounds.y) * scale,
                width: tile.page_document_rect.width * scale,
                height: tile.page_document_rect.height * scale,
            };
            painter.draw_tile(tile, destination, ImageSampling::Linear);
        }
    }
    painter.pop_clip();

    draw_page_number_marker(
        painter,
        RectI::from(RectF {
            x: popup.x,
            y: image.bottom(),
            width: popup.width,
            height: LABEL_HEIGHT + PADDING,
        }),
        page.0 + 1,
        theme.colors.text,
    );
}

fn paint_link_peek(
    painter: &mut SceneBuilder<'_>,
    theme: &Theme,
    canvas: RectF,
    layout: &PageLayoutIndex,
    previews: &PagePreviewCache,
    peek: LinkPeekView,
) {
    let Some(placement) = layout.placement(peek.target_page) else {
        return;
    };
    const POPUP_WIDTH: f64 = 240.0;
    const PADDING: f64 = 8.0;
    const LABEL_HEIGHT: f64 = 18.0;
    const GAP: f64 = 14.0;
    let image_width = POPUP_WIDTH - PADDING * 2.0;
    let image_height = (image_width * placement.bounds.height / placement.bounds.width.max(1.0))
        .clamp(80.0, 280.0);
    let popup_height = image_height + LABEL_HEIGHT + PADDING * 2.0;
    let popup_x = if peek.pointer.x + GAP + POPUP_WIDTH <= canvas.right() {
        peek.pointer.x + GAP
    } else {
        (peek.pointer.x - GAP - POPUP_WIDTH).max(canvas.x)
    };
    let popup_y = (peek.pointer.y - popup_height * 0.35)
        .clamp(canvas.y, (canvas.bottom() - popup_height).max(canvas.y));
    let popup = RectF {
        x: popup_x,
        y: popup_y,
        width: POPUP_WIDTH,
        height: popup_height,
    };
    painter.fill_rect(RectI::from(popup), theme.colors.chrome);
    painter.stroke_rect(RectI::from(popup), 1, theme.colors.page_border);

    let image = RectF {
        x: popup.x + PADDING,
        y: popup.y + PADDING,
        width: image_width,
        height: image_height,
    };
    painter.fill_rect(RectI::from(image), theme.colors.paper);
    painter.stroke_rect(RectI::from(image), 1, theme.colors.page_border);
    let scale = (image.width / placement.bounds.width.max(1.0))
        .min(image.height / placement.bounds.height.max(1.0));
    let fitted = RectF {
        x: image.x + (image.width - placement.bounds.width * scale) * 0.5,
        y: image.y + (image.height - placement.bounds.height * scale) * 0.5,
        width: placement.bounds.width * scale,
        height: placement.bounds.height * scale,
    };
    painter.push_clip(RectI::from(image));
    if let Some(preview) = previews.get_variant(peek.target_page, layout.render_variant) {
        painter.draw_tile(preview, fitted, ImageSampling::Linear);
    }
    if let Some(region) = peek.target_region {
        let target = RectF {
            x: fitted.x + (region.x - placement.view_box.x) * scale,
            y: fitted.y + (region.y - placement.view_box.y) * scale,
            width: (region.width * scale).max(3.0),
            height: (region.height * scale).max(3.0),
        };
        painter.blend_rect(RectI::from(target), 0x78_20_72_c8);
        painter.stroke_rect(RectI::from(target), 1, 0x00_20_72_c8);
    }
    painter.pop_clip();
    draw_page_number_marker(
        painter,
        RectI::from(RectF {
            x: popup.x,
            y: image.bottom(),
            width: popup.width,
            height: LABEL_HEIGHT + PADDING,
        }),
        peek.target_page.0 + 1,
        theme.colors.text,
    );
}

fn draw_page_number_marker(
    painter: &mut SceneBuilder<'_>,
    page_rect: RectI,
    page: u32,
    color: u32,
) {
    let digits = page.to_string().len() as u32;
    let width = digits * 4 + 4;
    let marker = RectI {
        x: page_rect.x + (page_rect.width.saturating_sub(width) / 2) as i32,
        y: page_rect.bottom() - 10,
        width,
        height: 3,
    };
    painter.fill_rect(marker, color);
}

#[cfg(all(test, feature = "softbuffer-presenter"))]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use crate::document::engine::{CancellationFlag, RasterPass};
    use crate::document::synthetic::SyntheticEngine;

    #[test]
    fn automatic_zoom_tracks_viewport_size_between_page_and_width_fit() {
        let page = SizeF {
            width: 600.0,
            height: 800.0,
        };
        let small_fit = fit_page_scale(
            SizeF {
                width: 600.0,
                height: 450.0,
            },
            page,
            12.0,
        );
        let large_fit = fit_page_scale(
            SizeF {
                width: 1_200.0,
                height: 900.0,
            },
            page,
            12.0,
        );
        assert!((large_fit - small_fit * 2.0).abs() < 0.000_001);

        let automatic = automatic_zoom_scale(large_fit, 1.9);
        assert!(automatic > large_fit);
        assert!(automatic < 1.9);
    }

    #[test]
    fn narrow_documents_are_centered_in_the_canvas() {
        let canvas = RectF {
            x: 260.0,
            y: 42.0,
            width: 1_000.0,
            height: 700.0,
        };
        assert_eq!(centered_document_origin_x(canvas, 600.0), 460.0);
        assert_eq!(centered_document_origin_x(canvas, 1_200.0), canvas.x);
    }

    #[test]
    fn external_links_only_allow_explicit_web_and_mail_schemes() {
        assert!(external_uri_is_allowed("https://example.com"));
        assert!(external_uri_is_allowed("HTTP://example.com"));
        assert!(external_uri_is_allowed("mailto:reader@example.com"));
        assert!(!external_uri_is_allowed("file:///C:/secret.txt"));
        assert!(!external_uri_is_allowed("javascript:alert(1)"));
        assert!(!external_uri_is_allowed("relative/path"));
    }

    #[test]
    fn toolbar_opens_the_options_popup_for_paper_colors() {
        assert_eq!(
            toolbar_action_at(OPTIONS_GROUP_X + OPTIONS_GROUP_WIDTH / 2.0),
            Some(ToolbarAction::ToggleOptions)
        );
    }

    #[test]
    fn raised_button_labels_are_centered_and_use_contrasting_text() {
        let bounds = RectI {
            x: 10,
            y: 5,
            width: 90,
            height: 32,
        };
        let label = centered_button_text(
            "Appearance",
            bounds,
            12.0,
            contrast_text_color(0x0068_6868),
            true,
        );
        assert!(label.x > bounds.x);
        assert!(label.x < bounds.right());
        assert!(label.y > bounds.y);
        assert_eq!(label.color, 0x00ff_ffff);
        assert_eq!(contrast_text_color(0x00e8_e8e8), 0x0010_1010);
    }

    #[test]
    fn raised_buttons_are_thirty_percent_shorter_and_hover_tracks_theme_brightness() {
        assert_eq!(button_bounds(0, 5, 90, 42).height, 22);
        assert!(
            color_luminance(hover_adjusted_color(0x0040_4040, true, true))
                > color_luminance(0x0040_4040)
        );
        assert!(
            color_luminance(hover_adjusted_color(0x00d0_d0d0, true, false))
                < color_luminance(0x00d0_d0d0)
        );
    }

    #[test]
    fn toolbar_opens_a_document_from_its_leading_button() {
        assert_eq!(
            toolbar_action_at(OPEN_GROUP_WIDTH / 2.0),
            Some(ToolbarAction::OpenDocument)
        );
        // The button must not swallow the zoom controls beside it.
        assert_eq!(
            toolbar_action_at(ZOOM_GROUP_X),
            Some(ToolbarAction::ZoomOut)
        );
    }

    fn compose_synthetic_frame() -> Vec<u32> {
        let engine = SyntheticEngine::new(2);
        let theme = Theme::light();
        let layout = PageLayoutIndex::build(&engine.descriptor().page_geometries, &theme.metrics);
        let app_layout = AppLayout::calculate(
            SizeF {
                width: 800.0,
                height: 600.0,
            },
            1.0,
            false,
            &theme.metrics,
        );
        let intent = ViewportPlanner::default().build(
            1,
            &layout,
            Vec2d::ZERO,
            Vec2d::ZERO,
            SizeF {
                width: app_layout.canvas.width,
                height: app_layout.canvas.height,
            },
            1.0,
            None,
        );
        let memory = MemoryArbiter::new(64 * 1024 * 1024);
        let tiles = TileCache::new(engine.descriptor().id, memory.clone());
        let previews = PagePreviewCache::new(engine.descriptor().page_count, memory.clone());
        let cancellation = CancellationFlag::default();
        let mut artifacts = HashMap::new();
        for demand in intent.visible_tiles.iter().copied() {
            let compiled = artifacts.entry(demand.page).or_insert_with(|| {
                let placement = layout.placement(demand.page).expect("synthetic placement");
                engine
                    .compile_page(demand.page, placement.page_to_doc, &cancellation)
                    .expect("compile synthetic frame page")
            });
            let tile = engine
                .raster_tile(
                    compiled,
                    intent.bucket,
                    demand,
                    RasterPass::Final,
                    intent.generation,
                    &cancellation,
                )
                .expect("raster synthetic frame tile");
            tiles.insert(Arc::new(tile), demand.distance_from_viewport);
        }

        let scrollbar = ScrollbarGeometry::calculate(
            app_layout.vertical_scrollbar,
            layout.total_height,
            app_layout.canvas.height,
            0.0,
        );
        let mut scene = FrameScene::new(800, 600);
        let tile_snapshot = tiles.frame_snapshot();
        let page_artifacts = HashMap::new();
        let selection = SelectionModel::default();
        {
            let mut painter = scene.begin(theme.colors.canvas);
            paint_scene(
                &mut painter,
                &theme,
                app_layout,
                &layout,
                intent.viewport_document,
                1.0,
                Vec2d::ZERO,
                &intent,
                &tile_snapshot,
                &previews,
                &mut Vec::new(),
                &mut HashSet::new(),
                &HashMap::new(),
                &page_artifacts,
                &[],
                None,
                &selection,
                scrollbar,
                None,
                None,
            );
        }
        let mut compositor = crate::present::softbuffer::SoftwareCompositor::new(800, 600);
        compositor.render(
            &scene,
            &[RectI {
                x: 0,
                y: 0,
                width: 800,
                height: 600,
            }],
            None,
        );
        compositor.buffer().pixels.clone()
    }

    #[test]
    fn synthetic_headless_composition_is_deterministic() {
        let first = compose_synthetic_frame();
        let second = compose_synthetic_frame();
        assert_eq!(first, second);
        assert!(
            first
                .iter()
                .any(|pixel| *pixel == Theme::light().colors.paper)
        );
        assert!(
            first
                .iter()
                .any(|pixel| *pixel == Theme::light().colors.canvas)
        );
        assert!(
            first
                .iter()
                .any(|pixel| *pixel == Theme::light().colors.muted_text)
        );
    }
}
