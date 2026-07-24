use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::sync::atomic::{AtomicU64, Ordering};

use super::tile::{TileCoord, TileDemand, TileKey, TileSurface, TileTier, ZoomBucket};
use super::{DocumentId, PageIndex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CacheCategory {
    Compiled,
    Tiles,
    Thumbnails,
    Text,
    Images,
}

#[derive(Debug)]
struct MemoryState {
    budget: u64,
    total: AtomicU64,
    compiled: AtomicU64,
    tiles: AtomicU64,
    thumbnails: AtomicU64,
    text: AtomicU64,
    images: AtomicU64,
}

#[derive(Debug, Clone)]
pub struct MemoryArbiter {
    inner: Arc<MemoryState>,
}

impl MemoryArbiter {
    pub fn new(budget: u64) -> Self {
        Self {
            inner: Arc::new(MemoryState {
                budget,
                total: AtomicU64::new(0),
                compiled: AtomicU64::new(0),
                tiles: AtomicU64::new(0),
                thumbnails: AtomicU64::new(0),
                text: AtomicU64::new(0),
                images: AtomicU64::new(0),
            }),
        }
    }

    pub fn reserve(&self, category: CacheCategory, bytes: u64) -> MemoryLease {
        self.inner.total.fetch_add(bytes, Ordering::AcqRel);
        counter(&self.inner, category).fetch_add(bytes, Ordering::AcqRel);
        MemoryLease {
            inner: Arc::new(MemoryLeaseInner {
                owner: Arc::downgrade(&self.inner),
                category,
                bytes,
            }),
        }
    }

    pub fn total_bytes(&self) -> u64 {
        self.inner.total.load(Ordering::Acquire)
    }

    pub fn budget_bytes(&self) -> u64 {
        self.inner.budget
    }

    pub fn over_budget(&self) -> u64 {
        self.total_bytes().saturating_sub(self.budget_bytes())
    }

    pub fn category_bytes(&self, category: CacheCategory) -> u64 {
        counter(&self.inner, category).load(Ordering::Acquire)
    }
}

#[derive(Debug, Clone)]
pub struct MemoryLease {
    inner: Arc<MemoryLeaseInner>,
}

impl MemoryLease {
    pub fn bytes(&self) -> u64 {
        self.inner.bytes
    }
}

#[derive(Debug)]
struct MemoryLeaseInner {
    owner: Weak<MemoryState>,
    category: CacheCategory,
    bytes: u64,
}

impl Drop for MemoryLeaseInner {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.upgrade() {
            owner.total.fetch_sub(self.bytes, Ordering::AcqRel);
            counter(&owner, self.category).fetch_sub(self.bytes, Ordering::AcqRel);
        }
    }
}

fn counter(state: &MemoryState, category: CacheCategory) -> &AtomicU64 {
    match category {
        CacheCategory::Compiled => &state.compiled,
        CacheCategory::Tiles => &state.tiles,
        CacheCategory::Thumbnails => &state.thumbnails,
        CacheCategory::Text => &state.text,
        CacheCategory::Images => &state.images,
    }
}

#[derive(Debug)]
struct TileEntry {
    surface: Arc<TileSurface>,
    lease: MemoryLease,
    last_used: u64,
    reproduction_cost: f64,
    distance_from_viewport: f64,
}

#[derive(Debug)]
pub struct TileCache {
    document: DocumentId,
    arbiter: MemoryArbiter,
    entries: Mutex<HashMap<TileKey, TileEntry>>,
    clock: AtomicU64,
}

impl TileCache {
    pub fn new(document: DocumentId, arbiter: MemoryArbiter) -> Self {
        Self {
            document,
            arbiter,
            entries: Mutex::new(HashMap::new()),
            clock: AtomicU64::new(1),
        }
    }

