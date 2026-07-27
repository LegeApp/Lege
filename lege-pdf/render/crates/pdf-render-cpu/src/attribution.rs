//! Diagnostic per-pixel attribution for differential rendering.
//!
//! This records the topmost prepared operation whose geometric coverage
//! reaches a pixel. It deliberately ignores paint alpha, blend modes, soft
//! masks, and knockout semantics: the result is a triage aid, not proof that
//! the attributed PDF object caused a visible difference.

use pdf_page_ir::{DeviceRect, PaintLeaf, PaintOrigin};

use crate::kernels::mul_div_255;
use crate::mask::{ClipGeom, ClipMask, build_clip_mask};
use crate::prepared::{CpuPreparedPage, PreparedOp};
use crate::raster::RasterKernel;

/// Default minimum geometric coverage accepted as painted, in 8-bit alpha.
pub const DEFAULT_COVERAGE_THRESHOLD: u8 = 8;

/// Two canonical, row-major diagnostic planes.
#[derive(Debug, Clone)]
pub struct AttributionMap {
    pub width: u32,
    pub height: u32,
    /// `PaintLeaf as u8`; zero is `Unpainted`.
    pub leaf: Vec<u8>,
    /// `PaintOrigin as u8`; meaningful wherever `leaf != 0`.
    pub origin: Vec<u8>,
    pub coverage_threshold: u8,
}

/// Render attribution with [`DEFAULT_COVERAGE_THRESHOLD`].
pub fn render_attribution(page: &CpuPreparedPage) -> AttributionMap {
    render_attribution_with_threshold(page, DEFAULT_COVERAGE_THRESHOLD)
}

/// Render attribution using an explicit geometric-coverage threshold.
pub fn render_attribution_with_threshold(
    page: &CpuPreparedPage,
    coverage_threshold: u8,
) -> AttributionMap {
    let len = page.size.width as usize * page.size.height as usize;
    let mut map = AttributionMap {
        width: page.size.width,
        height: page.size.height,
        leaf: vec![PaintLeaf::Unpainted as u8; len],
        origin: vec![PaintOrigin::PageContent as u8; len],
        coverage_threshold,
    };
    let mut raster = RasterKernel::default();
    let mut clips: Vec<Option<ClipMask>> = (0..page.clips.len()).map(|_| None).collect();

    let mut i = 0usize;
    while i < page.ops.len() {
        match &page.ops[i] {
            PreparedOp::Draw(cmd) => {
                let leaf = if cmd.shading.is_some() {
                    PaintLeaf::Shading
                } else {
                    PaintLeaf::Path
                };
                stamp_path(
                    page,
                    &mut map,
                    &mut raster,
                    &mut clips,
                    cmd.subpath_range,
                    cmd.rule,
                    cmd.bounds,
                    cmd.clip.filter(|_| cmd.clip_has_mask),
                    leaf,
                    cmd.origin,
                );
                i += 1;
            }
            PreparedOp::TiledFill(t) => {
                stamp_path(
                    page,
                    &mut map,
                    &mut raster,
                    &mut clips,
                    t.fill_subpaths,
                    t.fill_rule,
                    t.bounds,
                    t.clip.filter(|_| t.clip_has_mask),
                    PaintLeaf::TilingPattern,
                    // The visible paint comes from replicated pattern-cell
                    // content; this is the innermost containing construct.
                    PaintOrigin::TilingPatternCell,
                );
                i += 1;
            }
            PreparedOp::GlyphRun(run) => {
                ensure_clip(
                    page,
                    &mut raster,
                    &mut clips,
                    run.clip.filter(|_| run.clip_has_mask),
                );
                for placement in &run.placements {
                    let bitmap = &placement.bitmap;
                    for by in 0..bitmap.height as i32 {
                        let y = placement.dy + by;
                        if y < run.bounds.y
                            || y >= run.bounds.y + run.bounds.height as i32
                            || y < 0
                            || y >= map.height as i32
                        {
                            continue;
                        }
                        for bx in 0..bitmap.width as i32 {
                            let x = placement.dx + bx;
                            if x < run.bounds.x
                                || x >= run.bounds.x + run.bounds.width as i32
                                || x < 0
                                || x >= map.width as i32
                            {
                                continue;
                            }
                            let glyph =
                                bitmap.cov[by as usize * bitmap.width as usize + bx as usize];
                            let coverage = combined_coverage(
                                glyph,
                                clip_at(&clips, run.clip.filter(|_| run.clip_has_mask), x, y),
                            );
                            if coverage >= coverage_threshold {
                                map.stamp(x as u32, y as u32, PaintLeaf::Text, run.origin);
                            }
                        }
                    }
                }
                i += 1;
            }
            PreparedOp::Image(image) => {
                ensure_clip(
                    page,
                    &mut raster,
                    &mut clips,
                    image.clip.filter(|_| image.clip_has_mask),
                );
                for_each_pixel(image.bounds, map.width, map.height, |x, y| {
                    let dx = x as f64 + 0.5;
                    let dy = y as f64 + 0.5;
                    let u = image.inv.a * dx + image.inv.c * dy + image.inv.e;
                    let v = image.inv.b * dx + image.inv.d * dy + image.inv.f;
                    if (0.0..1.0).contains(&u) && (0.0..1.0).contains(&v) {
                        let coverage = clip_at(
                            &clips,
                            image.clip.filter(|_| image.clip_has_mask),
                            x as i32,
                            y as i32,
                        );
                        if coverage >= coverage_threshold {
                            map.stamp(x, y, PaintLeaf::Image, image.origin);
                        }
                    }
                });
                i += 1;
            }
            PreparedOp::PushSoftMask { content_end, .. } => {
                // The enclosed ops paint only into the mask's offscreen.
                i = (*content_end as usize).max(i + 1);
            }
            PreparedOp::BeginGroup { .. }
            | PreparedOp::EndGroup
            | PreparedOp::PushSoftMaskNone
            | PreparedOp::PopSoftMask => i += 1,
        }
    }
    map
}

