use std::ffi::c_void;
use std::ptr::copy_nonoverlapping;

use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use crate::binarization::{
    BinarizationMode, BinarizationParams, BinarizeParamsStd140, GpuBinarizationError, Result,
};
use crate::resize::hlsl::dx12::{ComputePipelineState, D3D12Context};

#[allow(dead_code)]
mod shaders_include {
    #[cfg(not(any(doc, feature = "no-include-shaders")))]
    include!(env!("LEGE_GPU_SHADER_INCLUDE_PATH"));
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

struct MultiPassBuffers {
    upload_src: ID3D12Resource,
    upload_bg: ID3D12Resource,
    gpu_src: ID3D12Resource,
    gpu_dst: ID3D12Resource,
    readback: ID3D12Resource,
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
    pixel_count: usize,
    padded_pixel_count: usize,
    integral_pixel_count: usize,
}

struct PipelineSet {
    fixed_pass: ComputePipelineState,
    pad: ComputePipelineState,
    integral_h: ComputePipelineState,
    integral_v: ComputePipelineState,
    integral_h_f32: ComputePipelineState,
    integral_v_f32: ComputePipelineState,
    bg_max_h: ComputePipelineState,
    bg_max_v: ComputePipelineState,
    final_pass: ComputePipelineState,
}

pub struct HlslBinarizer {
    ctx: D3D12Context,
    pipelines: Option<PipelineSet>,
    buffers: Option<MultiPassBuffers>,
    verbose: bool,
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
        })
    }

    pub fn binarize_gray_raw(
        &mut self,
        gray: &[u8],
        params: &BinarizationParams,
    ) -> Result<Vec<u8>> {
        params.validate_gray(gray)?;
        match params.mode {
            BinarizationMode::FixedThreshold | BinarizationMode::Adaptive => {
                self.binarize(gray, params)
            }
        }
    }

    fn binarize(&mut self, gray: &[u8], params: &BinarizationParams) -> Result<Vec<u8>> {
        self.ensure_pipelines()?;

        let pixel_count = params.width as usize * params.height as usize;
        let src_bytes = pixel_count * 4;
        let dst_bytes = pixel_count * 4;

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

        self.ensure_buffers(
            src_bytes,
            dst_bytes,
            padded_pixel_count * 4,
            integral_pixel_count * 4,
        )?;

        let buffers = self
            .buffers
            .as_ref()
            .ok_or_else(|| GpuBinarizationError::Execution("buffers not initialized".into()))?;
        let pipelines = self
            .pipelines
            .as_ref()
            .ok_or_else(|| GpuBinarizationError::Shader("pipeline cache miss".into()))?;

        unsafe {
            let mut map_ptr: *mut c_void = std::ptr::null_mut();
            buffers
                .upload_src
                .Map(0, None, Some(&mut map_ptr))
                .map_err(|e| GpuBinarizationError::Execution(e.to_string()))?;
            let dst_u32 = map_ptr.cast::<u32>();
            for (i, &byte) in gray.iter().enumerate() {
                *dst_u32.add(i) = byte as u32;
            }
            buffers.upload_src.Unmap(0, None);

            self.ctx
                .reset_command_list()
                .map_err(|e| GpuBinarizationError::Execution(e.to_string()))?;

            // Re-set descriptor heap after command list reset
            let heaps: [Option<ID3D12DescriptorHeap>; 1] = [Some(self.ctx.descriptor_heap.clone())];
            unsafe {
                self.ctx.command_list.SetDescriptorHeaps(&heaps);
            }

            let heaps = [Some(self.ctx.descriptor_heap.clone())];
            self.ctx.command_list.SetDescriptorHeaps(&heaps);

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
                src_bytes as u64,
            );
            self.transition(
                &buffers.gpu_src,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_COMMON,
            );

            if params.mode == BinarizationMode::FixedThreshold {
                let cbuf = BinarizeParamsStd140 {
                    width: params.width,
                    height: params.height,
                    mode: 0,
                    invert_output: params.invert_output as u32,
                    fixed_threshold: params.fixed_threshold as u32,
                    sauvola_window: params.adaptive.sauvola_window,
                    bg_window: params.adaptive.bg_window,
                    otsu_threshold: params.adaptive.otsu_threshold as u32,
                    k_factor: params.k_factor,
                    percentile_c: params.adaptive.percentile_c as f32,
                    padded_width: 0,
                    padded_height: 0,
                    integral_width: 0,
                    sauvola_radius: 0,
                    debug_mode: 0,
                    _pad2: 0,
                    _pad3: 0,
                    _pad4: 0,
                    _pad5: 0,
                    _pad6: 0,
                    _pad7: 0,
                };
                self.upload_cbuf(&buffers.cb_final, &cbuf);

                self.create_srv(&buffers.gpu_src, 0, buffers.pixel_count as u32);
                self.create_uav(&buffers.gpu_dst, 1, buffers.pixel_count as u32);

                self.transition(
                    &buffers.gpu_dst,
                    D3D12_RESOURCE_STATE_COMMON,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                );

                self.ctx
                    .command_list
                    .SetPipelineState(&pipelines.fixed_pass.pipeline_state);
                self.ctx
                    .command_list
                    .SetComputeRootSignature(&pipelines.fixed_pass.root_signature);

                let gpu_heap_start = self.ctx.get_gpu_descriptor_handle(0);
                self.ctx
                    .command_list
                    .SetComputeRootDescriptorTable(0, gpu_heap_start);
                let mut gpu_uav = gpu_heap_start;
                gpu_uav.ptr += self.ctx.cbv_srv_uav_descriptor_size as u64;
                self.ctx
                    .command_list
                    .SetComputeRootDescriptorTable(1, gpu_uav);
                self.ctx
                    .command_list
                    .SetComputeRootConstantBufferView(2, buffers.cb_final.GetGPUVirtualAddress());

                self.ctx.command_list.Dispatch(
                    params.width.div_ceil(16),
                    params.height.div_ceil(16),
                    1,
                );

                self.transition(
                    &buffers.gpu_dst,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                );
                self.ctx.command_list.CopyBufferRegion(
                    &buffers.readback,
                    0,
                    &buffers.gpu_dst,
                    0,
                    dst_bytes as u64,
                );

                self.ctx
                    .execute_command_list()
                    .map_err(|e| GpuBinarizationError::Execution(e.to_string()))?;

                let mut gpu_words = vec![0u32; pixel_count];
                let mut rb_ptr: *mut c_void = std::ptr::null_mut();
                buffers
                    .readback
                    .Map(0, None, Some(&mut rb_ptr))
                    .map_err(|e| GpuBinarizationError::Execution(e.to_string()))?;
                copy_nonoverlapping(rb_ptr.cast::<u32>(), gpu_words.as_mut_ptr(), pixel_count);
                buffers.readback.Unmap(0, None);

                if self.verbose {
                    log::debug!(
                        "HLSL binarization dispatched (fixed): {}x{}",
                        params.width,
                        params.height
                    );
                }

                return Ok(gpu_words.into_iter().map(|v| v as u8).collect());
            }

            self.upload_pad_params(buffers, params, padded_width, padded_height, sauvola_radius);
            self.upload_integral_params(buffers, padded_width, padded_height, integral_width);
            self.upload_bg_params(buffers, params, bg_radius);
            self.upload_final_params(
                buffers,
                params,
                padded_width,
                padded_height,
                integral_width,
                sauvola_radius,
            );

            // Unique descriptor slots per pass to avoid heap aliasing.
            // All descriptors are written before GPU execution, so each pass
            // needs non-overlapping slots in the 16-entry descriptor heap.
            // pad: 0,1,2 | integral_h: 3,4 | integral_h_f32: 5,6
            // integral_v: 7,8 | integral_v_f32: 9,10 | final: 11-15

            self.create_srv(&buffers.gpu_src, 0, buffers.pixel_count as u32);
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
            self.ctx
                .command_list
                .SetPipelineState(&pipelines.pad.pipeline_state);
            self.ctx
                .command_list
                .SetComputeRootSignature(&pipelines.pad.root_signature);
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

            // integral_h: slots 3, 4
            self.create_srv(&buffers.padded_gray, 3, buffers.padded_pixel_count as u32);
            self.create_uav(&buffers.row_integral, 4, buffers.integral_pixel_count as u32);

            self.transition(
                &buffers.row_integral,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            );

            self.ctx
                .command_list
                .SetPipelineState(&pipelines.integral_h.pipeline_state);
            self.ctx
                .command_list
                .SetComputeRootSignature(&pipelines.integral_h.root_signature);
            self.set_roots_integral(buffers, 3, 4);

            self.ctx
                .command_list
                .Dispatch(1, padded_height, 1);

            // integral_h_f32: slots 5, 6
            self.create_srv(&buffers.padded_gray_sq, 5, buffers.padded_pixel_count as u32);
            self.create_uav(&buffers.row_integral_sq, 6, buffers.integral_pixel_count as u32);

            self.transition(
                &buffers.row_integral_sq,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            );

            self.ctx
                .command_list
                .SetPipelineState(&pipelines.integral_h_f32.pipeline_state);
            self.ctx
                .command_list
                .SetComputeRootSignature(&pipelines.integral_h_f32.root_signature);
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

            // integral_v: slots 7, 8
            self.create_srv(&buffers.row_integral, 7, buffers.integral_pixel_count as u32);
            self.create_uav(&buffers.integral, 8, buffers.integral_pixel_count as u32);

            self.transition(
                &buffers.integral,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            );

            self.ctx
                .command_list
                .SetPipelineState(&pipelines.integral_v.pipeline_state);
            self.ctx
                .command_list
                .SetComputeRootSignature(&pipelines.integral_v.root_signature);
            self.set_roots_integral(buffers, 7, 8);

            self.ctx
                .command_list
                .Dispatch(integral_width, 1, 1);

            // integral_v_f32: slots 9, 10
            self.create_srv(&buffers.row_integral_sq, 9, buffers.integral_pixel_count as u32);
            self.create_uav(&buffers.integral_sq, 10, buffers.integral_pixel_count as u32);

            self.transition(
                &buffers.integral_sq,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            );

            self.ctx
                .command_list
                .SetPipelineState(&pipelines.integral_v_f32.pipeline_state);
            self.ctx
                .command_list
                .SetComputeRootSignature(&pipelines.integral_v_f32.root_signature);
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

            // CPU bg estimate (GPU bg_max passes produce zeros due to descriptor issues)
            self.upload_cpu_bg(buffers, gray, params, sauvola_radius)?;

            self.transition(
                &buffers.bg_buffer,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            );

            // final_pass: slots 11, 12, 13, 14, 15
            self.create_srv(&buffers.gpu_src, 11, buffers.pixel_count as u32);
            self.create_srv(&buffers.integral, 12, buffers.integral_pixel_count as u32);
            self.create_srv(&buffers.integral_sq, 13, buffers.integral_pixel_count as u32);
            self.create_srv(&buffers.bg_buffer, 14, buffers.pixel_count as u32);
            self.create_uav(&buffers.gpu_dst, 15, buffers.pixel_count as u32);

            self.transition(
                &buffers.gpu_dst,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            );

            self.ctx
                .command_list
                .SetPipelineState(&pipelines.final_pass.pipeline_state);
            self.ctx
                .command_list
                .SetComputeRootSignature(&pipelines.final_pass.root_signature);
            self.set_roots_final_pass(buffers, 11, 12, 13, 14, 15);

            self.ctx.command_list.Dispatch(
                params.width.div_ceil(16),
                params.height.div_ceil(16),
                1,
            );

            self.transition(
                &buffers.gpu_dst,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            );
            self.ctx.command_list.CopyBufferRegion(
                &buffers.readback,
                0,
                &buffers.gpu_dst,
                0,
                dst_bytes as u64,
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

            let mut gpu_words = vec![0u32; pixel_count];
            let mut rb_ptr: *mut c_void = std::ptr::null_mut();
            buffers
                .readback
                .Map(0, None, Some(&mut rb_ptr))
                .map_err(|e| GpuBinarizationError::Execution(e.to_string()))?;
            copy_nonoverlapping(rb_ptr.cast::<u32>(), gpu_words.as_mut_ptr(), pixel_count);
            buffers.readback.Unmap(0, None);

            if self.verbose {
                log::debug!(
                    "HLSL binarization dispatched (multi-pass): {}x{}",
                    params.width,
                    params.height
                );
            }

            Ok(gpu_words.into_iter().map(|v| v as u8).collect())
        }
    }

    fn ensure_pipelines(&mut self) -> Result<()> {
        if self.pipelines.is_some() {
            return Ok(());
        }

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
        .map_err(|e| GpuBinarizationError::Shader(format!("fixed_pass: {}", e.to_string())))?;

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
        .map_err(|e| GpuBinarizationError::Shader(format!("pad: {}", e.to_string())))?;

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
        .map_err(|e| GpuBinarizationError::Shader(format!("integral_h: {}", e.to_string())))?;

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
        .map_err(|e| GpuBinarizationError::Shader(format!("integral_v: {}", e.to_string())))?;

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
        .map_err(|e| GpuBinarizationError::Shader(format!("integral_h_f32: {}", e.to_string())))?;

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
        .map_err(|e| GpuBinarizationError::Shader(format!("integral_v_f32: {}", e.to_string())))?;

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
        .map_err(|e| GpuBinarizationError::Shader(format!("bg_max_h: {}", e.to_string())))?;

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
        .map_err(|e| GpuBinarizationError::Shader(format!("bg_max_v: {}", e.to_string())))?;

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
        .map_err(|e| GpuBinarizationError::Shader(format!("final_pass: {}", e.to_string())))?;

        self.pipelines = Some(PipelineSet {
            fixed_pass,
            pad,
            integral_h,
            integral_v,
            integral_h_f32,
            integral_v_f32,
            bg_max_h,
            bg_max_v,
            final_pass,
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
        src_bytes: usize,
        dst_bytes: usize,
        padded_bytes: usize,
        integral_bytes: usize,
    ) -> Result<()> {
        let src_aligned = align_up(src_bytes, 256);
        let dst_aligned = align_up(dst_bytes, 256);
        let padded_aligned = align_up(padded_bytes, 256);
        let integral_aligned = align_up(integral_bytes, 256);
        let cb_aligned = 256;

        let needs_new = match &self.buffers {
            Some(b) => {
                b.pixel_count * 4 < src_bytes
                    || b.padded_pixel_count * 4 < padded_bytes
                    || b.integral_pixel_count * 4 < integral_bytes
            }
            None => true,
        };
        if !needs_new {
            return Ok(());
        }

        let upload_src = self
            .ctx
            .create_buffer(src_aligned, D3D12_HEAP_TYPE_UPLOAD)
            .map_err(|e| GpuBinarizationError::Initialization(e.to_string()))?;
        let upload_bg = self
            .ctx
            .create_buffer(src_aligned, D3D12_HEAP_TYPE_UPLOAD)
            .map_err(|e| GpuBinarizationError::Initialization(e.to_string()))?;
        let gpu_src = self
            .ctx
            .create_buffer(src_aligned, D3D12_HEAP_TYPE_DEFAULT)
            .map_err(|e| GpuBinarizationError::Initialization(e.to_string()))?;
        let gpu_dst = self
            .ctx
            .create_buffer(dst_aligned, D3D12_HEAP_TYPE_DEFAULT)
            .map_err(|e| GpuBinarizationError::Initialization(e.to_string()))?;
        let readback = self
            .ctx
            .create_buffer(dst_aligned, D3D12_HEAP_TYPE_READBACK)
            .map_err(|e| GpuBinarizationError::Initialization(e.to_string()))?;

        let padded_gray = self
            .ctx
            .create_buffer(padded_aligned, D3D12_HEAP_TYPE_DEFAULT)
            .map_err(|e| GpuBinarizationError::Initialization(e.to_string()))?;
        let padded_gray_sq = self
            .ctx
            .create_buffer(padded_aligned, D3D12_HEAP_TYPE_DEFAULT)
            .map_err(|e| GpuBinarizationError::Initialization(e.to_string()))?;
        let row_integral = self
            .ctx
            .create_buffer(integral_aligned, D3D12_HEAP_TYPE_DEFAULT)
            .map_err(|e| GpuBinarizationError::Initialization(e.to_string()))?;
        let row_integral_sq = self
            .ctx
            .create_buffer(integral_aligned, D3D12_HEAP_TYPE_DEFAULT)
            .map_err(|e| GpuBinarizationError::Initialization(e.to_string()))?;
        let integral = self
            .ctx
            .create_buffer(integral_aligned, D3D12_HEAP_TYPE_DEFAULT)
            .map_err(|e| GpuBinarizationError::Initialization(e.to_string()))?;
        let integral_sq = self
            .ctx
            .create_buffer(integral_aligned, D3D12_HEAP_TYPE_DEFAULT)
            .map_err(|e| GpuBinarizationError::Initialization(e.to_string()))?;
        let bg_tmp = self
            .ctx
            .create_buffer(src_aligned, D3D12_HEAP_TYPE_DEFAULT)
            .map_err(|e| GpuBinarizationError::Initialization(e.to_string()))?;
        let bg_buffer = self
            .ctx
            .create_buffer(src_aligned, D3D12_HEAP_TYPE_DEFAULT)
            .map_err(|e| GpuBinarizationError::Initialization(e.to_string()))?;

        let cb_pad = self
            .ctx
            .create_buffer(cb_aligned, D3D12_HEAP_TYPE_UPLOAD)
            .map_err(|e| GpuBinarizationError::Initialization(e.to_string()))?;
        let cb_integral = self
            .ctx
            .create_buffer(cb_aligned, D3D12_HEAP_TYPE_UPLOAD)
            .map_err(|e| GpuBinarizationError::Initialization(e.to_string()))?;
        let cb_bg = self
            .ctx
            .create_buffer(cb_aligned, D3D12_HEAP_TYPE_UPLOAD)
            .map_err(|e| GpuBinarizationError::Initialization(e.to_string()))?;
        let cb_final = self
            .ctx
            .create_buffer(cb_aligned, D3D12_HEAP_TYPE_UPLOAD)
            .map_err(|e| GpuBinarizationError::Initialization(e.to_string()))?;

        let pixel_count = src_bytes / 4;
        let padded_pixel_count = padded_bytes / 4;
        let integral_pixel_count = integral_bytes / 4;

        self.buffers = Some(MultiPassBuffers {
            upload_src,
            upload_bg,
            gpu_src,
            gpu_dst,
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

    unsafe fn upload_cbuf<T: Copy>(&self, cbuf: &ID3D12Resource, data: &T) {
        let mut mapped: *mut c_void = std::ptr::null_mut();
        cbuf.Map(0, None, Some(&mut mapped))
            .expect("cbuf map failed");
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
            mode: 1,
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

    fn upload_cpu_bg(
        &self,
        buffers: &MultiPassBuffers,
        gray: &[u8],
        params: &BinarizationParams,
        sauvola_radius: u32,
    ) -> Result<()> {
        let width = params.width as usize;
        let height = params.height as usize;
        let bg_window = if params.adaptive.bg_window > 0 {
            params.adaptive.bg_window as usize
        } else {
            let mut s = height / 200;
            if s < 3 { s = 3; }
            if s > 15 { s = 15; }
            if s % 2 == 0 { s += 1; }
            s
        };
        let r = bg_window / 2;

        // Horizontal max pass
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

        // Vertical max pass
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

        // Upload to GPU
        unsafe {
            let mut map_ptr: *mut c_void = std::ptr::null_mut();
            buffers.upload_bg.Map(0, None, Some(&mut map_ptr))
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
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
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
        if idx < 0 { -idx - 1 }
        else if idx >= len { 2 * len - idx - 1 }
        else { idx }
    }

    unsafe fn create_srv(
        &self,
        resource: &ID3D12Resource,
        slot: u32,
        num_elements: u32,
    ) {
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

    unsafe fn create_uav(
        &self,
        resource: &ID3D12Resource,
        slot: u32,
        num_elements: u32,
    ) {
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

    fn set_roots_pad(
        &self,
        buffers: &MultiPassBuffers,
        srv_slot: u32,
        uav_slot_0: u32,
        uav_slot_1: u32,
    ) {
        let gpu_heap_start = self.ctx.get_gpu_descriptor_handle(0);
        let mut gpu_srv = gpu_heap_start;
        gpu_srv.ptr += (srv_slot as u64) * self.ctx.cbv_srv_uav_descriptor_size as u64;
        let mut gpu_uav_0 = gpu_heap_start;
        gpu_uav_0.ptr += (uav_slot_0 as u64) * self.ctx.cbv_srv_uav_descriptor_size as u64;
        let mut gpu_uav_1 = gpu_heap_start;
        gpu_uav_1.ptr += (uav_slot_1 as u64) * self.ctx.cbv_srv_uav_descriptor_size as u64;

        unsafe {
            self.ctx
                .command_list
                .SetComputeRootDescriptorTable(0, gpu_srv);
            self.ctx
                .command_list
                .SetComputeRootDescriptorTable(1, gpu_uav_0);
            self.ctx
                .command_list
                .SetComputeRootDescriptorTable(2, gpu_uav_1);
            self.ctx
                .command_list
                .SetComputeRootConstantBufferView(3, buffers.cb_pad.GetGPUVirtualAddress());
        }
    }

    fn set_roots_integral(
        &self,
        buffers: &MultiPassBuffers,
        srv_slot: u32,
        uav_slot: u32,
    ) {
        let gpu_heap_start = self.ctx.get_gpu_descriptor_handle(0);
        let mut gpu_srv = gpu_heap_start;
        gpu_srv.ptr += (srv_slot as u64) * self.ctx.cbv_srv_uav_descriptor_size as u64;
        let mut gpu_uav = gpu_heap_start;
        gpu_uav.ptr += (uav_slot as u64) * self.ctx.cbv_srv_uav_descriptor_size as u64;

        unsafe {
            self.ctx
                .command_list
                .SetComputeRootDescriptorTable(0, gpu_srv);
            self.ctx
                .command_list
                .SetComputeRootDescriptorTable(1, gpu_uav);
            self.ctx
                .command_list
                .SetComputeRootConstantBufferView(2, buffers.cb_integral.GetGPUVirtualAddress());
        }
    }

    fn set_roots_bg(&self, buffers: &MultiPassBuffers, srv_slot: u32, uav_slot: u32) {
        let gpu_heap_start = self.ctx.get_gpu_descriptor_handle(0);
        let mut gpu_srv = gpu_heap_start;
        gpu_srv.ptr += (srv_slot as u64) * self.ctx.cbv_srv_uav_descriptor_size as u64;
        let mut gpu_uav = gpu_heap_start;
        gpu_uav.ptr += (uav_slot as u64) * self.ctx.cbv_srv_uav_descriptor_size as u64;

        unsafe {
            self.ctx
                .command_list
                .SetComputeRootDescriptorTable(0, gpu_srv);
            self.ctx
                .command_list
                .SetComputeRootDescriptorTable(1, gpu_uav);
            self.ctx
                .command_list
                .SetComputeRootConstantBufferView(2, buffers.cb_bg.GetGPUVirtualAddress());
        }
    }

    fn set_roots_final(
        &self,
        buffers: &MultiPassBuffers,
        srv_slot: u32,
        uav_slot: u32,
    ) {
        let gpu_heap_start = self.ctx.get_gpu_descriptor_handle(0);
        let mut gpu_srv = gpu_heap_start;
        gpu_srv.ptr += (srv_slot as u64) * self.ctx.cbv_srv_uav_descriptor_size as u64;
        let mut gpu_uav = gpu_heap_start;
        gpu_uav.ptr += (uav_slot as u64) * self.ctx.cbv_srv_uav_descriptor_size as u64;

        unsafe {
            self.ctx
                .command_list
                .SetComputeRootDescriptorTable(1, gpu_srv);
            self.ctx
                .command_list
                .SetComputeRootDescriptorTable(2, gpu_uav);
            self.ctx
                .command_list
                .SetComputeRootConstantBufferView(0, buffers.cb_final.GetGPUVirtualAddress());
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
        let gpu_heap_start = self.ctx.get_gpu_descriptor_handle(0);
        let mut gpu_srv_0 = gpu_heap_start;
        gpu_srv_0.ptr += (srv_0 as u64) * self.ctx.cbv_srv_uav_descriptor_size as u64;
        let mut gpu_srv_1 = gpu_heap_start;
        gpu_srv_1.ptr += (srv_1 as u64) * self.ctx.cbv_srv_uav_descriptor_size as u64;
        let mut gpu_srv_2 = gpu_heap_start;
        gpu_srv_2.ptr += (srv_2 as u64) * self.ctx.cbv_srv_uav_descriptor_size as u64;
        let mut gpu_srv_3 = gpu_heap_start;
        gpu_srv_3.ptr += (srv_3 as u64) * self.ctx.cbv_srv_uav_descriptor_size as u64;
        let mut gpu_uav = gpu_heap_start;
        gpu_uav.ptr += (uav_slot as u64) * self.ctx.cbv_srv_uav_descriptor_size as u64;

        unsafe {
            self.ctx
                .command_list
                .SetComputeRootDescriptorTable(0, gpu_srv_0);
            self.ctx
                .command_list
                .SetComputeRootDescriptorTable(1, gpu_srv_1);
            self.ctx
                .command_list
                .SetComputeRootDescriptorTable(2, gpu_srv_2);
            self.ctx
                .command_list
                .SetComputeRootDescriptorTable(3, gpu_srv_3);
            self.ctx
                .command_list
                .SetComputeRootDescriptorTable(4, gpu_uav);
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
