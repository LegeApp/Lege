use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::thread::JoinHandle;

use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, select, unbounded};

use crate::geometry::RectF;

use super::cache::{CacheCategory, MemoryArbiter, MemoryLease, TileCache};
use super::engine::{
    CancellationFlag, CompiledArtifacts, DocumentEngine, DocumentEngineError, RasterPass,
};
use super::layout::PageLayoutIndex;
use super::session::{PageArtifactUpdate, SessionUpdate, UpdateQueue};
use super::tile::{TileDemand, TileKey, TileTier};
use super::viewport::{ScrollDirection, ViewportIntent, thumbnail_demands};
use super::PageIndex;

#[derive(Debug)]
pub enum ConductorCommand {
    IntentChanged,
    LayoutChanged(Arc<PageLayoutIndex>),
    LinkPeek {
        page: PageIndex,
        region: RectF,
    },
    Shutdown,
}

#[derive(Debug)]
pub struct ConductorHandle {
    commands: Sender<ConductorCommand>,
    intent: Arc<ArcSwap<ViewportIntent>>,
    thread: Option<JoinHandle<()>>,
}

impl ConductorHandle {
    pub fn spawn(
        engine: Arc<dyn DocumentEngine>,
        layout: Arc<PageLayoutIndex>,
        updates: Arc<UpdateQueue>,
        memory: MemoryArbiter,
        tiles: Arc<TileCache>,
    ) -> Self {
        let (commands, command_rx) = unbounded();
        let intent = Arc::new(ArcSwap::from_pointee(ViewportIntent::empty()));
        let thread_intent = intent.clone();
        let thread = std::thread::Builder::new()
            .name("lege-viewer-conductor".to_owned())
            .spawn(move || {
                Conductor::new(engine, layout, updates, memory, tiles, thread_intent, command_rx).run();
            })
            .expect("spawn viewer conductor");
        Self {
            commands,
            intent,
            thread: Some(thread),
        }
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
    Raster(TileKey),
}

#[derive(Debug)]
enum CompileJob {
    Page {
        key: WorkKey,
        page: PageIndex,
        generation: u64,
        page_to_doc: crate::geometry::Affine,
        cancellation: CancellationFlag,
    },
}

#[derive(Debug)]
enum RasterJob {
    Tile {
        key: WorkKey,
        artifacts: Arc<CompiledArtifacts>,
        demand: TileDemand,
        bucket: super::ZoomBucket,
        pass: RasterPass,
        generation: u64,
        cancellation: CancellationFlag,
    },
}

#[derive(Debug)]
enum WorkerResult {
    Compiled {
        key: WorkKey,
        page: PageIndex,
        generation: u64,
        result: Result<Arc<CompiledArtifacts>, DocumentEngineError>,
    },
    Rastered {
        key: WorkKey,
        demand: TileDemand,
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
    intent: Arc<ArcSwap<ViewportIntent>>,
    command_rx: Receiver<ConductorCommand>,
    compile_tx: Sender<CompileJob>,
    raster_tx: Sender<RasterJob>,
    result_rx: Receiver<WorkerResult>,
    compile_pending: BinaryHeap<Prioritized<CompileJob>>,
    raster_pending: BinaryHeap<Prioritized<RasterJob>>,
    sequence: AtomicU64,
    compiled: HashMap<PageIndex, CompiledEntry>,
    queued: HashSet<WorkKey>,
    in_flight: HashMap<WorkKey, CancellationFlag>,
    quarantined_pages: HashSet<PageIndex>,
}

impl Conductor {
    fn new(
        engine: Arc<dyn DocumentEngine>,
        layout: Arc<PageLayoutIndex>,
        updates: Arc<UpdateQueue>,
        memory: MemoryArbiter,
        tiles: Arc<TileCache>,
        intent: Arc<ArcSwap<ViewportIntent>>,
        command_rx: Receiver<ConductorCommand>,
    ) -> Self {
        let descriptor = engine.descriptor();
        let workers = std::thread::available_parallelism()
            .map_or(4, std::num::NonZeroUsize::get);
        let compile_workers = (workers / 3).max(1);
        let raster_workers = workers.saturating_sub(compile_workers).max(1);
        let (compile_tx, compile_rx) = bounded(compile_workers * 2);
        let (raster_tx, raster_rx) = bounded(raster_workers * 3);
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
            intent,
            command_rx,
            compile_tx,
            raster_tx,
            result_rx,
            compile_pending: BinaryHeap::new(),
            raster_pending: BinaryHeap::new(),
            sequence: AtomicU64::new(1),
            compiled: HashMap::new(),
            queued: HashSet::new(),
            in_flight: HashMap::new(),
            quarantined_pages: HashSet::new(),
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
                        self.replan();
                    }
                    Ok(ConductorCommand::LinkPeek { page, region: _ }) => {
                        // The crop-demand shape already exists. Implementation
                        // should enqueue only the few tiles intersecting the
                        // target region at priority between visible finals and
                        // overscan drafts.
                        self.ensure_compiled(page, 925);
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
        self.cancel_irrelevant(&intent);

        for (order, page) in intent.compile_pages.iter().copied().enumerate() {
            let priority = 800 - order.min(500) as i32;
            self.ensure_compiled(page, priority);
        }

        for demand in intent.visible_tiles.iter().copied() {
            self.schedule_visible_tile(&intent, demand);
        }
        for demand in intent.overscan_tiles.iter().copied() {
            self.schedule_raster(&intent, demand, RasterPass::Draft, 650);
        }
        for (order, page) in intent.thumbnail_pages.iter().copied().enumerate() {
            self.ensure_compiled(page, 725 - order.min(25) as i32);
            if self.compiled.contains_key(&page) {
                self.schedule_thumbnail_page(&intent, page, 700 - order.min(25) as i32);
            }
        }
        self.dispatch();
        self.publish_depths();
    }

