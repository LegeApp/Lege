use super::super::Jbig2Encoder;
use super::dictionary::encode_symbol_dict;
use super::types::{PlannedDocument, SymbolInstance};
use crate::jbig2arith::Jbig2ArithCoder;
use crate::jbig2structs::{
    FileHeader, GenericRegionConfig, GenericRegionParams, Segment, SegmentType,
};
use crate::jbig2sym::BitImage;
use anyhow::{Result, anyhow};
#[cfg(feature = "parallel")]
use rayon::prelude::*;

impl<'a> Jbig2Encoder<'a> {
    pub(crate) fn serialize_full_document(&self, plan: &PlannedDocument) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        if let Some(header) = &plan.file_header {
            output.extend(header.to_bytes());
        }
        for seg in &plan.global_segments {
            seg.write_into(&mut output)?;
        }
        for page in &plan.pages {
            for seg in &page.segments {
                seg.write_into(&mut output)?;
            }
        }
        if let Some(eof) = &plan.eof_segment {
            eof.write_into(&mut output)?;
        }
        Ok(output)
    }

    pub(crate) fn serialize_pdf_split(
        &self,
        plan: &PlannedDocument,
    ) -> Result<(
        Option<Vec<u8>>,
        Vec<Vec<u8>>,
        Vec<usize>,
        Vec<usize>,
        Vec<usize>,
    )> {
        let global_segments = if plan.global_segments.is_empty() {
            None
        } else {
            let mut out = Vec::new();
            for seg in &plan.global_segments {
                seg.write_into(&mut out)?;
            }
            Some(out)
        };

        #[cfg(feature = "parallel")]
        let page_streams = plan
            .pages
            .par_iter()
            .map(|page| {
                let mut page_out = Vec::new();
                let mut local_dict_bytes = 0usize;
                let mut text_region_bytes = 0usize;
                let mut generic_region_bytes = 0usize;
                for seg in &page.segments {
                    let start_len = page_out.len();
                    seg.write_into(&mut page_out)?;
                    let seg_len = page_out.len().saturating_sub(start_len);
                    match seg.seg_type {
                        SegmentType::SymbolDictionary => local_dict_bytes += seg_len,
                        SegmentType::ImmediateTextRegion => text_region_bytes += seg_len,
                        SegmentType::ImmediateGenericRegion => generic_region_bytes += seg_len,
                        _ => {}
                    }
                }
                Ok((
                    page_out,
                    local_dict_bytes,
                    text_region_bytes,
                    generic_region_bytes,
                ))
            })
            .collect::<Vec<Result<(Vec<u8>, usize, usize, usize)>>>()
            .into_iter()
            .collect::<Result<Vec<_>>>()?;

        #[cfg(not(feature = "parallel"))]
        let page_streams = {
            let mut page_streams = Vec::with_capacity(plan.pages.len());
            for page in &plan.pages {
                let mut page_out = Vec::new();
                let mut local_dict_bytes = 0usize;
                let mut text_region_bytes = 0usize;
                let mut generic_region_bytes = 0usize;
                for seg in &page.segments {
                    let start_len = page_out.len();
                    seg.write_into(&mut page_out)?;
                    let seg_len = page_out.len().saturating_sub(start_len);
                    match seg.seg_type {
                        SegmentType::SymbolDictionary => local_dict_bytes += seg_len,
                        SegmentType::ImmediateTextRegion => text_region_bytes += seg_len,
                        SegmentType::ImmediateGenericRegion => generic_region_bytes += seg_len,
                        _ => {}
                    }
                }
                page_streams.push((
                    page_out,
                    local_dict_bytes,
                    text_region_bytes,
                    generic_region_bytes,
                ));
            }
            page_streams
        };

        let mut raw_pages = Vec::with_capacity(page_streams.len());
        let mut local_dict_bytes_per_page = Vec::with_capacity(page_streams.len());
        let mut text_region_bytes_per_page = Vec::with_capacity(page_streams.len());
        let mut generic_region_bytes_per_page = Vec::with_capacity(page_streams.len());
        for (page_out, local_dict_bytes, text_region_bytes, generic_region_bytes) in page_streams {
            raw_pages.push(page_out);
            local_dict_bytes_per_page.push(local_dict_bytes);
            text_region_bytes_per_page.push(text_region_bytes);
            generic_region_bytes_per_page.push(generic_region_bytes);
        }

        Ok((
            global_segments,
            raw_pages,
            local_dict_bytes_per_page,
            text_region_bytes_per_page,
            generic_region_bytes_per_page,
        ))
    }

    pub(crate) fn prune_symbols_if_needed(&mut self) {
        // No pruning — JBIG2 supports large dictionaries and pruning drops
        // symbol instances, leaving holes in the rendered output.
    }

    pub(crate) fn next_segment_number(&mut self) -> u32 {
        let num = self.next_segment_number;
        self.next_segment_number += 1;
        num
    }

    pub(crate) fn flush_dict(&mut self) -> Result<Vec<u8>> {
        if self.global_symbols.is_empty() {
            return Ok(Vec::new());
        }

        let symbol_refs: Vec<&BitImage> = self.global_symbols.iter().collect();
        let dict_data = encode_symbol_dict(&symbol_refs, &self.config, 0)?;

        let dict_segment = Segment {
            number: self.next_segment_number,
            seg_type: SegmentType::SymbolDictionary,
            deferred_non_retain: false,
            retain_flags: 0,
            page_association_type: if self.state.pdf_mode { 2 } else { 0 },
            referred_to: Vec::new(),
            page: if self.state.pdf_mode { None } else { Some(1) },
            payload: dict_data,
        };
        self.next_segment_number += 1;

        let mut output = Vec::new();
        if self.state.pdf_mode {
            dict_segment.write_into(&mut output)?;
            return Ok(output);
        }

        let header = FileHeader {
            // Sequential organisation — payload follows each segment header.
            organisation_type: false,
            unknown_n_pages: false,
            n_pages: 1,
        };
        output.extend(header.to_bytes());
        dict_segment.write_into(&mut output)?;

        Ok(output)
    }

    pub(crate) fn build_instance_residual_bitmap(
        &self,
        instances: &[SymbolInstance],
    ) -> Result<Option<(BitImage, u32, u32)>> {
        if instances.is_empty() {
            return Ok(None);
        }

        let mut min_x = u32::MAX;
        let mut min_y = u32::MAX;
        let mut max_x = 0u32;
        let mut max_y = 0u32;
        let mut has_pixels = false;

        for instance in instances {
            if instance.instance_bitmap.count_ones() == 0 {
                continue;
            }
            has_pixels = true;
            min_x = min_x.min(instance.position.x);
            min_y = min_y.min(instance.position.y);
            max_x = max_x.max(instance.position.x + instance.instance_bitmap.width as u32);
            max_y = max_y.max(instance.position.y + instance.instance_bitmap.height as u32);
        }

        if !has_pixels || max_x <= min_x || max_y <= min_y {
            return Ok(None);
        }

        let width = max_x - min_x;
        let height = max_y - min_y;
        let mut residual = BitImage::new(width, height).map_err(|e| anyhow!(e))?;
        for instance in instances {
            let offset_x = (instance.position.x - min_x) as usize;
            let offset_y = (instance.position.y - min_y) as usize;
            for y in 0..instance.instance_bitmap.height {
                for x in 0..instance.instance_bitmap.width {
                    if instance.instance_bitmap.get_usize(x, y) {
                        residual.set_usize(offset_x + x, offset_y + y, true);
                    }
                }
            }
        }

        if residual.count_ones() == 0 {
            return Ok(None);
        }

        Ok(Some((residual, min_x, min_y)))
    }

    pub(crate) fn encode_generic_region_payload_at(
        &self,
        image: &BitImage,
        x: u32,
        y: u32,
    ) -> Result<Vec<u8>> {
        let mut gr_cfg = GenericRegionConfig::new(
            image.width as u32,
            image.height as u32,
            self.config.generic.dpi,
        );
        gr_cfg.x = x;
        gr_cfg.y = y;
        gr_cfg.comb_operator = self.config.generic.comb_operator;
        gr_cfg.mmr = self.config.generic.mmr;
        gr_cfg.tpgdon = self.config.generic.tpgdon;
        gr_cfg.validate().map_err(|e: &'static str| anyhow!(e))?;

        let coder_data = Jbig2ArithCoder::encode_generic_payload_cfg(image, &gr_cfg)?;
        let params: GenericRegionParams = gr_cfg.clone().into();
        let mut payload = params.to_bytes();
        payload.extend_from_slice(&coder_data);
        Ok(payload)
    }
}
