use std::ffi::c_void;
use std::ptr::copy_nonoverlapping;

use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use crate::binarization::{
    BinarizationMode, BinarizationParams, BinarizeParamsStd140, GpuBinarizationError, Result,
};
use crate::resize::hlsl::dx12::{ComputePipelineState, D3D12Context};

/// Map the readback buffer (which already holds packed bytes — 1 byte per pixel
/// after the pack pass) and run a callback against the borrowed slice while it
/// is still mapped. The callback observes exactly `pixel_count` bytes; any tail
/// bytes from the u32 alignment of the packed buffer are not exposed.
///
/// True zero-copy: no Vec is materialized inside this helper. The mutex around
/// the binarizer keeps any other GPU work from clobbering the buffer for the
/// duration of `f`. Encoders can read straight from GPU-mapped memory.
unsafe fn readback_packed_with<F, R>(
    readback: &ID3D12Resource,
    pixel_count: usize,
    f: F,
) -> Result<R>
where
    F: FnOnce(&[u8]) -> R,
{
    let mut rb_ptr: *mut c_void = std::ptr::null_mut();
    unsafe {
        readback
            .Map(0, None, Some(&mut rb_ptr))
            .map_err(|e| GpuBinarizationError::Execution(e.to_string()))?;
    }
    let slice = unsafe { std::slice::from_raw_parts(rb_ptr.cast::<u8>(), pixel_count) };
    let result = f(slice);
    unsafe {
        readback.Unmap(0, None);
    }
    Ok(result)
}

#[allow(dead_code)]
mod shaders_include {
    #[cfg(not(any(doc, feature = "no-include-shaders")))]
    include!(env!("LEGE_GPU_SHADER_INCLUDE_PATH"));
}

