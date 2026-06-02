use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Context, Result, bail};

use crate::vision::onnx::attrs::shape_i64_to_usize;
use crate::vision::onnx::graph::PreparedGraph;
use crate::vision::onnx::types::{PlannedOp, PlannedOpKind, UnaryKind};

/// Resolves a buffer name to its canonical (alias-target) name.
pub(crate) fn resolve_alias(name: &str, aliases: &HashMap<String, String>) -> String {
    let mut cur = name.to_owned();
    while let Some(next) = aliases.get(&cur) {
        if *next == cur {
            break;
        }
        cur = next.clone();
    }
    cur
}

/// Builds the zero-cost alias map: Identity/Reshape/Unsqueeze/Squeeze outputs
/// share their input's buffer. Resident buffer planning and the compiled
/// executor must agree on this map so liveness covers every aliased consumer.
pub(crate) fn build_alias_map(ops: &[PlannedOp]) -> HashMap<String, String> {
    let mut aliases: HashMap<String, String> = HashMap::new();
    for op in ops {
        if matches!(
            &op.kind,
            PlannedOpKind::Unary(UnaryKind::Identity)
                | PlannedOpKind::Reshape { .. }
                | PlannedOpKind::Unsqueeze { .. }
                | PlannedOpKind::Squeeze { .. }
        ) {
            if let (Some(inp), Some(out)) = (op.inputs.first(), op.outputs.first()) {
                let canonical = resolve_alias(inp, &aliases);
                aliases.insert(out.clone(), canonical);
            }
        }
    }
    aliases
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum BufferRole {
    Input,
    Initializer,
    Intermediate,
    Output,
}

#[derive(Debug)]
pub(crate) struct BufferPlanEntry {
    pub(crate) id: usize,
    pub(crate) name: String,
    pub(crate) role: BufferRole,
    pub(crate) reuse_slot: Option<usize>,
    pub(crate) shape: Vec<usize>,
    pub(crate) elements: usize,
    pub(crate) bytes: u64,
    pub(crate) first_use: Option<usize>,
    pub(crate) last_use: Option<usize>,
}

#[derive(Debug)]
pub(crate) struct ResidentMemoryPlan {
    entries: Vec<BufferPlanEntry>,
    total_bytes: u64,
    peak_live_bytes: u64,
    reuse_slot_count: usize,
    reuse_total_bytes: u64,
    input_count: usize,
    initializer_count: usize,
    intermediate_count: usize,
    output_count: usize,
}

impl ResidentMemoryPlan {
    pub(crate) fn from_graph(graph: &PreparedGraph) -> Result<Self> {
        // Alias-aware planning: zero-cost ops (Identity/Reshape/Unsqueeze/Squeeze)
        // make their output share the input's buffer. We resolve every name to its
        // canonical buffer so a buffer's liveness covers all aliased consumers and
        // aliased outputs never claim a separate (prematurely freed) slot.
        let aliases = build_alias_map(&graph.planned_ops);
        let canon = |name: &str| resolve_alias(name, &aliases);

        let input_names = graph
            .inputs
            .iter()
            .map(|n| canon(n))
            .collect::<BTreeSet<_>>();
        let output_names = graph
            .outputs
            .iter()
            .map(|n| canon(n))
            .collect::<BTreeSet<_>>();
        let initializer_names = graph.constants.keys().cloned().collect::<BTreeSet<_>>();

        let mut buffer_names = BTreeSet::<String>::new();
        buffer_names.extend(input_names.iter().cloned());
        buffer_names.extend(output_names.iter().cloned());
        buffer_names.extend(initializer_names.iter().cloned());
        for op in &graph.planned_ops {
            buffer_names.extend(op.inputs.iter().map(|n| canon(n)));
            buffer_names.extend(op.outputs.iter().map(|n| canon(n)));
        }

        let mut first_use = HashMap::<String, usize>::new();
        let mut last_use = HashMap::<String, usize>::new();
        for (index, op) in graph.planned_ops.iter().enumerate() {
            for name in op.inputs.iter().chain(op.outputs.iter()) {
                let name = canon(name);
                first_use.entry(name.clone()).or_insert(index);
                last_use.insert(name, index);
            }
        }

        let mut entries = Vec::with_capacity(buffer_names.len());
        let mut total_bytes = 0u64;
        let mut input_count = 0usize;
        let mut initializer_count = 0usize;
        let mut intermediate_count = 0usize;
        let mut output_count = 0usize;

        for name in buffer_names {
            let shape_i64 = graph
                .known_shapes
                .get(&name)
                .with_context(|| format!("missing known shape for resident buffer `{name}`"))?;
            let shape = shape_i64_to_usize(shape_i64)?;
            let elements = shape.iter().product::<usize>();
            let bytes = (elements as u64)
                .checked_mul(std::mem::size_of::<f32>() as u64)
                .with_context(|| format!("resident buffer `{name}` byte size overflow"))?;
            if bytes == 0 {
                if initializer_names.contains(&name) {
                    continue;
                }
                bail!("resident runtime buffer `{name}` has zero bytes");
            }

            let role = if input_names.contains(&name) {
                input_count += 1;
                BufferRole::Input
            } else if output_names.contains(&name) {
                output_count += 1;
                BufferRole::Output
            } else if initializer_names.contains(&name) {
                initializer_count += 1;
                BufferRole::Initializer
            } else {
                intermediate_count += 1;
                BufferRole::Intermediate
            };

            total_bytes = total_bytes
                .checked_add(bytes)
                .context("resident memory total byte size overflow")?;
            entries.push(BufferPlanEntry {
                id: entries.len(),
                name,
                role,
                reuse_slot: None,
                shape,
                elements,
                bytes,
                first_use: None,
                last_use: None,
            });
        }

        for entry in &mut entries {
            entry.first_use = if initializer_names.contains(&entry.name) {
                Some(0)
            } else {
                first_use.get(&entry.name).copied()
            };
            entry.last_use =
                if initializer_names.contains(&entry.name) || output_names.contains(&entry.name) {
                    graph.planned_ops.len().checked_sub(1)
                } else {
                    last_use.get(&entry.name).copied()
                };
        }

        let peak_live_bytes = peak_live_bytes(&entries, graph.planned_ops.len());
        let reuse_slots = assign_reuse_slots(&mut entries);
        let reuse_total_bytes = reuse_slots.iter().map(|slot| slot.bytes).sum::<u64>();

        Ok(Self {
            entries,
            total_bytes,
            peak_live_bytes,
            reuse_slot_count: reuse_slots.len(),
            reuse_total_bytes,
            input_count,
            initializer_count,
            intermediate_count,
            output_count,
        })
    }

    pub(crate) fn print(&self, verbose: bool) {
        println!("resident GPU memory plan (v1 one-buffer-per-tensor)");
        println!("  buffers: {}", self.entries.len());
        println!(
            "    inputs={} initializers={} intermediates={} outputs={}",
            self.input_count, self.initializer_count, self.intermediate_count, self.output_count
        );
        println!(
            "  total storage: {} bytes ({:.2} MiB)",
            self.total_bytes,
            self.total_bytes as f64 / (1024.0 * 1024.0)
        );
        println!(
            "  peak live storage from first/last-use metadata: {} bytes ({:.2} MiB)",
            self.peak_live_bytes,
            self.peak_live_bytes as f64 / (1024.0 * 1024.0)
        );
        println!(
            "  reusable slot plan: {} physical buffers, {} bytes ({:.2} MiB)",
            self.reuse_slot_count,
            self.reuse_total_bytes,
            self.reuse_total_bytes as f64 / (1024.0 * 1024.0)
        );

        if let Some(entry) = self.entries.iter().max_by_key(|entry| entry.bytes) {
            println!(
                "  largest buffer: #{} {} {:?} {} bytes ({:.2} MiB) shape={:?}",
                entry.id,
                entry.name,
                entry.role,
                entry.bytes,
                entry.bytes as f64 / (1024.0 * 1024.0),
                entry.shape
            );
        }

        let mut by_role = BTreeMap::<BufferRole, (usize, u64)>::new();
        for entry in &self.entries {
            let totals = by_role.entry(entry.role).or_default();
            totals.0 += 1;
            totals.1 += entry.bytes;
        }
        println!("  storage by role:");
        for (role, (count, bytes)) in by_role {
            println!(
                "    {role:?}: {count} buffers, {bytes} bytes ({:.2} MiB)",
                bytes as f64 / (1024.0 * 1024.0)
            );
        }

        if verbose {
            println!("  buffers:");
            for entry in &self.entries {
                println!(
                    "    #{:04} slot={} {:?} bytes={} elems={} shape={:?} first_use={} last_use={} name={}",
                    entry.id,
                    format_optional_index(entry.reuse_slot),
                    entry.role,
                    entry.bytes,
                    entry.elements,
                    entry.shape,
                    format_optional_index(entry.first_use),
                    format_optional_index(entry.last_use),
                    entry.name
                );
            }
        }
    }

    pub(crate) fn slot_sizes(&self) -> Vec<u64> {
        let mut sizes = vec![0u64; self.reuse_slot_count];
        for entry in &self.entries {
            if let Some(slot) = entry.reuse_slot {
                sizes[slot] = sizes[slot].max(entry.bytes);
            }
        }
        sizes
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = &BufferPlanEntry> {
        self.entries.iter()
    }

    pub(crate) fn input_entries(&self) -> impl Iterator<Item = &BufferPlanEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.role == BufferRole::Input)
    }

    pub(crate) fn initializer_entries(&self) -> impl Iterator<Item = &BufferPlanEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.role == BufferRole::Initializer)
    }

    pub(crate) fn output_entries(&self) -> impl Iterator<Item = &BufferPlanEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.role == BufferRole::Output)
    }
}

