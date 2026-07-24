use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

use crate::chrome::{AppLayout, ScrollbarGeometry, ScrollbarState, StatusState};
use crate::damage::DamageRegion;
use crate::diagnostics::FrameMetrics;
use crate::document::engine::DocumentEngine;
use crate::document::layout::PageLayoutIndex;
use crate::document::session::{SessionUpdate, UpdateQueue};
use crate::document::synthetic::SyntheticEngine;
use crate::document::{
    ConductorHandle, MemoryArbiter, MemoryLease, PageIndex, PageStructure, TileCache, TileTier,
    ViewportIntent, ViewportPlanner,
};
use crate::event::ViewerEvent;
use crate::frame::FrameScheduler;
use crate::geometry::{PointF, RectF, RectI, SizeF, Vec2d};
use crate::input::{HitTarget, InputState, PointerCapture, ScrollbarDragState, ScrollbarPart};
use crate::paint::{Painter, WindowBuffer, scroll_blit};
use crate::present::Presenter;
use crate::scroll::{
    DocumentLocation, NavigationHistory, PagingDirection, ReadingAnchor, ScrollCommand, ScrollModel,
    paging_target,
};
use crate::text::{SearchIndex, SelectionModel, TextSubstrate};
use crate::theme::Theme;

#[derive(Debug)]
struct PageViewArtifacts {
    text: Arc<TextSubstrate>,
    structure: PageStructure,
    operation_count: usize,
    lowering_degraded: bool,
    _memory_lease: MemoryLease,
}

pub struct ViewerApp {
    engine: Arc<dyn DocumentEngine>,
    layout: Arc<PageLayoutIndex>,
    planner: ViewportPlanner,
    conductor: ConductorHandle,
    updates: Arc<UpdateQueue>,
    memory: MemoryArbiter,
    tiles: Arc<TileCache>,
    page_artifacts: HashMap<PageIndex, PageViewArtifacts>,
    page_errors: HashMap<PageIndex, String>,
    search: SearchIndex,
    selection: SelectionModel,
    history: NavigationHistory,

    window: Option<Arc<Window>>,
    presenter: Option<Box<dyn Presenter>>,
    buffer: WindowBuffer,
    damage: DamageRegion,
    frame: FrameScheduler,
    metrics: FrameMetrics,

    theme: Theme,
    app_layout: AppLayout,
    input: InputState,
    scrollbar: ScrollbarState,
    status: StatusState,
    scroll: ScrollModel,
    presented_scroll: Vec2d,
    zoom: f64,
    fit_width_mode: bool,
    scale_factor: f64,
    sidebar_visible: bool,
    fullscreen: bool,
    generation: u64,
    intent: ViewportIntent,
    intent_dirty: bool,
}

impl ViewerApp {
    pub fn new(engine: Arc<dyn DocumentEngine>, updates: Arc<UpdateQueue>) -> Self {
        let theme = Theme::light();
        let layout = Arc::new(PageLayoutIndex::build(
            &engine.descriptor().page_geometries,
            &theme.metrics,
        ));
        let memory = MemoryArbiter::new(1024 * 1024 * 1024);
        let tiles = Arc::new(TileCache::new(engine.descriptor().id, memory.clone()));
        let conductor = ConductorHandle::spawn(
            engine.clone(),
            layout.clone(),
            updates.clone(),
            memory.clone(),
            tiles.clone(),
        );
        let zero_layout = AppLayout::calculate(
            SizeF::default(),
            1.0,
            false,
            &theme.metrics,
        );
        Self {
            engine,
            layout,
            planner: ViewportPlanner::default(),
            conductor,
            updates,
            memory,
            tiles,
            page_artifacts: HashMap::new(),
            page_errors: HashMap::new(),
            search: SearchIndex::with_memory(memory.clone()),
            selection: SelectionModel::default(),
            history: NavigationHistory::default(),
            window: None,
            presenter: None,
            buffer: WindowBuffer::new(1, 1),
            damage: DamageRegion::new(1, 1),
            frame: FrameScheduler::new(),
            metrics: FrameMetrics::default(),
            theme,
            app_layout: zero_layout,
            input: InputState::default(),
            scrollbar: ScrollbarState::default(),
            status: StatusState::default(),
            scroll: ScrollModel::new(),
            presented_scroll: Vec2d::ZERO,
            zoom: 1.0,
            fit_width_mode: true,
            scale_factor: 1.0,
            sidebar_visible: false,
            fullscreen: false,
            generation: 1,
            intent: ViewportIntent::empty(),
            intent_dirty: true,
        }
    }

