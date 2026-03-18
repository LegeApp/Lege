use crate::jbig2classify::{FamilyBucketKey, SymbolSignature, family_bucket_key_for_symbol};
use crate::jbig2enc::PageData;
use crate::jbig2structs::SymbolContextMode;
use crate::jbig2sym::Rect;
use rustc_hash::FxHashMap;

#[derive(Debug, Clone, Default)]
pub struct SymbolContextModel {
    symbol_contexts: Vec<SymbolContextStats>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextDecision {
    Allow,
    Reject,
    Unknown,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ContextSimilarity {
    pub evidence: usize,
    pub left_jaccard: f32,
    pub right_jaccard: f32,
    pub left_cosine: f32,
    pub right_cosine: f32,
    pub left_jsd: f32,
    pub right_jsd: f32,
    pub start_delta: f32,
    pub end_delta: f32,
}

#[derive(Debug, Clone, Default)]
struct SymbolContextStats {
    occurrences: usize,
    left_tokens: FxHashMap<u64, u16>,
    right_tokens: FxHashMap<u64, u16>,
    start_like: usize,
    end_like: usize,
}

#[derive(Debug, Clone, Copy)]
struct LineInstance {
    symbol_index: usize,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

#[derive(Debug, Clone, Default)]
struct LineGroup {
    min_y: i32,
    max_y: i32,
    instances: Vec<LineInstance>,
}

impl LineGroup {
    fn can_accept(&self, instance: LineInstance) -> bool {
        let center_y = instance.y + instance.height / 2;
        let line_center = (self.min_y + self.max_y) / 2;
        let tolerance = ((self.max_y - self.min_y).max(instance.height) / 2).max(3);
        (center_y - line_center).abs() <= tolerance
    }

    fn push(&mut self, instance: LineInstance) {
        self.min_y = self.min_y.min(instance.y);
        self.max_y = self.max_y.max(instance.y + instance.height);
        self.instances.push(instance);
    }
}

pub fn build_symbol_context_model(
    pages: &[PageData],
    symbols: &[crate::jbig2sym::BitImage],
    signatures: &[SymbolSignature],
) -> SymbolContextModel {
    let bucket_keys: Vec<FamilyBucketKey> = symbols
        .iter()
        .zip(signatures.iter())
        .map(|(symbol, signature)| family_bucket_key_for_symbol(symbol, signature))
        .collect();
    let mut symbol_contexts = vec![SymbolContextStats::default(); symbols.len()];

    for page in pages {
        let mut lines: Vec<LineGroup> = Vec::new();
        let mut instances: Vec<LineInstance> = page
            .symbol_instances
            .iter()
            .map(|instance| line_instance(instance.symbol_index, instance.position))
            .collect();
        instances.sort_unstable_by_key(|instance| (instance.y, instance.x));

        for instance in instances {
            if let Some(line) = lines.iter_mut().find(|line| line.can_accept(instance)) {
                line.push(instance);
            } else {
                lines.push(LineGroup {
                    min_y: instance.y,
                    max_y: instance.y + instance.height,
                    instances: vec![instance],
                });
            }
        }

        for line in &mut lines {
            line.instances.sort_unstable_by_key(|instance| instance.x);
            for (idx, instance) in line.instances.iter().enumerate() {
                let stats = &mut symbol_contexts[instance.symbol_index];
                stats.occurrences += 1;

                let left_gap = idx
                    .checked_sub(1)
                    .and_then(|i| line.instances.get(i))
                    .map(|left| {
                        bump_token(
                            &mut stats.left_tokens,
                            context_token(
                                true,
                                bucket_keys[left.symbol_index],
                                spacing_bin(*instance, *left, true),
                            ),
                        );
                        instance.x - (left.x + left.width)
                    });
                let right_gap = line.instances.get(idx + 1).map(|right| {
                    bump_token(
                        &mut stats.right_tokens,
                        context_token(
                            false,
                            bucket_keys[right.symbol_index],
                            spacing_bin(*instance, *right, false),
                        ),
                    );
                    right.x - (instance.x + instance.width)
                });

                if left_gap.is_none_or(|gap| gap >= word_gap_threshold(*instance)) {
                    stats.start_like += 1;
                }
                if right_gap.is_none_or(|gap| gap >= word_gap_threshold(*instance)) {
                    stats.end_like += 1;
                }
            }
        }
    }

    SymbolContextModel { symbol_contexts }
}

impl SymbolContextModel {
    pub fn similarity(&self, lhs: usize, rhs: usize) -> Option<ContextSimilarity> {
        let lhs_stats = self.symbol_contexts.get(lhs)?;
        let rhs_stats = self.symbol_contexts.get(rhs)?;

        let evidence = lhs_stats.occurrences.min(rhs_stats.occurrences);
        if evidence == 0 {
            return None;
        }

        Some(ContextSimilarity {
            evidence,
            left_jaccard: weighted_jaccard(&lhs_stats.left_tokens, &rhs_stats.left_tokens),
            right_jaccard: weighted_jaccard(&lhs_stats.right_tokens, &rhs_stats.right_tokens),
            left_cosine: cosine_similarity(&lhs_stats.left_tokens, &rhs_stats.left_tokens),
            right_cosine: cosine_similarity(&lhs_stats.right_tokens, &rhs_stats.right_tokens),
            left_jsd: jensen_shannon_divergence(&lhs_stats.left_tokens, &rhs_stats.left_tokens),
            right_jsd: jensen_shannon_divergence(&lhs_stats.right_tokens, &rhs_stats.right_tokens),
            start_delta: ratio_delta(
                lhs_stats.start_like,
                lhs_stats.occurrences,
                rhs_stats.start_like,
                rhs_stats.occurrences,
            ),
            end_delta: ratio_delta(
                lhs_stats.end_like,
                lhs_stats.occurrences,
                rhs_stats.end_like,
                rhs_stats.occurrences,
            ),
        })
    }

    pub fn merge_decision(
        &self,
        lhs: usize,
        rhs: usize,
        mode: SymbolContextMode,
    ) -> ContextDecision {
        let Some(lhs_stats) = self.symbol_contexts.get(lhs) else {
            return ContextDecision::Unknown;
        };
        let Some(rhs_stats) = self.symbol_contexts.get(rhs) else {
            return ContextDecision::Unknown;
        };
        let Some(similarity) = self.similarity(lhs, rhs) else {
            return ContextDecision::Unknown;
        };

        let left_mass: u32 = lhs_stats
            .left_tokens
            .values()
            .map(|&v| v as u32)
            .sum::<u32>()
            .min(
                rhs_stats
                    .left_tokens
                    .values()
                    .map(|&v| v as u32)
                    .sum::<u32>(),
            );
        let right_mass: u32 = lhs_stats
            .right_tokens
            .values()
            .map(|&v| v as u32)
            .sum::<u32>()
            .min(
                rhs_stats
                    .right_tokens
                    .values()
                    .map(|&v| v as u32)
                    .sum::<u32>(),
            );

        if similarity.evidence < 4 || left_mass + right_mass < 4 {
            return ContextDecision::Unknown;
        }

        let combined_jaccard = 0.5 * (similarity.left_jaccard + similarity.right_jaccard);
        let combined_cosine = 0.5 * (similarity.left_cosine + similarity.right_cosine);
        let max_jsd = similarity.left_jsd.max(similarity.right_jsd);
        let edge_delta = similarity.start_delta.max(similarity.end_delta);

        match mode {
            SymbolContextMode::WeightedJaccard => {
                if combined_jaccard >= 0.55
                    && similarity.left_jaccard >= 0.40
                    && similarity.right_jaccard >= 0.40
                    && edge_delta <= 0.50
                {
                    ContextDecision::Allow
                } else if similarity.evidence >= 6
                    && (combined_jaccard <= 0.15 || edge_delta >= 0.75)
                {
                    ContextDecision::Reject
                } else {
                    ContextDecision::Unknown
                }
            }
            SymbolContextMode::Cosine => {
                if combined_cosine >= 0.82
                    && similarity.left_cosine >= 0.70
                    && similarity.right_cosine >= 0.70
                    && edge_delta <= 0.45
                {
                    ContextDecision::Allow
                } else if similarity.evidence >= 6
                    && (combined_cosine <= 0.35 || edge_delta >= 0.75)
                {
                    ContextDecision::Reject
                } else {
                    ContextDecision::Unknown
                }
            }
            SymbolContextMode::Hybrid => {
                if combined_jaccard >= 0.42
                    && combined_cosine >= 0.72
                    && max_jsd <= 0.45
                    && edge_delta <= 0.40
                {
                    ContextDecision::Allow
                } else if similarity.evidence >= 6
                    && (combined_jaccard <= 0.12
                        || combined_cosine <= 0.35
                        || max_jsd >= 0.75
                        || edge_delta >= 0.75)
                {
                    ContextDecision::Reject
                } else {
                    ContextDecision::Unknown
                }
            }
        }
    }
}

fn line_instance(symbol_index: usize, rect: Rect) -> LineInstance {
    LineInstance {
        symbol_index,
        x: rect.x as i32,
        y: rect.y as i32,
        width: rect.width as i32,
        height: rect.height as i32,
    }
}

#[inline]
fn bump_token(map: &mut FxHashMap<u64, u16>, token: u64) {
    let entry = map.entry(token).or_insert(0);
    *entry = entry.saturating_add(1);
}

#[inline]
fn context_token(is_left: bool, key: FamilyBucketKey, spacing_bin: u8) -> u64 {
    let dir = if is_left { 1u64 } else { 2u64 };
    (dir << 56)
        | ((key.w_bin as u64) << 40)
        | ((key.h_bin as u64) << 24)
        | ((key.density_bin as u64) << 16)
        | ((key.aspect_bin as u64) << 12)
        | ((key.centroid_y_bin as u64) << 8)
        | ((key.lr_balance_bin as u64) << 4)
        | (spacing_bin as u64)
}

#[inline]
fn spacing_bin(curr: LineInstance, neighbor: LineInstance, is_left: bool) -> u8 {
    let gap = if is_left {
        curr.x - (neighbor.x + neighbor.width)
    } else {
        neighbor.x - (curr.x + curr.width)
    };

    if gap <= 0 {
        0
    } else if gap <= 2 {
        1
    } else if gap <= 6 {
        2
    } else {
        3
    }
}

#[inline]
fn word_gap_threshold(instance: LineInstance) -> i32 {
    (instance.width.max(instance.height) / 2).clamp(3, 8)
}

fn weighted_jaccard(a: &FxHashMap<u64, u16>, b: &FxHashMap<u64, u16>) -> f32 {
    let mut min_sum = 0u32;
    let mut max_sum = 0u32;
    let mut seen = rustc_hash::FxHashSet::default();

    for (&k, &av) in a {
        let bv = b.get(&k).copied().unwrap_or(0);
        min_sum += av.min(bv) as u32;
        max_sum += av.max(bv) as u32;
        seen.insert(k);
    }

    for (&k, &bv) in b {
        if seen.contains(&k) {
            continue;
        }
        max_sum += bv as u32;
    }

    if max_sum == 0 {
        1.0
    } else {
        min_sum as f32 / max_sum as f32
    }
}

fn cosine_similarity(a: &FxHashMap<u64, u16>, b: &FxHashMap<u64, u16>) -> f32 {
    let mut dot = 0f32;
    let mut norm_a = 0f32;
    let mut norm_b = 0f32;

    for &av in a.values() {
        let av = av as f32;
        norm_a += av * av;
    }
    for &bv in b.values() {
        let bv = bv as f32;
        norm_b += bv * bv;
    }
    for (&k, &av) in a {
        if let Some(&bv) = b.get(&k) {
            dot += (av as f32) * (bv as f32);
        }
    }

    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a.sqrt() * norm_b.sqrt())
    }
}

fn jensen_shannon_divergence(a: &FxHashMap<u64, u16>, b: &FxHashMap<u64, u16>) -> f32 {
    let mut keys = rustc_hash::FxHashSet::default();
    for &k in a.keys() {
        keys.insert(k);
    }
    for &k in b.keys() {
        keys.insert(k);
    }

    let total_a: f32 = a.values().map(|&v| v as f32).sum::<f32>() + keys.len() as f32;
    let total_b: f32 = b.values().map(|&v| v as f32).sum::<f32>() + keys.len() as f32;

    let mut js = 0.0f32;

    for &k in &keys {
        let pa = (a.get(&k).copied().unwrap_or(0) as f32 + 1.0) / total_a;
        let pb = (b.get(&k).copied().unwrap_or(0) as f32 + 1.0) / total_b;
        let m = 0.5 * (pa + pb);

        js += 0.5 * pa * (pa / m).ln();
        js += 0.5 * pb * (pb / m).ln();
    }

    js
}

#[inline]
fn ratio_delta(lhs_hits: usize, lhs_total: usize, rhs_hits: usize, rhs_total: usize) -> f32 {
    let lhs_ratio = if lhs_total == 0 {
        0.0
    } else {
        lhs_hits as f32 / lhs_total as f32
    };
    let rhs_ratio = if rhs_total == 0 {
        0.0
    } else {
        rhs_hits as f32 / rhs_total as f32
    };
    (lhs_ratio - rhs_ratio).abs()
}