    pub fn insert(&self, surface: Arc<TileSurface>, distance_from_viewport: f64) {
        let mut entries = self.entries.lock().expect("tile cache poisoned");
        if entries
            .get(&surface.key)
            .is_some_and(|existing| existing.surface.generation > surface.generation)
        {
            return;
        }
        let category = if surface.key.tier == TileTier::Thumbnail {
            CacheCategory::Thumbnails
        } else {
            CacheCategory::Tiles
        };
        let bytes = surface.byte_len();
        let lease = self.arbiter.reserve(category, bytes);
        let reproduction_cost = match surface.key.tier {
            TileTier::Thumbnail => 0.2,
            TileTier::Draft => 0.4,
            TileTier::TextFirst => 0.8,
            TileTier::Final => 1.0,
        };
        entries.insert(
            surface.key,
            TileEntry {
                surface,
                lease,
                last_used: self.clock.fetch_add(1, Ordering::Relaxed),
                reproduction_cost,
                distance_from_viewport,
            },
        );
        drop(entries);
        self.evict_over_budget();
    }

    /// Resolve the fallback ladder for one requested page region. Exact-
    /// bucket tiles are selected by coordinate. Cross-bucket fallbacks are
    /// selected by page-space intersection, because the same tile coordinate
    /// covers a different region at every zoom bucket.
    pub fn best_covering(
        &self,
        demand: TileDemand,
        requested_bucket: ZoomBucket,
    ) -> Vec<Arc<TileSurface>> {
        let mut entries = self.entries.lock().expect("tile cache poisoned");
        let tick = self.clock.fetch_add(1, Ordering::Relaxed);

        let exact_key = entries
            .keys()
            .filter(|key| {
                key.document == self.document
                    && key.page == demand.page
                    && key.bucket == requested_bucket
                    && key.coord == demand.coord
                    && key.tier != TileTier::Thumbnail
            })
            .max_by_key(|key| key.tier.rank())
            .copied();
        if let Some(key) = exact_key {
            if let Some(entry) = entries.get_mut(&key) {
                entry.last_used = tick;
                return vec![entry.surface.clone()];
            }
        }

        let fallback_bucket = entries
            .keys()
            .filter(|key| {
                key.document == self.document
                    && key.page == demand.page
                    && key.tier != TileTier::Thumbnail
            })
            .filter_map(|key| {
                let entry = entries.get(key)?;
                entry
                    .surface
                    .page_document_rect
                    .intersects(demand.page_document_rect)
                    .then_some(key.bucket)
            })
            .min_by_key(|bucket| bucket.distance(requested_bucket));

        if let Some(bucket) = fallback_bucket {
            return best_intersecting_at_bucket(
                &mut entries,
                self.document,
                demand,
                bucket,
                false,
                tick,
            );
        }

        let thumbnail_bucket = entries
            .keys()
            .filter(|key| {
                key.document == self.document
                    && key.page == demand.page
                    && key.tier == TileTier::Thumbnail
            })
            .filter_map(|key| {
                let entry = entries.get(key)?;
                entry
                    .surface
                    .page_document_rect
                    .intersects(demand.page_document_rect)
                    .then_some(key.bucket)
            })
            .min_by_key(|bucket| bucket.distance(requested_bucket));
        thumbnail_bucket.map_or_else(Vec::new, |bucket| {
            best_intersecting_at_bucket(
                &mut entries,
                self.document,
                demand,
                bucket,
                true,
                tick,
            )
        })
    }

