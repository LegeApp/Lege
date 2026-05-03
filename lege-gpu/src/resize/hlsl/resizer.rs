use super::dx12::{ComputePipelineState, D3D12Context};
use super::{BorderMode, FilterType, ResizerError, Result};
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::copy_nonoverlapping;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

/// Specific use cases for the resizer with optimized settings
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UseCase {
    /// PaddleX inference preprocessing (3000x2000 -> 640x640)
    PaddleX,
    /// OCR preprocessing (proportional resize to 1200px width)
    OCR,
    /// Margin calculation preprocessing (similar to OCR dimensions)
    MarginCalculation,
}

impl UseCase {
    /// Get recommended resize parameters for this use case
    pub fn default_parameters(&self, src_width: u32, src_height: u32) -> ResizeParameters {
        match self {
            UseCase::PaddleX => ResizeParameters {
                src_width,
                src_height,
                dst_width: 640,
                dst_height: 640,
                filter: FilterType::Bilinear,
                border_mode: BorderMode::Clamp,
                border_value: 0.0,
                channel_count: 3,
                no_srgb: false,
            },
            UseCase::OCR => {
                let aspect_ratio = src_height as f32 / src_width as f32;
                let dst_width = 1200;
                let dst_height = (dst_width as f32 * aspect_ratio).round() as u32;

                ResizeParameters {
                    src_width,
                    src_height,
                    dst_width,
                    dst_height,
                    filter: FilterType::Bell,
                    border_mode: BorderMode::Clamp,
                    border_value: 0.0,
                    channel_count: 3,
                    no_srgb: false,
                }
            }
            UseCase::MarginCalculation => {
                let aspect_ratio = src_height as f32 / src_width as f32;
                let dst_width = 1024;
                let dst_height = (dst_width as f32 * aspect_ratio).round() as u32;

                ResizeParameters {
                    src_width,
                    src_height,
                    dst_width,
                    dst_height,
                    filter: FilterType::Bilinear,
                    border_mode: BorderMode::Clamp,
                    border_value: 0.0,
                    channel_count: 3,
                    no_srgb: false,
                }
            }
        }
    }
}

/// Parameters for resize operation
#[derive(Debug, Clone)]
pub struct ResizeParameters {
    pub src_width: u32,
    pub src_height: u32,
    pub dst_width: u32,
    pub dst_height: u32,
    pub filter: FilterType,
    pub border_mode: BorderMode,
    pub border_value: f32,
    pub channel_count: u32,
    /// When true, disable sRGB<->linear conversions in shader (interpolate in sRGB/gamma space)
    pub no_srgb: bool,
}

impl ResizeParameters {
    pub fn new(src_width: u32, src_height: u32, dst_width: u32, dst_height: u32) -> Self {
        Self {
            src_width,
            src_height,
            dst_width,
            dst_height,
            filter: FilterType::default(),
            border_mode: BorderMode::default(),
            border_value: 0.0,
            channel_count: 3,
            no_srgb: false,
        }
    }

    pub fn with_filter(mut self, filter: FilterType) -> Self {
        self.filter = filter;
        self
    }

    pub fn with_border_mode(mut self, mode: BorderMode, value: f32) -> Self {
        self.border_mode = mode;
        self.border_value = value;
        self
    }

    pub fn with_channels(mut self, channels: u32) -> Self {
        self.channel_count = channels;
        self
    }

    pub fn with_no_srgb(mut self, no_srgb: bool) -> Self {
        self.no_srgb = no_srgb;
        self
    }

    pub fn validate(&self) -> Result<()> {
        if self.src_width == 0 || self.src_height == 0 {
            return Err(ResizerError::InvalidDimensions {
                width: self.src_width,
                height: self.src_height,
            });
        }

        if self.dst_width == 0 || self.dst_height == 0 {
            return Err(ResizerError::InvalidDimensions {
                width: self.dst_width,
                height: self.dst_height,
            });
        }

        if self.channel_count != 1 && self.channel_count != 3 && self.channel_count != 4 {
            return Err(ResizerError::InvalidChannelCount(self.channel_count));
        }

        Ok(())
    }

    pub fn src_buffer_size(&self) -> usize {
        (self.src_width * self.src_height * self.channel_count) as usize
    }

    pub fn dst_buffer_size(&self) -> usize {
        (self.dst_width * self.dst_height * self.channel_count) as usize
    }
}