impl AttributionMap {
    #[inline]
    fn stamp(&mut self, x: u32, y: u32, leaf: PaintLeaf, origin: PaintOrigin) {
        let index = y as usize * self.width as usize + x as usize;
        self.leaf[index] = leaf as u8;
        self.origin[index] = origin as u8;
    }
}

#[allow(clippy::too_many_arguments)]
fn stamp_path(
    page: &CpuPreparedPage,
    map: &mut AttributionMap,
    raster: &mut RasterKernel,
    clips: &mut [Option<ClipMask>],
    subpath_range: (u32, u32),
    rule: crate::raster::FillRule,
    bounds: DeviceRect,
    clip: Option<u32>,
    leaf: PaintLeaf,
    origin: PaintOrigin,
) {
    ensure_clip(page, raster, clips, clip);
    let subpaths = &page.subpaths[subpath_range.0 as usize..subpath_range.1 as usize];
    let width = map.width as usize;
    let height = map.height as usize;
    let threshold = map.coverage_threshold;
    raster.fill(
        &page.points,
        subpaths,
        width,
        height,
        rule,
        |y, x0, x1, coverage| {
            if y < bounds.y.max(0) as usize
                || y >= (bounds.y + bounds.height as i32).max(0) as usize
            {
                return;
            }
            let left = x0.max(bounds.x.max(0) as usize);
            let right_exclusive = (bounds.x + bounds.width as i32).max(0) as usize;
            if left >= right_exclusive {
                return;
            }
            let right = x1.min(right_exclusive - 1);
            for x in left..=right {
                let combined =
                    combined_coverage(coverage[x], clip_at(clips, clip, x as i32, y as i32));
                if combined >= threshold {
                    map.stamp(x as u32, y as u32, leaf, origin);
                }
            }
        },
    );
}

fn ensure_clip(
    page: &CpuPreparedPage,
    raster: &mut RasterKernel,
    clips: &mut [Option<ClipMask>],
    clip: Option<u32>,
) {
    if let Some(id) = clip
        && clips[id as usize].is_none()
    {
        clips[id as usize] = Some(build_clip_mask(raster, ClipGeom::of(page), id));
    }
}

#[inline]
fn combined_coverage(paint: u8, clip: u8) -> u8 {
    mul_div_255(paint as u16, clip as u16) as u8
}