    /// Return one coherent raster bucket for a page/tier, suitable for a
    /// thumbnail popup or other page-level preview. The page still remains
    /// tile-backed; this method never assembles a whole-page bitmap.
    pub fn page_tiles_at_tier(
        &self,
        page: PageIndex,
        tier: TileTier,
    ) -> Vec<Arc<TileSurface>> {
        let mut entries = self.entries.lock().expect("tile cache poisoned");
        let mut bucket_counts: HashMap<ZoomBucket, usize> = HashMap::new();
        for key in entries.keys() {
            if key.document == self.document && key.page == page && key.tier == tier {
                *bucket_counts.entry(key.bucket).or_default() += 1;
            }
        }
        let Some(bucket) = bucket_counts
            .into_iter()
            .max_by_key(|(bucket, count)| (*count, std::cmp::Reverse(bucket.0.abs())))
            .map(|(bucket, _)| bucket)
        else {
            return Vec::new();
        };

        let tick = self.clock.fetch_add(1, Ordering::Relaxed);
        let mut keys = entries
            .keys()
            .filter(|key| {
                key.document == self.document
                    && key.page == page
                    && key.bucket == bucket
                    && key.tier == tier
            })
            .copied()
            .collect::<Vec<_>>();
        keys.sort_by_key(|key| key.coord);
        keys.into_iter()
            .filter_map(|key| {
                let entry = entries.get_mut(&key)?;
                entry.last_used = tick;
                Some(entry.surface.clone())
            })
            .collect()
    }

    pub fn contains_at_or_above(&self, key: TileKey) -> bool {
        let entries = self.entries.lock().expect("tile cache poisoned");
        if key.tier == TileTier::Thumbnail {
            return entries.contains_key(&key);
        }
        entries.keys().any(|candidate| {
            candidate.document == key.document
                && candidate.page == key.page
                && candidate.bucket == key.bucket
                && candidate.coord == key.coord
                && candidate.tier != TileTier::Thumbnail
                && candidate.tier.rank() >= key.tier.rank()
        })
    }

    pub fn contains(&self, key: TileKey) -> bool {
        self.entries.lock().expect("tile cache poisoned").contains_key(&key)
    }

    pub fn len(&self) -> usize {
        self.entries.lock().expect("tile cache poisoned").len()
    }

    fn evict_over_budget(&self) {
        let mut entries = self.entries.lock().expect("tile cache poisoned");
        while self.arbiter.over_budget() > 0 && entries.len() > 1 {
            let now = self.clock.load(Ordering::Relaxed);
            let candidate = entries
                .iter()
                .max_by(|(_, left), (_, right)| {
                    eviction_score(left, now)
                        .partial_cmp(&eviction_score(right, now))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|(key, _)| *key);
            let Some(candidate) = candidate else {
                break;
            };
            entries.remove(&candidate);
        }
    }
}

fn best_intersecting_at_bucket(
    entries: &mut HashMap<TileKey, TileEntry>,
    document: DocumentId,
    demand: TileDemand,
    bucket: ZoomBucket,
    thumbnails: bool,
    tick: u64,
) -> Vec<Arc<TileSurface>> {
    let mut best_by_coord: HashMap<TileCoord, TileKey> = HashMap::new();
    for (&key, entry) in entries.iter() {
        if key.document != document
            || key.page != demand.page
            || key.bucket != bucket
            || (key.tier == TileTier::Thumbnail) != thumbnails
            || !entry
                .surface
                .page_document_rect
                .intersects(demand.page_document_rect)
        {
            continue;
        }
        best_by_coord
            .entry(key.coord)
            .and_modify(|best| {
                if key.tier.rank() > best.tier.rank() {
                    *best = key;
                }
            })
            .or_insert(key);
    }
    let mut keys = best_by_coord.into_values().collect::<Vec<_>>();
    keys.sort_by_key(|key| key.coord);
    keys.into_iter()
        .filter_map(|key| {
            let entry = entries.get_mut(&key)?;
            entry.last_used = tick;
            Some(entry.surface.clone())
        })
        .collect()
}

fn eviction_score(entry: &TileEntry, now: u64) -> f64 {
    let age = now.saturating_sub(entry.last_used) as f64;
    let bytes = entry.lease.bytes().max(1) as f64;
    // Distant, cheap-to-reproduce, old, large entries leave first.
    (entry.distance_from_viewport + 1.0)
        * (1.2 - entry.reproduction_cost)
        * (age + 1.0)
        * bytes.sqrt()
}
