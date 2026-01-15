use std::mem;
use std::ptr;
use windows::{
    core::*,
    Win32::Foundation::*,
    Win32::Graphics::{
        Direct3D::*,
        Direct3D12::*,
        Dxgi::{*, Common::*},
    },
    Win32::System::Threading::*,
};
use windows::core::PCWSTR;
use super::{ResizerError, Result};

/// DirectX 12 context for compute operations
pub struct D3D12Context {
    pub device: ID3D12Device,
    pub command_queue: ID3D12CommandQueue,
    pub command_allocator: ID3D12CommandAllocator,
    pub command_list: ID3D12GraphicsCommandList,
    pub fence: ID3D12Fence,
    pub fence_event: HANDLE,
    pub fence_value: u64,
    pub descriptor_heap: ID3D12DescriptorHeap,
    pub cbv_srv_uav_descriptor_size: u32,
    pub verbose: bool,
    pub dedicated_video_memory: u64,
    pub allocated_memory: std::sync::atomic::AtomicU64,
}

impl D3D12Context {
    /// Create a new DirectX 12 context for compute operations
    pub fn new() -> Result<Self> {
        let verbose = std::env::var("LEGE_HLSL_VERBOSE").ok().as_deref() == Some("1");
        Self::new_with_verbose(verbose)
    }

    /// Create a new DirectX 12 context with optional verbose logging
    pub fn new_with_verbose(verbose: bool) -> Result<Self> {
        unsafe {
            // Enable debug layer in debug builds
            #[cfg(feature = "debug-layers")]
            {
                if let Ok(debug) = D3D12GetDebugInterface::<ID3D12Debug>() {
                    debug.EnableDebugLayer();
                }
            }

            // Create DXGI factory
            let dxgi_factory: IDXGIFactory4 = CreateDXGIFactory1()?;

            // Find the best adapter (prefer high-performance GPU)
            let mut adapter: Option<IDXGIAdapter1> = None;
            for i in 0.. {
                match dxgi_factory.EnumAdapters1(i) {
                    Ok(candidate_adapter) => {
                        let desc = candidate_adapter.GetDesc1()?;

                        // Skip software adapters
                        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                            continue;
                        }

                        // Try to create device with this adapter
                        let mut test_device: Option<ID3D12Device> = None;
                        if D3D12CreateDevice(&candidate_adapter, D3D_FEATURE_LEVEL_11_0, &mut test_device).is_ok() {
                            adapter = Some(candidate_adapter);
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }

            let adapter = adapter.ok_or_else(||
                ResizerError::D3D12InitializationFailed("No compatible adapter found".to_string())
            )?;

            // Create device
            let mut device_opt: Option<ID3D12Device> = None;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_11_0, &mut device_opt)?;
            let device: ID3D12Device = device_opt.unwrap();

            // Create command queue
            let queue_desc = D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_COMPUTE,
                Priority: D3D12_COMMAND_QUEUE_PRIORITY_NORMAL.0,
                Flags: D3D12_COMMAND_QUEUE_FLAG_NONE,
                NodeMask: 0,
            };
            let command_queue: ID3D12CommandQueue = device.CreateCommandQueue(&queue_desc)?;

            // Create command allocator
            let command_allocator: ID3D12CommandAllocator =
                device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COMPUTE)?;

            // Create command list
            let command_list: ID3D12GraphicsCommandList = device.CreateCommandList(
                0,
                D3D12_COMMAND_LIST_TYPE_COMPUTE,
                &command_allocator,
                None,
            )?;
            command_list.Close()?;

            // Create fence for GPU synchronization
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
            let fence_event = CreateEventW(None, false, false, PCWSTR::null())?;
            if fence_event.is_invalid() {
                return Err(ResizerError::D3D12InitializationFailed("CreateEventW failed".to_string()));
            }

            // Create descriptor heap for UAV/SRV
            let heap_desc = D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 8, // Enough for source, destination, and constant buffers
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            };
            let descriptor_heap = device.CreateDescriptorHeap(&heap_desc)?;

