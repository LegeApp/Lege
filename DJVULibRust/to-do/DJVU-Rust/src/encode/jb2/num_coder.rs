//! DjVu-spec-compatible integer coder using the 4-phase multivalue extension.

use crate::encode::zc::ZEncoder;
use crate::encode::jb2::error::Jb2Error;
use std::io::Write;

/// Bounds for signed integer coding.
pub const BIG_POSITIVE: i32 = 262_142;
pub const BIG_NEGATIVE: i32 = -262_143;

/// Number of contexts used for phase 2 range decisions (per DjVu spec)
const PHASE2_CONTEXTS: usize = 18;

/// DjVu-spec-compatible integer encoder using the 4-phase multivalue extension.
/// This follows the exact algorithm described in the DjVu v3 specification.
pub struct NumCoder {
    base_context: u8,
    max_contexts: u8,
    next_context: u8,
}

impl NumCoder {
    /// Creates a new NumCoder that will use a specific range of contexts.
    pub fn new(base_context_index: u8, max_contexts: u8) -> Self {
        Self {
            base_context: base_context_index,
            max_contexts,
            next_context: base_context_index,
        }
    }

    /// Allocates a new context and returns its handle
    pub fn alloc_context(&mut self) -> u8 {
        if self.next_context >= self.base_context + self.max_contexts {
            // Reuse contexts if we run out (simple round-robin)
            self.next_context = self.base_context;
        }
        let ctx = self.next_context;
        self.next_context += 1;
        ctx
    }

    /// Encodes an integer using the DjVu 4-phase multivalue extension algorithm.
    /// This follows the exact specification from the DjVu v3 spec.
    pub fn encode_integer<W: Write>(
        &self,
        zc: &mut ZEncoder<W>,
        contexts: &mut [u8],
        base_context: usize,
        value: i32,
        low: i32,
        high: i32,
    ) -> Result<(), Jb2Error> {
        if value < low || value > high {
            return Err(Jb2Error::InvalidNumber(format!(
                "Value {} outside range [{}, {}]", value, low, high
            )));
        }

        if low == high {
            // No encoding needed if range is single value
            return Ok(());
        }

        // Phase 1: Sign bit (if range includes both positive and negative)
        let v = if low < 0 && high >= 0 {
            let negative = value < 0;
            zc.encode(negative, &mut contexts[base_context])?;
            
            if negative {
                (-value - 1) as u32
            } else {
                value as u32
            }
        } else if low >= 0 {
            value as u32
        } else {
            (-value - 1) as u32
        };

        // Phase 2: Range determination using the DjVu-spec ranges
        let ranges = [
            (0, 0),       // Range 0: 0
            (1, 2),       // Range 1: 1-2  
            (3, 6),       // Range 2: 3-6
            (7, 14),      // Range 3: 7-14
            (15, 30),     // Range 4: 15-30
            (31, 62),     // Range 5: 31-62
            (63, 126),    // Range 6: 63-126
            (127, 254),   // Range 7: 127-254
            (255, 510),   // Range 8: 255-510
            (511, 1022),  // Range 9: 511-1022
            (1023, 2046), // Range 10: 1023-2046
            (2047, 4094), // Range 11: 2047-4094
            (4095, 8190), // Range 12: 4095-8190
            (8191, 16382), // Range 13: 8191-16382
            (16383, 32766), // Range 14: 16383-32766
            (32767, 65534), // Range 15: 32767-65534
            (65535, 131070), // Range 16: 65535-131070
            (131071, 262142), // Range 17: 131071-262142
        ];

        let mut range_index = None;
        for (i, &(start, end)) in ranges.iter().enumerate() {
            if v >= start && v <= end {
                range_index = Some(i);
                break;
            }
        }

        let range_index = range_index.ok_or_else(|| {
            Jb2Error::InvalidNumber(format!("Value {} outside all ranges", v))
        })?;

        // Encode range decisions (use exactly 18 contexts for phase 2)
        for i in 0..range_index {
            if base_context + 1 + i >= contexts.len() {
                return Err(Jb2Error::ContextOverflow);
            }
            zc.encode(false, &mut contexts[base_context + 1 + i])?; // Not in this range
        }
        if range_index < ranges.len() && base_context + 1 + range_index < contexts.len() {
            zc.encode(true, &mut contexts[base_context + 1 + range_index])?; // In this range
        }

        // Phase 3: Exact value within range (LSB-first per DjVu spec)
        let (range_start, range_end) = ranges[range_index];
        if range_start != range_end {
            let offset = v - range_start;
            let range_size = range_end - range_start + 1;
            
            // Encode bits of offset, LSB first (least significant bit first)
            let bits_needed = (range_size as f64).log2().ceil() as u32;
            let phase3_context_base = base_context + 1 + PHASE2_CONTEXTS;
            
            for bit_pos in 0..bits_needed {
                if phase3_context_base + bit_pos as usize >= contexts.len() {
                    return Err(Jb2Error::ContextOverflow);
                }
                let bit = (offset >> bit_pos) & 1;
                zc.encode(bit != 0, &mut contexts[phase3_context_base + bit_pos as usize])?;
            }
        }

        // Phase 4: Sign bit for mixed ranges (already handled in Phase 1)
        
        Ok(())
    }


}