    pub fn synthetic(updates: Arc<UpdateQueue>) -> Self {
        Self::new(Arc::new(SyntheticEngine::new(10_000)), updates)
    }

    fn create_window(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title(format!("Lege Viewer — {}", self.engine.descriptor().display_name))
            .with_inner_size(PhysicalSize::new(1280, 900));
        let window = Arc::new(
            event_loop
                .create_window(attributes)
                .expect("create Lege viewer window"),
        );
        self.scale_factor = window.scale_factor();
        let size = window.inner_size();
        self.resize(size.width, size.height, true);

        #[cfg(feature = "softbuffer-presenter")]
        {
            let presenter = crate::present::softbuffer::SoftbufferPresenter::new(window.clone())
                .expect("create softbuffer presenter");
            self.presenter = Some(Box::new(presenter));
        }
        self.window = Some(window);
        self.request_redraw();
    }

    fn resize(&mut self, width: u32, height: u32, initial: bool) {
        let anchor = self.reading_anchor();
        self.buffer.resize(width.max(1), height.max(1));
        self.damage.resize(width.max(1), height.max(1));
        self.app_layout = AppLayout::calculate(
            SizeF {
                width: f64::from(width),
                height: f64::from(height),
            },
            self.scale_factor,
            self.sidebar_visible,
            &self.theme.metrics,
        );
        if self.fit_width_mode || initial {
            self.zoom = self.fit_width_zoom();
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
            (self.app_layout.canvas.height / placement.bounds.height)
                .min(self.app_layout.canvas.width / placement.bounds.width)
                .clamp(0.05, 12.0)
        })
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

    fn set_zoom(&mut self, zoom: f64, fit_width: bool) {
        let anchor = self.reading_anchor();
        self.zoom = zoom.clamp(0.05, 12.0);
        self.fit_width_mode = fit_width;
        self.update_scroll_extents();
        self.restore_anchor(anchor);
        self.bump_generation();
    }

