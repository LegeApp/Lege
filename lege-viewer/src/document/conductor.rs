use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, select, unbounded};

use crate::geometry::RectF;

use super::PageIndex;
use super::cache::{CacheCategory, MemoryArbiter, MemoryLease, TileCache};
use super::engine::{
    CancellationFlag, CompiledArtifacts, DocumentEngine, DocumentEngineError, RasterPass,
};
use super::layout::PageLayoutIndex;
use super::preview::PagePreviewCache;
use super::session::{PageArtifactUpdate, SessionUpdate, UpdateQueue};
use super::tile::{TileDemand, TileKey, TileTier};
use super::viewport::{NavigationMode, ScrollDirection, ViewportIntent};

#[derive(Debug)]
pub enum ConductorCommand {
    IntentChanged,
    LayoutChanged(Arc<PageLayoutIndex>),
    LinkPeek { page: PageIndex, region: RectF },
    Warm(WarmHint),
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarmReason {
    Sequential,
    OutlineHover,
    OutlineTarget,
    SearchActive,
    SearchAdjacent,
    LinkHover,
    History,
    ScrollbarPrediction,
    PageNumberInput,
    BackgroundSweep,
}

#[derive(Debug, Clone)]
pub struct WarmHint {
    pub page: PageIndex,
    pub region: Option<RectF>,
    pub reason: WarmReason,
    pub probability: f32,
    pub expires_at: Instant,
}

impl WarmHint {
    pub fn for_duration(
        page: PageIndex,
        reason: WarmReason,
        probability: f32,
        duration: Duration,
    ) -> Self {
        let probability = if probability.is_finite() {
            probability.clamp(0.0, 1.0)
        } else {
            0.0
        };
        Self {
            page,
            region: None,
            reason,
            probability,
            expires_at: Instant::now() + duration,
        }
    }
}

fn warm_priority(hint: &WarmHint) -> i32 {
    let base = match hint.reason {
        WarmReason::OutlineTarget | WarmReason::SearchActive | WarmReason::History => 1_100,
        WarmReason::OutlineHover | WarmReason::LinkHover | WarmReason::SearchAdjacent => 975,
        WarmReason::ScrollbarPrediction | WarmReason::PageNumberInput => 900,
        WarmReason::Sequential => 800,
        WarmReason::BackgroundSweep => 20,
    };
    base + (hint.probability * 100.0).round() as i32
}

#[derive(Debug)]
pub struct ConductorHandle {
    commands: Sender<ConductorCommand>,
    intent: Arc<ArcSwap<ViewportIntent>>,
    previews: Arc<PagePreviewCache>,
    thread: Option<JoinHandle<()>>,
}

impl ConductorHandle {
    pub fn spawn(
        engine: Arc<dyn DocumentEngine>,
        layout: Arc<PageLayoutIndex>,
        updates: Arc<UpdateQueue>,
        memory: MemoryArbiter,
        tiles: Arc<TileCache>,
    ) -> std::io::Result<Self> {
        let (commands, command_rx) = unbounded();
        let intent = Arc::new(ArcSwap::from_pointee(ViewportIntent::empty()));
        let previews = Arc::new(PagePreviewCache::new(
            engine.descriptor().page_count,
            memory.clone(),
        ));
        let thread_intent = intent.clone();
        let thread_previews = previews.clone();
        let thread = std::thread::Builder::new()
            .name("lege-viewer-conductor".to_owned())
            .spawn(move || {
                Conductor::new(
                    engine,
                    layout,
                    updates,
                    memory,
                    tiles,
                    thread_previews,
                    thread_intent,
                    command_rx,
                )
                .run();
            })?;
        Ok(Self {
            commands,
            intent,
            previews,
            thread: Some(thread),
        })
    }

    pub fn publish_intent(&self, intent: ViewportIntent) {
        self.intent.store(Arc::new(intent));
        let _ = self.commands.send(ConductorCommand::IntentChanged);
    }

    pub fn publish_layout(&self, layout: Arc<PageLayoutIndex>) {
        let _ = self.commands.send(ConductorCommand::LayoutChanged(layout));
    }

    pub fn request_link_peek(&self, page: PageIndex, region: RectF) {
        let _ = self
            .commands
            .send(ConductorCommand::LinkPeek { page, region });
    }

    pub fn warm(&self, hint: WarmHint) {
        let _ = self.commands.send(ConductorCommand::Warm(hint));
    }

