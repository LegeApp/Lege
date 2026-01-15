use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::copy_nonoverlapping;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::dx12::{ComputePipelineState, D3D12Context};
use super::{GpuResizeOutput, ResizerError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConversionType {
    Rgba8ToNchwF32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ConversionParamsCbuf {
    width: u32,
    height: u32,
}

struct GpuBuffers {
    gpu_dst_nchw: ID3D12Resource,
    readback: ID3D12Resource,
    cb_upload: ID3D12Resource,
    dst_capacity_bytes: usize,
}

/// GPU-side converter: RGBA8 interleaved -> NCHW f32 planar tensor
pub struct HlslConverter {
    ctx: D3D12Context,
    pipelines: HashMap<ConversionType, ComputePipelineState>,
    buffers: Option<GpuBuffers>,
    verbose: bool,
}

impl HlslConverter {
    /// Create a new converter with its own Direct3D12 context
    pub fn new() -> Result<Self> {
        let ctx = D3D12Context::new()?;
        let verbose = ctx.verbose;
        Ok(Self { ctx, pipelines: HashMap::new(), buffers: None, verbose })
    }

    /// Create a new converter that shares the given D3D12 device.
    pub fn with_shared_device(device: &ID3D12Device, verbose: bool) -> Result<Self> {
        let ctx = D3D12Context::new_with_device(device.clone(), verbose)?;
        Ok(Self { ctx, pipelines: HashMap::new(), buffers: None, verbose })
    }

    pub fn set_verbose(&mut self, verbose: bool) {
        self.verbose = verbose;
        self.ctx.set_verbose(verbose);
    }

    pub fn convert_to_nchw_f32_on_gpu(
        &mut self,
        input: &GpuResizeOutput,
        batch_size: usize,
    ) -> Result<ID3D12Resource> {
        self.ensure_pipeline(ConversionType::Rgba8ToNchwF32)?;

        let dst_bytes = (batch_size * 3 * input.width as usize * input.height as usize) * size_of::<f32>();
        self.ensure_buffers(dst_bytes)?;

        let buffers = self.buffers.as_ref().ok_or_else(|| ResizerError::ResourceAllocationFailed("Converter buffers not initialized".into()))?;
        let pso = self.pipelines.get(&ConversionType::Rgba8ToNchwF32).expect("Pipeline must exist");

        unsafe {
            self.ctx.reset_command_list()?;

            // Descriptors
            let cpu_srv = self.ctx.get_cpu_descriptor_handle(0);
            let cpu_uav = self.ctx.get_cpu_descriptor_handle(1);

            // SRV for input RGBA8 buffer
            let srv_elements = (input.size_in_bytes / 4) as u32;
            if self.verbose { eprintln!("[Convert] SRV elements: {} ({} bytes total)", srv_elements, input.size_in_bytes); }
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
            self.ctx.device.CreateShaderResourceView(&input.resource, Some(&srv_desc), cpu_srv);

            // UAV for output NCHW f32 buffer
            let uav_elements = (dst_bytes / 4) as u32;
            if self.verbose { eprintln!("[Convert] UAV elements: {} ({} bytes total)", uav_elements, dst_bytes); }
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
            self.ctx.device.CreateUnorderedAccessView(&buffers.gpu_dst_nchw, None, Some(&uav_desc), cpu_uav);

            let heaps = [Some(self.ctx.descriptor_heap.clone())];
            self.ctx.command_list.SetDescriptorHeaps(&heaps);
            self.ctx.command_list.SetPipelineState(&pso.pipeline_state);
            self.ctx.command_list.SetComputeRootSignature(&pso.root_signature);

            // Constant buffer
            let cbuf = ConversionParamsCbuf { width: input.width, height: input.height };
            let mut cb_ptr: *mut c_void = std::ptr::null_mut();
            buffers.cb_upload.Map(0, None, Some(&mut cb_ptr))?;
            copy_nonoverlapping(&cbuf, cb_ptr.cast::<ConversionParamsCbuf>(), 1);
            buffers.cb_upload.Unmap(0, None);

            // Bind root parameters
            let gpu_heap_start = self.ctx.get_gpu_descriptor_handle(0);
            let gpu_srv = gpu_heap_start;
            let mut gpu_uav = gpu_heap_start;
            gpu_uav.ptr += self.ctx.cbv_srv_uav_descriptor_size as u64;

            self.ctx.command_list.SetComputeRootConstantBufferView(0, buffers.cb_upload.GetGPUVirtualAddress());
            self.ctx.command_list.SetComputeRootDescriptorTable(1, gpu_srv);
            self.ctx.command_list.SetComputeRootDescriptorTable(2, gpu_uav);

            // Barriers
            if self.verbose { eprintln!("[Convert] Barrier: input GENERIC_READ -> NON_PIXEL_SHADER_RESOURCE"); }
            self.transition_barrier(&input.resource, D3D12_RESOURCE_STATE_GENERIC_READ, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE);
            if self.verbose { eprintln!("[Convert] Barrier: dst COMMON -> UNORDERED_ACCESS"); }
            self.transition_barrier(&buffers.gpu_dst_nchw, D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_UNORDERED_ACCESS);

            // Dispatch
            let (tgx, tgy) = (16u32, 16u32);
            let groups_x = (input.width + tgx - 1) / tgx;
            let groups_y = (input.height + tgy - 1) / tgy;
            if self.verbose { eprintln!("[Convert] Dispatch: groups=({}, {}, {})", groups_x, groups_y, batch_size as u32); }
            self.ctx.command_list.Dispatch(groups_x, groups_y, batch_size as u32);

            // Prepare for read by DML / ORT
            if self.verbose { eprintln!("[Convert] Barrier: dst UNORDERED_ACCESS -> GENERIC_READ"); }
            self.transition_barrier(&buffers.gpu_dst_nchw, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_GENERIC_READ);

            self.ctx.execute_command_list()?;
        }

        Ok(self.buffers.as_ref().unwrap().gpu_dst_nchw.clone())
    }

    fn ensure_pipeline(&mut self, ctype: ConversionType) -> Result<()> {
        if self.pipelines.contains_key(&ctype) {
            return Ok(());
        }
        // Shader bytecode is generated by build.rs into env var include
        let shader: &[u8] = super::RGBA8_TO_NCHW_F32_SHADER;
        if shader.is_empty() {
            return Err(ResizerError::ShaderCompilationFailed("Converter shader bytecode not available".into()));
        }
        let pso = ComputePipelineState::new(&self.ctx.device, shader)?;
        self.pipelines.insert(ctype, pso);
        Ok(())
    }

    fn ensure_buffers(&mut self, dst_bytes: usize) -> Result<()> {
        let needs_new = match &self.buffers {
            Some(b) => b.dst_capacity_bytes < dst_bytes,
            None => true,
        };
        if !needs_new { return Ok(()); }

        let cb_size = (size_of::<ConversionParamsCbuf>() + 255) & !255;
        let gpu_dst_nchw = self.ctx.create_buffer(dst_bytes, D3D12_HEAP_TYPE_DEFAULT)?;
        let readback = self.ctx.create_buffer(dst_bytes, D3D12_HEAP_TYPE_READBACK)?;
        let cb_upload = self.ctx.create_buffer(cb_size, D3D12_HEAP_TYPE_UPLOAD)?;
        self.buffers = Some(GpuBuffers { gpu_dst_nchw, readback, cb_upload, dst_capacity_bytes: dst_bytes });
        Ok(())
    }

    fn transition_barrier(&self, resource: &ID3D12Resource, before: D3D12_RESOURCE_STATES, after: D3D12_RESOURCE_STATES) {
        unsafe {
            let transition = D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: std::mem::ManuallyDrop::new(Some(resource.clone())),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            };
            let barrier = D3D12_RESOURCE_BARRIER {
                Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                Anonymous: D3D12_RESOURCE_BARRIER_0 { Transition: std::mem::ManuallyDrop::new(transition) },
            };
            self.ctx.command_list.ResourceBarrier(&[barrier]);
        }
    }

    /// Convenience: run conversion on GPU and read back into CPU Vec<f32>
    pub fn convert_to_nchw_f32_then_readback(
        &mut self,
        input: &GpuResizeOutput,
        batch_size: usize,
    ) -> Result<Vec<f32>> {
        let dst_res = self.convert_to_nchw_f32_on_gpu(input, batch_size)?;

        let buffers = self.buffers.as_ref().ok_or_else(|| ResizerError::ResourceAllocationFailed("Converter buffers not initialized".into()))?;
        let dst_bytes = (batch_size * 3 * input.width as usize * input.height as usize) * size_of::<f32>();

        unsafe {
            // Transition to COPY_SOURCE for readback copy
            self.ctx.reset_command_list()?;
            self.transition_barrier(&dst_res, D3D12_RESOURCE_STATE_GENERIC_READ, D3D12_RESOURCE_STATE_COPY_SOURCE);
            self.ctx.command_list.CopyBufferRegion(&buffers.readback, 0, &dst_res, 0, dst_bytes as u64);
            // Optionally transition back to GENERIC_READ
            self.transition_barrier(&dst_res, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_GENERIC_READ);
            self.ctx.execute_command_list()?;

            // Map and copy to Vec<f32>
            let mut out = vec![0f32; dst_bytes / size_of::<f32>()];
            let mut mapped: *mut c_void = std::ptr::null_mut();
            buffers.readback.Map(0, None, Some(&mut mapped))?;
            copy_nonoverlapping(mapped.cast::<u8>(), out.as_mut_ptr().cast::<u8>(), dst_bytes);
            buffers.readback.Unmap(0, None);
            if self.verbose { eprintln!("[Convert] Readback NCHW f32 completed: {} floats", out.len()); }

            // Optional dump small sample for inspection
            if std::env::var("LEGE_DUMP_HLSL_CONVERT").ok().as_deref() == Some("1") {
                let sample = out.iter().take(64).cloned().collect::<Vec<_>>();
                let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
                let filename = format!("hlsl-convert-nchw-sample-{}.txt", ts);
                let mut s = String::new();
                for (i, v) in sample.iter().enumerate() {
                    if i > 0 { s.push_str(", "); }
                    s.push_str(&format!("{:.6}", v));
                }
                let _ = std::fs::write(&filename, s);
                if self.verbose { eprintln!("[Convert] Dumped NCHW sample to {}", filename); }
            }
            Ok(out)
        }
    }
}

// SAFETY: Holds D3D12 resources; synchronized by engine
unsafe impl Send for HlslConverter {}
unsafe impl Sync for HlslConverter {}