            let cbv_srv_uav_descriptor_size = device
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);

            // Get dedicated video memory from adapter
            let dedicated_video_memory = if let Ok(desc1) = adapter.GetDesc1() {
                desc1.DedicatedVideoMemory as u64
            } else {
                1024 * 1024 * 1024 // Default to 1GB if we can't get the actual value
            };

            let ctx = Self {
                device,
                command_queue,
                command_allocator,
                command_list,
                fence,
                fence_event,
                fence_value: 1,
                descriptor_heap,
                cbv_srv_uav_descriptor_size,
                verbose,
                dedicated_video_memory,
                allocated_memory: std::sync::atomic::AtomicU64::new(0),
            };

            if ctx.verbose {
                // Log adapter information
                if let Ok(desc1) = adapter.GetDesc1() {
                    let name = {
                        let slice = &desc1.Description;
                        let len = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
                        String::from_utf16_lossy(&slice[..len])
                    };
                    eprintln!("[DX12] Adapter: {} (Vendor {:04X}, Device {:04X})", name, desc1.VendorId, desc1.DeviceId);
                    eprintln!("[DX12] DedicatedVideoMemory: {} MB", (ctx.dedicated_video_memory / (1024*1024)));
                }
                eprintln!("[DX12] Descriptor increment size (CBV/SRV/UAV): {}", ctx.cbv_srv_uav_descriptor_size);
            }

            Ok(ctx)
        }
    }

    /// Create a new DirectX 12 context that shares an existing device.
    /// This allows multiple pipelines to interoperate on the same resources.
    pub fn new_with_device(existing_device: ID3D12Device, verbose: bool) -> Result<Self> {
        unsafe {
            // Create command queue
            let queue_desc = D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_COMPUTE,
                Priority: D3D12_COMMAND_QUEUE_PRIORITY_NORMAL.0,
                Flags: D3D12_COMMAND_QUEUE_FLAG_NONE,
                NodeMask: 0,
            };
            let command_queue: ID3D12CommandQueue = existing_device.CreateCommandQueue(&queue_desc)?;

            // Create command allocator
            let command_allocator: ID3D12CommandAllocator =
                existing_device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COMPUTE)?;

            // Create command list
            let command_list: ID3D12GraphicsCommandList = existing_device.CreateCommandList(
                0,
                D3D12_COMMAND_LIST_TYPE_COMPUTE,
                &command_allocator,
                None,
            )?;
            command_list.Close()?;

            // Create fence for GPU synchronization
            let fence: ID3D12Fence = existing_device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
            let fence_event = CreateEventW(None, false, false, PCWSTR::null())?;
            if fence_event.is_invalid() {
                return Err(ResizerError::D3D12InitializationFailed("CreateEventW failed".to_string()));
            }

            // Create descriptor heap for UAV/SRV
            let heap_desc = D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 8,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            };
            let descriptor_heap = existing_device.CreateDescriptorHeap(&heap_desc)?;

            let cbv_srv_uav_descriptor_size = existing_device
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);

            let ctx = Self {
                device: existing_device,
                command_queue,
                command_allocator,
                command_list,
                fence,
                fence_event,
                fence_value: 1,
                descriptor_heap,
                cbv_srv_uav_descriptor_size,
                verbose,
                dedicated_video_memory: 1024 * 1024 * 1024, // Default to 1GB for shared device
                allocated_memory: std::sync::atomic::AtomicU64::new(0),
            };

            if ctx.verbose {
                eprintln!("[DX12] Shared device context created. Descriptor increment size (CBV/SRV/UAV): {}", ctx.cbv_srv_uav_descriptor_size);
            }

            Ok(ctx)
        }
    }

    /// Create a buffer resource
    pub fn create_buffer(&self, size: usize, heap_type: D3D12_HEAP_TYPE) -> Result<ID3D12Resource> {
        unsafe {
            // Check for memory pressure before allocation
            let current_allocated = self.allocated_memory.load(std::sync::atomic::Ordering::Relaxed);
            let new_total = current_allocated + size as u64;
            
            // Use 80% of dedicated video memory as threshold to prevent exhaustion
            let memory_threshold = (self.dedicated_video_memory * 80) / 100;
            
            if new_total > memory_threshold {
                if self.verbose {
                    eprintln!("[DX12] Memory pressure detected: current={} MB, requested={} MB, threshold={} MB", 
                        current_allocated / (1024*1024), 
                        size / (1024*1024), 
                        memory_threshold / (1024*1024));
                }
                return Err(ResizerError::ResourceAllocationFailed(format!(
                    "GPU memory pressure: would exceed {}% of available memory ({} MB)", 
                    80, memory_threshold / (1024*1024)
                )));
            }
            
            if self.verbose {
                eprintln!("[DX12] Create buffer: size={} bytes, heap={:?}, allocated={} MB", 
                    size, heap_type, current_allocated / (1024*1024));
            }
            let heap_props = D3D12_HEAP_PROPERTIES {
                Type: heap_type,
                CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
                MemoryPoolPreference: D3D12_MEMORY_POOL_UNKNOWN,
                CreationNodeMask: 0,
                VisibleNodeMask: 0,
            };

            let resource_desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Alignment: 0,
                Width: size as u64,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_UNKNOWN,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                Flags: match heap_type {
                    D3D12_HEAP_TYPE_DEFAULT => D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                    _ => D3D12_RESOURCE_FLAG_NONE,
                },
            };

            let initial_state = match heap_type {
                D3D12_HEAP_TYPE_UPLOAD => D3D12_RESOURCE_STATE_GENERIC_READ,
                D3D12_HEAP_TYPE_READBACK => D3D12_RESOURCE_STATE_COPY_DEST,
                _ => D3D12_RESOURCE_STATE_COMMON,
            };

            let mut resource: Option<ID3D12Resource> = None;
            self.device.CreateCommittedResource(
                &heap_props,
                D3D12_HEAP_FLAG_NONE,
                &resource_desc,
                initial_state,
                None,
                &mut resource,
            )?;

            match resource {
                Some(res) => {
                    // Track allocated memory
                    self.allocated_memory.fetch_add(size as u64, std::sync::atomic::Ordering::Relaxed);
                    if self.verbose {
                        let total_allocated = self.allocated_memory.load(std::sync::atomic::Ordering::Relaxed);
                        eprintln!("[DX12] Buffer allocated successfully. Total allocated: {} MB", 
                            total_allocated / (1024*1024));
                    }
                    Ok(res)
                }
                None => Err(ResizerError::ResourceAllocationFailed(format!(
                    "Failed to create DirectX buffer: size={} bytes, heap_type={:?}", 
                    size, heap_type
                )))
            }
        }
    }

    /// Wait for GPU to finish current work
    pub fn wait_for_gpu(&mut self) -> Result<()> {
        unsafe {
            let current_fence_value = self.fence_value;
            self.command_queue.Signal(&self.fence, current_fence_value)?;
            self.fence_value += 1;

            if self.fence.GetCompletedValue() < current_fence_value {
                self.fence.SetEventOnCompletion(current_fence_value, self.fence_event)?;
                WaitForSingleObject(self.fence_event, INFINITE);
            }
        }
        Ok(())
    }

    /// Reset command list for recording new commands
    pub fn reset_command_list(&self) -> Result<()> {
        unsafe {
            if self.verbose { eprintln!("[DX12] Reset command list"); }
            self.command_allocator.Reset()?;
            self.command_list.Reset(&self.command_allocator, None)?;
        }
        Ok(())
    }

    /// Execute command list
    pub fn execute_command_list(&mut self) -> Result<()> {
        unsafe {
            if self.verbose { eprintln!("[DX12] Close and submit command list"); }
            self.command_list.Close()?;

            let command_lists = [Some(self.command_list.cast()?)];
            self.command_queue.ExecuteCommandLists(&command_lists);

            if self.verbose {
                eprintln!("[DX12] ExecuteCommandLists submitted; waiting for GPU fence {}", self.fence_value);
            }
            self.wait_for_gpu()?;
            if self.verbose {
                eprintln!("[DX12] GPU work completed (fence advanced to {})", self.fence_value);
            }
        }
        Ok(())
    }

    /// Enable or disable verbose logging
    pub fn set_verbose(&mut self, verbose: bool) {
        self.verbose = verbose;
    }

    /// Get CPU descriptor handle at the given index
    pub fn get_cpu_descriptor_handle(&self, index: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        unsafe {
            let mut handle = self.descriptor_heap.GetCPUDescriptorHandleForHeapStart();
            handle.ptr += (index * self.cbv_srv_uav_descriptor_size) as usize;
            handle
        }
    }

    /// Get GPU descriptor handle at the given index
    pub fn get_gpu_descriptor_handle(&self, index: u32) -> D3D12_GPU_DESCRIPTOR_HANDLE {
        unsafe {
            let mut handle = self.descriptor_heap.GetGPUDescriptorHandleForHeapStart();
            handle.ptr += (index * self.cbv_srv_uav_descriptor_size) as u64;
            handle
        }
    }

    /// Track buffer deallocation for memory management
    pub fn deallocate_buffer(&self, size: usize) {
        let current = self.allocated_memory.fetch_sub(size as u64, std::sync::atomic::Ordering::Relaxed);
        if self.verbose {
            eprintln!("[DX12] Buffer deallocated: size={} bytes, remaining={} MB", 
                size, (current - size as u64) / (1024*1024));
        }
    }
}