    fn schedule_thumbnail_page(
        &mut self,
        intent: &ViewportIntent,
        page: PageIndex,
        priority: i32,
    ) {
        let Some((bucket, demands)) = thumbnail_demands(&self.layout, page, 160) else {
            return;
        };
        for demand in demands {
            self.schedule_raster_at_bucket(
                intent,
                demand,
                bucket,
                RasterPass::Thumbnail,
                priority,
            );
        }
    }

    fn schedule_visible_tile(&mut self, intent: &ViewportIntent, demand: TileDemand) {
        let Some(artifacts) = self
            .compiled
            .get(&demand.page)
            .map(|entry| entry.artifacts.clone())
        else {
            self.ensure_compiled(demand.page, 1000);
            return;
        };
        let text_first = self.engine.supports_text_first(&artifacts);
        if text_first {
            self.schedule_raster(intent, demand, RasterPass::TextFirst, 1100);
        } else {
            self.schedule_raster(intent, demand, RasterPass::Draft, 1050);
        }
        if intent.speed_pages_per_second_hint() < 3.0 {
            self.schedule_raster(intent, demand, RasterPass::Final, 1000);
        }
    }

    fn ensure_compiled(&mut self, page: PageIndex, priority: i32) {
        let key = WorkKey::Compile(page);
        if self.compiled.contains_key(&page)
            || self.quarantined_pages.contains(&page)
            || self.queued.contains(&key)
            || self.in_flight.contains_key(&key)
        {
            return;
        }
        let Some(placement) = self.layout.placement(page) else {
            return;
        };
        let cancellation = CancellationFlag::default();
        self.queued.insert(key);
        self.compile_pending.push(Prioritized {
            priority,
            sequence: self.next_sequence(),
            value: CompileJob::Page {
                key,
                page,
                generation: self.intent.load().generation,
                page_to_doc: placement.page_to_doc,
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
        self.schedule_raster_at_bucket(
            intent,
            demand,
            intent.bucket,
            pass,
            base_priority,
        );
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
        if self.tiles.contains_at_or_above(tile_key)
            || self.queued.contains(&key)
            || self.in_flight.contains_key(&key)
        {
            return;
        }
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
        let cancellation = CancellationFlag::default();
        self.queued.insert(key);
        self.raster_pending.push(Prioritized {
            priority: base_priority + exposure_bias - distance_penalty,
            sequence: self.next_sequence(),
            value: RasterJob::Tile {
                key,
                artifacts,
                demand,
                bucket,
                pass,
                generation: intent.generation,
                cancellation,
            },
        });
    }

    fn dispatch(&mut self) {
        loop {
            let Some(job) = self.compile_pending.pop() else {
                break;
            };
            let (key, cancellation) = compile_job_identity(&job.value);
            match self.compile_tx.try_send(job.value) {
                Ok(()) => {
                    self.queued.remove(&key);
                    self.in_flight.insert(key, cancellation);
                }
                Err(TrySendError::Full(value)) => {
                    self.compile_pending.push(Prioritized {
                        priority: job.priority,
                        sequence: job.sequence,
                        value,
                    });
                    break;
                }
                Err(TrySendError::Disconnected(_)) => break,
            }
        }
        loop {
            let Some(job) = self.raster_pending.pop() else {
                break;
            };
            let (key, cancellation) = raster_job_identity(&job.value);
            match self.raster_tx.try_send(job.value) {
                Ok(()) => {
                    self.queued.remove(&key);
                    self.in_flight.insert(key, cancellation);
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
            WorkerResult::Compiled {
                key,
                page,
                generation,
                result,
            } => {
                self.in_flight.remove(&key);
                match result {
                    Ok(artifacts) => {
                        let compiled_lease = self.memory.reserve(
                            CacheCategory::Compiled,
                            artifacts.estimated_compiled_bytes(),
                        );
                        let text_lease = self.memory.reserve(
                            CacheCategory::Text,
                            artifacts.estimated_text_bytes(),
                        );
                        self.compiled.insert(
                            page,
                            CompiledEntry {
                                artifacts: artifacts.clone(),
                                _compiled_lease: compiled_lease,
                                _text_lease: text_lease.clone(),
                            },
                        );
                        self.updates.push(SessionUpdate::PageCompiled(PageArtifactUpdate {
                            page,
                            generation,
                            text: Arc::clone(artifacts.text.substrate()),
                            structure: artifacts.structure.clone(),
                            operation_count: artifacts.compiled.operation_count(),
                            lowering_degraded: artifacts.lowering_degraded,
                            memory_lease: text_lease,
                        }));
                        self.evict_compiled_over_budget();
                        needs_replan = true;
                    }
                    Err(DocumentEngineError::Cancelled) => needs_replan = true,
                    Err(error) => self.report_page_error(page, error),
                }
            }
            WorkerResult::Rastered {
                key,
                demand,
                result,
            } => {
                self.in_flight.remove(&key);
                match result {
                    Ok(tile) => {
                        let tile = Arc::new(tile);
                        self.tiles.insert(tile.clone(), demand.distance_from_viewport);
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
        let quarantined = matches!(error, DocumentEngineError::Panic(_));
        if quarantined {
            self.quarantined_pages.insert(page);
        }
        self.updates.push(SessionUpdate::PageError {
            page,
            message: error.to_string(),
            quarantined,
        });
    }

    fn cancel_irrelevant(&mut self, intent: &ViewportIntent) {
        for (key, cancellation) in &self.in_flight {
            let relevant = match key {
                WorkKey::Compile(page) => intent.page_is_relevant(*page),
                WorkKey::Raster(tile) => {
                    if tile.tier == TileTier::Thumbnail {
                        intent.thumbnail_page_is_relevant(tile.page)
                    } else {
                        tile.bucket == intent.bucket
                            && intent.tile_is_relevant(tile.page, tile.coord)
                    }
                }
            };
            if !relevant {
                cancellation.cancel();
            }
        }
        self.compile_pending.retain(|job| match &job.value {
            CompileJob::Page { page, .. } => intent.page_is_relevant(*page),
        });
        self.raster_pending.retain(|job| match &job.value {
            RasterJob::Tile {
                key: WorkKey::Raster(tile),
                demand,
                ..
            } => {
                if tile.tier == TileTier::Thumbnail {
                    intent.thumbnail_page_is_relevant(demand.page)
                } else {
                    tile.bucket == intent.bucket
                        && intent.tile_is_relevant(demand.page, demand.coord)
                }
            },
            RasterJob::Tile { .. } => false,
        });
        self.queued.retain(|key| match key {
            WorkKey::Compile(page) => intent.page_is_relevant(*page),
            WorkKey::Raster(tile) => {
                if tile.tier == TileTier::Thumbnail {
                    intent.thumbnail_page_is_relevant(tile.page)
                } else {
                    tile.bucket == intent.bucket
                        && intent.tile_is_relevant(tile.page, tile.coord)
                }
            }
        });
    }

    fn cancel_all(&mut self) {
        for cancellation in self.in_flight.values() {
            cancellation.cancel();
        }
        self.in_flight.clear();
        self.queued.clear();
        self.compile_pending.clear();
        self.raster_pending.clear();
    }

    fn publish_depths(&self) {
        self.updates.push(SessionUpdate::QueueDepths {
            compile_pending: self.compile_pending.len(),
            raster_pending: self.raster_pending.len(),
            in_flight: self.in_flight.len(),
        });
    }

    fn next_sequence(&self) -> u64 {
        self.sequence.fetch_add(1, AtomicOrdering::Relaxed)
    }
}

fn spawn_compile_worker(
    index: usize,
    engine: Arc<dyn DocumentEngine>,
    intent: Arc<ArcSwap<ViewportIntent>>,
    jobs: Receiver<CompileJob>,
    results: Sender<WorkerResult>,
) {
    let _ = std::thread::Builder::new()
        .name(format!("lege-viewer-compile-{index}"))
        .spawn(move || {
            let mut worker = engine.create_compile_worker();
            while let Ok(job) = jobs.recv() {
                match job {
                    CompileJob::Page {
                        key,
                        page,
                        generation,
                        page_to_doc,
                        cancellation,
                    } => {
                        if !intent.load().page_is_relevant(page) {
                            cancellation.cancel();
                        }
                        let result = catch_unwind(AssertUnwindSafe(|| {
                            worker.compile_page(page, page_to_doc, &cancellation)
                        }))
                        .unwrap_or_else(|payload| {
                            Err(DocumentEngineError::Panic(panic_message(payload)))
                        });
                        let _ = results.send(WorkerResult::Compiled {
                            key,
                            page,
                            generation,
                            result,
                        });
                    }
                }
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
                        key,
                        artifacts,
                        demand,
                        bucket,
                        pass,
                        generation,
                        cancellation,
                    } => {
                        let current = intent.load();
                        let relevant = if pass == RasterPass::Thumbnail {
                            current.thumbnail_page_is_relevant(demand.page)
                        } else {
                            current.bucket == bucket
                                && current.tile_is_relevant(demand.page, demand.coord)
                        };
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
                            key,
                            demand,
                            result,
                        });
                    }
                }
            }
        });
}

fn compile_job_identity(job: &CompileJob) -> (WorkKey, CancellationFlag) {
    match job {
        CompileJob::Page {
            key, cancellation, ..
        } => (*key, cancellation.clone()),
    }
}

fn raster_job_identity(job: &RasterJob) -> (WorkKey, CancellationFlag) {
    match job {
        RasterJob::Tile {
            key, cancellation, ..
        } => (*key, cancellation.clone()),
    }
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