    pub fn previews(&self) -> Arc<PagePreviewCache> {
        self.previews.clone()
    }
}

impl Drop for ConductorHandle {
    fn drop(&mut self) {
        let _ = self.commands.send(ConductorCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum WorkKey {
    Compile(PageIndex),
    Preview(PageIndex),
    Raster(TileKey),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkClass {
    InteractiveVisible,
    VisibleFallback,
    Predictive,
    Background,
}

#[derive(Debug)]
struct InFlightWork {
    id: u64,
    cancellation: CancellationFlag,
    class: WorkClass,
}

#[derive(Debug, Clone, Copy, Default)]
struct CompileNeeds {
    interactive_generation: Option<u64>,
    text_index: bool,
    preview: bool,
}

impl CompileNeeds {
    fn merge(&mut self, other: Self) {
        if other.interactive_generation.is_some() {
            self.interactive_generation = other.interactive_generation;
        }
        self.text_index |= other.text_index;
        self.preview |= other.preview;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompilePhase {
    Pending,
    InFlight,
}

#[derive(Debug)]
struct CompileState {
    needs: CompileNeeds,
    priority: i32,
    revision: u64,
    phase: CompilePhase,
    cancellation: CancellationFlag,
}

#[derive(Debug)]
struct CompileTicket {
    page: PageIndex,
    revision: u64,
}

#[derive(Debug)]
struct CompileJob {
    id: u64,
    page: PageIndex,
    page_to_doc: crate::geometry::Affine,
    cancellation: CancellationFlag,
}

#[derive(Debug)]
enum RasterJob {
    Tile {
        id: u64,
        key: WorkKey,
        artifacts: Arc<CompiledArtifacts>,
        demand: TileDemand,
        bucket: super::ZoomBucket,
        pass: RasterPass,
        class: WorkClass,
        generation: u64,
        cancellation: CancellationFlag,
    },
    PagePreview {
        id: u64,
        key: WorkKey,
        artifacts: Arc<CompiledArtifacts>,
        demand: TileDemand,
        bucket: super::ZoomBucket,
        class: WorkClass,
        cancellation: CancellationFlag,
    },
}

#[derive(Debug)]
enum WorkerResult {
    Compiled {
        id: u64,
        page: PageIndex,
        result: Result<Arc<CompiledArtifacts>, DocumentEngineError>,
    },
    Rastered {
        id: u64,
        key: WorkKey,
        demand: TileDemand,
        result: Result<super::TileSurface, DocumentEngineError>,
    },
    Previewed {
        id: u64,
        key: WorkKey,
        page: PageIndex,
        result: Result<super::TileSurface, DocumentEngineError>,
    },
}

#[derive(Debug)]
struct Prioritized<T> {
    priority: i32,
    sequence: u64,
    value: T,
}

impl<T> PartialEq for Prioritized<T> {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.sequence == other.sequence
    }
}

impl<T> Eq for Prioritized<T> {}

impl<T> PartialOrd for Prioritized<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for Prioritized<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority
            .cmp(&other.priority)
            .then_with(|| other.sequence.cmp(&self.sequence))
    }
}

#[derive(Debug)]
struct CompiledEntry {
    artifacts: Arc<CompiledArtifacts>,
    _compiled_lease: MemoryLease,
    _text_lease: MemoryLease,
}

struct Conductor {
    engine: Arc<dyn DocumentEngine>,
    layout: Arc<PageLayoutIndex>,
    updates: Arc<UpdateQueue>,
    memory: MemoryArbiter,
    tiles: Arc<TileCache>,
    previews: Arc<PagePreviewCache>,
    intent: Arc<ArcSwap<ViewportIntent>>,
    command_rx: Receiver<ConductorCommand>,
    compile_tx: Sender<CompileJob>,
    raster_tx: Sender<RasterJob>,
    result_rx: Receiver<WorkerResult>,
    compile_pending: BinaryHeap<Prioritized<CompileTicket>>,
    raster_pending: BinaryHeap<Prioritized<RasterJob>>,
    sequence: AtomicU64,
    compiled: HashMap<PageIndex, CompiledEntry>,
    compile_states: HashMap<PageIndex, CompileState>,
    published_pages: HashSet<PageIndex>,
    queued: HashSet<WorkKey>,
    in_flight: HashMap<WorkKey, InFlightWork>,
    quarantined_pages: HashSet<PageIndex>,
    /// How many times each page has failed for a reason other than
    /// cancellation.
    page_failures: HashMap<PageIndex, u32>,
    indexed_pages: HashSet<PageIndex>,
    index_cursor: u32,
    compile_workers: usize,
    warm_hints: HashMap<PageIndex, WarmHint>,
}

impl Conductor {
    #[allow(clippy::too_many_arguments)]
    fn new(
        engine: Arc<dyn DocumentEngine>,
        layout: Arc<PageLayoutIndex>,
        updates: Arc<UpdateQueue>,
        memory: MemoryArbiter,
        tiles: Arc<TileCache>,
        previews: Arc<PagePreviewCache>,
        intent: Arc<ArcSwap<ViewportIntent>>,
        command_rx: Receiver<ConductorCommand>,
    ) -> Self {
        let descriptor = engine.descriptor();
        let workers = std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get);
        let compile_workers = (workers / 3).max(1);
        let raster_workers = workers.saturating_sub(compile_workers).max(1);
        // Keep hand-off queues deliberately shallow. Priority lives in the
        // conductor, where pending work can still be promoted or cancelled.
        let (compile_tx, compile_rx) = bounded(1);
        let (raster_tx, raster_rx) = bounded(1);
        let (result_tx, result_rx) = unbounded();

        for index in 0..compile_workers {
            spawn_compile_worker(
                index,
                engine.clone(),
                intent.clone(),
                compile_rx.clone(),
                result_tx.clone(),
            );
        }
        for index in 0..raster_workers {
            spawn_raster_worker(
                index,
                engine.clone(),
                intent.clone(),
                raster_rx.clone(),
                result_tx.clone(),
            );
        }
        drop(result_tx);
        debug_assert_eq!(descriptor.page_count as usize, layout.placements().len());

        Self {
            engine,
            layout,
            updates,
            memory,
            tiles,
            previews,
            intent,
            command_rx,
            compile_tx,
            raster_tx,
            result_rx,
            compile_pending: BinaryHeap::new(),
            raster_pending: BinaryHeap::new(),
            sequence: AtomicU64::new(1),
            compiled: HashMap::new(),
            compile_states: HashMap::new(),
            published_pages: HashSet::new(),
            queued: HashSet::new(),
            in_flight: HashMap::new(),
            quarantined_pages: HashSet::new(),
            page_failures: HashMap::new(),
            indexed_pages: HashSet::new(),
            index_cursor: 0,
            compile_workers,
            warm_hints: HashMap::new(),
        }
    }

    fn run(mut self) {
        loop {
            select! {
                recv(self.command_rx) -> command => match command {
                    Ok(ConductorCommand::IntentChanged) => self.replan(),
                    Ok(ConductorCommand::LayoutChanged(layout)) => {
                        self.layout = layout;
                        self.cancel_all();
                        self.compiled.clear();
                        self.published_pages.clear();
                        self.warm_hints.clear();
                        self.replan();
                    }
                    Ok(ConductorCommand::LinkPeek { page, region }) => {
                        let mut hint = WarmHint::for_duration(
                            page,
                            WarmReason::LinkHover,
                            0.9,
                            Duration::from_millis(900),
                        );
                        hint.region = Some(region);
                        self.register_warm_hint(hint);
                        self.replan();
                    }
                    Ok(ConductorCommand::Warm(hint)) => {
                        self.register_warm_hint(hint);
                        self.replan();
                    }
                    Ok(ConductorCommand::Shutdown) | Err(_) => break,
                },
                recv(self.result_rx) -> result => match result {
                    Ok(result) => self.handle_result(result),
                    Err(_) => break,
                },
            }
        }
        self.cancel_all();
    }

    fn replan(&mut self) {
        let intent = self.intent.load_full();
        let latency_critical = latency_critical_navigation(intent.navigation_mode);
        let now = Instant::now();
        self.warm_hints.retain(|_, hint| hint.expires_at > now);
        self.cancel_irrelevant(&intent);
        self.tiles.refresh_distances(|surface| {
            if intent.tile_is_visible(surface.key.page, surface.key.coord) {
                0.0
            } else if intent.tile_is_final_prefetch(surface.key.page, surface.key.coord) {
                0.25
            } else {
                vertical_distance(surface.page_document_rect, intent.viewport_document)
            }
        });

        let visible_pages = intent
            .visible_tiles
            .iter()
            .map(|demand| demand.page)
            .collect::<HashSet<_>>();
        if latency_critical {
            for page in &visible_pages {
                if let Some(state) = self.compile_states.get_mut(page) {
                    state.needs.interactive_generation = Some(intent.generation);
                    state.needs.text_index = false;
                    state.needs.preview = false;
                }
            }
        }
        for (order, page) in intent.compile_pages.iter().copied().enumerate() {
            let visible = visible_pages.contains(&page);
            if latency_critical && !visible {
                continue;
            }
            let priority = 1_250 - order.min(500) as i32;
            self.request_compile(
                page,
                CompileNeeds {
                    interactive_generation: visible.then_some(intent.generation),
                    text_index: !latency_critical,
                    // Even a random seek should publish a small canonical page
                    // immediately while the exact tiles finish. This is the
                    // Sumatra-style "never show an empty loading page" path.
                    preview: visible || !latency_critical,
                },
                if visible { 1_400 } else { priority },
            );
        }

        for demand in intent.visible_tiles.iter().copied() {
            self.schedule_visible_tile(&intent, demand, latency_critical);
        }
        if !latency_critical {
            for demand in intent.overscan_tiles.iter().copied() {
                self.schedule_structural_tile(&intent, demand, 850);
            }
            for demand in intent.final_prefetch_tiles.iter().copied() {
                self.schedule_raster(&intent, demand, RasterPass::Final, 900);
            }
        }
        if !latency_critical {
            for (order, page) in intent.preview_pages.iter().copied().enumerate() {
                self.request_compile(
                    page,
                    CompileNeeds {
                        preview: true,
                        text_index: true,
                        ..CompileNeeds::default()
                    },
                    650 - order.min(25) as i32,
                );
            }
            for (order, page) in intent.thumbnail_pages.iter().copied().enumerate() {
                self.request_compile(
                    page,
                    CompileNeeds {
                        preview: true,
                        text_index: true,
                        ..CompileNeeds::default()
                    },
                    975 - order.min(25) as i32,
                );
            }
            let mut warm_hints = self.warm_hints.values().cloned().collect::<Vec<_>>();
            warm_hints.sort_by_key(|hint| std::cmp::Reverse(warm_priority(hint)));
            for hint in warm_hints {
                self.request_compile(
                    hint.page,
                    CompileNeeds {
                        text_index: true,
                        preview: true,
                        ..CompileNeeds::default()
                    },
                    warm_priority(&hint),
                );
            }
        }
        self.schedule_text_index(&intent);
        self.dispatch();
        self.publish_depths();
    }

    fn register_warm_hint(&mut self, hint: WarmHint) {
        if hint.expires_at <= Instant::now() || self.layout.placement(hint.page).is_none() {
            return;
        }
        self.warm_hints
            .entry(hint.page)
            .and_modify(|current| {
                if warm_priority(&hint) >= warm_priority(current) {
                    current.reason = hint.reason;
                }
                current.probability = current.probability.max(hint.probability);
                current.expires_at = current.expires_at.max(hint.expires_at);
                if hint.region.is_some() {
                    current.region = hint.region;
                }
            })
            .or_insert(hint);
    }

    fn page_is_warm(&self, page: PageIndex) -> bool {
        self.warm_hints
            .get(&page)
            .is_some_and(|hint| hint.expires_at > Instant::now())
    }

    fn schedule_visible_tile(
        &mut self,
        intent: &ViewportIntent,
        demand: TileDemand,
        exact_only: bool,
    ) {
        let Some(artifacts) = self
            .compiled
            .get(&demand.page)
            .map(|entry| entry.artifacts.clone())
        else {
            self.request_compile(
                demand.page,
                CompileNeeds {
                    interactive_generation: Some(intent.generation),
                    text_index: !exact_only,
                    preview: true,
                },
                1_400,
            );
            return;
        };
        // Exact work owns the foreground lane. Text-first is useful only when
        // interaction is not latency-critical and always ranks below Final.
        self.schedule_raster(intent, demand, RasterPass::Final, 1_500);
        if !exact_only && self.engine.supports_text_first(&artifacts) {
            self.schedule_raster(intent, demand, RasterPass::TextFirst, 1_350);
        }
    }

    fn schedule_structural_tile(
        &mut self,
        intent: &ViewportIntent,
        demand: TileDemand,
        priority: i32,
    ) {
        let Some(artifacts) = self
            .compiled
            .get(&demand.page)
            .map(|entry| entry.artifacts.clone())
        else {
            return;
        };
        if self.engine.supports_text_first(&artifacts) {
            self.schedule_raster(intent, demand, RasterPass::TextFirst, priority);
        }
    }

    fn request_compile(&mut self, page: PageIndex, needs: CompileNeeds, priority: i32) {
        if self.quarantined_pages.contains(&page) {
            return;
        }
        if self.compiled.contains_key(&page) {
            self.fulfill_resident(page, needs);
            return;
        }
        if self.layout.placement(page).is_none() {
            return;
        }

        let mut push_ticket = None;
        match self.compile_states.get_mut(&page) {
            Some(state) => {
                state.needs.merge(needs);
                if state.phase == CompilePhase::Pending && priority > state.priority {
                    state.priority = priority;
                    state.revision = state.revision.wrapping_add(1);
                    push_ticket = Some((state.priority, state.revision));
                }
            }
            None => {
                let revision = 1;
                self.compile_states.insert(
                    page,
                    CompileState {
                        needs,
                        priority,
                        revision,
                        phase: CompilePhase::Pending,
                        cancellation: CancellationFlag::default(),
                    },
                );
                push_ticket = Some((priority, revision));
            }
        }
        if let Some((priority, revision)) = push_ticket {
            let sequence = self.next_sequence();
            self.compile_pending.push(Prioritized {
                priority,
                sequence,
                value: CompileTicket { page, revision },
            });
        }
    }

    fn schedule_text_index(&mut self, intent: &ViewportIntent) {
        if intent.navigation_mode != NavigationMode::Idle {
            return;
        }
        let max_background_jobs = self.compile_workers.saturating_sub(1).max(1);
        let outstanding = self
            .compile_states
            .keys()
            .filter(|page| !intent.page_is_relevant(**page) && !self.page_is_warm(**page))
            .count();
        let preview_backlog = self
            .queued
            .iter()
            .chain(self.in_flight.keys())
            .filter(|key| matches!(key, WorkKey::Preview(_)))
            .count();
        if preview_backlog >= 4 {
            return;
        }
        let mut remaining = max_background_jobs.saturating_sub(outstanding);
        let page_count = self.engine.descriptor().page_count;
        let mut examined = 0_u32;
        while remaining > 0 && examined < page_count {
            let page = PageIndex(self.index_cursor);
            self.index_cursor = if self.index_cursor + 1 >= page_count {
                0
            } else {
                self.index_cursor + 1
            };
            examined += 1;
            let preview_key = WorkKey::Preview(page);
            if (self.indexed_pages.contains(&page)
                && self
                    .previews
                    .contains_variant(page, self.layout.render_variant))
                || self.quarantined_pages.contains(&page)
                || self.compiled.contains_key(&page)
                || self.compile_states.contains_key(&page)
                || self.queued.contains(&preview_key)
                || self.in_flight.contains_key(&preview_key)
            {
                continue;
            }
            if self.layout.placement(page).is_none() {
                continue;
            }
            self.request_compile(
                page,
                CompileNeeds {
                    text_index: !self.indexed_pages.contains(&page),
                    preview: !self
                        .previews
                        .contains_variant(page, self.layout.render_variant),
                    ..CompileNeeds::default()
                },
                10,
            );
            remaining -= 1;
        }
    }

    fn fulfill_resident(&mut self, page: PageIndex, needs: CompileNeeds) {
        let Some((artifacts, text_lease)) = self
            .compiled
            .get(&page)
            .map(|entry| (entry.artifacts.clone(), entry._text_lease.clone()))
        else {
            return;
        };
        if needs.preview {
            let priority = if needs.interactive_generation.is_some() {
                1_400
            } else {
                700
            };
            self.schedule_page_preview(page, artifacts.clone(), priority);
        }
        if let Some(generation) = needs.interactive_generation {
            let newly_indexed = self.indexed_pages.insert(page);
            if self.published_pages.insert(page) {
                self.updates
                    .push(SessionUpdate::PageCompiled(PageArtifactUpdate {
                        page,
                        generation,
                        text: Arc::clone(artifacts.text.substrate()),
                        structure: artifacts.structure.clone(),
                        operation_count: artifacts.compiled.operation_count(),
                        lowering_degraded: artifacts.lowering_degraded,
                        memory_lease: text_lease,
                    }));
            }
            if newly_indexed {
                self.publish_index_progress();
            }
        } else if needs.text_index && self.indexed_pages.insert(page) {
            self.updates.push(SessionUpdate::PageIndexed {
                page,
                text: Arc::clone(artifacts.text.substrate()),
                structure: artifacts.structure.clone(),
            });
            self.publish_index_progress();
        }
    }

    fn schedule_page_preview(
        &mut self,
        page: PageIndex,
        artifacts: Arc<CompiledArtifacts>,
        priority: i32,
    ) {
        let key = WorkKey::Preview(page);
        if self
            .previews
            .contains_variant(page, self.layout.render_variant)
            || self.queued.contains(&key)
            || self.in_flight.contains_key(&key)
        {
            return;
        }
        let Some((bucket, demand)) = self.previews.demand(&self.layout, page) else {
            return;
        };
        let cancellation = CancellationFlag::default();
        self.queued.insert(key);
        self.raster_pending.push(Prioritized {
            priority,
            sequence: self.next_sequence(),
            value: RasterJob::PagePreview {
                id: self.next_sequence(),
                key,
                artifacts,
                demand,
                bucket,
                class: if priority >= 1_000 {
                    WorkClass::VisibleFallback
                } else {
                    WorkClass::Background
                },
                cancellation,
            },
        });
    }

    fn schedule_raster(
        &mut self,
        intent: &ViewportIntent,
        demand: TileDemand,
        pass: RasterPass,
        base_priority: i32,
    ) {
        self.schedule_raster_at_bucket(intent, demand, intent.bucket, pass, base_priority);
    }

    fn schedule_raster_at_bucket(
        &mut self,
        intent: &ViewportIntent,
        demand: TileDemand,
        bucket: super::ZoomBucket,
        pass: RasterPass,
        base_priority: i32,
    ) {
        let Some(artifacts) = self
            .compiled
            .get(&demand.page)
            .map(|entry| entry.artifacts.clone())
        else {
            return;
        };
        let tier = match pass {
            RasterPass::Thumbnail => TileTier::Thumbnail,
            RasterPass::Draft => TileTier::Draft,
            RasterPass::TextFirst => TileTier::TextFirst,
            RasterPass::Final => TileTier::Final,
        };
        let tile_key = demand.key(self.engine.descriptor().id, bucket, tier);
        let key = WorkKey::Raster(tile_key);
        let exposure_bias = match intent.direction {
            ScrollDirection::Down if pass != RasterPass::Thumbnail => demand.coord.y,
            ScrollDirection::Up if pass != RasterPass::Thumbnail => -demand.coord.y,
            ScrollDirection::Down | ScrollDirection::Up | ScrollDirection::Stationary => 0,
        };
        let distance_penalty = if pass == RasterPass::Thumbnail {
            0
        } else {
            demand.distance_from_viewport.min(1000.0) as i32
        };
        let priority = base_priority + exposure_bias - distance_penalty;
        if self.tiles.contains_at_or_above(tile_key) || self.in_flight.contains_key(&key) {
            return;
        }
        if self.queued.contains(&key) {
            let current_priority = self
                .raster_pending
                .iter()
                .filter(|job| raster_job_identity(&job.value).1 == key)
                .map(|job| job.priority)
                .max();
            if current_priority.is_some_and(|current| current >= priority) {
                return;
            }
            self.raster_pending
                .retain(|job| raster_job_identity(&job.value).1 != key);
            self.queued.remove(&key);
        }
        let cancellation = CancellationFlag::default();
        let class = if demand.visible && pass == RasterPass::Final {
            WorkClass::InteractiveVisible
        } else if demand.visible {
            WorkClass::VisibleFallback
        } else {
            WorkClass::Predictive
        };
        self.queued.insert(key);
        self.raster_pending.push(Prioritized {
            priority,
            sequence: self.next_sequence(),
            value: RasterJob::Tile {
                id: self.next_sequence(),
                key,
                artifacts,
                demand,
                bucket,
                pass,
                class,
                generation: intent.generation,
                cancellation,
            },
        });
    }

    fn dispatch(&mut self) {
        while let Some(ticket) = self.compile_pending.pop() {
            let page = ticket.value.page;
            let Some(state) = self.compile_states.get(&page) else {
                continue;
            };
            if state.phase != CompilePhase::Pending || state.revision != ticket.value.revision {
                continue;
            }
            let Some(placement) = self.layout.placement(page) else {
                self.compile_states.remove(&page);
                continue;
            };
            let class = if state.needs.interactive_generation.is_some() {
                WorkClass::InteractiveVisible
            } else {
                WorkClass::Background
            };
            let cancellation = state.cancellation.clone();
            let id = self.next_sequence();
            let job = CompileJob {
                id,
                page,
                page_to_doc: placement.page_to_doc,
                cancellation: cancellation.clone(),
            };
            match self.compile_tx.try_send(job) {
                Ok(()) => {
                    if let Some(state) = self.compile_states.get_mut(&page) {
                        state.phase = CompilePhase::InFlight;
                    }
                    self.in_flight.insert(
                        WorkKey::Compile(page),
                        InFlightWork {
                            id,
                            cancellation,
                            class,
                        },
                    );
                }
                Err(TrySendError::Full(_)) => {
                    self.compile_pending.push(Prioritized {
                        priority: ticket.priority,
                        sequence: ticket.sequence,
                        value: ticket.value,
                    });
                    break;
                }
                Err(TrySendError::Disconnected(_)) => break,
            }
        }
        while let Some(job) = self.raster_pending.pop() {
            let (id, key, cancellation, class) = raster_job_identity(&job.value);
            match self.raster_tx.try_send(job.value) {
                Ok(()) => {
                    self.queued.remove(&key);
                    self.in_flight.insert(
                        key,
                        InFlightWork {
                            id,
                            cancellation,
                            class,
                        },
                    );
                }
                Err(TrySendError::Full(value)) => {
                    self.raster_pending.push(Prioritized {
                        priority: job.priority,
                        sequence: job.sequence,
                        value,
                    });
                    break;
                }
                Err(TrySendError::Disconnected(_)) => break,
            }
        }
    }

    fn handle_result(&mut self, result: WorkerResult) {
        let mut needs_replan = false;
        match result {
            WorkerResult::Compiled { id, page, result } => {
                if !remove_matching_in_flight(&mut self.in_flight, WorkKey::Compile(page), id) {
                    self.dispatch();
                    self.publish_depths();
                    return;
                }
                let mut needs = self
                    .compile_states
                    .remove(&page)
                    .map_or_else(CompileNeeds::default, |state| state.needs);
                match result {
                    Ok(artifacts) => {
                        let current = self.intent.load_full();
                        if latency_critical_navigation(current.navigation_mode) {
                            needs.text_index = false;
                            needs.preview = false;
                            if !current
                                .visible_tiles
                                .iter()
                                .any(|demand| demand.page == page)
                            {
                                needs.interactive_generation = None;
                            }
                        }
                        if current.page_is_relevant(page) || self.page_is_warm(page) {
                            let compiled_lease = self.memory.reserve(
                                CacheCategory::Compiled,
                                artifacts.estimated_compiled_bytes(),
                            );
                            let text_lease = self
                                .memory
                                .reserve(CacheCategory::Text, artifacts.estimated_text_bytes());
                            self.compiled.insert(
                                page,
                                CompiledEntry {
                                    artifacts: artifacts.clone(),
                                    _compiled_lease: compiled_lease,
                                    _text_lease: text_lease,
                                },
                            );
                            self.fulfill_resident(page, needs);
                            self.evict_compiled_over_budget();
                        } else {
                            if needs.preview {
                                self.schedule_page_preview(page, artifacts.clone(), 20);
                            }
                            if needs.text_index && self.indexed_pages.insert(page) {
                                self.updates.push(SessionUpdate::PageIndexed {
                                    page,
                                    text: Arc::clone(artifacts.text.substrate()),
                                    structure: artifacts.structure.clone(),
                                });
                                self.publish_index_progress();
                            }
                        }
                        needs_replan = true;
                    }
                    Err(DocumentEngineError::Cancelled) => needs_replan = true,
                    Err(error) => self.report_page_error(page, error),
                }
            }
            WorkerResult::Rastered {
                id,
                key,
                demand,
                result,
            } => {
                if !remove_matching_in_flight(&mut self.in_flight, key, id) {
                    self.dispatch();
                    self.publish_depths();
                    return;
                }
                match result {
                    Ok(tile) => {
                        let current = self.intent.load();
                        let relevant = current.raster_tile_is_relevant(
                            tile.key.page,
                            tile.key.bucket,
                            tile.key.coord,
                            tile.key.tier,
                        );
                        if !relevant {
                            self.dispatch();
                            self.publish_depths();
                            return;
                        }
                        let tile = Arc::new(tile);
                        self.tiles
                            .insert(tile.clone(), demand.distance_from_viewport);
                        self.updates.push(SessionUpdate::TileReady {
                            key: tile.key,
                            generation: tile.generation,
                        });
                    }
                    Err(DocumentEngineError::Cancelled) => needs_replan = true,
                    Err(DocumentEngineError::TextFirstUnsupported) => {}
                    Err(error) => self.report_page_error(demand.page, error),
                }
            }
            WorkerResult::Previewed {
                id,
                key,
                page,
                result,
            } => {
                if !remove_matching_in_flight(&mut self.in_flight, key, id) {
                    self.dispatch();
                    self.publish_depths();
                    return;
                }
                match result {
                    Ok(surface) => {
                        self.previews.insert(Arc::new(surface));
                        let current = self.intent.load();
                        if current.page_is_relevant(page)
                            || current.thumbnail_page_is_relevant(page)
                        {
                            self.updates.push(SessionUpdate::PreviewReady { page });
                        }
                        needs_replan = true;
                    }
                    Err(DocumentEngineError::Cancelled) => needs_replan = true,
                    Err(error) => self.report_page_error(page, error),
                }
            }
        }
        if needs_replan {
            self.replan();
        } else {
            self.dispatch();
            self.publish_depths();
        }
    }

    fn evict_compiled_over_budget(&mut self) {
        while self.memory.over_budget() > 0 {
            let intent = self.intent.load_full();
            let viewport_center = intent.viewport_document.center().y;
            let candidate = self
                .compiled
                .keys()
                .filter(|page| !intent.page_is_relevant(**page))
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
                        .unwrap_or(Ordering::Equal)
                })
                .copied();
            let Some(candidate) = candidate else {
                // The visible/compile-ahead ring is a hard floor. Other cache
                // owners must shed memory before we discard its last IR/text.
                break;
            };
            self.compiled.remove(&candidate);
        }
    }