impl Drop for D3D12Context {
    fn drop(&mut self) {
        if let Err(e) = self.wait_for_gpu() {
            eprintln!("Failed to wait for GPU during cleanup: {}", e);
        }

        unsafe {
            if !self.fence_event.is_invalid() {
                let _ = CloseHandle(self.fence_event);
            }
        }
    }
}

/// Pipeline state for compute shaders
pub struct ComputePipelineState {
    pub pipeline_state: ID3D12PipelineState,
    pub root_signature: ID3D12RootSignature,
}

impl ComputePipelineState {
    /// Create a new compute pipeline state from shader bytecode
    pub fn new(device: &ID3D12Device, shader_bytecode: &[u8]) -> Result<Self> {
        unsafe {
            // Create root signature
            // Descriptor ranges need stable storage
            let srv_range = [D3D12_DESCRIPTOR_RANGE {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
                NumDescriptors: 1,
                BaseShaderRegister: 0,
                RegisterSpace: 0,
                OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
            }];
            let uav_range = [D3D12_DESCRIPTOR_RANGE {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
                NumDescriptors: 1,
                BaseShaderRegister: 0,
                RegisterSpace: 0,
                OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
            }];

            let root_parameters = [
                // CBV for constants at b0
                D3D12_ROOT_PARAMETER {
                    ParameterType: D3D12_ROOT_PARAMETER_TYPE_CBV,
                    Anonymous: D3D12_ROOT_PARAMETER_0 {
                        Descriptor: D3D12_ROOT_DESCRIPTOR {
                            ShaderRegister: 0,
                            RegisterSpace: 0,
                        },
                    },
                    ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
                },
                // SRV descriptor table for input at t0
                D3D12_ROOT_PARAMETER {
                    ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
                    Anonymous: D3D12_ROOT_PARAMETER_0 {
                        DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                            NumDescriptorRanges: srv_range.len() as u32,
                            pDescriptorRanges: srv_range.as_ptr(),
                        },
                    },
                    ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
                },
                // UAV descriptor table for output at u0
                D3D12_ROOT_PARAMETER {
                    ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
                    Anonymous: D3D12_ROOT_PARAMETER_0 {
                        DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                            NumDescriptorRanges: uav_range.len() as u32,
                            pDescriptorRanges: uav_range.as_ptr(),
                        },
                    },
                    ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
                },
            ];

