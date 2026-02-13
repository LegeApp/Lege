//! This module contains the main JBIG2 encoder logic.
use crate::jbig2arith::{IntProc, Jbig2ArithCoder};
use crate::jbig2comparator::Comparator;
// Symbol extraction using CC analysis
#[cfg(feature = "cc-analysis")]
use crate::jbig2cc::analyze_page;
use crate::jbig2structs::{
    FileHeader, GenericRegionConfig, GenericRegionParams, Jbig2Config, PageInfo, Segment,
    SegmentType, SymbolDictParams, TextRegionParams,
};

use crate::jbig2sym::{BitImage, Rect};
use anyhow::{anyhow, Result};

// Define debug and trace macros at the crate root
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {
        #[cfg(feature = "trace_encoder")]
        log::debug!($($arg)*);

        #[cfg(not(feature = "trace_encoder"))]
        let _ = format_args!($($arg)*);
    };
}

#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => {
        #[cfg(feature = "trace_encoder")]
        log::trace!($($arg)*);

        #[cfg(not(feature = "trace_encoder"))]
        let _ = format_args!($($arg)*);
    };
}

// Import the macros for use in this module
#[allow(unused_imports)]
use crate::{debug, trace};

use ndarray::Array2;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

/// A key type for hashing bitmaps efficiently
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HashKey(u64);

impl Hash for HashKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl std::fmt::Display for HashKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HashKey({:x})", self.0)
    }
}

/// A candidate symbol extracted from a document image.
#[derive(Debug, Clone)]
pub struct SymbolCandidate {
    /// The bitmap image of the symbol.
    pub bitmap: BitImage,
    /// The bounding box of the symbol in the original image.
    pub bbox: Rect,
}

/// Segment a document image into symbol candidates.
///
/// This function finds connected components in the input image and returns
/// them as symbol candidates. Each candidate has a bitmap and a bounding box.
///
/// # Arguments
/// * `image` - The input binary image to segment
/// * `dpi` - Resolution in dots per inch (typically 300 for scanned documents)
/// * `losslevel` - 0 for lossless, >0 to enable noise removal
pub fn segment_symbols(
    image: &BitImage,
    dpi: i32,
    losslevel: i32,
) -> Result<Vec<SymbolCandidate>> {
    #[cfg(feature = "cc-analysis")]
    {
        // Use the new CC analysis pipeline from jbig2cc
        let cc_image = analyze_page(image, dpi, losslevel);
        let shapes = cc_image.extract_shapes();
        
        let mut candidates = Vec::with_capacity(shapes.len());
        for (bitmap, bbox) in shapes {
            let rect = Rect {
                x: bbox.xmin as u32,
                y: bbox.ymin as u32,
                width: bbox.width() as u32,
                height: bbox.height() as u32,
            };
            candidates.push(SymbolCandidate { bitmap, bbox: rect });
        }
        Ok(candidates)
    }
    #[cfg(not(feature = "cc-analysis"))]
    {
        Err(anyhow!("Symbol segmentation requires cc-analysis feature"))
    }
}

// Jbig2EncConfig has been removed. Use jbig2structs::Jbig2Config directly.

#[derive(Clone)]
pub struct SymbolInstance {
    pub symbol_index: usize,
    pub position: Rect,
    pub instance_bitmap: BitImage,
}

impl SymbolInstance {
    pub fn symbol_index(&self) -> usize {
        self.symbol_index
    }

    pub fn position(&self) -> Rect {
        self.position
    }

    pub fn instance_bitmap(&self) -> &BitImage {
        &self.instance_bitmap
    }
}

#[derive(Clone)]
pub struct PageData {
    pub image: BitImage,
    pub symbol_instances: Vec<SymbolInstance>,
}

/// Mutable state for the encoder that can change during encoding.
#[derive(Debug, Default)]
struct EncoderState {
    pdf_mode: bool,
    full_headers_remaining: bool,
    segment: bool,
    use_refinement: bool,
    use_delta_encoding: bool,
}

/// Main JBIG2 encoder that handles document encoding
///
/// This struct manages the encoding state and configuration for JBIG2 documents.
/// It supports both symbol-based and generic region encoding strategies.
pub struct Jbig2Encoder<'a> {
    /// Configuration for the encoder
    config: &'a Jbig2Config,

    /// Internal encoder state
    state: EncoderState,

    /// Global symbols (shared across pages)
    global_symbols: Vec<BitImage>,

    /// Usage count for each global symbol
    symbol_usage: Vec<usize>,

    /// Set of page indices where each symbol appears
    symbol_pages: Vec<HashSet<usize>>,

    /// Hash map for quick symbol lookup
    hash_map: HashMap<HashKey, Vec<usize>>,

    /// Page data for each page in the document
    pages: Vec<PageData>,

    /// Next available segment number
    next_segment_number: u32,

    /// Segment number of the global dictionary (if any)
    global_dict_segment_number: Option<u32>,
}

impl<'a> Jbig2Encoder<'a> {
    /// Creates a new JBIG2 encoder with the specified configuration
    ///
    /// # Arguments
    /// * `config` - Configuration for the encoder
    pub fn new(config: &'a Jbig2Config) -> Self {
        if config.refine && !config.symbol_mode {
            panic!("Refinement requires symbol mode to be enabled.");
        }

        Self {
            config,
            state: EncoderState {
                pdf_mode: false, // start in raw mode
                full_headers_remaining: config.want_full_headers,
                segment: true,                 // Default to using segments
                use_refinement: config.refine, // Enable refinement based on config
                use_delta_encoding: true,      // Default to using delta encoding
            },
            global_symbols: Vec::new(),
            symbol_usage: Vec::new(),
            symbol_pages: Vec::new(),
            hash_map: HashMap::new(),
            pages: Vec::new(),
            next_segment_number: 0,
            global_dict_segment_number: None,
        }
    }

    pub fn dict_only(mut self) -> Self {
        self.state.full_headers_remaining = false;
        self.state.pdf_mode = true;
        self
    }

    /// Returns the number of pages currently added to the encoder
    pub fn get_page_count(&self) -> usize {
        self.pages.len()
    }

    /// Returns debug information about symbol usage
    pub fn get_symbol_stats(&self) -> String {
        let total_symbols = self.global_symbols.len();
        let avg_usage = if total_symbols > 0 {
            self.symbol_usage.iter().sum::<usize>() as f32 / total_symbols as f32
        } else {
            0.0
        };
        let low_usage_count = self.symbol_usage.iter().filter(|&&u| u < 2).count();

        format!(
            "Total symbols: {}, Average usage: {:.1}, Low usage (<2): {}",
            total_symbols, avg_usage, low_usage_count
        )
    }