    fn report_page_error(&mut self, page: PageIndex, error: DocumentEngineError) {
        let failures = self.page_failures.entry(page).or_insert(0);
        *failures = failures.saturating_add(1);
        let quarantined = page_is_exhausted(*failures, &error);
        if quarantined {
            self.quarantined_pages.insert(page);
        }
        self.indexed_pages.insert(page);
        self.publish_index_progress();
        self.updates.push(SessionUpdate::PageError {
            page,
            message: error.to_string(),
            quarantined,
        });
    }

    fn cancel_irrelevant(&mut self, intent: &ViewportIntent) {
        let latency_critical = latency_critical_navigation(intent.navigation_mode);
        let visible_pages = intent
            .visible_tiles
            .iter()
            .map(|demand| demand.page)
            .collect::<HashSet<_>>();
        let warm_pages = self
            .warm_hints
            .iter()
            .filter(|(_, hint)| hint.expires_at > Instant::now())
            .map(|(page, _)| *page)
            .collect::<HashSet<_>>();
        for (key, work) in &self.in_flight {
            let relevant = if latency_critical {
                match key {
                    WorkKey::Compile(page) => visible_pages.contains(page),
                    WorkKey::Preview(page) => visible_pages.contains(page),
                    WorkKey::Raster(tile) => {
                        tile.tier == TileTier::Final
                            && intent.tile_is_visible(tile.page, tile.coord)
                            && matches!(
                                work.class,
                                WorkClass::InteractiveVisible | WorkClass::Predictive
                            )
                    }
                }
            } else {
                match key {
                    WorkKey::Compile(page) => {
                        intent.page_is_relevant(*page)
                            || warm_pages.contains(page)
                            || intent.navigation_mode == NavigationMode::Idle
                    }
                    WorkKey::Preview(page) => {
                        intent.page_is_relevant(*page)
                            || intent.thumbnail_page_is_relevant(*page)
                            || warm_pages.contains(page)
                            || intent.navigation_mode == NavigationMode::Idle
                    }
                    WorkKey::Raster(tile) => intent.raster_tile_is_relevant(
                        tile.page,
                        tile.bucket,
                        tile.coord,
                        tile.tier,
                    ),
                }
            };
            if !relevant {
                work.cancellation.cancel();
            }
        }
        self.compile_states.retain(|page, state| {
            let relevant = if latency_critical {
                visible_pages.contains(page)
            } else {
                intent.page_is_relevant(*page)
                    || warm_pages.contains(page)
                    || intent.navigation_mode == NavigationMode::Idle
            };
            if !relevant {
                state.cancellation.cancel();
            }
            relevant || state.phase == CompilePhase::InFlight
        });
        self.compile_pending.retain(|ticket| {
            self.compile_states
                .get(&ticket.value.page)
                .is_some_and(|state| {
                    state.phase == CompilePhase::Pending && state.revision == ticket.value.revision
                })
        });
        self.raster_pending.retain(|job| match &job.value {
            RasterJob::Tile {
                key: WorkKey::Raster(tile),
                demand,
                class,
                ..
            } => {
                if latency_critical {
                    tile.tier == TileTier::Final
                        && intent.tile_is_visible(demand.page, demand.coord)
                        && *class == WorkClass::InteractiveVisible
                } else {
                    intent.raster_tile_is_relevant(
                        demand.page,
                        tile.bucket,
                        demand.coord,
                        tile.tier,
                    )
                }
            }
            RasterJob::Tile { .. } => false,
            RasterJob::PagePreview { demand, .. } => {
                (latency_critical && visible_pages.contains(&demand.page))
                    || (!latency_critical
                        && (intent.page_is_relevant(demand.page)
                            || intent.thumbnail_page_is_relevant(demand.page)
                            || warm_pages.contains(&demand.page)
                            || intent.navigation_mode == NavigationMode::Idle))
            }
        });
        self.queued.retain(|key| match key {
            WorkKey::Compile(page) => {
                if latency_critical {
                    visible_pages.contains(page)
                } else {
                    intent.page_is_relevant(*page)
                        || warm_pages.contains(page)
                        || intent.navigation_mode == NavigationMode::Idle
                }
            }
            WorkKey::Preview(page) => {
                (latency_critical && visible_pages.contains(page))
                    || (!latency_critical
                        && (intent.page_is_relevant(*page)
                            || intent.thumbnail_page_is_relevant(*page)
                            || warm_pages.contains(page)
                            || intent.navigation_mode == NavigationMode::Idle))
            }
            WorkKey::Raster(tile) => {
                if latency_critical {
                    tile.tier == TileTier::Final && intent.tile_is_visible(tile.page, tile.coord)
                } else {
                    intent.raster_tile_is_relevant(tile.page, tile.bucket, tile.coord, tile.tier)
                }
            }
        });
    }