/// GPU resize output with resource metadata
#[derive(Clone)]
pub struct GpuResizeOutput {
    pub resource: ID3D12Resource,
    pub width: u32,
    pub height: u32,
    pub channel_count: u32,
    pub bytes_per_pixel: u32,
    pub size_in_bytes: usize,
}

struct BufferSet {
    upload_src: ID3D12Resource,
    gpu_src: ID3D12Resource,
    gpu_dst: ID3D12Resource,
    readback: ID3D12Resource,
    cb_upload: ID3D12Resource,
    src_capacity: usize,
    dst_capacity: usize,
    ctx: Option<*const D3D12Context>, // Weak reference for deallocation tracking
}

impl Drop for BufferSet {
    fn drop(&mut self) {
        // Track memory deallocation if context is available
        if let Some(ctx_ptr) = self.ctx {
            unsafe {
                let ctx = &*ctx_ptr;
                let cb_size_aligned = align_up(size_of::<ResizeParamsCbuf>(), 256);

                // Track deallocation of all buffers
                ctx.deallocate_buffer(self.src_capacity); // upload_src
                ctx.deallocate_buffer(self.src_capacity); // gpu_src
                ctx.deallocate_buffer(self.dst_capacity); // gpu_dst
                ctx.deallocate_buffer(self.dst_capacity); // readback
                ctx.deallocate_buffer(cb_size_aligned); // cb_upload
            }
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ResizeParamsCbuf {
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
    scale_x: f32,
    scale_y: f32,
    offset_x: f32,
    offset_y: f32,
    border_mode: u32,
    border_value: f32,
    channel_count: u32,
    no_srgb: u32,
}

fn align_up(value: usize, alignment: usize) -> usize {
    (value + alignment - 1) & !(alignment - 1)
}

fn expand_to_rgba(src_data: &[u8], channel_count: u32, pixel_count: usize) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(pixel_count * 4);
    match channel_count {
        4 => rgba.extend_from_slice(src_data),
        3 => {
            for px in src_data.chunks_exact(3) {
                rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
        }
        1 => {
            for &g in src_data.iter() {
                rgba.extend_from_slice(&[g, g, g, 255]);
            }
        }
        _ => unreachable!("Unsupported channel count"),
    }
    debug_assert_eq!(rgba.len(), pixel_count * 4);
    rgba
}

/// High-performance HLSL-based image resizer
pub struct HlslResizer {
    ctx: D3D12Context,
    pipelines: HashMap<FilterType, ComputePipelineState>,
    buffers: Option<BufferSet>,
    verbose: bool,
}

impl HlslResizer {
    pub fn new() -> Result<Self> {
        let ctx = D3D12Context::new()?;
        Ok(Self {
            ctx,
            pipelines: HashMap::new(),
            buffers: None,
            verbose: std::env::var("LEGE_HLSL_VERBOSE").ok().as_deref() == Some("1"),
        })
    }

    pub fn with_verbose(verbose: bool) -> Result<Self> {
        let mut ctx = D3D12Context::new_with_verbose(verbose)?;
        ctx.set_verbose(verbose);
        Ok(Self {
            ctx,
            pipelines: HashMap::new(),
            buffers: None,
            verbose,
        })
    }

    pub fn for_use_case(use_case: UseCase) -> Result<Self> {
        let mut resizer = Self::new()?;
        resizer.ensure_pipeline(FilterType::recommended_for_use_case(use_case))?;
        Ok(resizer)
    }

    /// Expose the underlying D3D12 device for creating shared-context helpers.
    pub fn device(&self) -> &ID3D12Device {
        &self.ctx.device
    }

    pub fn resize(&mut self, src_data: &[u8], params: &ResizeParameters) -> Result<Vec<u8>> {
        params.validate()?;

        let src_size = params.src_buffer_size();
        if src_data.len() != src_size {
            return Err(ResizerError::BufferSizeMismatch {
                expected: src_size,
                actual: src_data.len(),
            });
        }

        if self.verbose {
            eprintln!(
                "[Resizer] Resize {:?}: {}x{} -> {}x{}, channels(req)={}, filter={:?}, border={:?} val={}",
                if cfg!(debug_assertions) {
                    "debug"
                } else {
                    "release"
                },
                params.src_width,
                params.src_height,
                params.dst_width,
                params.dst_height,
                params.channel_count,
                params.filter,
                params.border_mode,
                params.border_value
            );
            eprintln!(
                "[Resizer] src_size(req)={}, dst_size(req)={}",
                src_size,
                params.dst_buffer_size()
            );
        }

        self.ensure_pipeline(params.filter)?;

        let pixel_count_src = (params.src_width as usize) * (params.src_height as usize);
        let pixel_count_dst = (params.dst_width as usize) * (params.dst_height as usize);
        let gpu_src_size = pixel_count_src * 4;
        let gpu_dst_size = pixel_count_dst * 4;

        if self.verbose {
            eprintln!(
                "[Resizer] GPU sizes: src={} bytes ({} px * 4), dst={} bytes ({} px * 4)",
                gpu_src_size, pixel_count_src, gpu_dst_size, pixel_count_dst
            );
        }

        self.ensure_buffers(gpu_src_size, gpu_dst_size)?;
        let src_rgba = expand_to_rgba(src_data, params.channel_count, pixel_count_src);

        let buffers = self.buffers.as_ref().unwrap();
        if buffers.src_capacity < gpu_src_size || buffers.dst_capacity < gpu_dst_size {
            return Err(ResizerError::ResourceAllocationFailed(format!(
                "GPU buffer capacity smaller than required: src {} < {}, dst {} < {}",
                buffers.src_capacity, gpu_src_size, buffers.dst_capacity, gpu_dst_size
            )));
        }

        let result = self
            .dispatch_resize(params, &src_rgba, gpu_src_size, gpu_dst_size, true)?
            .ok_or_else(|| {
                ResizerError::CommandExecutionFailed(
                    "Missing readback data from GPU resize".to_string(),
                )
            })?;

        Ok(result)
    }

    pub fn resize_to_gpu(
        &mut self,
        src_data: &[u8],
        params: &ResizeParameters,
    ) -> Result<GpuResizeOutput> {
        params.validate()?;

        let src_size = params.src_buffer_size();
        if src_data.len() != src_size {
            return Err(ResizerError::BufferSizeMismatch {
                expected: src_size,
                actual: src_data.len(),
            });
        }

        self.ensure_pipeline(params.filter)?;

        let pixel_count_src = (params.src_width as usize) * (params.src_height as usize);
        let pixel_count_dst = (params.dst_width as usize) * (params.dst_height as usize);
        let gpu_src_size = pixel_count_src * 4;
        let gpu_dst_size = pixel_count_dst * 4;

        self.ensure_buffers(gpu_src_size, gpu_dst_size)?;
        let src_rgba = expand_to_rgba(src_data, params.channel_count, pixel_count_src);
        self.dispatch_resize(params, &src_rgba, gpu_src_size, gpu_dst_size, false)?;

        if let Some(buffers) = &self.buffers {
            Ok(GpuResizeOutput {
                resource: buffers.gpu_dst.clone(),
                width: params.dst_width,
                height: params.dst_height,
                channel_count: 4,
                bytes_per_pixel: 4,
                size_in_bytes: gpu_dst_size,
            })
        } else {
            Err(ResizerError::ResourceAllocationFailed(
                "GPU buffers were not initialized".to_string(),
            ))
        }
    }

    fn ensure_pipeline(&mut self, filter: FilterType) -> Result<()> {
        if self.pipelines.contains_key(&filter) {
            return Ok(());
        }

        let shader = filter.shader_bytecode();
        if shader.is_empty() {
            return Err(ResizerError::ShaderCompilationFailed(
                "Shader bytecode not available (was the build script run?)".to_string(),
            ));
        }

        let pso = ComputePipelineState::new(&self.ctx.device, shader)?;
        self.pipelines.insert(filter, pso);
        Ok(())
    }

    fn ensure_buffers(&mut self, src_bytes: usize, dst_bytes: usize) -> Result<()> {
        let src_aligned = align_up(src_bytes, 256);
        let dst_aligned = align_up(dst_bytes, 256);
        let cb_size_aligned = align_up(size_of::<ResizeParamsCbuf>(), 256);

        let needs_new = match &self.buffers {
            Some(existing) => {
                existing.src_capacity < src_aligned || existing.dst_capacity < dst_aligned
            }
            None => true,
        };

        if !needs_new {
            return Ok(());
        }

        if self.verbose {
            eprintln!(
                "[Resizer] Allocating GPU buffers: src_capacity={} dst_capacity={} cb_size={}",
                src_aligned, dst_aligned, cb_size_aligned
            );
        }

        self.buffers = None;

        let upload_src = self
            .ctx
            .create_buffer(src_aligned, D3D12_HEAP_TYPE_UPLOAD)?;
        let gpu_src = self
            .ctx
            .create_buffer(src_aligned, D3D12_HEAP_TYPE_DEFAULT)?;
        let gpu_dst = self
            .ctx
            .create_buffer(dst_aligned, D3D12_HEAP_TYPE_DEFAULT)?;
        let readback = self
            .ctx
            .create_buffer(dst_aligned, D3D12_HEAP_TYPE_READBACK)?;
        let cb_upload = self
            .ctx
            .create_buffer(cb_size_aligned, D3D12_HEAP_TYPE_UPLOAD)?;

        self.buffers = Some(BufferSet {
            upload_src,
            gpu_src,
            gpu_dst,
            readback,
            cb_upload,
            src_capacity: src_aligned,
            dst_capacity: dst_aligned,
            ctx: Some(&self.ctx as *const D3D12Context),
        });

        Ok(())
    }

    fn dispatch_resize(
        &mut self,
        params: &ResizeParameters,
        src_rgba: &[u8],
        gpu_src_size: usize,
        gpu_dst_size: usize,
        readback: bool,
    ) -> Result<Option<Vec<u8>>> {
        let buffers = self.buffers.as_ref().ok_or_else(|| {
            ResizerError::ResourceAllocationFailed("Internal buffer state missing".to_string())
        })?;

        unsafe {
            let upload_src = buffers.upload_src.clone();
            let mut mapped: *mut c_void = std::ptr::null_mut();
            upload_src.Map(0, None, Some(&mut mapped))?;
            copy_nonoverlapping(src_rgba.as_ptr(), mapped.cast(), gpu_src_size);
            upload_src.Unmap(0, None);

            if self.verbose {
                eprintln!("[Resizer] Upload: {} bytes", gpu_src_size);
            }

            self.ctx.reset_command_list()?;

            // Create SRV/UAV descriptors
            let cpu_srv = self.ctx.get_cpu_descriptor_handle(0);
            let cpu_uav = self.ctx.get_cpu_descriptor_handle(1);

            let gpu_src = buffers.gpu_src.clone();
            let gpu_dst = buffers.gpu_dst.clone();

            let srv_elements = (gpu_src_size / 4) as u32;
            let srv_desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT_R32_TYPELESS,
                ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Buffer: D3D12_BUFFER_SRV {
                        FirstElement: 0,
                        NumElements: srv_elements,
                        StructureByteStride: 0,
                        Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                    },
                },
            };
            self.ctx
                .device
                .CreateShaderResourceView(&gpu_src, Some(&srv_desc), cpu_srv);

            let uav_elements = (gpu_dst_size / 4) as u32;
            let uav_desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
                Format: DXGI_FORMAT_R32_TYPELESS,
                ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                    Buffer: D3D12_BUFFER_UAV {
                        FirstElement: 0,
                        NumElements: uav_elements,
                        StructureByteStride: 0,
                        CounterOffsetInBytes: 0,
                        Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                    },
                },
            };
            self.ctx
                .device
                .CreateUnorderedAccessView(&gpu_dst, None, Some(&uav_desc), cpu_uav);

            if self.verbose {
                let gpu_heap_start = self.ctx.get_gpu_descriptor_handle(0);
                let gpu_srv = gpu_heap_start;
                let mut gpu_uav = gpu_heap_start;
                gpu_uav.ptr += self.ctx.cbv_srv_uav_descriptor_size as u64;
                eprintln!(
                    "[Resizer] SRV NumElements={}, UAV NumElements={}, GPU SRV handle=0x{:X}, UAV handle=0x{:X}",
                    srv_elements, uav_elements, gpu_srv.ptr, gpu_uav.ptr
                );
            }

            let heaps = [Some(self.ctx.descriptor_heap.clone())];
            self.ctx.command_list.SetDescriptorHeaps(&heaps);

            let pso = self
                .pipelines
                .get(&params.filter)
                .expect("Pipeline must be initialized");
            self.ctx.command_list.SetPipelineState(&pso.pipeline_state);
            self.ctx
                .command_list
                .SetComputeRootSignature(&pso.root_signature);

            let scale_x = params.src_width as f32 / params.dst_width as f32;
            let scale_y = params.src_height as f32 / params.dst_height as f32;
            let cbuf = ResizeParamsCbuf {
                src_width: params.src_width,
                src_height: params.src_height,
                dst_width: params.dst_width,
                dst_height: params.dst_height,
                scale_x,
                scale_y,
                offset_x: 0.0,
                offset_y: 0.0,
                border_mode: params.border_mode as u32,
                border_value: params.border_value,
                channel_count: 4,
                no_srgb: if params.no_srgb { 1 } else { 0 },
            };

            let cb_upload = buffers.cb_upload.clone();
            let mut cb_ptr: *mut c_void = std::ptr::null_mut();
            cb_upload.Map(0, None, Some(&mut cb_ptr))?;
            copy_nonoverlapping(&cbuf, cb_ptr.cast::<ResizeParamsCbuf>(), 1);
            cb_upload.Unmap(0, None);

            let gpu_heap_start = self.ctx.get_gpu_descriptor_handle(0);
            let gpu_srv = gpu_heap_start;
            let mut gpu_uav = gpu_heap_start;
            gpu_uav.ptr += self.ctx.cbv_srv_uav_descriptor_size as u64;

            self.ctx
                .command_list
                .SetComputeRootConstantBufferView(0, cb_upload.GetGPUVirtualAddress());
            self.ctx
                .command_list
                .SetComputeRootDescriptorTable(1, gpu_srv);
            self.ctx
                .command_list
                .SetComputeRootDescriptorTable(2, gpu_uav);

            if self.verbose {
                eprintln!("[Resizer] Upload src_rgba: {} bytes", gpu_src_size);
            }
            // Transition gpu_src: COMMON -> COPY_DEST, then copy upload -> gpu_src
            {
                let transition = D3D12_RESOURCE_TRANSITION_BARRIER {
                    pResource: std::mem::ManuallyDrop::new(Some(gpu_src.clone())),
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: D3D12_RESOURCE_STATE_COMMON,
                    StateAfter: D3D12_RESOURCE_STATE_COPY_DEST,
                };
                let barrier = D3D12_RESOURCE_BARRIER {
                    Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                    Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                    Anonymous: D3D12_RESOURCE_BARRIER_0 {
                        Transition: std::mem::ManuallyDrop::new(transition),
                    },
                };
                self.ctx.command_list.ResourceBarrier(&[barrier]);
                if self.verbose {
                    eprintln!("[Resizer] Barrier: gpu_src COMMON -> COPY_DEST");
                }
            }

            self.ctx.command_list.CopyBufferRegion(
                &gpu_src,
                0,
                &upload_src,
                0,
                gpu_src_size as u64,
            );
            if self.verbose {
                eprintln!(
                    "[Resizer] CopyBufferRegion upload_src -> gpu_src ({} bytes)",
                    gpu_src_size
                );
            }

            // Transition gpu_src: COPY_DEST -> NON_PIXEL_SHADER_RESOURCE for SRV consumption
            {
                let transition = D3D12_RESOURCE_TRANSITION_BARRIER {
                    pResource: std::mem::ManuallyDrop::new(Some(gpu_src.clone())),
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: D3D12_RESOURCE_STATE_COPY_DEST,
                    StateAfter: D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                };
                let barrier = D3D12_RESOURCE_BARRIER {
                    Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                    Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                    Anonymous: D3D12_RESOURCE_BARRIER_0 {
                        Transition: std::mem::ManuallyDrop::new(transition),
                    },
                };
                self.ctx.command_list.ResourceBarrier(&[barrier]);
                if self.verbose {
                    eprintln!("[Resizer] Barrier: gpu_src COPY_DEST -> NON_PIXEL_SHADER_RESOURCE");
                }
            }

            {
                let transition = D3D12_RESOURCE_TRANSITION_BARRIER {
                    pResource: std::mem::ManuallyDrop::new(Some(gpu_dst.clone())),
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: D3D12_RESOURCE_STATE_COMMON,
                    StateAfter: D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                };
                let barrier = D3D12_RESOURCE_BARRIER {
                    Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                    Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                    Anonymous: D3D12_RESOURCE_BARRIER_0 {
                        Transition: std::mem::ManuallyDrop::new(transition),
                    },
                };
                self.ctx.command_list.ResourceBarrier(&[barrier]);
                if self.verbose {
                    eprintln!("[Resizer] Barrier: gpu_dst COMMON -> UNORDERED_ACCESS");
                }
            }

            let (tgx, tgy, _) = params.filter.thread_group_size();
            let groups_x = (params.dst_width + tgx - 1) / tgx;
            let groups_y = (params.dst_height + tgy - 1) / tgy;
            if self.verbose {
                eprintln!("[Resizer] Dispatch: groups=({}, {}, 1)", groups_x, groups_y);
            }
            self.ctx.command_list.Dispatch(groups_x, groups_y, 1);

            if readback {
                let transition = D3D12_RESOURCE_TRANSITION_BARRIER {
                    pResource: std::mem::ManuallyDrop::new(Some(gpu_dst.clone())),
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    StateAfter: D3D12_RESOURCE_STATE_COPY_SOURCE,
                };
                let barrier = D3D12_RESOURCE_BARRIER {
                    Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                    Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                    Anonymous: D3D12_RESOURCE_BARRIER_0 {
                        Transition: std::mem::ManuallyDrop::new(transition),
                    },
                };
                self.ctx.command_list.ResourceBarrier(&[barrier]);

                if self.verbose {
                    eprintln!("[Resizer] Barrier: gpu_dst UNORDERED_ACCESS -> COPY_SOURCE");
                }
                if self.verbose {
                    eprintln!("[Resizer] Copy gpu_dst->readback: {} bytes", gpu_dst_size);
                }
                let readback = buffers.readback.clone();
                self.ctx.command_list.CopyBufferRegion(
                    &readback,
                    0,
                    &gpu_dst,
                    0,
                    gpu_dst_size as u64,
                );
            } else {
                let transition = D3D12_RESOURCE_TRANSITION_BARRIER {
                    pResource: std::mem::ManuallyDrop::new(Some(gpu_dst.clone())),
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    StateAfter: D3D12_RESOURCE_STATE_GENERIC_READ,
                };
                let barrier = D3D12_RESOURCE_BARRIER {
                    Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                    Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                    Anonymous: D3D12_RESOURCE_BARRIER_0 {
                        Transition: std::mem::ManuallyDrop::new(transition),
                    },
                };
                self.ctx.command_list.ResourceBarrier(&[barrier]);
                if self.verbose {
                    eprintln!("[Resizer] Barrier: gpu_dst UNORDERED_ACCESS -> GENERIC_READ");
                }
            }

            self.ctx.execute_command_list()?;

            if readback {
                let mut gpu_out = vec![0u8; gpu_dst_size];
                let readback = buffers.readback.clone();
                let mut rb_ptr: *mut c_void = std::ptr::null_mut();
                readback.Map(0, None, Some(&mut rb_ptr))?;
                copy_nonoverlapping(rb_ptr.cast::<u8>(), gpu_out.as_mut_ptr(), gpu_dst_size);
                readback.Unmap(0, None);

                if self.verbose {
                    eprintln!("[Resizer] Readback completed: {} bytes", gpu_dst_size);
                }

                let mut out = vec![0u8; params.dst_buffer_size()];
                match params.channel_count {
                    4 => out.copy_from_slice(&gpu_out),
                    3 => {
                        for (i, dst_px) in out.chunks_exact_mut(3).enumerate() {
                            let s = i * 4;
                            dst_px[0] = gpu_out[s];
                            dst_px[1] = gpu_out[s + 1];
                            dst_px[2] = gpu_out[s + 2];
                        }
                    }
                    1 => {
                        for (i, dst_px) in out.iter_mut().enumerate() {
                            let s = i * 4;
                            *dst_px = gpu_out[s];
                        }
                    }
                    _ => unreachable!(),
                }

                Ok(Some(out))
            } else {
                Ok(None)
            }
        }
    }

    /// Enable/disable verbose logging at runtime
    pub fn set_verbose(&mut self, verbose: bool) {
        self.verbose = verbose;
        self.ctx.set_verbose(verbose);
    }

    /// Convenience method to resize with a specific use case
    pub fn resize_for_use_case(
        &mut self,
        src_data: &[u8],
        src_width: u32,
        src_height: u32,
        use_case: UseCase,
    ) -> Result<(Vec<u8>, ResizeParameters)> {
        let params = use_case.default_parameters(src_width, src_height);
        let result = self.resize(src_data, &params)?;
        Ok((result, params))
    }

    /// Internal: expose mutable access to the underlying D3D12 context for
    /// follow-up GPU passes (e.g., format conversion) that must share the same device.
    /// This is intended to be used by tightly-coupled components like HlslConverter.
    #[allow(dead_code)]
    pub(crate) fn context_mut(&mut self) -> &mut D3D12Context {
        &mut self.ctx
    }
}

// SAFETY: HlslResizer holds thread-safe Direct3D12 resources and all external access is
// synchronized by the engine before use.
unsafe impl Send for HlslResizer {}
unsafe impl Sync for HlslResizer {}
