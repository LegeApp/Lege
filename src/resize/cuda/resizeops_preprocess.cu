#include "preprocess.h"
#include "warpaffine.h"
#include "reformat.h"
#include "cuda_utils.h"
#include <cuda_runtime.h>
#include <vector_types.h> // For make_float3
#include <cuda_fp16.h>
#include <stdexcept>
#include <iostream>
#include <string>

using std::cout;
using std::endl;

namespace document_layout {

// constexpr int PADDING_SIZE = 24;  // Currently unused - commented out to suppress warning

static unsigned char* img_buffer_host = nullptr;
static unsigned char* img_buffer_device = nullptr;
static float* temp_buffer_device = nullptr; // Temporary buffer for HWC output

// CUDA kernel for FP32 to FP16 casting
__global__ void cast_fp32_to_fp16(const float* src, half* dst, size_t num_elements) {
    int idx = blockIdx.x * blockDim.x + threadIdx.x;
    if (idx < num_elements) {
        dst[idx] = __float2half(src[idx]);
    }
}

void cuda_preprocess_init(int max_image_size) {
    cudaError_t err;
    err = cudaMallocHost((void**)&img_buffer_host, max_image_size * 3);
    if (err != cudaSuccess) {
        throw std::runtime_error("Failed to allocate img_buffer_host: " + std::string(cudaGetErrorString(err)));
    }
    err = cudaMalloc((void**)&img_buffer_device, max_image_size * 3);
    if (err != cudaSuccess) {
        cudaFreeHost(img_buffer_host);
        throw std::runtime_error("Failed to allocate img_buffer_device: " + std::string(cudaGetErrorString(err)));
    }
    err = cudaMalloc((void**)&temp_buffer_device, max_image_size * 3 * sizeof(float));
    if (err != cudaSuccess) {
        cudaFree(img_buffer_device);
        cudaFreeHost(img_buffer_host);
        throw std::runtime_error("Failed to allocate temp_buffer_device: " + std::string(cudaGetErrorString(err)));
    }
}

void cuda_preprocess_destroy() {
    CUDA_CHECK(cudaFree(img_buffer_device));
    CUDA_CHECK(cudaFreeHost(img_buffer_host));
    CUDA_CHECK(cudaFree(temp_buffer_device));
    img_buffer_host = nullptr;
    img_buffer_device = nullptr;
    temp_buffer_device = nullptr;
}

void* get_temp_buffer() {
    if (!temp_buffer_device) {
        throw std::runtime_error("Temporary buffer not initialized");
    }
    return temp_buffer_device;
}

void cuda_preprocess(
    unsigned char* src, int src_width, int src_height,
    float* dst, int dst_width, int dst_height,
    cudaStream_t stream) {
    if (!src) {
        throw std::runtime_error("Null source image in cuda_preprocess");
    }

    // Calculate correct letterboxing without pre-padding the source
    int src_size = src_width * src_height * 3;

    // Make sure we have enough buffer space for the original image
    static int last_allocated_size = 0;
    if (src_size > last_allocated_size) {
        // Free existing buffers if needed
        if (img_buffer_host) {
            CUDA_CHECK(cudaFreeHost(img_buffer_host));
            img_buffer_host = nullptr;
        }
        if (img_buffer_device) {
            CUDA_CHECK(cudaFree(img_buffer_device));
            img_buffer_device = nullptr;
        }
        
        // Allocate new buffers with proper size
        CUDA_CHECK(cudaMallocHost((void**)&img_buffer_host, src_size));
        CUDA_CHECK(cudaMalloc((void**)&img_buffer_device, src_size));
        
        last_allocated_size = src_size;
        std::cout << "Resized buffer to " << src_size << " bytes for image of size " 
                  << src_width << "x" << src_height << std::endl;
    }

    // Copy the source image directly without padding
    std::memcpy(img_buffer_host, src, src_size);
    CUDA_CHECK(cudaMemcpyAsync(img_buffer_device, img_buffer_host, src_size, cudaMemcpyHostToDevice, stream));

    // Calculate proper letterboxing using aspect ratio preservation
    deploy::AffineTransform transform;

    // Calculate the scaling factor to preserve aspect ratio
    float scale = std::min(
        static_cast<float>(dst_width) / src_width,
        static_cast<float>(dst_height) / src_height
    );
    
    // Calculate the new dimensions after scaling
    float new_width = src_width * scale;
    float new_height = src_height * scale;
    
    // Calculate padding offsets to center the image
    float pad_x = (dst_width - new_width) / 2;
    float pad_y = (dst_height - new_height) / 2;
    
    // Set the INVERSE transform matrix (from destination to source coordinates)
    // This maps destination pixels back to source pixels
    float inv_scale = 1.0f / scale;
    transform.matrix[0] = make_float3(inv_scale, 0.0f, -pad_x * inv_scale);
    transform.matrix[1] = make_float3(0.0f, inv_scale, -pad_y * inv_scale);
    
    // Store offsets for potential future use
    transform.dst_offset_x = pad_x;
    transform.dst_offset_y = pad_y;

    // Add logging to debug the transformation
    std::cout << "Letterboxing info: orig=" << src_width << "x" << src_height 
              << ", new=" << new_width << "x" << new_height 
              << ", scale=" << scale
              << ", offset=(" << transform.dst_offset_x << "," << transform.dst_offset_y << ")" << std::endl;

    deploy::ProcessConfig config;
    config.alpha = make_float3(1.0f, 1.0f, 1.0f); // Alpha is 1.0 as srgb_to_lin handles normalization
    config.beta = make_float3(0.0f, 0.0f, 0.0f);
    config.border_value = 114.0f / 255.0f; // This will be sRGB, warp_affine_bilinear will linearize it.
                                          // This is only if this specific cuda_preprocess is called directly.
                                          // The main path via resize_ops.cpp already provides linear border_value to ProcessConfig.
    config.swap_rb = true;

    deploy::cudaWarpAffine(
        img_buffer_device, src_width, src_height, // Use original dimensions
        temp_buffer_device, dst_width, dst_height,
        transform.matrix, config, stream
    );

    deploy::launch_reformat_kernel_float(
        temp_buffer_device, dst,
        1, 3, dst_height, dst_width,
        stream
    );
    CUDA_CHECK(cudaGetLastError());
    CUDA_CHECK(cudaStreamSynchronize(stream));
}

void cuda_batch_preprocess(
    const unsigned char** src_batch, int batch_size, int src_width, int src_height,
    float* hwc_output, int dst_width, int dst_height,
    const deploy::ProcessConfig& config,
    cudaStream_t stream) {
    if (!src_batch || batch_size <= 0) {
        throw std::runtime_error("Invalid input: null batch or non-positive batch size");
    }

    // Instead of padding the source image before transformation, we'll use proper letterboxing
    // by calculating the correct transformation matrix first, then letting warpAffine handle the padding
    int src_size = src_width * src_height * 3;
    
    // Directly copy source images to the host buffer without padding
    for (int i = 0; i < batch_size; ++i) {
        if (!src_batch[i]) {
            throw std::runtime_error("Null source image at batch index " + std::to_string(i));
        }
        
        // Direct copy of the original image without padding
        cudaMemcpy(img_buffer_host + i * src_size, src_batch[i], src_size, cudaMemcpyHostToHost);
    }
    
    // Copy all batch images to device
    CUDA_CHECK(cudaMemcpyAsync(img_buffer_device, img_buffer_host, batch_size * src_size, cudaMemcpyHostToDevice, stream));

    // Create transform using proper letterboxing calculation
    document_layout::deploy::AffineTransform transform;
    
    // Calculate the scaling factor to preserve aspect ratio
    float scale = std::min(
        static_cast<float>(dst_width) / src_width,
        static_cast<float>(dst_height) / src_height
    );
    
    // Calculate the new dimensions after scaling
    float new_width = src_width * scale;
    float new_height = src_height * scale;
    
    // Calculate padding offsets to center the image
    float pad_x = (dst_width - new_width) / 2;
    float pad_y = (dst_height - new_height) / 2;
    
    // Set the INVERSE transform matrix (from destination to source coordinates)
    // This ensures proper letterboxing without pre-padding the source
    float inv_scale = 1.0f / scale;
    transform.matrix[0] = make_float3(inv_scale, 0.0f, -pad_x * inv_scale);
    transform.matrix[1] = make_float3(0.0f, inv_scale, -pad_y * inv_scale);
    
    // Store offsets for potential future use
    transform.dst_offset_x = pad_x;
    transform.dst_offset_y = pad_y;

    // Add debug log for the letterboxing calculation
    if (batch_size > 0) {
        printf("[ResizeOps] Letterboxing: orig=%dx%d, new=%.1fx%.1f, scale=%.3f, offset=(%.1f,%.1f)\n", 
               src_width, src_height, new_width, new_height, scale, pad_x, pad_y);
    }

    // Use cudaMultiWarpAffine to handle the transformation and border padding in one step
    document_layout::deploy::cudaMultiWarpAffine(
        img_buffer_device, src_width, src_height, // Use original dimensions here
        hwc_output, dst_width, dst_height,
        transform.matrix, config, batch_size, stream
    );

    CUDA_CHECK(cudaGetLastError());
    CUDA_CHECK(cudaStreamSynchronize(stream));
}
void cuda_batch_preprocess_with_cast(
    const unsigned char** src_batch, int batch_size, int src_width, int src_height,
    void* dst, int dst_width, int dst_height, bool input_is_fp16,
    void* temp_buffer, const deploy::ProcessConfig& config,
    cudaStream_t stream)
{
    if (!src_batch || batch_size <= 0) {
        throw std::runtime_error("Invalid input: null batch or non-positive batch size");
    }
    if (!temp_buffer) {
        throw std::runtime_error("Null temporary buffer");
    }

    float* temp_nchw = nullptr;
    if (input_is_fp16) {
        size_t nchw_size = batch_size * 3 * dst_width * dst_height * sizeof(float);
        CUDA_CHECK(cudaMalloc(&temp_nchw, nchw_size));
        CUDA_CHECK(cudaMemsetAsync(temp_nchw, 0, nchw_size, stream));
    }

    // Call cuda_batch_preprocess to temp_buffer (HWC, FP32)
    cuda_batch_preprocess(src_batch, batch_size, src_width, src_height,
                          static_cast<float*>(temp_buffer), dst_width, dst_height,
                          config, stream);
    CUDA_CHECK(cudaStreamSynchronize(stream));
    std::cout << "Preprocess complete, starting reformat..." << std::endl;

    // Reformat HWC to NCHW
    if (input_is_fp16) {
        // For FP16, reformat to temp_nchw, then cast to FP16
        deploy::launch_reformat_kernel_float(
            static_cast<const float*>(temp_buffer), temp_nchw,
            batch_size, 3, dst_height, dst_width,
            stream
        );
        CUDA_CHECK(cudaGetLastError());
        CUDA_CHECK(cudaStreamSynchronize(stream)); // Synchronize after reformat

        std::cout << "FP32 to NCHW reformat complete, casting to FP16..." << std::endl;

        size_t num_elements = batch_size * 3 * dst_width * dst_height;
        cast_fp32_to_fp16<<<(num_elements + 255) / 256, 256, 0, stream>>>(
            temp_nchw, static_cast<half*>(dst), num_elements);
        CUDA_CHECK(cudaGetLastError());
        CUDA_CHECK(cudaStreamSynchronize(stream)); // Synchronize after casting

        // Free temp buffer only after ensuring all kernels are complete
        CUDA_CHECK(cudaFree(temp_nchw));
        std::cout << "FP16 conversion complete" << std::endl;
    } else {
        // For FP32, reformat directly to dst
        deploy::launch_reformat_kernel_float(
            static_cast<const float*>(temp_buffer), static_cast<float*>(dst),
            batch_size, 3, dst_height, dst_width,
            stream
        );
        CUDA_CHECK(cudaGetLastError());
        CUDA_CHECK(cudaStreamSynchronize(stream)); // Synchronize after reformat
        std::cout << "FP32 reformat complete" << std::endl;
    }

    // Final synchronization is redundant but kept for safety
    CUDA_CHECK(cudaStreamSynchronize(stream));
}

} // namespace document_layout