    pub fn add_page(&mut self, image: &Array2<u8>) -> Result<()> {
        let bitimage = crate::jbig2sym::array_to_bitimage(image);
        let page_num = self.pages.len();
        let mut symbol_instances = Vec::new();
        let mut comparator = Comparator::default();

        // Extract symbols if symbol mode is enabled
        if self.config.symbol_mode && self.state.segment {
            #[cfg(feature = "cc-analysis")]
            let extracted = {
                let dpi = 300; // Default DPI
                let losslevel = if self.config.is_lossless { 0 } else { 1 };
                let cc_image = analyze_page(&bitimage, dpi, losslevel);
                let shapes = cc_image.extract_shapes();
                // Convert to (Rect, BitImage) format
                shapes.into_iter().map(|(bitmap, bbox)| {
                    let rect = Rect {
                        x: bbox.xmin as u32,
                        y: bbox.ymin as u32,
                        width: bbox.width() as u32,
                        height: bbox.height() as u32,
                    };
                    (rect, bitmap)
                }).collect::<Vec<_>>()
            };
            #[cfg(not(feature = "cc-analysis"))]
            let extracted: Vec<(Rect, BitImage)> = Vec::new();

            // Check if symbol extraction makes sense for this image
            // If we only get one symbol that covers the entire image,
            // it's better to use generic region encoding
            let should_use_symbols = if extracted.len() == 1 {
                let (rect, _) = &extracted[0];
                // Check if the single symbol is essentially the entire image
                !(rect.x == 0
                    && rect.y == 0
                    && rect.width as usize >= bitimage.width - 2
                    && rect.height as usize >= bitimage.height - 2)
            } else {
                extracted.len() > 0
            };

            if should_use_symbols {
                for (rect, symbol) in extracted {
                    let (_, trimmed) = symbol.trim();
                    let key = hash_key(&trimmed);
                    let mut matched = false;
                    if let Some(bucket) = self.hash_map.get(&key) {
                        for &idx in bucket {
                            // Allow more differences between symbols (up to 10% of pixels) for better deduplication
                            let max_err = ((trimmed.width * trimmed.height) / 10).max(3) as u32;
                            if let Some((err, dx, dy)) =
                                comparator.distance(&trimmed, &self.global_symbols[idx], max_err)
                            {
                                self.symbol_usage[idx] += 1;
                                self.symbol_pages[idx].insert(page_num);
                                symbol_instances.push(SymbolInstance {
                                    symbol_index: idx,
                                    position: rect,
                                    instance_bitmap: symbol.clone(),
                                });
                                matched = true;
                                break;
                            }
                        }
                    }
                    if !matched {
                        let idx = self.global_symbols.len();
                        self.global_symbols.push(trimmed.clone());
                        self.symbol_usage.push(1);
                        self.symbol_pages.push([page_num].into_iter().collect());
                        self.hash_map.entry(key).or_default().push(idx);
                        symbol_instances.push(SymbolInstance {
                            symbol_index: idx,
                            position: rect,
                            instance_bitmap: symbol.clone(),
                        });
                    }
                }
            }
        }

        self.pages.push(PageData {
            image: bitimage,
            symbol_instances,
        });
        Ok(())
    }

    pub fn collect_symbols(&mut self, roi: &Array2<u8>) -> Result<()> {
        let bitimage = crate::jbig2sym::array_to_bitimage(roi);
        let (_, trimmed) = bitimage.trim();
        let key = hash_key(&trimmed);
        let page_num = self.pages.len();

        if !self.hash_map.contains_key(&key) {
            let idx = self.global_symbols.len();
            self.global_symbols.push(trimmed);
            self.symbol_usage.push(1);
            self.symbol_pages.push([page_num].into_iter().collect());
            self.hash_map.insert(key, vec![idx]);
        }
        Ok(())
    }

    pub fn flush(&mut self) -> Result<Vec<u8>> {
        // Log symbol statistics for debugging
        debug!("Symbol stats before encoding: {}", self.get_symbol_stats());

        let mut current_segment_number = self.next_segment_number;
        if self.config.auto_thresh {
            if self.config.hash {
                self.auto_threshold_using_hash()?;
            } else {
                self.auto_threshold()?;
            }
        }

        let mut output = Vec::new();

        if self.state.full_headers_remaining {
            let header = FileHeader {
                organisation_type: true,
                unknown_n_pages: false,
                n_pages: self.pages.len() as u32,
            };
            output.extend(header.to_bytes());
            // only write the header once
            self.state.full_headers_remaining = false;
        }

        // Prune low-usage symbols if we have too many (frequency-based pruning)
        if self.global_symbols.len() > 300 {
            let mut symbol_indices_with_usage: Vec<(usize, usize)> = self
                .symbol_usage
                .iter()
                .enumerate()
                .map(|(i, &usage)| (i, usage))
                .collect();

            // Sort by usage count (descending), then by size (descending)
            symbol_indices_with_usage.sort_by(|a, b| {
                b.1.cmp(&a.1).then_with(|| {
                    let size_a = self.global_symbols[a.0].count_ones();
                    let size_b = self.global_symbols[b.0].count_ones();
                    size_b.cmp(&size_a)
                })
            });

            // Keep only the top 300 most used symbols
            let kept_indices: HashSet<usize> = symbol_indices_with_usage
                .into_iter()
                .take(300)
                .map(|(i, _)| i)
                .collect();

            // Remove pruned symbols from pages
            for page in &mut self.pages {
                page.symbol_instances
                    .retain(|inst| kept_indices.contains(&inst.symbol_index));
            }

            debug!(
                "Pruned symbols from {} to 300 based on usage frequency",
                self.global_symbols.len()
            );
        }

        let global_symbol_indices: Vec<usize> = self
            .global_symbols
            .iter()
            .enumerate()
            .filter(|(i, _)| self.symbol_usage[*i] > 1 || self.pages.len() == 1)
            .map(|(i, _)| i)
            .collect();

        let page_local_symbols: Vec<Vec<usize>> = self
            .pages
            .iter()
            .enumerate()
            .map(|(page_num, _)| {
                self.global_symbols
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| {
                        self.symbol_usage[*i] == 1 && self.symbol_pages[*i].contains(&page_num)
                    })
                    .map(|(i, _)| i)
                    .collect()
            })
            .collect();

        // Encode Global Symbol Dictionary (if not empty)
        if !global_symbol_indices.is_empty() {
            let refs: Vec<&BitImage> = global_symbol_indices
                .iter()
                .map(|&i| &self.global_symbols[i])
                .collect();
            let global_dict_payload = encode_symbol_dict(&refs, &self.config, 0)?;
            let global_dict_segment = Segment {
                number: current_segment_number,
                seg_type: SegmentType::SymbolDictionary,
                deferred_non_retain: false,
                retain_flags: 0,          // Default retention
                page_association_type: 2, // Global (all pages)
                referred_to: Vec::new(),
                page: None, // Global dictionary
                payload: global_dict_payload,
            };

            global_dict_segment.write_into(&mut output)?;
            self.global_dict_segment_number = Some(global_dict_segment.number);
            current_segment_number += 1;
        }