#[inline]
fn clip_at(clips: &[Option<ClipMask>], clip: Option<u32>, x: i32, y: i32) -> u8 {
    let Some(mask) = clip.and_then(|id| clips[id as usize].as_ref()) else {
        return 255;
    };
    if x < mask.bounds.x
        || y < mask.bounds.y
        || x >= mask.bounds.x + mask.bounds.width as i32
        || y >= mask.bounds.y + mask.bounds.height as i32
    {
        return mask.outside;
    }
    let lx = (x - mask.bounds.x) as usize;
    let ly = (y - mask.bounds.y) as usize;
    mask.data[ly * mask.stride() + lx]
}

fn for_each_pixel(bounds: DeviceRect, width: u32, height: u32, mut f: impl FnMut(u32, u32)) {
    let x0 = bounds.x.max(0) as u32;
    let y0 = bounds.y.max(0) as u32;
    let x1 = (bounds.x + bounds.width as i32).max(0).min(width as i32) as u32;
    let y1 = (bounds.y + bounds.height as i32).max(0).min(height as i32) as u32;
    for y in y0..y1 {
        for x in x0..x1 {
            f(x, y);
        }
    }
}

#[cfg(all(test, not(feature = "profiling")))]
mod tests {
    use super::*;
    use crate::prepared::{DrawClass, PreparedCommand, RenderDiagnostics};
    use pdf_page_ir::{BlendMode, DeviceSize};

    fn page(
        ops: Vec<PreparedOp>,
        points: Vec<[f32; 2]>,
        subpaths: Vec<(usize, usize)>,
    ) -> CpuPreparedPage {
        CpuPreparedPage {
            size: DeviceSize {
                width: 4,
                height: 4,
            },
            ops,
            clips: Vec::new(),
            points,
            subpaths,
            codecs: pdf_image::CodecRegistry::default(),
            decode_limits: pdf_image::DecodeLimits::default(),
            hinting: pdf_font::HintingPolicy::None,
            color_policy: pdf_render_api::RenderColorPolicy::Original,
            diagnostics: RenderDiagnostics::default(),
            image_cache: None,
        }
    }

    fn draw(range: (u32, u32), bounds: DeviceRect, origin: PaintOrigin) -> PreparedOp {
        PreparedOp::Draw(PreparedCommand {
            origin,
            class: DrawClass::SolidPath,
            subpath_range: range,
            rule: crate::raster::FillRule::NonZero,
            rgb: [0, 0, 0],
            premul: [0, 0, 0, 255],
            alpha: 255,
            opaque: true,
            bounds,
            clip: None,
            clip_has_mask: false,
            blend: BlendMode::Normal,
            shading: None,
            stencil: None,
        })
    }

    #[test]
    fn blank_page_is_entirely_unpainted() {
        let map = render_attribution(&page(Vec::new(), Vec::new(), Vec::new()));
        assert!(
            map.leaf
                .iter()
                .all(|&value| value == PaintLeaf::Unpainted as u8)
        );
    }

    #[test]
    fn later_painter_replaces_origin() {
        let points = vec![
            [0.0, 0.0],
            [4.0, 0.0],
            [4.0, 4.0],
            [0.0, 4.0],
            [1.0, 1.0],
            [3.0, 1.0],
            [3.0, 3.0],
            [1.0, 3.0],
        ];
        let ops = vec![
            draw(
                (0, 1),
                DeviceRect {
                    x: 0,
                    y: 0,
                    width: 4,
                    height: 4,
                },
                PaintOrigin::FormXObject,
            ),
            draw(
                (1, 2),
                DeviceRect {
                    x: 1,
                    y: 1,
                    width: 2,
                    height: 2,
                },
                PaintOrigin::AnnotationAppearance,
            ),
        ];
        let map = render_attribution(&page(ops, points, vec![(0, 4), (4, 8)]));
        assert_eq!(map.leaf[0], PaintLeaf::Path as u8);
        assert_eq!(map.origin[0], PaintOrigin::FormXObject as u8);
        assert_eq!(
            map.origin[1 * 4 + 1],
            PaintOrigin::AnnotationAppearance as u8
        );
    }
}
