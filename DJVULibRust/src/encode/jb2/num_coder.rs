//! DjVu-compatible integer coder using tree-based NumContext.
//!
//! This implements the exact algorithm from DjVuLibre's JB2Image.cpp CodeNum function.
//! The algorithm uses a binary tree where each node contains a bit context, and
//! left/right child pointers to navigate based on encoding decisions.

use crate::encode::jb2::error::Jb2Error;
use crate::encode::zc::ZEncoder;
use std::io::Write;

/// Bounds for signed integer coding (from DjVuLibre).
pub const BIG_POSITIVE: i32 = 262_142;
pub const BIG_NEGATIVE: i32 = -262_143;

/// Chunk size for cell allocation (matches DjVuLibre CELLCHUNK).
const CELLCHUNK: usize = 20000;

/// A NumContext is an index into the tree structure.
pub type NumContext = u32;

/// Tree-based number coder matching DjVuLibre's exact algorithm.
///
/// This maintains a binary tree where:
/// - `bitcells[ctx]` is the bit context for node `ctx`
/// - `leftcell[ctx]` is the left child (decision = false)
/// - `rightcell[ctx]` is the right child (decision = true)
pub struct NumCoder {
    /// Bit contexts for each tree node
    pub bitcells: Vec<u8>,
    /// Left child pointers (decision = false)
    pub leftcell: Vec<NumContext>,
    /// Right child pointers (decision = true)
    pub rightcell: Vec<NumContext>,
    /// Next cell to allocate
    pub cur_ncell: NumContext,
}

impl Default for NumCoder {
    fn default() -> Self {
        Self::new()
    }
}

impl NumCoder {
    /// Creates a new NumCoder with initial capacity.
    pub fn new() -> Self {
        let mut coder = Self {
            bitcells: vec![0; CELLCHUNK],
            leftcell: vec![0; CELLCHUNK],
            rightcell: vec![0; CELLCHUNK],
            cur_ncell: 1, // Cell 0 is a dummy cell
        };
        // Initialize cell 0 as dummy
        coder.bitcells[0] = 0;
        coder.leftcell[0] = 0;
        coder.rightcell[0] = 0;
        coder
    }

    /// Resets all numerical contexts (called after REQUIRED_DICT_OR_RESET record).
    pub fn reset(&mut self) {
        // Clear all cells and reset counter
        for i in 0..self.bitcells.len() {
            self.bitcells[i] = 0;
            self.leftcell[i] = 0;
            self.rightcell[i] = 0;
        }
        self.cur_ncell = 1;
    }

    /// Returns true if we need to send a reset (too many cells allocated).
    pub fn needs_reset(&self) -> bool {
        self.cur_ncell as usize > CELLCHUNK
    }

    /// Encodes an integer using the DjVuLibre tree-based algorithm.
    ///
    /// This implements the exact algorithm from JB2Image.cpp CodeNum():
    /// - Phase 1: Sign encoding
    /// - Phase 2: Exponential range search (cutoff doubles)
    /// - Phase 3: Binary search to find exact value
    ///
    /// The `ctx` parameter is the root context for this number type (e.g., dist_record_type).
    /// It will be updated as the tree grows.
    pub fn code_num<W: Write>(
        &mut self,
        zc: &mut ZEncoder<W>,
        ctx: &mut NumContext,
        mut low: i32,
        mut high: i32,
        mut v: i32,
    ) -> Result<(), Jb2Error> {
        if v < low || v > high {
            return Err(Jb2Error::InvalidNumber(format!(
                "Value {} outside range [{}, {}]",
                v, low, high
            )));
        }

        let mut cutoff: i32 = 0;
        let mut phase = 1;
        let mut range: u32 = 0xffffffff;
        let mut negative;

        // We track the current position using an enum to handle the pointer-to-pointer semantics
        // In DjVuLibre: pctx points to either the root ctx, or to leftcell[x] or rightcell[x]
        enum CtxRef {
            Root,
            Left(usize),  // leftcell[idx]
            Right(usize), // rightcell[idx]
        }

        let mut ctx_ref = CtxRef::Root;

        // Navigate through the tree
        while range != 1 {
            // Get the current context value
            let current_ctx = match ctx_ref {
                CtxRef::Root => *ctx,
                CtxRef::Left(idx) => self.leftcell[idx],
                CtxRef::Right(idx) => self.rightcell[idx],
            };

            // Ensure we have a valid cell, allocating if necessary
            let current_ctx = if current_ctx == 0 {
                // Grow arrays if needed
                if self.cur_ncell as usize >= self.bitcells.len() {
                    let new_size = self.bitcells.len() + CELLCHUNK;
                    self.bitcells.resize(new_size, 0);
                    self.leftcell.resize(new_size, 0);
                    self.rightcell.resize(new_size, 0);
                }
                let new_cell = self.cur_ncell;
                self.cur_ncell += 1;
                self.bitcells[new_cell as usize] = 0;
                self.leftcell[new_cell as usize] = 0;
                self.rightcell[new_cell as usize] = 0;

                // Update the pointer
                match ctx_ref {
                    CtxRef::Root => *ctx = new_cell,
                    CtxRef::Left(idx) => self.leftcell[idx] = new_cell,
                    CtxRef::Right(idx) => self.rightcell[idx] = new_cell,
                }
                new_cell
            } else {
                current_ctx
            };

            // Determine the decision (encoding path)
            let decision = if low < cutoff && high >= cutoff {
                // Need to encode a bit
                let bit = v >= cutoff;
                zc.encode(bit, &mut self.bitcells[current_ctx as usize])?;
                bit
            } else {
                // No encoding needed - decision is determined by range
                v >= cutoff
            };

            // Navigate to child based on decision
            ctx_ref = if decision {
                CtxRef::Right(current_ctx as usize)
            } else {
                CtxRef::Left(current_ctx as usize)
            };

            // Phase-dependent logic
            match phase {
                1 => {
                    // Phase 1: Sign encoding
                    negative = !decision;
                    if negative {
                        v = -v - 1;
                        let temp = -low - 1;
                        low = -high - 1;
                        high = temp;
                    }
                    phase = 2;
                    cutoff = 1;
                }
                2 => {
                    // Phase 2: Exponential range search
                    if !decision {
                        // Found our range
                        phase = 3;
                        range = ((cutoff + 1) / 2) as u32;
                        if range == 1 {
                            cutoff = 0;
                        } else {
                            cutoff -= (range / 2) as i32;
                        }
                    } else {
                        // Keep doubling
                        cutoff += cutoff + 1;
                    }
                }
                3 => {
                    // Phase 3: Binary search within range
                    range /= 2;
                    if range != 1 {
                        if !decision {
                            cutoff -= (range / 2) as i32;
                        } else {
                            cutoff += (range / 2) as i32;
                        }
                    } else if !decision {
                        cutoff -= 1;
                    }
                }
                _ => unreachable!(),
            }
        }

        Ok(())
    }