    fn bump_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.intent_dirty = true;
        self.damage.mark_full();
        self.frame.interactive();
        self.request_redraw();
    }

    fn request_redraw(&mut self) {
        if self.frame.request_redraw() {
            self.metrics.redraw_requested = Some(Instant::now());
            if let Some(window) = &self.window {
                window.request_redraw();
            }
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
        self.intent = self.planner.build(
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
        );
        self.conductor.publish_intent(self.intent.clone());
        self.intent_dirty = false;
    }

    fn page_at_document_fraction(&self, fraction: f64) -> Option<PageIndex> {
        self.layout
            .page_at_y(self.layout.total_height * fraction.clamp(0.0, 1.0))
            .map(|placement| placement.page)
    }

    fn drain_updates(&mut self) {
        let mut any_visible_change = false;
        for update in self.updates.drain() {
            match update {
                SessionUpdate::PageCompiled(update) => {
                    self.search.insert(update.page, Arc::clone(&update.text));
                    self.page_artifacts.insert(
                        update.page,
                        PageViewArtifacts {
                            text: update.text,
                            structure: update.structure,
                            operation_count: update.operation_count,
                            lowering_degraded: update.lowering_degraded,
                            _memory_lease: update.memory_lease,
                        },
                    );
                    any_visible_change = true;
                }
                SessionUpdate::TileReady { key: _, generation: _ } => {
                    any_visible_change = true;
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
        self.evict_page_artifacts();
        if any_visible_change {
            self.damage.mark_full();
            self.request_redraw();
        }
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
        self.generation = self.generation.wrapping_add(1);
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
            let exposed = scroll_blit(&mut self.buffer, canvas, delta_x, delta_y);
            for rect in exposed.rects {
                self.damage.add(rect);
            }
        } else if !can_blit {
            self.damage.mark_full();
        }
        self.damage.add(RectI::from(self.app_layout.vertical_scrollbar));
        self.damage.add(RectI::from(self.app_layout.status));
        self.presented_scroll = snapped;
        self.request_redraw();
    }

    fn page_step(&mut self, direction: PagingDirection) {
        let viewport = self.viewport_document();
        let visible_range = self.layout.visible_pages(viewport);
        let lines = self.layout.placements()[visible_range]
            .iter()
            .filter_map(|placement| self.page_artifacts.get(&placement.page))
            .flat_map(|artifacts| artifacts.text.lines.lines.iter().cloned())
            .collect::<Vec<_>>();
        let target = paging_target(direction, viewport, lines, self.layout.total_height);
        self.scroll.apply(ScrollCommand::SetAbsolute(Vec2d {
            x: self.scroll.position.x,
            y: target * self.zoom,
        }));
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
            self.input.hover = HitTarget::VerticalScrollbar(if geometry
                .thumb
                .contains(self.input.pointer_position)
            {
                ScrollbarPart::Thumb
            } else if y < geometry.thumb.y {
                ScrollbarPart::DecrementTrack
            } else {
                ScrollbarPart::IncrementTrack
            });
            self.scrollbar
                .enter_or_move(geometry.document_fraction_at(y), Instant::now());
            self.intent_dirty = true;
        } else if self.app_layout.canvas.contains(self.input.pointer_position) {
            self.input.hover = HitTarget::Canvas;
            self.scrollbar.leave();
            self.intent_dirty = true;
        } else {
            self.input.hover = HitTarget::None;
            self.scrollbar.leave();
            self.intent_dirty = true;
        }
        self.damage.mark_full();
        self.request_redraw();
    }

    fn handle_mouse_input(&mut self, state: ElementState, button: MouseButton) {
        if button != MouseButton::Left {
            return;
        }
        self.input.left_button_down = state == ElementState::Pressed;
        match state {
            ElementState::Pressed => {
                let geometry = self.scrollbar_geometry();
                if geometry.thumb.contains(self.input.pointer_position) {
                    self.input.capture = Some(PointerCapture::VerticalThumb(
                        ScrollbarDragState {
                            pointer_offset_in_thumb: self.input.pointer_position.y
                                - geometry.thumb.y,
                        },
                    ));
                    self.scrollbar.begin_drag();
                } else if geometry.track.contains(self.input.pointer_position) {
                    if self.input.modifiers.shift_key() {
                        let fraction = geometry
                            .document_fraction_at(self.input.pointer_position.y);
                        self.scroll.apply(ScrollCommand::SetAbsolute(Vec2d {
                            x: self.scroll.position.x,
                            y: fraction
                                * (self.layout.total_height * self.zoom
                                    - self.app_layout.canvas.height)
                                    .max(0.0),
                        }));
                        self.bump_generation();
                    } else if self.input.pointer_position.y < geometry.thumb.y {
                        self.page_step(PagingDirection::Up);
                    } else {
                        self.page_step(PagingDirection::Down);
                    }
                }
            }
            ElementState::Released => {
                self.input.capture = None;
                self.scrollbar.end_drag();
                self.scroll.settle();
                self.intent_dirty = true;
            }
        }
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
        let target_y = location
            .target_region
            .map_or(placement.bounds.y, |region| placement.bounds.y + region.y);
        self.scroll.apply(ScrollCommand::SetAbsolute(Vec2d {
            x: self.scroll.position.x,
            y: target_y * self.zoom,
        }));
        self.bump_generation();
    }

    fn handle_key(&mut self, key: &Key, state: ElementState) {
        if state != ElementState::Pressed {
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
                    self.navigate_to(location, false);
                }
            }
            Key::Named(NamedKey::ArrowRight) if self.input.modifiers.alt_key() => {
                if let Some(location) = self.history.forward() {
                    self.navigate_to(location, false);
                }
            }
            Key::Named(NamedKey::ArrowDown) => self.fine_step(1.0),
            Key::Named(NamedKey::ArrowUp) => self.fine_step(-1.0),
            Key::Named(NamedKey::Home) => {
                self.scroll.apply(ScrollCommand::SetAbsolute(Vec2d::ZERO));
                self.bump_generation();
            }
            Key::Named(NamedKey::End) => {
                self.scroll.apply(ScrollCommand::SetAbsolute(Vec2d {
                    x: self.scroll.position.x,
                    y: self.scroll.max_position().y,
                }));
                self.bump_generation();
            }
            Key::Named(NamedKey::F11) => self.toggle_fullscreen(),
            Key::Character(character) if character.as_str() == "+" || character.as_str() == "=" => {
                self.set_zoom(self.zoom * std::f64::consts::SQRT_2, false);
            }
            Key::Character(character) if character.as_str() == "-" => {
                self.set_zoom(self.zoom / std::f64::consts::SQRT_2, false);
            }
            Key::Character(character) if character.as_str().eq_ignore_ascii_case("w") => {
                self.set_zoom(self.fit_width_zoom(), true);
            }
            Key::Character(character) if character.as_str().eq_ignore_ascii_case("p") => {
                self.set_zoom(self.fit_page_zoom(), false);
            }
            Key::Character(character) if character.as_str().eq_ignore_ascii_case("b") => {
                let anchor = self.reading_anchor();
                self.sidebar_visible = !self.sidebar_visible;
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
                if self.fit_width_mode {
                    self.zoom = self.fit_width_zoom();
                }
                self.update_scroll_extents();
                self.restore_anchor(anchor);
                self.bump_generation();
            }
            _ => {}
        }
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

    fn compose(&mut self) {
        self.metrics.begin_frame();
        self.publish_intent_if_needed();
        let damage_rects = self.damage.rects().to_vec();
        if damage_rects.is_empty() {
            return;
        }
        let viewport_document = self.viewport_document();
        let scrollbar = self.scrollbar_geometry();
        let memory_bytes = self.memory.total_bytes();
        let hover_preview = self
            .scrollbar
            .preview_visible()
            .then(|| {
                let fraction = self.scrollbar.hover_document_fraction?;
                let page = self.page_at_document_fraction(fraction)?;
                Some((page, fraction))
            })
            .flatten();
        let mut painter = Painter::new(&mut self.buffer);
        for damage in &damage_rects {
            painter.push_clip(*damage);
            paint_scene(
                &mut painter,
                &self.theme,
                self.app_layout,
                &self.layout,
                viewport_document,
                self.zoom,
                self.scroll.position,
                &self.intent,
                &self.tiles,
                &self.page_errors,
                scrollbar,
                hover_preview,
                &self.status,
                memory_bytes,
            );
            painter.pop_clip();
        }
        self.metrics.damaged_pixels = damage_rects
            .iter()
            .map(|rect| u64::from(rect.width) * u64::from(rect.height))
            .sum();
        self.metrics.finish_compose();

        if let Some(presenter) = self.presenter.as_mut() {
            let _ = presenter.present(&self.buffer, &damage_rects);
        }
        self.metrics.finish_present();
        self.presented_scroll = Vec2d {
            x: self.scroll.position.x.round(),
            y: self.scroll.position.y.round(),
        };
        self.damage.clear();
    }

    fn refresh_status(&mut self) {
        let viewport = self.viewport_document();
        self.status.update(&self.layout, viewport, self.zoom);
    }
}