        for (page_num, page) in self.pages.iter().enumerate() {
            // 2. Page-information segment (segment #0)
            let page_info_payload = PageInfo {
                width: page.image.width as u32,
                height: page.image.height as u32,
                default_pixel: false, // white background (safer default)
                xres: self.config.generic.dpi,
                yres: self.config.generic.dpi,
                ..Default::default()
            }
            .to_bytes();

            Segment {
                number: current_segment_number,
                seg_type: SegmentType::PageInformation, // 0x30
                deferred_non_retain: false,
                retain_flags: 0,
                page_association_type: 0, // explicit
                referred_to: vec![],
                page: Some(page_num as u32 + 1),
                payload: page_info_payload,
            }
            .write_into(&mut output)?;
            current_segment_number += 1;

            if self.config.symbol_mode && !page.symbol_instances.is_empty() {
                // SYMBOL MODE: Encode Local Symbol Dictionary and Text Region
                let local_symbols = &page_local_symbols[page_num];
                let mut referred_to_for_text_region = Vec::new();
                if let Some(global_dict_seg_num) = self.global_dict_segment_number {
                    referred_to_for_text_region.push(global_dict_seg_num);
                }

                if !local_symbols.is_empty() {
                    let local_dict_payload = {
                        let refs: Vec<&BitImage> = local_symbols
                            .iter()
                            .map(|&i| &self.global_symbols[i])
                            .collect();
                        let num_global_symbols_for_local_dict = global_symbol_indices.len() as u32;
                        encode_symbol_dict(&refs, &self.config, num_global_symbols_for_local_dict)?
                    };
                    let local_dict_segment = Segment {
                        number: current_segment_number,
                        seg_type: SegmentType::SymbolDictionary,
                        deferred_non_retain: false,
                        retain_flags: 0,
                        page_association_type: 0, // Explicit page association
                        referred_to: Vec::new(),
                        page: Some(page_num as u32 + 1),
                        payload: local_dict_payload,
                    };

                    local_dict_segment.write_into(&mut output)?;
                    current_segment_number += 1;
                    referred_to_for_text_region.push(local_dict_segment.number);
                }

                let region_payload = if self.config.refine {
                    Vec::new()
                } else {
                    // Convert Vec<BitImage> to Vec<&BitImage> and then to a slice
                    let global_symbol_refs: Vec<&BitImage> = self.global_symbols.iter().collect();
                    encode_text_region(
                        &page.symbol_instances,
                        &self.config,
                        &global_symbol_refs,
                        &global_symbol_indices,
                        &page_local_symbols[page_num],
                    )?
                };

                let text_region_segment = Segment {
                    number: current_segment_number,
                    seg_type: SegmentType::ImmediateTextRegion,
                    deferred_non_retain: false,
                    retain_flags: 0,
                    page_association_type: 0, // Explicit page association
                    referred_to: referred_to_for_text_region,
                    page: Some(page_num as u32 + 1),
                    payload: region_payload,
                };
                text_region_segment.write_into(&mut output)?;
                current_segment_number += 1;
            } else {
                // NON-SYMBOL MODE or NO SYMBOLS FOUND: Use generic region encoding
                // Build generic region config
                let mut gr_cfg = GenericRegionConfig::new(
                    page.image.width as u32,
                    page.image.height as u32,
                    self.config.generic.dpi,
                );
                gr_cfg.comb_operator = self.config.generic.comb_operator;
                gr_cfg.mmr = self.config.generic.mmr;
                gr_cfg.tpgdon = self.config.generic.tpgdon;
                gr_cfg.validate().map_err(|e: &'static str| anyhow!(e))?;

                let coder_data = Jbig2ArithCoder::encode_generic_payload_cfg(&page.image, &gr_cfg)?;

                let params: GenericRegionParams = gr_cfg.clone().into();

                let mut generic_region_payload = params.to_bytes();
                generic_region_payload.extend_from_slice(&coder_data);

                let generic_region_segment = Segment {
                    number: current_segment_number,
                    seg_type: SegmentType::ImmediateGenericRegion,
                    deferred_non_retain: false,
                    retain_flags: 0,
                    page_association_type: 0,
                    referred_to: Vec::new(),
                    page: Some(page_num as u32 + 1),
                    payload: generic_region_payload,
                };
                generic_region_segment.write_into(&mut output)?;
                current_segment_number += 1;
            }

            let end_page_segment = Segment {
                number: current_segment_number,
                seg_type: SegmentType::EndOfPage,
                deferred_non_retain: false,
                retain_flags: 0,
                page_association_type: 0, // Explicit page association
                referred_to: Vec::new(),
                page: Some(page_num as u32 + 1),
                payload: Vec::new(),
            };
            end_page_segment.write_into(&mut output)?;
            current_segment_number += 1;
        }

        // 4. End-of-file segment
        Segment {
            number: current_segment_number,
            seg_type: SegmentType::EndOfFile, // 51 / 0x33
            deferred_non_retain: false,
            retain_flags: 0,
            page_association_type: 2, // “all pages”
            referred_to: vec![],
            page: None,
            payload: vec![],
        }
        .write_into(&mut output)?;
        current_segment_number += 1;

        self.next_segment_number = current_segment_number;
        Ok(output)
    }

    fn auto_threshold(&mut self) -> Result<()> {
        let mut i = 0;
        while i < self.global_symbols.len() {
            let mut j = i + 1;
            while j < self.global_symbols.len() {
                let mut comparator = Comparator::default();
                if comparator
                    .distance(&self.global_symbols[i], &self.global_symbols[j], 0)
                    .is_some()
                {
                    self.unite_templates(i, j)?;
                } else {
                    j += 1;
                }
            }
            i += 1;
        }
        Ok(())
    }

    fn auto_threshold_using_hash(&mut self) -> Result<()> {
    let mut hashed_templates: HashMap<u32, Vec<usize>> = HashMap::new();
        for (i, symbol) in self.global_symbols.iter().enumerate() {
            let hash = compute_symbol_hash(symbol);
            hashed_templates.entry(hash).or_default().push(i);
        }

        for (_, bucket) in hashed_templates {
            let mut indices: Vec<usize> = bucket;
            let mut i = 0;
            while i < indices.len() {
                let mut j = i + 1;
                while j < indices.len() {
                    let mut comparator = Comparator::default();
                    if comparator
                        .distance(
                            &self.global_symbols[indices[i]],
                            &self.global_symbols[indices[j]],
                            0,
                        )
                        .is_some()
                    {
                        self.unite_templates(indices[i], indices[j])?;
                        indices.remove(j);
                    } else {
                        j += 1;
                    }
                }
                i += 1;
            }
        }
        Ok(())
    }