    /// Helper function to allocate a new context and return its pointer.
    /// The context starts at 0 which will be allocated on first use.
    pub fn alloc_context(&self) -> NumContext {
        0
    }
}

/// Legacy wrapper for compatibility with old API.
/// This uses a simple approach that may not match DjVuLibre exactly.
/// For full compatibility, use NumCoder directly.
#[deprecated(note = "Use NumCoder::code_num directly for DjVuLibre compatibility")]
pub fn encode_integer_simple<W: Write>(
    zc: &mut ZEncoder<W>,
    contexts: &mut [u8],
    base_context: usize,
    value: i32,
    low: i32,
    high: i32,
) -> Result<(), Jb2Error> {
    // This is a simplified fallback that doesn't use the tree structure
    // It's kept for backward compatibility but should not be used for JB2

    if value < low || value > high {
        return Err(Jb2Error::InvalidNumber(format!(
            "Value {} outside range [{}, {}]",
            value, low, high
        )));
    }

    if low == high {
        return Ok(());
    }

    // For simple cases, just encode sign and magnitude
    let mut v = value;
    let lo = low;
    let hi = high;

    // Phase 1: Sign
    if lo < 0 && hi >= 0 {
        let negative = v < 0;
        zc.encode(negative, &mut contexts[base_context])?;
        if negative {
            v = -v - 1;
            // Range transformation not needed in simplified encoder
        }
    } else if lo < 0 {
        v = -v - 1;
        // Range transformation not needed in simplified encoder
    }

    // Phase 2 & 3: Binary encode the value
    let mut cutoff = 1;
    let mut ctx_idx = base_context + 1;

    // Find range
    while v >= cutoff {
        if ctx_idx < contexts.len() {
            zc.encode(true, &mut contexts[ctx_idx])?;
        }
        ctx_idx += 1;
        cutoff = cutoff * 2 + 1;
    }
    if ctx_idx < contexts.len() {
        zc.encode(false, &mut contexts[ctx_idx])?;
    }

    // Binary encode within range
    let prev_cutoff = (cutoff - 1) / 2;
    let mut range = prev_cutoff + 1;
    let mut target = cutoff - 1 - range / 2;

    while range > 1 {
        range /= 2;
        let decision = v >= target;
        ctx_idx += 1;
        if ctx_idx < contexts.len() {
            zc.encode(decision, &mut contexts[ctx_idx])?;
        }
        if decision {
            target += range / 2;
        } else {
            target -= range / 2;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_num_coder_basic() {
        let mut coder = NumCoder::new();
        let mut buffer = Vec::new();
        let mut zc = ZEncoder::new(&mut buffer, false).unwrap();

        // Test encoding a few values
        let mut ctx1 = coder.alloc_context();
        coder.code_num(&mut zc, &mut ctx1, 0, 10, 5).unwrap();

        let mut ctx2 = coder.alloc_context();
        coder.code_num(&mut zc, &mut ctx2, -10, 10, -3).unwrap();

        let mut ctx3 = coder.alloc_context();
        coder.code_num(&mut zc, &mut ctx3, 0, 262142, 1000).unwrap();

        zc.finish().unwrap();
        assert!(!buffer.is_empty());
    }

    #[test]
    fn test_reset() {
        let mut coder = NumCoder::new();
        let mut buffer = Vec::new();
        let mut zc = ZEncoder::new(&mut buffer, false).unwrap();

        let mut ctx = coder.alloc_context();
        coder.code_num(&mut zc, &mut ctx, 0, 100, 50).unwrap();

        let cells_before = coder.cur_ncell;
        coder.reset();

        assert_eq!(coder.cur_ncell, 1);
        assert!(cells_before > 1);
    }
}