/// Source pixel layout for the GPU binarizer.
#[derive(Clone, Copy)]
pub enum SourceFormat {
    /// 1 byte per pixel — splatted to (G, G, G, 0) before upload.
    Gray,
    /// 3 bytes per pixel: [R, G, B, R, G, B, …]. Padded to RGBA on upload.
    Rgb,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct PadParamsStd140 {
    width: u32,
    height: u32,
    padded_width: u32,
    padded_height: u32,
    radius: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct IntegralParamsStd140 {
    padded_width: u32,
    padded_height: u32,
    integral_width: u32,
    _pad0: u32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct BgParamsStd140 {
    width: u32,
    height: u32,
    bg_window: u32,
    bg_radius: u32,
}

// ---------------------------------------------------------------------------
// GPU buffer set
//
// Descriptor slot layout (heap size 24):
//   pad:          0=SRV gray_buf, 1=UAV padded_gray, 2=UAV padded_gray_sq
//   integral_h:   3=SRV padded_gray, 4=UAV row_integral
//   integral_hf:  5=SRV padded_gray_sq, 6=UAV row_integral_sq
//   integral_v:   7=SRV row_integral, 8=UAV integral
//   integral_vf:  9=SRV row_integral_sq, 10=UAV integral_sq
//   final:        11=SRV gray_buf, 12=SRV integral, 13=SRV integral_sq,
//                 14=SRV bg_buffer, 15=UAV gpu_dst
//   fixed:        0=SRV gray_buf (reused), 15=UAV gpu_dst (reused)
//   linearize:    16=SRV gpu_src (RGBA), 17=UAV gray_buf, 18=SRV srgb_lut
// ---------------------------------------------------------------------------

struct MultiPassBuffers {
    upload_src: ID3D12Resource,    // UPLOAD: RGBA-packed input, pixel_count×4 bytes
    upload_bg: ID3D12Resource,     // UPLOAD: CPU bg_max result staging
    gpu_src: ID3D12Resource,       // DEFAULT: RGBA input (pixel_count×4 bytes), SRV for linearize
    gray_buf: ID3D12Resource,      // DEFAULT: linearize output, 1 u32/pixel
    gpu_dst: ID3D12Resource,       // DEFAULT: binarize output, 1 u32/pixel
    packed_dst: ID3D12Resource,    // DEFAULT: pack-pass output, 4 pixels per u32 (1 byte/pixel)
    readback: ID3D12Resource,      // READBACK: sized to packed bytes (≈ pixel_count, not 4×)
    padded_gray: ID3D12Resource,
    padded_gray_sq: ID3D12Resource,
    row_integral: ID3D12Resource,
    row_integral_sq: ID3D12Resource,
    integral: ID3D12Resource,
    integral_sq: ID3D12Resource,
    bg_tmp: ID3D12Resource,
    bg_buffer: ID3D12Resource,
    cb_pad: ID3D12Resource,
    cb_integral: ID3D12Resource,
    cb_bg: ID3D12Resource,
    cb_final: ID3D12Resource,
    pixel_count: usize,           // actual pixel count (width × height)
    padded_pixel_count: usize,
    integral_pixel_count: usize,
}

struct PipelineSet {
    linearize_pass: ComputePipelineState,
    fixed_pass: ComputePipelineState,
    pad: ComputePipelineState,
    integral_h: ComputePipelineState,
    integral_v: ComputePipelineState,
    integral_h_f32: ComputePipelineState,
    integral_v_f32: ComputePipelineState,
    bg_max_h: ComputePipelineState,
    bg_max_v: ComputePipelineState,
    final_pass: ComputePipelineState,
    pack_output: ComputePipelineState,
}

pub struct HlslBinarizer {
    ctx: D3D12Context,
    pipelines: Option<PipelineSet>,
    buffers: Option<MultiPassBuffers>,
    // sRGB→linear LUT (256 f32 = 1024 bytes), uploaded once at first binarize call.
    srgb_lut: Option<ID3D12Resource>,
    // Persistent CPU staging for RGBA expansion before D3D12 upload.
    upload_staging: Vec<u8>,
    verbose: bool,
}

// ---------------------------------------------------------------------------
// sRGB→linear LUT (CPU-side, identical to WGPU backend)
// ---------------------------------------------------------------------------

fn srgb_to_linear_lut() -> [f32; 256] {
    let mut lut = [0.0f32; 256];
    for i in 0..256 {
        let c = i as f32 / 255.0;
        lut[i] = if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        };
    }
    lut
}

impl HlslBinarizer {
    pub fn new() -> Result<Self> {
        let ctx =
            D3D12Context::new().map_err(|e| GpuBinarizationError::Initialization(e.to_string()))?;
        Ok(Self {
            verbose: std::env::var("LEGE_HLSL_BINARIZE_VERBOSE").ok().as_deref() == Some("1"),
            ctx,
            pipelines: None,
            buffers: None,
            srgb_lut: None,
            upload_staging: Vec::new(),
        })
    }

    pub fn binarize_gray_raw(
        &mut self,
        gray: &[u8],
        params: &BinarizationParams,
    ) -> Result<Vec<u8>> {
        params.validate_gray(gray)?;
        self.binarize_inner_with(gray, params, SourceFormat::Gray, |data| data.to_vec())
    }

    pub fn binarize_batch(
        &mut self,
        pages: &[(&[u8], &BinarizationParams)],
    ) -> Result<Vec<Vec<u8>>> {
        pages.iter().map(|&(gray, params)| self.binarize_gray_raw(gray, params)).collect()
    }

    /// Zero-copy callback variant: `f` runs while the GPU readback buffer is
    /// still mapped. No intermediate `Vec<u8>` is materialized here — the
    /// callback can stream the bytes straight into an encoder.
    pub fn binarize_gray_raw_with<F, R>(
        &mut self,
        gray: &[u8],
        params: &BinarizationParams,
        f: F,
    ) -> Result<R>
    where
        F: FnOnce(&[u8]) -> R,
    {
        params.validate_gray(gray)?;
        self.binarize_inner_with(gray, params, SourceFormat::Gray, f)
    }

    /// Binarize from RGB bytes (3 bytes/pixel). The GPU linearize pre-pass converts
    /// RGB→gray via sRGB→linear LUT + BT.709 luma + sRGB re-encode on the GPU.
    /// `f` receives the binarized bytes while the GPU readback buffer is mapped.
    pub fn binarize_rgb_raw_with<F, R>(
        &mut self,
        rgb: &[u8],
        params: &BinarizationParams,
        f: F,
    ) -> Result<R>
    where
        F: FnOnce(&[u8]) -> R,
    {
        let pixel_count = params.width as usize * params.height as usize;
        if rgb.len() != pixel_count * 3 {
            return Err(GpuBinarizationError::BufferSizeMismatch {
                expected: pixel_count * 3,
                actual: rgb.len(),
            });
        }
        self.binarize_inner_with(rgb, params, SourceFormat::Rgb, f)
    }

    // -----------------------------------------------------------------------
    // Core binarize path — accepts gray or RGB input.
    // -----------------------------------------------------------------------

    fn binarize_inner_with<F, R>(
        &mut self,
        pixels: &[u8],
        params: &BinarizationParams,
        fmt: SourceFormat,
        consume: F,
    ) -> Result<R>
    where
        F: FnOnce(&[u8]) -> R,
    {
        self.ensure_pipelines()?;
        self.ensure_srgb_lut()?;

        let pixel_count = params.width as usize * params.height as usize;
        let sauvola_radius = if params.mode == BinarizationMode::Adaptive {
            params.adaptive.sauvola_window / 2
        } else {
            0
        };
        let bg_radius = if params.mode == BinarizationMode::Adaptive {
            params.adaptive.bg_window / 2
        } else {
            0
        };
        let padded_width = params.width + 2 * sauvola_radius;
        let padded_height = params.height + 2 * sauvola_radius;
        let integral_width = padded_width + 1;
        let padded_pixel_count = padded_width as usize * padded_height as usize;
        let integral_pixel_count = integral_width as usize * (padded_height as usize + 1);

        self.ensure_buffers(pixel_count, padded_pixel_count, integral_pixel_count)?;

        // Fill upload_staging with RGBA-packed pixels before borrowing buffers.
        let rgba_bytes = pixel_count * 4;
        self.upload_staging.resize(rgba_bytes, 0);
        {
            let staging = &mut self.upload_staging[..rgba_bytes];
            match fmt {
                SourceFormat::Gray => {
                    staging.chunks_exact_mut(4).zip(pixels.iter()).for_each(|(o, &g)| {
                        o[0] = g; o[1] = g; o[2] = g; o[3] = 0;
                    });
                }
                SourceFormat::Rgb => {
                    staging.chunks_exact_mut(4).zip(pixels.chunks_exact(3)).for_each(|(o, p)| {
                        o[0] = p[0]; o[1] = p[1]; o[2] = p[2]; o[3] = 0;
                    });
                }
            }
        }
        // Raw pointer extracted before buffers borrow to avoid borrow conflict.
        let staging_ptr = self.upload_staging.as_ptr();

        // For the adaptive CPU bg_max, build a gray vector for the Rec.601 path (Rgb only).
        let cpu_bg_gray_opt: Option<Vec<u8>> = if params.mode == BinarizationMode::Adaptive {
            match fmt {
                SourceFormat::Gray => None,
                SourceFormat::Rgb => {
                    let mut g = vec![0u8; pixel_count];
                    g.iter_mut().zip(pixels.chunks_exact(3)).for_each(|(out, p)| {
                        let r = p[0] as u32;
                        let gv = p[1] as u32;
                        let b = p[2] as u32;
                        *out = ((r * 77 + gv * 150 + b * 29) >> 8) as u8;
                    });
                    Some(g)
                }
            }
        } else {
            None
        };
        let bg_gray: &[u8] = match &cpu_bg_gray_opt {
            Some(g) => g,
            None => pixels,
        };

        let buffers = self
            .buffers
            .as_ref()
            .ok_or_else(|| GpuBinarizationError::Execution("buffers not initialized".into()))?;
        let pipelines = self
            .pipelines
            .as_ref()
            .ok_or_else(|| GpuBinarizationError::Shader("pipeline cache miss".into()))?;

        unsafe {
            // Upload RGBA to upload_src (CPU map → write → unmap).
            let mut map_ptr: *mut c_void = std::ptr::null_mut();
            buffers
                .upload_src
                .Map(0, None, Some(&mut map_ptr))
                .map_err(|e| GpuBinarizationError::Execution(e.to_string()))?;
            copy_nonoverlapping(staging_ptr, map_ptr.cast::<u8>(), rgba_bytes);
            buffers.upload_src.Unmap(0, None);

            self.ctx
                .reset_command_list()
                .map_err(|e| GpuBinarizationError::Execution(e.to_string()))?;

            let heaps: [Option<ID3D12DescriptorHeap>; 1] = [Some(self.ctx.descriptor_heap.clone())];
            self.ctx.command_list.SetDescriptorHeaps(&heaps);

            // Copy upload_src → gpu_src (RGBA).
            self.transition(
                &buffers.upload_src,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            );
            self.transition(
                &buffers.gpu_src,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_COPY_DEST,
            );
            self.ctx.command_list.CopyBufferRegion(
                &buffers.gpu_src,
                0,
                &buffers.upload_src,
                0,
                rgba_bytes as u64,
            );
            self.transition(
                &buffers.gpu_src,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_COMMON,
            );
            self.transition(
                &buffers.upload_src,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
                D3D12_RESOURCE_STATE_COMMON,
            );

            // ---------------------------------------------------------------
            // Linearize pre-pass: RGBA gpu_src → sRGB-perceptual gray_buf.
            // Slots: 16=SRV gpu_src, 17=UAV gray_buf, 18=SRV srgb_lut.
            // ---------------------------------------------------------------
            let srgb_lut = self.srgb_lut.as_ref().unwrap();
            self.create_srv(&buffers.gpu_src, 16, pixel_count as u32);
            self.create_uav(&buffers.gray_buf, 17, pixel_count as u32);
            self.create_srv(srgb_lut, 18, 256);

            // Upload cb_final (holds width+height used by linearize shader).
            self.upload_final_params(
                buffers,
                params,
                padded_width,
                padded_height,
                integral_width,
                sauvola_radius,
            );

            self.transition(
                &buffers.gray_buf,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            );

            self.ctx.command_list.SetPipelineState(&pipelines.linearize_pass.pipeline_state);
            self.ctx.command_list.SetComputeRootSignature(&pipelines.linearize_pass.root_signature);
            self.set_roots_linearize(buffers, 16, 18, 17);
            self.ctx.command_list.Dispatch(
                params.width.div_ceil(16),
                params.height.div_ceil(16),
                1,
            );

            self.uav_barrier(&buffers.gray_buf);
            self.transition(
                &buffers.gray_buf,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            );

            // ---------------------------------------------------------------
            // Fixed-threshold path.
            // ---------------------------------------------------------------
            if params.mode == BinarizationMode::FixedThreshold {
                // Slot 0=SRV gray_buf, 15=UAV gpu_dst.
                self.create_srv(&buffers.gray_buf, 0, pixel_count as u32);
                self.create_uav(&buffers.gpu_dst, 15, pixel_count as u32);

                self.transition(
                    &buffers.gpu_dst,
                    D3D12_RESOURCE_STATE_COMMON,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                );

                self.ctx.command_list.SetPipelineState(&pipelines.fixed_pass.pipeline_state);
                self.ctx.command_list.SetComputeRootSignature(&pipelines.fixed_pass.root_signature);

                let gpu_heap_start = self.ctx.get_gpu_descriptor_handle(0);
                self.ctx.command_list.SetComputeRootDescriptorTable(0, gpu_heap_start);
                let mut gpu_uav = self.ctx.get_gpu_descriptor_handle(15);
                self.ctx.command_list.SetComputeRootDescriptorTable(1, gpu_uav);
                self.ctx
                    .command_list
                    .SetComputeRootConstantBufferView(2, buffers.cb_final.GetGPUVirtualAddress());

                self.ctx.command_list.Dispatch(
                    params.width.div_ceil(16),
                    params.height.div_ceil(16),
                    1,
                );

                // Pack u32-per-pixel gpu_dst → byte-per-pixel packed_dst, then
                // copy packed_dst → readback. Both buffers end in COMMON state.
                self.encode_pack_and_copy_to_readback(buffers, pipelines, pixel_count);
                self.transition(
                    &buffers.gray_buf,
                    D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                    D3D12_RESOURCE_STATE_COMMON,
                );

                self.ctx
                    .execute_command_list()
                    .map_err(|e| GpuBinarizationError::Execution(e.to_string()))?;

                let out = readback_packed_with(&buffers.readback, pixel_count, consume)?;

                if self.verbose {
                    log::debug!(
                        "HLSL binarization dispatched (fixed+linearize): {}x{}",
                        params.width,
                        params.height
                    );
                }

                return Ok(out);
            }

            // ---------------------------------------------------------------
            // Adaptive multi-pass path.
            // ---------------------------------------------------------------
            self.upload_pad_params(buffers, params, padded_width, padded_height, sauvola_radius);
            self.upload_integral_params(buffers, padded_width, padded_height, integral_width);
            self.upload_bg_params(buffers, params, bg_radius);
            // cb_final already uploaded above for the linearize pass.

            // --- Pad pass: gray_buf → padded_gray / padded_gray_sq ---
            // Slot 0=SRV gray_buf, 1=UAV padded_gray, 2=UAV padded_gray_sq.
            self.create_srv(&buffers.gray_buf, 0, pixel_count as u32);
            self.create_uav(&buffers.padded_gray, 1, buffers.padded_pixel_count as u32);
            self.create_uav(&buffers.padded_gray_sq, 2, buffers.padded_pixel_count as u32);

            self.transition(
                &buffers.padded_gray,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            );
            self.transition(
                &buffers.padded_gray_sq,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            );
            self.ctx.command_list.SetPipelineState(&pipelines.pad.pipeline_state);
            self.ctx.command_list.SetComputeRootSignature(&pipelines.pad.root_signature);
            self.set_roots_pad(buffers, 0, 1, 2);
            self.ctx.command_list.Dispatch(
                padded_width.div_ceil(16),
                padded_height.div_ceil(16),
                1,
            );

            self.transition(
                &buffers.padded_gray,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            );
            self.transition(
                &buffers.padded_gray_sq,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            );

            // --- Integral row pass ---
            // Slot 3=SRV padded_gray, 4=UAV row_integral.
            self.create_srv(&buffers.padded_gray, 3, buffers.padded_pixel_count as u32);
            self.create_uav(&buffers.row_integral, 4, buffers.integral_pixel_count as u32);
            self.transition(
                &buffers.row_integral,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            );
            self.ctx.command_list.SetPipelineState(&pipelines.integral_h.pipeline_state);
            self.ctx.command_list.SetComputeRootSignature(&pipelines.integral_h.root_signature);
            self.set_roots_integral(buffers, 3, 4);
            self.ctx.command_list.Dispatch(1, padded_height, 1);

            // --- Integral row pass (f32) ---
            // Slot 5=SRV padded_gray_sq, 6=UAV row_integral_sq.
            self.create_srv(&buffers.padded_gray_sq, 5, buffers.padded_pixel_count as u32);
            self.create_uav(&buffers.row_integral_sq, 6, buffers.integral_pixel_count as u32);
            self.transition(
                &buffers.row_integral_sq,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            );
            self.ctx.command_list.SetPipelineState(&pipelines.integral_h_f32.pipeline_state);
            self.ctx.command_list.SetComputeRootSignature(&pipelines.integral_h_f32.root_signature);
            self.set_roots_integral(buffers, 5, 6);
            self.ctx.command_list.Dispatch(1, padded_height, 1);

            self.transition(
                &buffers.row_integral,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            );
            self.transition(
                &buffers.row_integral_sq,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            );

            // --- Integral column pass ---
            // Slot 7=SRV row_integral, 8=UAV integral.
            self.create_srv(&buffers.row_integral, 7, buffers.integral_pixel_count as u32);
            self.create_uav(&buffers.integral, 8, buffers.integral_pixel_count as u32);
            self.transition(
                &buffers.integral,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            );
            self.ctx.command_list.SetPipelineState(&pipelines.integral_v.pipeline_state);
            self.ctx.command_list.SetComputeRootSignature(&pipelines.integral_v.root_signature);
            self.set_roots_integral(buffers, 7, 8);
            self.ctx.command_list.Dispatch(integral_width, 1, 1);

            // --- Integral column pass (f32) ---
            // Slot 9=SRV row_integral_sq, 10=UAV integral_sq.
            self.create_srv(&buffers.row_integral_sq, 9, buffers.integral_pixel_count as u32);
            self.create_uav(&buffers.integral_sq, 10, buffers.integral_pixel_count as u32);
            self.transition(
                &buffers.integral_sq,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            );
            self.ctx.command_list.SetPipelineState(&pipelines.integral_v_f32.pipeline_state);
            self.ctx.command_list.SetComputeRootSignature(&pipelines.integral_v_f32.root_signature);
            self.set_roots_integral(buffers, 9, 10);
            self.ctx.command_list.Dispatch(integral_width, 1, 1);

            self.transition(
                &buffers.integral,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            );
            self.transition(
                &buffers.integral_sq,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            );

            // --- CPU background max (separable max on bg_gray → bg_buffer via upload_bg) ---
            self.upload_cpu_bg(buffers, bg_gray, params, sauvola_radius)?;

            self.transition(
                &buffers.bg_buffer,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            );

            // --- Final pass: Sauvola + Otsu + AND fusion ---
            // Slots: 11=SRV gray_buf, 12=SRV integral, 13=SRV integral_sq,
            //        14=SRV bg_buffer, 15=UAV gpu_dst.
            self.create_srv(&buffers.gray_buf, 11, pixel_count as u32);
            self.create_srv(&buffers.integral, 12, buffers.integral_pixel_count as u32);
            self.create_srv(&buffers.integral_sq, 13, buffers.integral_pixel_count as u32);
            self.create_srv(&buffers.bg_buffer, 14, pixel_count as u32);
            self.create_uav(&buffers.gpu_dst, 15, pixel_count as u32);

            self.transition(
                &buffers.gpu_dst,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            );

            self.ctx.command_list.SetPipelineState(&pipelines.final_pass.pipeline_state);
            self.ctx.command_list.SetComputeRootSignature(&pipelines.final_pass.root_signature);
            self.set_roots_final_pass(buffers, 11, 12, 13, 14, 15);

            self.ctx.command_list.Dispatch(
                params.width.div_ceil(16),
                params.height.div_ceil(16),
                1,
            );

            // Pack u32-per-pixel gpu_dst → byte-per-pixel packed_dst, then
            // copy packed_dst → readback. Both buffers end in COMMON state.
            self.encode_pack_and_copy_to_readback(buffers, pipelines, pixel_count);

            // Reset all other modified states for the next page.
            self.transition(
                &buffers.gray_buf,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                D3D12_RESOURCE_STATE_COMMON,
            );
            self.transition(
                &buffers.padded_gray,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                D3D12_RESOURCE_STATE_COMMON,
            );
            self.transition(
                &buffers.padded_gray_sq,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                D3D12_RESOURCE_STATE_COMMON,
            );
            self.transition(
                &buffers.integral,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                D3D12_RESOURCE_STATE_COMMON,
            );
            self.transition(
                &buffers.integral_sq,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                D3D12_RESOURCE_STATE_COMMON,
            );
            self.transition(
                &buffers.bg_buffer,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                D3D12_RESOURCE_STATE_COMMON,
            );

            self.ctx
                .execute_command_list()
                .map_err(|e| GpuBinarizationError::Execution(e.to_string()))?;

            let out = readback_packed_with(&buffers.readback, pixel_count, consume)?;

            if self.verbose {
                log::debug!(
                    "HLSL binarization dispatched (adaptive+linearize): {}x{}",
                    params.width,
                    params.height
                );
            }

            Ok(out)
        }
    }

    // -----------------------------------------------------------------------
    // Lazy initializers
    // -----------------------------------------------------------------------

    fn ensure_srgb_lut(&mut self) -> Result<()> {
        if self.srgb_lut.is_some() {
            return Ok(());
        }

        let lut_data = srgb_to_linear_lut();
        let lut_bytes = std::mem::size_of_val(&lut_data); // 1024 bytes
        let lut_al = align_up(lut_bytes, 256);

        let upload_lut = self
            .ctx
            .create_buffer(lut_al, D3D12_HEAP_TYPE_UPLOAD)
            .map_err(|e| GpuBinarizationError::Initialization(e.to_string()))?;
        let lut_resource = self
            .ctx
            .create_buffer(lut_al, D3D12_HEAP_TYPE_DEFAULT)
            .map_err(|e| GpuBinarizationError::Initialization(e.to_string()))?;

        unsafe {
            // CPU-side: write LUT to upload heap.
            let mut map_ptr: *mut c_void = std::ptr::null_mut();
            upload_lut
                .Map(0, None, Some(&mut map_ptr))
                .map_err(|e| GpuBinarizationError::Initialization(e.to_string()))?;
            copy_nonoverlapping(lut_data.as_ptr(), map_ptr.cast::<f32>(), 256);
            upload_lut.Unmap(0, None);

            // Submit a command list that copies upload → default heap.
            self.ctx
                .reset_command_list()
                .map_err(|e| GpuBinarizationError::Initialization(e.to_string()))?;

            let heaps: [Option<ID3D12DescriptorHeap>; 1] = [Some(self.ctx.descriptor_heap.clone())];
            self.ctx.command_list.SetDescriptorHeaps(&heaps);

            self.transition(
                &lut_resource,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_COPY_DEST,
            );
            self.ctx.command_list.CopyBufferRegion(
                &lut_resource,
                0,
                &upload_lut,
                0,
                lut_bytes as u64,
            );
            self.transition(
                &lut_resource,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            );

            self.ctx
                .execute_command_list()
                .map_err(|e| GpuBinarizationError::Initialization(e.to_string()))?;
        }

        self.srgb_lut = Some(lut_resource);
        Ok(())
    }

    fn ensure_pipelines(&mut self) -> Result<()> {
        if self.pipelines.is_some() {
            return Ok(());
        }

        // --- Linearize pipeline: t0=rgba_src, t1=srgb_lut, u0=gray_dst, b0=params ---
        let lin_srv0_range = [D3D12_DESCRIPTOR_RANGE {
            RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
            NumDescriptors: 1,
            BaseShaderRegister: 0,
            RegisterSpace: 0,
            OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
        }];
        let lin_srv1_range = [D3D12_DESCRIPTOR_RANGE {
            RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
            NumDescriptors: 1,
            BaseShaderRegister: 1,
            RegisterSpace: 0,
            OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
        }];
        let lin_uav_range = [D3D12_DESCRIPTOR_RANGE {
            RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
            NumDescriptors: 1,
            BaseShaderRegister: 0,
            RegisterSpace: 0,
            OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
        }];
        let linearize_pass = unsafe {
            Self::make_pipeline(
                &self.ctx.device,
                shaders_include::BINARIZE_LINEARIZE_SHADER,
                &[
                    Self::root_srv_table(0, 1),
                    Self::root_srv_table(1, 1),
                    Self::root_uav_table(0, 1),
                    Self::root_cbv(0),
                ],
                &[&lin_srv0_range, &lin_srv1_range, &lin_uav_range],
            )
        }
        .map_err(|e| GpuBinarizationError::Shader(format!("linearize_pass: {}", e)))?;

        let fixed_ranges = [
            D3D12_DESCRIPTOR_RANGE {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
                NumDescriptors: 1,
                BaseShaderRegister: 0,
                RegisterSpace: 0,
                OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
            },
            D3D12_DESCRIPTOR_RANGE {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
                NumDescriptors: 1,
                BaseShaderRegister: 0,
                RegisterSpace: 0,
                OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
            },
        ];
        let fixed_pass = unsafe {
            Self::make_pipeline(
                &self.ctx.device,
                shaders_include::BINARIZE_SHADER,
                &[
                    Self::root_srv_table(0, 1),
                    Self::root_uav_table(0, 1),
                    Self::root_cbv(0),
                ],
                &[&fixed_ranges[0..1], &fixed_ranges[1..2]],
            )
        }
        .map_err(|e| GpuBinarizationError::Shader(format!("fixed_pass: {}", e)))?;

        let pad_ranges = [
            D3D12_DESCRIPTOR_RANGE {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
                NumDescriptors: 1,
                BaseShaderRegister: 0,
                RegisterSpace: 0,
                OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
            },
            D3D12_DESCRIPTOR_RANGE {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
                NumDescriptors: 1,
                BaseShaderRegister: 0,
                RegisterSpace: 0,
                OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
            },
            D3D12_DESCRIPTOR_RANGE {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
                NumDescriptors: 1,
                BaseShaderRegister: 1,
                RegisterSpace: 0,
                OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
            },
        ];
        let pad = unsafe {
            Self::make_pipeline(
                &self.ctx.device,
                shaders_include::BINARIZE_PAD_SHADER,
                &[
                    Self::root_srv_table(0, 1),
                    Self::root_uav_table(0, 1),
                    Self::root_uav_table(1, 1),
                    Self::root_cbv(0),
                ],
                &[&pad_ranges[0..1], &pad_ranges[1..2], &pad_ranges[2..3]],
            )
        }
        .map_err(|e| GpuBinarizationError::Shader(format!("pad: {}", e)))?;

        let integral_ranges = [
            D3D12_DESCRIPTOR_RANGE {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
                NumDescriptors: 1,
                BaseShaderRegister: 0,
                RegisterSpace: 0,
                OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
            },
            D3D12_DESCRIPTOR_RANGE {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
                NumDescriptors: 1,
                BaseShaderRegister: 0,
                RegisterSpace: 0,
                OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
            },
        ];
        let integral_h = unsafe {
            Self::make_pipeline(
                &self.ctx.device,
                shaders_include::BINARIZE_INTEGRAL_H_SHADER,
                &[
                    Self::root_srv_table(0, 1),
                    Self::root_uav_table(0, 1),
                    Self::root_cbv(0),
                ],
                &[&integral_ranges[0..1], &integral_ranges[1..2]],
            )
        }
        .map_err(|e| GpuBinarizationError::Shader(format!("integral_h: {}", e)))?;

        let integral_v = unsafe {
            Self::make_pipeline(
                &self.ctx.device,
                shaders_include::BINARIZE_INTEGRAL_V_SHADER,
                &[
                    Self::root_srv_table(0, 1),
                    Self::root_uav_table(0, 1),
                    Self::root_cbv(0),
                ],
                &[&integral_ranges[0..1], &integral_ranges[1..2]],
            )
        }
        .map_err(|e| GpuBinarizationError::Shader(format!("integral_v: {}", e)))?;

        let integral_h_f32 = unsafe {
            Self::make_pipeline(
                &self.ctx.device,
                shaders_include::BINARIZE_INTEGRAL_H_F32_SHADER,
                &[
                    Self::root_srv_table(0, 1),
                    Self::root_uav_table(0, 1),
                    Self::root_cbv(0),
                ],
                &[&integral_ranges[0..1], &integral_ranges[1..2]],
            )
        }
        .map_err(|e| GpuBinarizationError::Shader(format!("integral_h_f32: {}", e)))?;

        let integral_v_f32 = unsafe {
            Self::make_pipeline(
                &self.ctx.device,
                shaders_include::BINARIZE_INTEGRAL_V_F32_SHADER,
                &[
                    Self::root_srv_table(0, 1),
                    Self::root_uav_table(0, 1),
                    Self::root_cbv(0),
                ],
                &[&integral_ranges[0..1], &integral_ranges[1..2]],
            )
        }
        .map_err(|e| GpuBinarizationError::Shader(format!("integral_v_f32: {}", e)))?;

        let bg_ranges = [
            D3D12_DESCRIPTOR_RANGE {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
                NumDescriptors: 1,
                BaseShaderRegister: 0,
                RegisterSpace: 0,
                OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
            },
            D3D12_DESCRIPTOR_RANGE {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
                NumDescriptors: 1,
                BaseShaderRegister: 0,
                RegisterSpace: 0,
                OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
            },
        ];
        let bg_max_h = unsafe {
            Self::make_pipeline(
                &self.ctx.device,
                shaders_include::BINARIZE_BG_MAX_H_SHADER,
                &[
                    Self::root_srv_table(0, 1),
                    Self::root_uav_table(0, 1),
                    Self::root_cbv(0),
                ],
                &[&bg_ranges[0..1], &bg_ranges[1..2]],
            )
        }
        .map_err(|e| GpuBinarizationError::Shader(format!("bg_max_h: {}", e)))?;

        let bg_max_v = unsafe {
            Self::make_pipeline(
                &self.ctx.device,
                shaders_include::BINARIZE_BG_MAX_V_SHADER,
                &[
                    Self::root_srv_table(0, 1),
                    Self::root_uav_table(0, 1),
                    Self::root_cbv(0),
                ],
                &[&bg_ranges[0..1], &bg_ranges[1..2]],
            )
        }
        .map_err(|e| GpuBinarizationError::Shader(format!("bg_max_v: {}", e)))?;

        let final_ranges = [
            D3D12_DESCRIPTOR_RANGE {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
                NumDescriptors: 1,
                BaseShaderRegister: 0,
                RegisterSpace: 0,
                OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
            },
            D3D12_DESCRIPTOR_RANGE {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
                NumDescriptors: 1,
                BaseShaderRegister: 1,
                RegisterSpace: 0,
                OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
            },
            D3D12_DESCRIPTOR_RANGE {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
                NumDescriptors: 1,
                BaseShaderRegister: 2,
                RegisterSpace: 0,
                OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
            },
            D3D12_DESCRIPTOR_RANGE {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
                NumDescriptors: 1,
                BaseShaderRegister: 3,
                RegisterSpace: 0,
                OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
            },
            D3D12_DESCRIPTOR_RANGE {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
                NumDescriptors: 1,
                BaseShaderRegister: 0,
                RegisterSpace: 0,
                OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
            },
        ];
        let final_pass = unsafe {
            Self::make_pipeline(
                &self.ctx.device,
                shaders_include::BINARIZE_FINAL_SHADER,
                &[
                    Self::root_srv_table(0, 1),
                    Self::root_srv_table(1, 1),
                    Self::root_srv_table(2, 1),
                    Self::root_srv_table(3, 1),
                    Self::root_uav_table(0, 1),
                    Self::root_cbv(0),
                ],
                &[
                    &final_ranges[0..1],
                    &final_ranges[1..2],
                    &final_ranges[2..3],
                    &final_ranges[3..4],
                    &final_ranges[4..5],
                ],
            )
        }
        .map_err(|e| GpuBinarizationError::Shader(format!("final_pass: {}", e)))?;

        // Pack-output pass: same shape as fixed_pass (1 SRV + 1 UAV + 1 CBV).
        let pack_ranges = [
            D3D12_DESCRIPTOR_RANGE {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
                NumDescriptors: 1,
                BaseShaderRegister: 0,
                RegisterSpace: 0,
                OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
            },
            D3D12_DESCRIPTOR_RANGE {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
                NumDescriptors: 1,
                BaseShaderRegister: 0,
                RegisterSpace: 0,
                OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
            },
        ];
        let pack_output = unsafe {
            Self::make_pipeline(
                &self.ctx.device,
                shaders_include::BINARIZE_PACK_SHADER,
                &[
                    Self::root_srv_table(0, 1),
                    Self::root_uav_table(0, 1),
                    Self::root_cbv(0),
                ],
                &[&pack_ranges[0..1], &pack_ranges[1..2]],
            )
        }
        .map_err(|e| GpuBinarizationError::Shader(format!("pack_output: {}", e)))?;

        self.pipelines = Some(PipelineSet {
            linearize_pass,
            fixed_pass,
            pad,
            integral_h,
            integral_v,
            integral_h_f32,
            integral_v_f32,
            bg_max_h,
            bg_max_v,
            final_pass,
            pack_output,
        });
        Ok(())
    }

    fn root_srv_table(_register: u32, _count: u32) -> D3D12_ROOT_PARAMETER {
        D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                    NumDescriptorRanges: 1,
                    pDescriptorRanges: std::ptr::null(),
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        }
    }

    fn root_uav_table(_register: u32, _count: u32) -> D3D12_ROOT_PARAMETER {
        D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                    NumDescriptorRanges: 1,
                    pDescriptorRanges: std::ptr::null(),
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        }
    }