    fn unite_templates(&mut self, target_idx: usize, source_idx: usize) -> Result<()> {
        if source_idx >= self.global_symbols.len() {
            anyhow::bail!("Source index out of range");
        }

        for page in &mut self.pages {
            for instance in &mut page.symbol_instances {
                if instance.symbol_index == source_idx {
                    instance.symbol_index = target_idx;
                } else if instance.symbol_index > source_idx {
                    instance.symbol_index -= 1;
                }
            }
        }

        self.symbol_usage[target_idx] += self.symbol_usage[source_idx];
        if target_idx != source_idx {
            // split_at_mut guarantees non-overlap
            let (left, right) = self.symbol_pages.split_at_mut(source_idx.max(target_idx));
            let (dst, src) = if target_idx < source_idx {
                (&mut left[target_idx], &right[0])
            } else {
                (&mut right[target_idx - source_idx - 1], &left[source_idx])
            };
            dst.extend(src);
        }
        self.global_symbols.remove(source_idx);
        self.symbol_usage.remove(source_idx);
        self.symbol_pages.remove(source_idx);

        self.hash_map.clear();
        for (idx, symbol) in self.global_symbols.iter().enumerate() {
            let key = hash_key(symbol);
            self.hash_map.entry(key).or_default().push(idx);
        }

        Ok(())
    }

    pub fn next_segment_number(&mut self) -> u32 {
        let num = self.next_segment_number;
        self.next_segment_number += 1;
        num
    }

    pub fn flush_dict(&mut self) -> Result<Vec<u8>> {
        if self.global_symbols.is_empty() {
            return Ok(Vec::new());
        }

        let symbol_refs: Vec<&BitImage> = self.global_symbols.iter().collect();
        let dict_data = encode_symbol_dict(&symbol_refs, &self.config, 0)?;

        if self.state.pdf_mode {
            return Ok(dict_data);
        }

        let mut output = Vec::new();
        let header = FileHeader {
            organisation_type: true,
            unknown_n_pages: false,
            n_pages: 1,
        };
        output.extend(header.to_bytes());
        output.extend(dict_data);

        Ok(output)
    }
}