fn format_optional_index(index: Option<usize>) -> String {
    index
        .map(|index| index.to_string())
        .unwrap_or_else(|| "-".to_owned())
}

fn peak_live_bytes(entries: &[BufferPlanEntry], step_count: usize) -> u64 {
    let mut peak = 0u64;
    for step in 0..step_count {
        let live = entries
            .iter()
            .filter(|entry| {
                let first = entry.first_use.unwrap_or(usize::MAX);
                let last = entry.last_use.unwrap_or(0);
                first <= step && step <= last
            })
            .map(|entry| entry.bytes)
            .sum::<u64>();
        peak = peak.max(live);
    }
    peak
}

#[derive(Debug)]
struct ReuseSlot {
    bytes: u64,
    last_use: usize,
}

fn assign_reuse_slots(entries: &mut [BufferPlanEntry]) -> Vec<ReuseSlot> {
    let mut sorted = (0..entries.len()).collect::<Vec<_>>();
    sorted.sort_by_key(|&index| {
        let entry = &entries[index];
        (
            entry.first_use.unwrap_or(usize::MAX),
            entry.last_use.unwrap_or(0),
            entry.bytes,
            entry.id,
        )
    });

    let mut slots = Vec::<ReuseSlot>::new();
    for entry_index in sorted {
        let entry = &entries[entry_index];
        let Some(first_use) = entry.first_use else {
            continue;
        };
        let last_use = entry.last_use.unwrap_or(first_use);
        let bytes = entry.bytes;

        let best_slot = slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.last_use.saturating_add(1) < first_use && slot.bytes >= bytes)
            .min_by_key(|(_, slot)| (slot.bytes, slot.last_use))
            .map(|(index, _)| index);

        let slot_index = if let Some(slot_index) = best_slot {
            slots[slot_index].last_use = last_use;
            slot_index
        } else {
            slots.push(ReuseSlot { bytes, last_use });
            slots.len() - 1
        };
        entries[entry_index].reuse_slot = Some(slot_index);
    }
    slots
}