impl ApplicationHandler<ViewerEvent> for ViewerApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.create_window(event_loop);
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: ViewerEvent) {
        match event {
            ViewerEvent::Wake => self.drain_updates(),
            ViewerEvent::FatalBackgroundError(message) => {
                eprintln!("viewer background failure: {message}");
            }
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
            WindowEvent::Resized(size) => self.resize(size.width, size.height, false),
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
                self.compose();
                self.frame.settle_if_quiet();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                self.metrics.input_received = Some(Instant::now());
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
                self.handle_key(&event.logical_key, event.state);
            }
            WindowEvent::Focused(false) => {
                self.input.capture = None;
                self.scrollbar.end_drag();
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
        if let Some(deadline) = self.scrollbar.preview_deadline() {
            event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_scene(
    painter: &mut Painter<'_>,
    theme: &Theme,
    app_layout: AppLayout,
    layout: &PageLayoutIndex,
    viewport_document: RectF,
    zoom: f64,
    scroll: Vec2d,
    intent: &ViewportIntent,
    tiles: &TileCache,
    page_errors: &HashMap<PageIndex, String>,
    scrollbar: ScrollbarGeometry,
    hover_preview: Option<(PageIndex, f64)>,
    status: &StatusState,
    memory_bytes: u64,
) {
    painter.fill_rect(RectI::from(app_layout.toolbar), theme.colors.chrome);
    painter.fill_rect(RectI::from(app_layout.sidebar), theme.colors.chrome);
    painter.fill_rect(RectI::from(app_layout.status), theme.colors.chrome);
    painter.fill_rect(RectI::from(app_layout.canvas), theme.colors.canvas);

    // Concrete toolbar controls, deliberately not a generic widget tree.
    let mut x = 10;
    for _ in 0..8 {
        painter.fill_rect(
            RectI {
                x,
                y: 8,
                width: 28,
                height: 26,
            },
            0x00d8_d8d8,
        );
        x += 34;
    }

    painter.push_clip(RectI::from(app_layout.canvas));
    for placement in &layout.placements()[layout.visible_pages(viewport_document)] {
        let page_screen = RectF {
            x: app_layout.canvas.x + placement.bounds.x * zoom - scroll.x,
            y: app_layout.canvas.y + placement.bounds.y * zoom - scroll.y,
            width: placement.bounds.width * zoom,
            height: placement.bounds.height * zoom,
        };
        let page_rect = RectI::from(page_screen);
        painter.fill_rect(page_rect, theme.colors.paper);
        painter.stroke_rect(page_rect, 1, theme.colors.page_border);

        painter.push_clip(page_rect);
        let mut painted_tiles = HashSet::new();
        for demand in intent.visible_tiles.iter().filter(|demand| demand.page == placement.page) {
            for tile in tiles.best_covering(*demand, intent.bucket) {
                if !painted_tiles.insert(tile.key) {
                    continue;
                }
                let destination = RectF {
                    x: app_layout.canvas.x + tile.page_document_rect.x * zoom - scroll.x,
                    y: app_layout.canvas.y + tile.page_document_rect.y * zoom - scroll.y,
                    width: tile.page_document_rect.width * zoom,
                    height: tile.page_document_rect.height * zoom,
                };
                painter.blit_scaled(&tile.pixels, RectI::from(destination));
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
        draw_page_number_marker(painter, page_rect, placement.page.0 + 1, theme.colors.muted_text);
    }
    painter.pop_clip();

    paint_scrollbar_document_map(painter, theme, layout, scrollbar);
    if let Some((page, fraction)) = hover_preview {
        paint_scrollbar_preview(painter, theme, layout, tiles, scrollbar, page, fraction);
    }

    // Status and diagnostics use geometric markers until the UI glyph cache is
    // connected to pdf-font. Their data model is already final.
    let page_fraction = status
        .current_page
        .map_or(0.0, |page| f64::from(page.0 + 1) / f64::from(status.page_count.max(1)));
    painter.fill_rect(
        RectI {
            x: 8,
            y: app_layout.status.y as i32 + 7,
            width: (180.0 * page_fraction) as u32,
            height: 4,
        },
        theme.colors.text,
    );
    let memory_fraction = (memory_bytes as f64 / (1024.0 * 1024.0 * 1024.0)).clamp(0.0, 1.0);
    painter.fill_rect(
        RectI {
            x: app_layout.status.right() as i32 - 110,
            y: app_layout.status.y as i32 + 7,
            width: (100.0 * memory_fraction) as u32,
            height: 4,
        },
        theme.colors.muted_text,
    );
}

fn paint_scrollbar_document_map(
    painter: &mut Painter<'_>,
    theme: &Theme,
    layout: &PageLayoutIndex,
    scrollbar: ScrollbarGeometry,
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
    painter.fill_rect(RectI::from(scrollbar.thumb), theme.colors.scrollbar_thumb);
}

fn paint_scrollbar_preview(
    painter: &mut Painter<'_>,
    theme: &Theme,
    layout: &PageLayoutIndex,
    tiles: &TileCache,
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
        .min(MAX_IMAGE_HEIGHT)
        .max(32.0);
    let popup_height = image_height + LABEL_HEIGHT + PADDING * 2.0;
    let pointer_y = scrollbar.track.y
        + document_fraction.clamp(0.0, 1.0) * scrollbar.track.height;
    let popup_y = (pointer_y - popup_height * 0.5)
        .clamp(scrollbar.track.y, (scrollbar.track.bottom() - popup_height).max(scrollbar.track.y));
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
    for tile in tiles.page_tiles_at_tier(page, TileTier::Thumbnail) {
        let destination = RectF {
            x: page_origin_x + (tile.page_document_rect.x - placement.bounds.x) * scale,
            y: page_origin_y + (tile.page_document_rect.y - placement.bounds.y) * scale,
            width: tile.page_document_rect.width * scale,
            height: tile.page_document_rect.height * scale,
        };
        painter.blit_scaled(&tile.pixels, RectI::from(destination));
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

fn draw_page_number_marker(painter: &mut Painter<'_>, page_rect: RectI, page: u32, color: u32) {
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