/// Encodes a generic region, optionally wrapping it in a complete JBIG2 file.
/// This function is intended to be the top-level entry point for encoding a single generic region.
pub fn encode_generic_region(img: &BitImage, cfg: &Jbig2Config) -> Result<Vec<u8>> {
    // Build generic region config from high-level parameters
    let mut gr_cfg = GenericRegionParams::new(img.width as u32, img.height as u32, cfg.generic.dpi);
    gr_cfg.comb_operator = cfg.generic.comb_operator;
    gr_cfg.mmr = cfg.generic.mmr;
    gr_cfg.tpgdon = cfg.generic.tpgdon;
    gr_cfg.validate().map_err(|e: &'static str| anyhow!(e))?;

    let coder_data =
        Jbig2ArithCoder::encode_generic_payload(img, gr_cfg.template, &gr_cfg.at_pixels)?;

    let params: GenericRegionParams = gr_cfg.clone();

    let mut generic_region_payload = params.to_bytes();
    generic_region_payload.extend_from_slice(&coder_data);

    // Create the generic region segment (segment number 1)
    let generic_region_segment = Segment {
        number: 1, // Segment number 1
        seg_type: SegmentType::ImmediateGenericRegion,
        deferred_non_retain: false,
        retain_flags: 0,
        page_association_type: 0, // Explicit page association
        referred_to: Vec::new(),
        page: Some(1),                           // Page 1
        payload: generic_region_payload.clone(), // Clone to avoid move
    };

    // If caller wants only the segment, we're done
    if !cfg.want_full_headers {
        let mut seg_bytes = Vec::new();
        generic_region_segment.write_into(&mut seg_bytes)?;
        return Ok(seg_bytes);
    }

    // Otherwise wrap it in a complete one-page JBIG2 file
    let mut out = Vec::with_capacity(generic_region_payload.len() + 64);

    // File header
    out.extend_from_slice(
        &FileHeader {
            organisation_type: true,
            unknown_n_pages: false,
            n_pages: 1,
        }
        .to_bytes(),
    );

    // Page Information segment (segment number 0)
    Segment {
        number: 0,
        seg_type: SegmentType::PageInformation,
        deferred_non_retain: false,
        retain_flags: 0,
        page_association_type: 0,
        referred_to: vec![],
        page: Some(1),
        payload: PageInfo {
            width: img.width as u32,
            height: img.height as u32,
            xres: cfg.generic.dpi,
            yres: cfg.generic.dpi,
            is_lossless: cfg.is_lossless,
            default_pixel: cfg.default_pixel,
            default_operator: cfg.generic.comb_operator,
            ..Default::default()
        }
        .to_bytes(),
    }
    .write_into(&mut out)?;

    // Generic region segment (segment number 1)
    generic_region_segment.write_into(&mut out)?;

    // EOF segment (segment number 2)
    Segment {
        number: 2,
        seg_type: SegmentType::EndOfFile,
        deferred_non_retain: false,
        retain_flags: 0,
        page_association_type: 2,
        referred_to: vec![],
        page: None,
        payload: vec![],
    }
    .write_into(&mut out)?;

    Ok(out)
}

pub fn encode_symbol_dict(
    symbols: &[&BitImage],
    _config: &Jbig2Config,
    num_imported_symbols: u32,
) -> Result<Vec<u8>> {
    // Filter out symbols with zero width or height to prevent encoding errors
    let valid_symbols: Vec<&BitImage> = symbols
        .iter()
        .filter(|sym| sym.width > 0 && sym.height > 0)
        .copied()
        .collect();

    // Validate input symbols
    if valid_symbols.is_empty() {
        return Err(anyhow!(
            "encode_symbol_dict: no valid symbols supplied (all symbols had zero width or height)"
        ));
    }

    // Deduplicate symbols by content hash
    let mut seen_hashes: HashSet<HashKey> = HashSet::new();
    let mut unique_symbols_list: Vec<&BitImage> = Vec::with_capacity(valid_symbols.len());
    for sym in &valid_symbols {
        if seen_hashes.insert(hash_key(sym)) {
            unique_symbols_list.push(sym);
        }
    }
    let symbols = unique_symbols_list; // Shadow the original slice with the deduplicated vec

    // Debug output for the symbols being encoded (disabled in release)
    #[cfg(debug_assertions)]
    {
        debug!("Encoding {} symbols", symbols.len());
        for (i, sym) in symbols.iter().enumerate() {
            debug!("Symbol {}: {}x{}", i, sym.width, sym.height);
            if i >= 10 {
                break;
            }
        }
    }

    // Verify symbol dimensions are within JBIG2 limits
    for (i, sym) in symbols.iter().enumerate() {
        if sym.width > (1 << 24) || sym.height > (1 << 24) {
            return Err(anyhow!(
                "Symbol at index {} exceeds maximum dimensions ({}x{})",
                i,
                sym.width,
                sym.height
            ));
        }
    }

    let mut payload = Vec::new();
    let mut coder = Jbig2ArithCoder::new();

    // Create symbol dictionary parameters
    let mut params = SymbolDictParams {
        // Made params mutable
        sd_template: 0,                       // Use standard template 0
        at: [(0, 0), (0, 0), (0, 0), (0, 0)], // Default AT pixels
        exsyms: num_imported_symbols,         // Number of exported symbols
        newsyms: symbols.len() as u32,        // Number of new symbols
    };

    // Set number of exported symbols to match number of new symbols (export all)
    let num_export_syms = symbols.len() as u32;
    params.exsyms = num_export_syms;

    if cfg!(debug_assertions) {
        debug!("encode_symbol_dict: Exporting {} symbols", num_export_syms);
        trace!("encode_symbol_dict: SymbolDictParams details: {:?}", params);
    }

    // Write the symbol dictionary parameters
    payload.extend(params.to_bytes());

    // Encode the export flags using IAID arithmetic integer procedure
    let k = (32 - num_export_syms.leading_zeros()).max(1) as u8;

    // Run of exported symbols from the imported dictionary (length is 0)
    let _ = coder.encode_int_with_ctx(0, k as i32, IntProc::Iaex);

    // Run of exported symbols from the new symbols in this dict (length is all of them)
    let _ = coder.encode_int_with_ctx(num_export_syms as i32, k as i32, IntProc::Iaex);

    // No terminating IAID(0) as per JBIG2 specification §7.4.3.1.7

    // No flush/align between export flags and symbol data - we need a continuous stream

    // 2. Group symbols by height using jbig2sym's utility function
    let height_classes = crate::jbig2sym::sort_symbols_for_dictionary(&symbols);

    let mut last_height = 0;

    // 4. Encode the height classes
    for symbols_in_class in &height_classes {
        let h = symbols_in_class[0].height; // All symbols in class have same height
                                            // A. Encode Delta Height
        let delta_h = h as i32 - last_height as i32;
        let _ = coder.encode_integer(crate::jbig2arith::IntProc::Iadh, delta_h);
        last_height = h;

        let mut last_width = 0;

        // Debug: check symbols in this height class (disabled in release)
        #[cfg(debug_assertions)]
        {
            debug!("Height class {} has {} symbols:", h, symbols_in_class.len());
            for (i, symbol) in symbols_in_class.iter().enumerate() {
                debug!("  Symbol {}: {}x{}", i, symbol.width, symbol.height);
            }
        }

        // B. Encode symbols within this height class
        for (i, symbol) in symbols_in_class.iter().enumerate() {
            // I. Encode Delta Width
            let delta_w = symbol.width as i32 - last_width;

            // Debug output to help diagnose the issue (disabled in release)
            #[cfg(debug_assertions)]
            debug!(
                "Height class {}, Symbol {}: width={}, last_width={}, delta_w={}",
                h, i, symbol.width, last_width, delta_w
            );

            let _ = coder.encode_integer(crate::jbig2arith::IntProc::Iadw, delta_w); // Assuming IntProc is in jbig2arith
            last_width += delta_w; // last_width becomes current width

            // II. Encode Symbol Bitmap using Generic Region Procedure
            let packed = symbol.to_packed_words();

            // Verify bit-order correctness: first black pixel should match between symbol and packed data
            if let Some(expected_first_pixel) = first_black_pixel(symbol) {
                let actual_first_pixel = crate::jbig2sym::first_black_pixel_in_packed(
                    &packed,
                    symbol.width,
                    symbol.height,
                );
                assert_eq!(
                    actual_first_pixel,
                    Some(expected_first_pixel),
                    "bit-order / row-order mismatch in symbol dict packer! Expected first black pixel at {:?}, got {:?}",
                    expected_first_pixel,
                    actual_first_pixel
                );
            }

            coder.encode_generic_region(
                &packed,
                symbol.width,
                symbol.height,
                params.sd_template,
                &[(-1, -1), (3, -1), (-3, -1), (-2, -2)],
            )?;
        }

        // Encode OOB (Out-Of-Band) value for IADW after all symbols in this height class
        // This signals to the decoder that there are no more symbols in this height class
        let _ = coder.encode_oob(crate::jbig2arith::IntProc::Iadw)?;
    }

    // 5. flush the coder ONCE
    coder.flush(true);

    // 6. Append the single, complete arithmetic payload
    payload.extend(coder.as_bytes());

    Ok(payload)
}

/// Computes the bounding box that contains all symbol instances.
///
/// # Arguments
/// * `instances` - Slice of symbol instances to compute bounds for
/// * `all_known_symbols` - All available symbol bitmaps
///
/// # Returns
/// A tuple of (min_x, min_y, width, height) representing the bounding box
fn compute_region_bounds(
    instances: &[TextRegionSymbolInstance],
    all_known_symbols: &[&BitImage],
) -> (u32, u32, u32, u32) {
    if instances.is_empty() {
        return (0, 0, 0, 0);
    }
    let mut min_x = u32::MAX;
    let mut min_y = u32::MAX;
    let mut max_x_coord = 0u32;
    let mut max_y_coord = 0u32;

    for instance in instances {
        let sym_idx = instance.symbol_id as usize;
        if sym_idx >= all_known_symbols.len() {
            continue; // Skip invalid symbol indices
        }

        let pos = Rect {
            x: instance.x as u32, // Convert i32 to u32
            y: instance.y as u32, // Convert i32 to u32
            width: crate::jbig2shared::usize_to_u32(all_known_symbols[sym_idx].width),
            height: crate::jbig2shared::usize_to_u32(all_known_symbols[sym_idx].height),
        };

        min_x = min_x.min(pos.x);
        min_y = min_y.min(pos.y);
        max_x_coord = max_x_coord.max(pos.x + pos.width);
        max_y_coord = max_y_coord.max(pos.y + pos.height);
    }

    // Handle potential underflow if max < min (shouldn't happen with valid coordinates)
    let region_width = if max_x_coord > min_x {
        max_x_coord - min_x
    } else {
        0
    };

    let region_height = if max_y_coord > min_y {
        max_y_coord - min_y
    } else {
        0
    };

    (min_x, min_y, region_width, region_height)
}

pub fn encode_refine(
    instances: &[TextRegionSymbolInstance],
    all_known_symbols: &[&BitImage],
    data: &mut Vec<u8>,
    coder: &mut Jbig2ArithCoder,
) -> Result<()> {
    // 1. Compute region bounds
    let (min_x, min_y, region_w, region_h) = compute_region_bounds(instances, all_known_symbols);
    let width = region_w.max(1);
    let height = region_h.max(1);

    // 2. Write TextRegion header (flags + params)
    // flags: TRREF=1, others zero (arithmetic coding)
    let mut flags: u8 = 0;
    flags |= 0x40; // TRREF bit
    data.push(flags);

    let params = TextRegionParams {
        width,
        height,
        x: min_x,
        y: min_y,
        ds_offset: 0,
        refine: true,
        log_strips: 0,
        ref_corner: 0,
        transposed: false,
        comb_op: 0,
        refine_template: 0,
    };
    data.extend(params.to_bytes());

    // 3. Encode number of instances
    let num_inst = instances.len() as u32;
    let _ = coder.encode_int_with_ctx(num_inst as i32, 16, IntProc::Iaai);

    // Calculate SBSYMCODELEN - the number of bits needed to encode symbol IDs
    let num_symbols = all_known_symbols.len() as u32;
    let symbol_id_bits = log2up(num_symbols.max(1)).max(1);

    // 4. Initialize an empty region buffer to track already emitted pixels
    let mut region_buf = BitImage::new(width, height).expect("region bitmap too large");

    // 5. Emit each instance
    for inst in instances {
        // Encode symbol ID using IAID procedure (per JBIG2 spec §6.4.4)
        let sym_id = inst.symbol_id;
        let _ = coder.encode_iaid(sym_id, symbol_id_bits);

        // Refinement deltas
        let _ = coder.encode_integer(IntProc::Iardx, inst.dx);
        let _ = coder.encode_integer(IntProc::Iardy, inst.dy);

        // If this is a refinement instance, encode pixel-by-pixel
        if inst.is_refinement {
            // locate the symbol bitmap
            if let Some(&sym) = all_known_symbols.get(sym_id as usize) {
                // offset of this instance in region coords
                let ox = inst.x as u32 - min_x;
                let oy = inst.y as u32 - min_y;

                // for each pixel in the symbol region
                for y in 0..sym.height as u32 {
                    for x in 0..sym.width as u32 {
                        // compute region coord
                        let rx = ox + x;
                        let ry = oy + y;

                        // skip out-of-bounds
                        if rx >= width || ry >= height {
                            continue;
                        }

                        // the reference pixel is from the symbol dict image
                        let ref_bit = sym.get(x, y) as u8;
                        // the predicted/context pixel is from region_buf
                        let pred_bit = region_buf.get(rx, ry) as u8;

                        // Context = combine ref_bit, pred_bit, template (here simple sum)
                        let ctx = ((ref_bit << 1) | pred_bit) as usize;

                        // Encode the actual pixel: 1 if sym has pixel, 0 otherwise
                        let bit = ref_bit;
                        coder.encode_bit(ctx, bit != 0);

                        // Update region buffer so subsequent instances see it
                        if bit != 0 {
                            region_buf.set(rx, ry, true);
                        }
                    }
                }
            }
        }
    }

    // 6. flush and append coder payload
    coder.flush(true);
    data.extend(coder.as_bytes());

    Ok(())
}

/// Encodes a text region segment to the output.
///
/// This function takes a list of symbols and their instances in the text region,
/// and encodes them according to JBIG2 spec §6.4.10. It supports both absolute coordinates
/// and IADW/IADH delta encoding for more efficient compression.
pub fn encode_text_region(
    instances: &[SymbolInstance],
    _config: &Jbig2Config,
    all_known_symbols: &[&BitImage],
    global_dict_indices: &[usize],
    local_dict_indices: &[usize],
) -> Result<Vec<u8>> {
    // Validate instances
    if instances.is_empty() {
        return Err(anyhow!("No symbol instances provided for text region"));
    }

    // Validate global dictionary indices
    if global_dict_indices
        .iter()
        .any(|&idx| idx >= all_known_symbols.len())
    {
        return Err(anyhow!("Invalid global dictionary index in text region"));
    }

    // Validate local dictionary indices if provided
    if !local_dict_indices.is_empty() {
        if local_dict_indices
            .iter()
            .any(|&idx| idx >= all_known_symbols.len())
        {
            return Err(anyhow!("Invalid local dictionary index in text region"));
        }
    }

    // Validate each instance
    for (i, instance) in instances.iter().enumerate() {
        if instance.symbol_index >= all_known_symbols.len() {
            return Err(anyhow!(
                "Symbol instance {} references invalid symbol index {} (max {})",
                i,
                instance.symbol_index,
                all_known_symbols.len() - 1
            ));
        }

        let symbol = &all_known_symbols[instance.symbol_index];
        if instance.position.x as u64 + symbol.width as u64 > u32::MAX as u64
            || instance.position.y as u64 + symbol.height as u64 > u32::MAX as u64
        {
            return Err(anyhow!(
                "Symbol instance {} at position ({}, {}) would overflow 32-bit coordinates",
                i,
                instance.position.x,
                instance.position.y
            ));
        }
    }
    let mut payload = Vec::new();
    let mut coder = Jbig2ArithCoder::new();

    let mut min_x = u32::MAX;
    let mut min_y = u32::MAX;
    let mut max_x_coord = 0;
    let mut max_y_coord = 0;

    if instances.is_empty() {
        min_x = 0;
        min_y = 0;
    } else {
        for instance in instances {
            let pos = instance.position();
            let sym_idx_in_all_known_list = instance.symbol_index();
            let symbol_width = all_known_symbols[sym_idx_in_all_known_list].width as i32;
            let symbol_height = all_known_symbols[sym_idx_in_all_known_list].height as i32;

            min_x = min_x.min(pos.x as u32);
            min_y = min_y.min(pos.y as u32);
            max_x_coord = max_x_coord.max((pos.x as i32 + symbol_width) as u32);
            max_y_coord = max_y_coord.max((pos.y as i32 + symbol_height) as u32);
        }
    }

    let region_width = if max_x_coord > min_x {
        max_x_coord - min_x
    } else {
        0
    };
    let region_height = if max_y_coord > min_y {
        max_y_coord - min_y
    } else {
        0
    };

    let params = TextRegionParams {
        width: region_width,
        height: region_height,
        x: min_x,
        y: min_y,
        ds_offset: 0,
        refine: false,
        log_strips: 0,
        ref_corner: 0,
        transposed: false,
        comb_op: 0,
        refine_template: 0,
    };
    if cfg!(debug_assertions) {
        trace!("encode_text_region: TextRegionParams details: {:?}", params);
    }
    // Write Text Region parameters (width/height/x/y + SBRFLAGS [+ SBRTEMPLATE if needed])
    // Note: No extra leading flags byte should be written here; SBRFLAGS encodes all required bits.
    payload.extend(params.to_bytes());

    // Encode the number of instances using IAID
    let num_instances = instances.len() as u32;
    let _ = coder.encode_int_with_ctx(num_instances as i32, 16, IntProc::Iaai); // Using 16 bits for instance count

    // Initialize variables for text region encoding as per JBIG2 spec (single strip model)
    let mut current_t_strip = 0; // T-coordinate of the current strip, initialized to 0 (spec 6.4.5.1)
    let mut last_s_coord = 0; // Last S-coordinate for delta encoding
    let mut is_first_instance_in_strip = true;

    // These are from the original code and needed for symbol ID encoding
    let num_total_dict_symbols = (global_dict_indices.len() + local_dict_indices.len()) as u32;
    let symbol_id_bits = log2up(num_total_dict_symbols.max(1)).max(1);

    for instance in instances.iter() {
        // Iterate through instances
        let sym_idx_in_all_known_list = instance.symbol_index();
        let symbol_props = &all_known_symbols[sym_idx_in_all_known_list];

        // Ensure instance_abs_pos has correct width and height
        let mut instance_abs_pos = instance.position();
        instance_abs_pos.width = symbol_props.width as u32;
        instance_abs_pos.height = symbol_props.height as u32;

        // Determine the symbol ID to encode (logic from original, confirmed correct by plan)
        let symbol_id_to_encode = if let Some(pos_global) = global_dict_indices
            .iter()
            .position(|&idx| idx == sym_idx_in_all_known_list)
        {
            pos_global as u32
        } else if let Some(pos_local) = local_dict_indices
            .iter()
            .position(|&idx| idx == sym_idx_in_all_known_list)
        {
            (global_dict_indices.len() + pos_local) as u32
        } else {
            anyhow::bail!(
                "Symbol instance (index {}) not found in referred dictionaries!",
                sym_idx_in_all_known_list
            );
        };

        debug!(
            "encode_text_region: sym_idx={}, id_to_encode={}, abs_pos={:?}, current_t_strip={}, last_s_coord={}",
            instance.symbol_index(), symbol_id_to_encode, instance_abs_pos, current_t_strip, last_s_coord
        );

        // Encode T-coordinate for the strip (IADT) - only for the first instance in a new strip
        // Assuming a single strip for now as per plan.
        if is_first_instance_in_strip {
            let delta_t = instance_abs_pos.y as i32 - current_t_strip; // Initial current_t_strip is 0
            let _ = coder.encode_integer(IntProc::Iadt, delta_t);
            current_t_strip += delta_t; // current_t_strip is now the T-coordinate of this strip (TCUR)
        }

        // Encode T-offset (CURT) for this instance relative to the strip's T-coordinate (IAIT)
        let t_offset = instance_abs_pos.y as i32 - current_t_strip;
        let _ = coder.encode_integer(IntProc::Iait, t_offset);

        // Encode S-coordinate
        if is_first_instance_in_strip {
            // First S coordinate in the strip is encoded with IAFS (absolute value)
            let _ = coder.encode_integer(IntProc::Iafs, instance_abs_pos.x as i32);
            is_first_instance_in_strip = false; // Subsequent instances in this strip will use IADS
        } else {
            // Subsequent S coordinates are delta-encoded with IADS
            let delta_s = instance_abs_pos.x as i32 - last_s_coord;
            let _ = coder.encode_integer(IntProc::Iads, delta_s);
        }

        // Always update last_s_coord for the next delta calculation
        last_s_coord = instance_abs_pos.x as i32;

        // Encode the Symbol ID using IAID procedure (per JBIG2 spec §6.4.4)
        let _ = coder.encode_iaid(symbol_id_to_encode, symbol_id_bits);
    }
    coder.flush(true);
    payload.extend(coder.as_bytes());

    Ok(payload)
}

fn compute_symbol_hash(symbol: &BitImage) -> u32 {
    let w = symbol.width as u32;
    let h = symbol.height as u32;
    (10 * h + 10000 * w) % 10000000
}

fn log2up(v: u32) -> u32 {
    if v == 0 {
        return 0;
    }
    let is_pow_of_2 = (v & (v - 1)) == 0;
    let mut r = 0;
    let mut val = v;
    while val > 1 {
        val >>= 1;
        r += 1;
    }
    r + if is_pow_of_2 { 0 } else { 1 }
}

/// Encodes a sequence of images as a JBIG2 document.
///
/// # Arguments
/// * `images` - A slice of 2D arrays containing the input images
/// * `config` - Configuration for the encoder
///
/// # Returns
/// A `Result` containing the encoded JBIG2 document as a byte vector if successful,
/// or an error if encoding fails.
pub fn encode_document(images: &[Array2<u8>], config: &Jbig2Config) -> Result<Vec<u8>> {
    let mut encoder = Jbig2Encoder::new(config);
    for image in images {
        encoder.add_page(image)?;
    }
    encoder.flush()
}

/// Represents a single symbol instance in a text region, with refinement info.
#[derive(Debug, Clone)]
pub struct TextRegionSymbolInstance {
    /// The ID of the symbol in the dictionary.
    pub symbol_id: u32,
    /// The x-coordinate of the instance's top-left corner.
    pub x: i32,
    /// The y-coordinate of the instance's top-left corner.
    pub y: i32,
    /// The horizontal refinement offset.
    pub dx: i32,
    /// The vertical refinement offset.
    pub dy: i32,
    /// Whether this instance is a refinement of a dictionary symbol.
    pub is_refinement: bool,
}

impl TextRegionSymbolInstance {
    /// Returns the position of this symbol instance as a Rect.
    pub fn position(&self) -> crate::jbig2sym::Rect {
        crate::jbig2sym::Rect {
            x: self.x as u32,
            y: self.y as u32,
            width: 0,  // These will be set by the caller
            height: 0, // These will be set by the caller
        }
    }

    /// Returns the symbol index for this instance.
    pub fn symbol_index(&self) -> usize {
        self.symbol_id as usize
    }

    /// Converts to a SymbolInstance
    pub fn to_symbol_instance(&self, symbol_bitmap: &BitImage) -> SymbolInstance {
        SymbolInstance {
            symbol_index: self.symbol_id as usize,
            position: self.position(),
            instance_bitmap: symbol_bitmap.clone(),
        }
    }
}

pub fn build_dictionary_and_get_instances(
    symbols: &[(Rect, BitImage)],
    comparator: &mut Comparator,
) -> (Vec<BitImage>, Vec<TextRegionSymbolInstance>) {
    let mut dictionary_symbols: Vec<BitImage> = Vec::new();
    let mut instances = Vec::new();

    for (rect, symbol_image) in symbols.iter() {
        let mut found_match = false;
        // Use a 10% error threshold for matching, as recommended.
        let max_err = ((symbol_image.width * symbol_image.height) / 10).max(3) as u32;

        for (dict_idx, dict_symbol) in dictionary_symbols.iter().enumerate() {
            // Use a low max_err for finding near-duplicates
            if let Some((err, dx, dy)) = comparator.distance(symbol_image, dict_symbol, max_err) {
                instances.push(TextRegionSymbolInstance {
                    symbol_id: dict_idx as u32,
                    x: rect.x as i32,
                    y: rect.y as i32,
                    dx,
                    dy,
                    is_refinement: err > 0,
                });
                found_match = true;
                break;
            }
        }

        if !found_match {
            let new_idx = dictionary_symbols.len();
            dictionary_symbols.push(symbol_image.clone());
            instances.push(TextRegionSymbolInstance {
                symbol_id: new_idx as u32,
                x: rect.x as i32,
                y: rect.y as i32,
                dx: 0,
                dy: 0,
                is_refinement: false,
            });
        }
    }

    (dictionary_symbols, instances)
}

/// Encodes a single page image using a symbol dictionary.
/// This is a high-level function that demonstrates the new encoding pipeline.
pub fn encode_page_with_symbol_dictionary(
    image: &BitImage,
    config: &Jbig2Config,
    next_segment_num: u32,
) -> Result<(Vec<u8>, u32)> {
    // 1. Extract symbols from the page image using CC analysis
    #[cfg(feature = "cc-analysis")]
    let extracted_symbols = {
        let dpi = 300; // Default DPI
        let losslevel = if config.is_lossless { 0 } else { 1 };
        let cc_image = analyze_page(image, dpi, losslevel);
        let shapes = cc_image.extract_shapes();
        // Convert to (Rect, BitImage) format
        shapes.into_iter().map(|(bitmap, bbox)| {
            let rect = Rect {
                x: bbox.xmin as u32,
                y: bbox.ymin as u32,
                width: bbox.width() as u32,
                height: bbox.height() as u32,
            };
            (rect, bitmap)
        }).collect::<Vec<_>>()
    };
    #[cfg(not(feature = "cc-analysis"))]
    let extracted_symbols: Vec<(Rect, BitImage)> = Vec::new();

    if extracted_symbols.is_empty() {
        return Ok((Vec::new(), next_segment_num));
    }

    // 2. Build the symbol dictionary and get symbol instances
    let mut comparator = Comparator::default();
    let (dictionary_symbols, text_region_instances) =
        build_dictionary_and_get_instances(&extracted_symbols, &mut comparator);
    debug!(
        "Built dictionary with {} symbols and {} instances",
        dictionary_symbols.len(),
        text_region_instances.len()
    );

    let mut output = Vec::new();
    let mut current_segment_number = next_segment_num;

    // 3. Encode the symbol dictionary segment
    let dict_payload = encode_symbol_dict(
        &dictionary_symbols.iter().collect::<Vec<_>>(),
        config,
        0, // No refinement in dictionary symbols themselves
    )?;
    let dict_segment = Segment {
        number: current_segment_number,
        seg_type: SegmentType::SymbolDictionary,
        referred_to: Vec::new(),
        page: Some(1), // Assuming page 1 for now
        payload: dict_payload,
        ..Default::default()
    };

    // You might want to log dict_params here too if they are accessible
    dict_segment.write_into(&mut output)?;
    let dictionary_segment_number = current_segment_number;
    current_segment_number += 1;

    // 4. Encode the text region segment
    let all_dict_indices: Vec<usize> = (0..dictionary_symbols.len()).collect();
    let mut encoder = Jbig2Encoder::new(config);
    let region_payload = if encoder.state.use_refinement {
        let mut coder = Jbig2ArithCoder::new();

        // Convert TextRegionSymbolInstance to SymbolInstance with instance_bitmap
        let symbol_instances: Vec<SymbolInstance> = text_region_instances
            .iter()
            .map(|instance| {
                let symbol_bitmap = if (instance.symbol_id as usize) < dictionary_symbols.len() {
                    &dictionary_symbols[instance.symbol_id as usize]
                } else {
                    &dictionary_symbols[0]
                };
                instance.to_symbol_instance(symbol_bitmap)
            })
            .collect();

        // Create a vector of references to the dictionary symbols
        let dict_refs: Vec<&BitImage> = dictionary_symbols.iter().collect();

        encode_text_region(
            &symbol_instances,
            config,
            &dict_refs,
            &all_dict_indices,
            &[],
        )?
    } else {
        // Convert TextRegionSymbolInstance to SymbolInstance with instance_bitmap
        let symbol_instances: Vec<SymbolInstance> = text_region_instances
            .iter()
            .map(|instance| {
                let symbol_bitmap = if (instance.symbol_id as usize) < dictionary_symbols.len() {
                    &dictionary_symbols[instance.symbol_id as usize]
                } else {
                    &dictionary_symbols[0]
                };
                instance.to_symbol_instance(symbol_bitmap)
            })
            .collect();

        // Create a vector of references to the dictionary symbols
        let dict_refs: Vec<&BitImage> = dictionary_symbols.iter().collect();

        encode_text_region(
            &symbol_instances,
            config,
            &dict_refs,
            &all_dict_indices,
            &[],
        )?
    };

    let region_segment = Segment {
        number: current_segment_number,
        seg_type: SegmentType::ImmediateTextRegion,
        retain_flags: 0,
        referred_to: vec![dictionary_segment_number], // Refers to the dictionary
        page: Some(1),                                // Assuming page 1
        payload: region_payload,
        ..Default::default()
    };

    // You might want to log text_region_params here too if they are accessible
    region_segment.write_into(&mut output)?;
    current_segment_number += 1;

    Ok((output, current_segment_number))
}

pub fn get_version() -> &'static str {
    "0.2.0"
}

#[inline]
pub fn hash_key(img: &BitImage) -> HashKey {
    // Use std library SipHash for hashing the bitmap data
    use std::collections::hash_map::DefaultHasher;
    let mut hasher = DefaultHasher::new();
    img.as_bytes().hash(&mut hasher);
    let hash = hasher.finish();
    HashKey(hash)
}

/// Helper function to find the first black pixel in the BitImage
/// Returns (x, y) coordinates of the first black pixel, or None if no black pixels
pub fn first_black_pixel(image: &BitImage) -> Option<(usize, usize)> {
    for y in 0..image.height {
        for x in 0..image.width {
            if image.get_usize(x, y) {
                return Some((x, y));
            }
        }
    }
    None
}