            let root_signature_desc = D3D12_ROOT_SIGNATURE_DESC {
                NumParameters: root_parameters.len() as u32,
                pParameters: root_parameters.as_ptr(),
                NumStaticSamplers: 0,
                pStaticSamplers: ptr::null(),
                Flags: D3D12_ROOT_SIGNATURE_FLAG_NONE,
            };

            let mut signature_blob: Option<ID3DBlob> = None;
            let mut error_blob: Option<ID3DBlob> = None;

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

            // Create compute pipeline state
            let compute_pipeline_desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: mem::transmute_copy(&root_signature),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: shader_bytecode.as_ptr() as *const std::ffi::c_void,
                    BytecodeLength: shader_bytecode.len(),
                },
                NodeMask: 0,
                CachedPSO: D3D12_CACHED_PIPELINE_STATE {
                    pCachedBlob: ptr::null(),
                    CachedBlobSizeInBytes: 0,
                },
                Flags: D3D12_PIPELINE_STATE_FLAG_NONE,
            };

            let pipeline_state = device.CreateComputePipelineState(&compute_pipeline_desc)?;

            Ok(Self {
                pipeline_state,
                root_signature,
            })
        }
    }
}

// SAFETY: The Direct3D12 context manages COM interfaces and synchronization primitives that
// are safe to move across threads. External synchronization is enforced by the caller.
unsafe impl Send for D3D12Context {}
unsafe impl Sync for D3D12Context {}