    fn cancel_all(&mut self) {
        for work in self.in_flight.values() {
            work.cancellation.cancel();
        }
        self.in_flight.clear();
        self.queued.clear();
        self.compile_states.clear();
        self.compile_pending.clear();
        self.raster_pending.clear();
    }

    fn publish_depths(&self) {
        let interactive_compile_pending = self
            .compile_states
            .values()
            .filter(|state| state.phase == CompilePhase::Pending)
            .count();
        let interactive_in_flight = self
            .in_flight
            .keys()
            .filter(|key| !matches!(key, WorkKey::Preview(_)))
            .count();
        self.updates.push(SessionUpdate::QueueDepths {
            compile_pending: interactive_compile_pending,
            raster_pending: self.raster_pending.len(),
            in_flight: interactive_in_flight,
        });
    }

    fn publish_index_progress(&self) {
        self.updates.push(SessionUpdate::TextIndexProgress {
            indexed_pages: self.indexed_pages.len().min(u32::MAX as usize) as u32,
            total_pages: self.engine.descriptor().page_count,
        });
    }

    fn next_sequence(&self) -> u64 {
        self.sequence.fetch_add(1, AtomicOrdering::Relaxed)
    }
}

fn spawn_compile_worker(
    index: usize,
    engine: Arc<dyn DocumentEngine>,
    _intent: Arc<ArcSwap<ViewportIntent>>,
    jobs: Receiver<CompileJob>,
    results: Sender<WorkerResult>,
) {
    let _ = std::thread::Builder::new()
        .name(format!("lege-viewer-compile-{index}"))
        .spawn(move || {
            let mut worker = engine.create_compile_worker();
            while let Ok(job) = jobs.recv() {
                let CompileJob {
                    id,
                    page,
                    page_to_doc,
                    cancellation,
                } = job;
                let result = catch_unwind(AssertUnwindSafe(|| {
                    worker.compile_page(page, page_to_doc, &cancellation)
                }))
                .unwrap_or_else(|payload| Err(DocumentEngineError::Panic(panic_message(payload))));
                let _ = results.send(WorkerResult::Compiled { id, page, result });
            }
        });
}

fn spawn_raster_worker(
    index: usize,
    engine: Arc<dyn DocumentEngine>,
    intent: Arc<ArcSwap<ViewportIntent>>,
    jobs: Receiver<RasterJob>,
    results: Sender<WorkerResult>,
) {
    let _ = std::thread::Builder::new()
        .name(format!("lege-viewer-raster-{index}"))
        .spawn(move || {
            let mut worker = engine.create_raster_worker();
            while let Ok(job) = jobs.recv() {
                match job {
                    RasterJob::Tile {
                        id,
                        key,
                        artifacts,
                        demand,
                        bucket,
                        pass,
                        generation,
                        cancellation,
                        ..
                    } => {
                        let current = intent.load();
                        let tier = match pass {
                            RasterPass::Thumbnail => TileTier::Thumbnail,
                            RasterPass::Draft => TileTier::Draft,
                            RasterPass::TextFirst => TileTier::TextFirst,
                            RasterPass::Final => TileTier::Final,
                        };
                        let relevant = current.raster_tile_is_relevant(
                            demand.page,
                            bucket,
                            demand.coord,
                            tier,
                        );
                        if !relevant {
                            cancellation.cancel();
                        }
                        let result = catch_unwind(AssertUnwindSafe(|| {
                            worker.raster_tile(
                                &artifacts,
                                bucket,
                                demand,
                                pass,
                                generation,
                                &cancellation,
                            )
                        }))
                        .unwrap_or_else(|payload| {
                            Err(DocumentEngineError::Panic(panic_message(payload)))
                        });
                        let _ = results.send(WorkerResult::Rastered {
                            id,
                            key,
                            demand,
                            result,
                        });
                    }
                    RasterJob::PagePreview {
                        id,
                        key,
                        artifacts,
                        demand,
                        bucket,
                        cancellation,
                        ..
                    } => {
                        let result = catch_unwind(AssertUnwindSafe(|| {
                            worker.raster_tile(
                                &artifacts,
                                bucket,
                                demand,
                                RasterPass::Thumbnail,
                                0,
                                &cancellation,
                            )
                        }))
                        .unwrap_or_else(|payload| {
                            Err(DocumentEngineError::Panic(panic_message(payload)))
                        });
                        let _ = results.send(WorkerResult::Previewed {
                            id,
                            key,
                            page: demand.page,
                            result,
                        });
                    }
                }
            }
        });
}

fn raster_job_identity(job: &RasterJob) -> (u64, WorkKey, CancellationFlag, WorkClass) {
    match job {
        RasterJob::Tile {
            id,
            key,
            cancellation,
            class,
            ..
        }
        | RasterJob::PagePreview {
            id,
            key,
            cancellation,
            class,
            ..
        } => (*id, *key, cancellation.clone(), *class),
    }
}

/// A page that has failed this many times is not going to succeed on the next
/// attempt either.
///
/// Without a limit the planner re-requests a permanently broken page on every
/// replan: a worker stays busy on it, the same error is republished, and the
/// viewer answers each report with a full-canvas redraw — an idle document
/// burning a core forever. A panic is conclusive on the first occurrence; an
/// ordinary error gets one retry, because a transient allocation or
/// cancellation race should not set a page aside permanently.
const MAX_PAGE_FAILURES: u32 = 2;

fn page_is_exhausted(failures: u32, error: &DocumentEngineError) -> bool {
    matches!(error, DocumentEngineError::Panic(_)) || failures >= MAX_PAGE_FAILURES
}

fn remove_matching_in_flight(
    in_flight: &mut HashMap<WorkKey, InFlightWork>,
    key: WorkKey,
    id: u64,
) -> bool {
    if in_flight.get(&key).is_some_and(|work| work.id == id) {
        in_flight.remove(&key);
        true
    } else {
        false
    }
}

fn latency_critical_navigation(mode: NavigationMode) -> bool {
    matches!(mode, NavigationMode::JumpLikely | NavigationMode::Skimming)
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

fn vertical_distance(rect: RectF, viewport: RectF) -> f64 {
    if rect.intersects(viewport) {
        0.0
    } else if rect.bottom() < viewport.y {
        viewport.y - rect.bottom()
    } else {
        rect.y - viewport.bottom()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panicking_page_is_set_aside_at_once_and_a_failing_one_after_a_retry() {
        let panic = DocumentEngineError::Panic("worker".to_owned());
        assert!(
            page_is_exhausted(1, &panic),
            "a panic is conclusive the first time"
        );
        let broken = DocumentEngineError::Engine("bad object".to_owned());
        assert!(
            !page_is_exhausted(1, &broken),
            "an ordinary failure earns one retry"
        );
        assert!(
            page_is_exhausted(2, &broken),
            "the second identical failure stops the retry loop"
        );
    }

    #[test]
    fn compile_needs_promote_in_place_without_losing_background_work() {
        let mut needs = CompileNeeds {
            text_index: true,
            preview: true,
            ..CompileNeeds::default()
        };
        needs.merge(CompileNeeds {
            interactive_generation: Some(42),
            ..CompileNeeds::default()
        });
        assert_eq!(needs.interactive_generation, Some(42));
        assert!(needs.text_index);
        assert!(needs.preview);
    }

    #[test]
    fn stale_worker_result_cannot_remove_replacement_work() {
        let key = WorkKey::Compile(PageIndex(7));
        let mut in_flight = HashMap::from([(
            key,
            InFlightWork {
                id: 12,
                cancellation: CancellationFlag::default(),
                class: WorkClass::InteractiveVisible,
            },
        )]);

        assert!(!remove_matching_in_flight(&mut in_flight, key, 11));
        assert_eq!(in_flight.get(&key).map(|work| work.id), Some(12));
        assert!(remove_matching_in_flight(&mut in_flight, key, 12));
        assert!(!in_flight.contains_key(&key));
    }
}