    fn root_cbv(register: u32) -> D3D12_ROOT_PARAMETER {
        D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_CBV,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                Descriptor: D3D12_ROOT_DESCRIPTOR {
                    ShaderRegister: register,
                    RegisterSpace: 0,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        }
    }

    unsafe fn make_pipeline(
        device: &ID3D12Device,
        shader_bytecode: &[u8],
        root_params: &[D3D12_ROOT_PARAMETER],
        descriptor_range_slices: &[&[D3D12_DESCRIPTOR_RANGE]],
    ) -> windows::core::Result<ComputePipelineState> {
        let mut owned_params: Vec<D3D12_ROOT_PARAMETER> = root_params.to_vec();
        let mut range_idx = 0;
        for param in owned_params.iter_mut() {
            if param.ParameterType == D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE {
                if range_idx < descriptor_range_slices.len() {
                    let slice = descriptor_range_slices[range_idx];
                    param.Anonymous.DescriptorTable.NumDescriptorRanges = slice.len() as u32;
                    param.Anonymous.DescriptorTable.pDescriptorRanges = slice.as_ptr();
                    range_idx += 1;
                }
            }
        }

        let root_signature_desc = D3D12_ROOT_SIGNATURE_DESC {
            NumParameters: owned_params.len() as u32,
            pParameters: owned_params.as_ptr(),
            NumStaticSamplers: 0,
            pStaticSamplers: std::ptr::null(),
            Flags: D3D12_ROOT_SIGNATURE_FLAG_NONE,
        };

        let mut signature_blob: Option<ID3DBlob> = None;
        let mut error_blob: Option<ID3DBlob> = None;

        unsafe {
            D3D12SerializeRootSignature(
                &root_signature_desc,
                D3D_ROOT_SIGNATURE_VERSION_1,
                &mut signature_blob,
                Some(&mut error_blob),
            )?;

            let signature_blob = signature_blob.unwrap();
            let root_signature = device.CreateRootSignature(
                0,
                std::slice::from_raw_parts(
                    signature_blob.GetBufferPointer() as *const u8,
                    signature_blob.GetBufferSize(),
                ),
            )?;

            let compute_pipeline_desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root_signature),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: shader_bytecode.as_ptr() as *const std::ffi::c_void,
                    BytecodeLength: shader_bytecode.len(),
                },
                NodeMask: 0,
                CachedPSO: D3D12_CACHED_PIPELINE_STATE {
                    pCachedBlob: std::ptr::null(),
                    CachedBlobSizeInBytes: 0,
                },
                Flags: D3D12_PIPELINE_STATE_FLAG_NONE,
            };

            let pipeline_state = device.CreateComputePipelineState(&compute_pipeline_desc)?;

            Ok(ComputePipelineState {
                pipeline_state,
                root_signature,
            })
        }
    }

    fn ensure_buffers(
        &mut self,
        pixel_count: usize,
        padded_pixel_count: usize,
        integral_pixel_count: usize,
    ) -> Result<()> {
        // All image-sized buffers use 1 u32 per pixel (4 bytes).
        // gpu_src is RGBA (4 bytes/pixel = same sizing as gray_buf / gpu_dst).
        let img_al = align_up(pixel_count * 4, 256);
        let padded_al = align_up(padded_pixel_count * 4, 256);
        let integral_al = align_up(integral_pixel_count * 4, 256);
        let cb_al = 256usize;

        let needs_new = match &self.buffers {
            Some(b) => {
                b.pixel_count < pixel_count
                    || b.padded_pixel_count < padded_pixel_count
                    || b.integral_pixel_count < integral_pixel_count
            }
            None => true,
        };
        if !needs_new {
            return Ok(());
        }

        let mk = |size: usize, heap: D3D12_HEAP_TYPE| -> Result<ID3D12Resource> {
            self.ctx
                .create_buffer(size, heap)
                .map_err(|e| GpuBinarizationError::Initialization(e.to_string()))
        };

        // Packed output: 4 pixels packed into one u32 → ceil(pixel_count/4) words.
        let packed_words = pixel_count.div_ceil(4);
        let packed_al = align_up(packed_words * 4, 256);

        let upload_src    = mk(img_al,     D3D12_HEAP_TYPE_UPLOAD)?;
        let upload_bg     = mk(img_al,     D3D12_HEAP_TYPE_UPLOAD)?;
        let gpu_src       = mk(img_al,     D3D12_HEAP_TYPE_DEFAULT)?;
        let gray_buf      = mk(img_al,     D3D12_HEAP_TYPE_DEFAULT)?;
        let gpu_dst       = mk(img_al,     D3D12_HEAP_TYPE_DEFAULT)?;
        let packed_dst    = mk(packed_al,  D3D12_HEAP_TYPE_DEFAULT)?;
        // readback now mirrors the packed output (≈1 byte/pixel) instead of 4×.
        let readback      = mk(packed_al,  D3D12_HEAP_TYPE_READBACK)?;
        let padded_gray   = mk(padded_al,  D3D12_HEAP_TYPE_DEFAULT)?;
        let padded_gray_sq= mk(padded_al,  D3D12_HEAP_TYPE_DEFAULT)?;
        let row_integral  = mk(integral_al,D3D12_HEAP_TYPE_DEFAULT)?;
        let row_integral_sq=mk(integral_al,D3D12_HEAP_TYPE_DEFAULT)?;
        let integral      = mk(integral_al,D3D12_HEAP_TYPE_DEFAULT)?;
        let integral_sq   = mk(integral_al,D3D12_HEAP_TYPE_DEFAULT)?;
        let bg_tmp        = mk(img_al,     D3D12_HEAP_TYPE_DEFAULT)?;
        let bg_buffer     = mk(img_al,     D3D12_HEAP_TYPE_DEFAULT)?;
        let cb_pad        = mk(cb_al,      D3D12_HEAP_TYPE_UPLOAD)?;
        let cb_integral   = mk(cb_al,      D3D12_HEAP_TYPE_UPLOAD)?;
        let cb_bg         = mk(cb_al,      D3D12_HEAP_TYPE_UPLOAD)?;
        let cb_final      = mk(cb_al,      D3D12_HEAP_TYPE_UPLOAD)?;

        self.buffers = Some(MultiPassBuffers {
            upload_src,
            upload_bg,
            gpu_src,
            gray_buf,
            gpu_dst,
            packed_dst,
            readback,
            padded_gray,
            padded_gray_sq,
            row_integral,
            row_integral_sq,
            integral,
            integral_sq,
            bg_tmp,
            bg_buffer,
            cb_pad,
            cb_integral,
            cb_bg,
            cb_final,
            pixel_count,
            padded_pixel_count,
            integral_pixel_count,
        });
        Ok(())
    }

    // -----------------------------------------------------------------------
    // D3D12 helper methods
    // -----------------------------------------------------------------------

    unsafe fn upload_cbuf<T: Copy>(&self, cbuf: &ID3D12Resource, data: &T) {
        let mut mapped: *mut c_void = std::ptr::null_mut();
        cbuf.Map(0, None, Some(&mut mapped)).expect("cbuf map failed");
        copy_nonoverlapping(data, mapped.cast::<T>(), 1);
        cbuf.Unmap(0, None);
    }

    unsafe fn upload_pad_params(
        &self,
        buffers: &MultiPassBuffers,
        params: &BinarizationParams,
        padded_width: u32,
        padded_height: u32,
        radius: u32,
    ) {
        let cbuf = PadParamsStd140 {
            width: params.width,
            height: params.height,
            padded_width,
            padded_height,
            radius,
            _pad0: 0,
            _pad1: 0,
            _pad2: 0,
        };
        self.upload_cbuf(&buffers.cb_pad, &cbuf);
    }

    unsafe fn upload_integral_params(
        &self,
        buffers: &MultiPassBuffers,
        padded_width: u32,
        padded_height: u32,
        integral_width: u32,
    ) {
        let cbuf = IntegralParamsStd140 {
            padded_width,
            padded_height,
            integral_width,
            _pad0: 0,
        };
        self.upload_cbuf(&buffers.cb_integral, &cbuf);
    }

    unsafe fn upload_bg_params(
        &self,
        buffers: &MultiPassBuffers,
        params: &BinarizationParams,
        bg_radius: u32,
    ) {
        let cbuf = BgParamsStd140 {
            width: params.width,
            height: params.height,
            bg_window: params.adaptive.bg_window,
            bg_radius,
        };
        self.upload_cbuf(&buffers.cb_bg, &cbuf);
    }

    unsafe fn upload_final_params(
        &self,
        buffers: &MultiPassBuffers,
        params: &BinarizationParams,
        padded_width: u32,
        _padded_height: u32,
        integral_width: u32,
        sauvola_radius: u32,
    ) {
        let cbuf = BinarizeParamsStd140 {
            width: params.width,
            height: params.height,
            mode: match params.mode {
                BinarizationMode::FixedThreshold => 0,
                BinarizationMode::Adaptive => 1,
            },
            invert_output: params.invert_output as u32,
            fixed_threshold: params.fixed_threshold as u32,
            sauvola_window: params.adaptive.sauvola_window,
            bg_window: params.adaptive.bg_window,
            otsu_threshold: params.adaptive.otsu_threshold as u32,
            k_factor: params.k_factor,
            percentile_c: params.adaptive.percentile_c as f32,
            padded_width,
            padded_height: params.height + 2 * sauvola_radius,
            integral_width,
            sauvola_radius,
            debug_mode: params.debug_mode,
            _pad2: 0,
            _pad3: 0,
            _pad4: 0,
            _pad5: 0,
            _pad6: 0,
            _pad7: 0,
        };
        self.upload_cbuf(&buffers.cb_final, &cbuf);
    }

    unsafe fn transition(
        &self,
        resource: &ID3D12Resource,
        before: D3D12_RESOURCE_STATES,
        after: D3D12_RESOURCE_STATES,
    ) {
        let transition = D3D12_RESOURCE_TRANSITION_BARRIER {
            pResource: std::mem::ManuallyDrop::new(Some(resource.clone())),
            Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
            StateBefore: before,
            StateAfter: after,
        };
        let barrier = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: std::mem::ManuallyDrop::new(transition),
            },
        };
        self.ctx.command_list.ResourceBarrier(&[barrier]);
    }

    /// Encode the pack pass (gpu_dst u32-per-pixel → packed_dst byte-per-pixel)
    /// followed by a CopyBufferRegion(packed_dst → readback). Caller must have
    /// gpu_dst in UNORDERED_ACCESS state on entry; both gpu_dst and packed_dst
    /// are returned to COMMON before this call returns.
    unsafe fn encode_pack_and_copy_to_readback(
        &self,
        buffers: &MultiPassBuffers,
        pipelines: &PipelineSet,
        pixel_count: usize,
    ) {
        let packed_words = pixel_count.div_ceil(4) as u32;
        let packed_bytes = packed_words as usize * 4;

        // gpu_dst: UAV (write) → SRV (read by pack pass).
        unsafe {
            self.transition(
                &buffers.gpu_dst,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            );
            self.transition(
                &buffers.packed_dst,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            );

            // Pack-pass descriptors: SRV gpu_dst at slot 19, UAV packed_dst at slot 20.
            self.create_srv(&buffers.gpu_dst, 19, pixel_count as u32);
            self.create_uav(&buffers.packed_dst, 20, packed_words);

            self.ctx.command_list.SetPipelineState(&pipelines.pack_output.pipeline_state);
            self.ctx
                .command_list
                .SetComputeRootSignature(&pipelines.pack_output.root_signature);
            let srv_handle = self.ctx.get_gpu_descriptor_handle(19);
            self.ctx.command_list.SetComputeRootDescriptorTable(0, srv_handle);
            let uav_handle = self.ctx.get_gpu_descriptor_handle(20);
            self.ctx.command_list.SetComputeRootDescriptorTable(1, uav_handle);
            self.ctx
                .command_list
                .SetComputeRootConstantBufferView(2, buffers.cb_final.GetGPUVirtualAddress());

            self.ctx
                .command_list
                .Dispatch(packed_words.div_ceil(256), 1, 1);

            self.transition(
                &buffers.packed_dst,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            );
            self.ctx.command_list.CopyBufferRegion(
                &buffers.readback,
                0,
                &buffers.packed_dst,
                0,
                packed_bytes as u64,
            );

            // Reset to COMMON for the next page.
            self.transition(
                &buffers.packed_dst,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
                D3D12_RESOURCE_STATE_COMMON,
            );
            self.transition(
                &buffers.gpu_dst,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                D3D12_RESOURCE_STATE_COMMON,
            );
        }
    }

    unsafe fn uav_barrier(&self, resource: &ID3D12Resource) {
        let barrier = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                    pResource: std::mem::ManuallyDrop::new(Some(resource.clone())),
                }),
            },
        };
        self.ctx.command_list.ResourceBarrier(&[barrier]);
    }

    fn upload_cpu_bg(
        &self,
        buffers: &MultiPassBuffers,
        gray: &[u8],
        params: &BinarizationParams,
        _sauvola_radius: u32,
    ) -> Result<()> {
        let width = params.width as usize;
        let height = params.height as usize;
        let bg_window = if params.adaptive.bg_window > 0 {
            params.adaptive.bg_window as usize
        } else {
            let mut s = height / 200;
            if s < 3 {
                s = 3;
            }
            if s > 15 {
                s = 15;
            }
            if s % 2 == 0 {
                s += 1;
            }
            s
        };
        let r = bg_window / 2;

        let mut tmp = vec![0u8; width * height];
        for y in 0..height {
            for x in 0..width {
                let mut m = 0u8;
                for dx in -(r as isize)..=(r as isize) {
                    let sx = Self::reflect_101(x as isize + dx, width as isize) as usize;
                    m = m.max(gray[y * width + sx]);
                }
                tmp[y * width + x] = m;
            }
        }

        let mut bg = vec![0u8; width * height];
        for y in 0..height {
            for x in 0..width {
                let mut m = 0u8;
                for dy in -(r as isize)..=(r as isize) {
                    let sy = Self::reflect_101(y as isize + dy, height as isize) as usize;
                    m = m.max(tmp[sy * width + x]);
                }
                bg[y * width + x] = m;
            }
        }

        unsafe {
            let mut map_ptr: *mut c_void = std::ptr::null_mut();
            buffers
                .upload_bg
                .Map(0, None, Some(&mut map_ptr))
                .map_err(|e| GpuBinarizationError::Execution(e.to_string()))?;
            let dst_u32 = map_ptr.cast::<u32>();
            for (i, &byte) in bg.iter().enumerate() {
                *dst_u32.add(i) = byte as u32;
            }
            buffers.upload_bg.Unmap(0, None);

            self.transition(
                &buffers.upload_bg,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            );
            self.transition(
                &buffers.bg_buffer,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_COPY_DEST,
            );
            self.ctx.command_list.CopyBufferRegion(
                &buffers.bg_buffer,
                0,
                &buffers.upload_bg,
                0,
                (width * height * 4) as u64,
            );
            self.transition(
                &buffers.bg_buffer,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_COMMON,
            );
            self.transition(
                &buffers.upload_bg,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
                D3D12_RESOURCE_STATE_COMMON,
            );
        }

        Ok(())
    }

    fn reflect_101(idx: isize, len: isize) -> isize {
        if idx < 0 {
            -idx - 1
        } else if idx >= len {
            2 * len - idx - 1
        } else {
            idx
        }
    }

    unsafe fn create_srv(&self, resource: &ID3D12Resource, slot: u32, num_elements: u32) {
        let cpu_handle = self.ctx.get_cpu_descriptor_handle(slot);
        let srv_desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_UNKNOWN,
            ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_SRV {
                    FirstElement: 0,
                    NumElements: num_elements,
                    StructureByteStride: 4,
                    Flags: D3D12_BUFFER_SRV_FLAG_NONE,
                },
            },
        };
        self.ctx
            .device
            .CreateShaderResourceView(resource, Some(&srv_desc), cpu_handle);
    }

    unsafe fn create_uav(&self, resource: &ID3D12Resource, slot: u32, num_elements: u32) {
        let cpu_handle = self.ctx.get_cpu_descriptor_handle(slot);
        let uav_desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_UNKNOWN,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_UAV {
                    FirstElement: 0,
                    NumElements: num_elements,
                    StructureByteStride: 4,
                    CounterOffsetInBytes: 0,
                    Flags: D3D12_BUFFER_UAV_FLAG_NONE,
                },
            },
        };
        self.ctx
            .device
            .CreateUnorderedAccessView(resource, None, Some(&uav_desc), cpu_handle);
    }

    // Root binding helpers — each named by which pass it serves.

    fn set_roots_linearize(
        &self,
        buffers: &MultiPassBuffers,
        rgba_slot: u32,  // t0: rgba_src SRV
        lut_slot: u32,   // t1: srgb_lut SRV
        gray_slot: u32,  // u0: gray_dst UAV
    ) {
        unsafe {
            self.ctx.command_list.SetComputeRootDescriptorTable(
                0,
                self.ctx.get_gpu_descriptor_handle(rgba_slot),
            );
            self.ctx.command_list.SetComputeRootDescriptorTable(
                1,
                self.ctx.get_gpu_descriptor_handle(lut_slot),
            );
            self.ctx.command_list.SetComputeRootDescriptorTable(
                2,
                self.ctx.get_gpu_descriptor_handle(gray_slot),
            );
            self.ctx
                .command_list
                .SetComputeRootConstantBufferView(3, buffers.cb_final.GetGPUVirtualAddress());
        }
    }

    fn set_roots_pad(
        &self,
        buffers: &MultiPassBuffers,
        srv_slot: u32,
        uav_slot_0: u32,
        uav_slot_1: u32,
    ) {
        unsafe {
            self.ctx.command_list.SetComputeRootDescriptorTable(
                0,
                self.ctx.get_gpu_descriptor_handle(srv_slot),
            );
            self.ctx.command_list.SetComputeRootDescriptorTable(
                1,
                self.ctx.get_gpu_descriptor_handle(uav_slot_0),
            );
            self.ctx.command_list.SetComputeRootDescriptorTable(
                2,
                self.ctx.get_gpu_descriptor_handle(uav_slot_1),
            );
            self.ctx
                .command_list
                .SetComputeRootConstantBufferView(3, buffers.cb_pad.GetGPUVirtualAddress());
        }
    }

    fn set_roots_integral(&self, buffers: &MultiPassBuffers, srv_slot: u32, uav_slot: u32) {
        unsafe {
            self.ctx.command_list.SetComputeRootDescriptorTable(
                0,
                self.ctx.get_gpu_descriptor_handle(srv_slot),
            );
            self.ctx.command_list.SetComputeRootDescriptorTable(
                1,
                self.ctx.get_gpu_descriptor_handle(uav_slot),
            );
            self.ctx
                .command_list
                .SetComputeRootConstantBufferView(2, buffers.cb_integral.GetGPUVirtualAddress());
        }
    }

    fn set_roots_final_pass(
        &self,
        buffers: &MultiPassBuffers,
        srv_0: u32,
        srv_1: u32,
        srv_2: u32,
        srv_3: u32,
        uav_slot: u32,
    ) {
        unsafe {
            self.ctx.command_list.SetComputeRootDescriptorTable(
                0,
                self.ctx.get_gpu_descriptor_handle(srv_0),
            );
            self.ctx.command_list.SetComputeRootDescriptorTable(
                1,
                self.ctx.get_gpu_descriptor_handle(srv_1),
            );
            self.ctx.command_list.SetComputeRootDescriptorTable(
                2,
                self.ctx.get_gpu_descriptor_handle(srv_2),
            );
            self.ctx.command_list.SetComputeRootDescriptorTable(
                3,
                self.ctx.get_gpu_descriptor_handle(srv_3),
            );
            self.ctx.command_list.SetComputeRootDescriptorTable(
                4,
                self.ctx.get_gpu_descriptor_handle(uav_slot),
            );
            self.ctx
                .command_list
                .SetComputeRootConstantBufferView(5, buffers.cb_final.GetGPUVirtualAddress());
        }
    }
}

fn align_up(value: usize, alignment: usize) -> usize {
    (value + alignment - 1) & !(alignment - 1)
}

unsafe impl Send for HlslBinarizer {}
unsafe impl Sync for HlslBinarizer {}
