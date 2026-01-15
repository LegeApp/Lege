/**
 * @file warpaffine.cu
 * @author ...
 * @brief CUDA implementation of affine transform with bilinear interpolation.
 * @date 2025-01-09
 *
 * @copyright Copyright ...
 *
 * @note This implementation uses bilinear interpolation for affine transformations.
 * Bilinear was chosen over Lanczos (e.g., Lanczos2 or Lanczos3) because it provides
 * better text quality (less ringing artifacts) and improved performance (2x2 neighborhood
 * vs. 4x4 or 6x6). A postprocessing sharpening step in main.cpp enhances text clarity.
 */

#include <cstdint>
#include <stdexcept>
#include <iostream>  // Added for std::cout and std::endl
#include "warpaffine.h"
#include "cuda_utils.h"

namespace document_layout {
namespace deploy {

inline __device__ __host__ int iDivUp(int a, int b) {
    return (a % b != 0) ? (a / b + 1) : (a / b);
}

__forceinline__ __device__ float3 operator*(float value0, float3 value1) {
    return make_float3(value0 * value1.x, value0 * value1.y, value0 * value1.z);
}

__forceinline__ __device__ float3 operator+(const float3& value0, const float3& value1) {
    return make_float3(value0.x + value1.x, value0.y + value1.y, value0.z + value1.z);
}

__forceinline__ __device__ float3 operator+=(float3& value0, const float3& value1) {
    value0.x += value1.x;
    value0.y += value1.y;
    value0.z += value1.z;
    return value0;
}

__forceinline__ __device__ float3 uchar3_to_float3(uchar3 v) {
    return make_float3(static_cast<float>(v.x), static_cast<float>(v.y), static_cast<float>(v.z));
}

__forceinline__ __device__ bool in_bounds(int x, int y, int cols, int rows) {
    return (x >= 0) && (x < cols) && (y >= 0) && (y < rows);
}

inline __device__ float srgb_to_lin_float_device(float v_srgb_norm) {
    return v_srgb_norm <= 0.04045f ? v_srgb_norm * (1.f / 12.92f)
                                   : powf((v_srgb_norm + 0.055f) * (1.f / 1.055f), 2.4f);
}

__device__ float bell_filter(float t) {
    t = fabsf(t);
    if (t < 0.5f) return 0.75f - (t * t);
    if (t < 1.5f) {
        t = (t - 1.5f);
        return 0.5f * (t * t);
    }
    return 0.0f;
}

template<bool UseBellFilter>
__device__ void warp_affine_generic(
    const uint8_t* src, const int src_cols, const int src_rows, const int src_pitch_bytes,
    float* dst, const int dst_cols, const int dst_rows, const int dst_pitch_elements,
    const float3 m0, const float3 m1, const ProcessConfig config, int element_x, int element_y) {
    
    if (element_x >= dst_cols || element_y >= dst_rows) {
        return;
    }

    float src_x = m0.x * element_x + m0.y * element_y + m0.z;
    float src_y = m1.x * element_x + m1.y * element_y + m1.z;

    const int support = UseBellFilter ? 3 : 1; // Bell filter has 3x3 support
    const int x0 = floorf(src_x) - support + 1;
    const int y0 = floorf(src_y) - support + 1;
    
    float3 sum = make_float3(0.0f, 0.0f, 0.0f);
    float w_sum = 0.0f;
    float3 border_value = make_float3(config.border_value, config.border_value, config.border_value);

    // Apply filter
    for (int dy = 0; dy < support * 2; ++dy) {
        for (int dx = 0; dx < support * 2; ++dx) {
            int x = x0 + dx;
            int y = y0 + dy;
            
            float wx = UseBellFilter ? 
                bell_filter((src_x - x) / 1.0f) : 
                (dx == 0 ? (1.0f - (src_x - floorf(src_x))) : (src_x - floorf(src_x)));
                
            float wy = UseBellFilter ? 
                bell_filter((src_y - y) / 1.0f) : 
                (dy == 0 ? (1.0f - (src_y - floorf(src_y))) : (src_y - floorf(src_y)));
            
            float weight = wx * wy;
            
            if (in_bounds(x, y, src_cols, src_rows)) {
                float3 val = uchar3_to_float3(reinterpret_cast<const uchar3*>(src + static_cast<size_t>(y) * src_pitch_bytes)[x]);
                float3 lin_val = make_float3(
                    srgb_to_lin_float_device(val.x / 255.0f),
                    srgb_to_lin_float_device(val.y / 255.0f),
                    srgb_to_lin_float_device(val.z / 255.0f)
                );
                sum = sum + weight * lin_val;
            } else {
                sum = sum + weight * border_value;
            }
            w_sum += weight;
        }
    }
    
    // Normalize
    if (w_sum > 1e-6f) {
        float inv_w = 1.0f / w_sum;
        sum.x *= inv_w;
        sum.y *= inv_w;
        sum.z *= inv_w;
    }

    if (config.swap_rb) {
        float temp = sum.x;
        sum.x = sum.z;
        sum.z = temp;
    }

    sum.x = min(max(sum.x, 0.0f), 1.0f);
    sum.y = min(max(sum.y, 0.0f), 1.0f);
    sum.z = min(max(sum.z, 0.0f), 1.0f);

    int output_pixel_idx = element_y * dst_pitch_elements + element_x;
    reinterpret_cast<float3*>(dst)[output_pixel_idx] = sum;
}

__global__ void gpuBilinearWarpAffine(
    const void* src, const int src_cols, const int src_rows,
    void* dst, const int dst_cols, const int dst_rows,
    const float3 m0, const float3 m1, const ProcessConfig config) {
    const int element_x = blockDim.x * blockIdx.x + threadIdx.x;
    const int element_y = blockDim.y * blockIdx.y + threadIdx.y;
    
    // Call generic implementation with bilinear interpolation
    warp_affine_generic<false>(
        static_cast<const uint8_t*>(src), src_cols, src_rows, src_cols * 3,
        static_cast<float*>(dst), dst_cols, dst_rows, dst_cols * 3,
        m0, m1, config, element_x, element_y
    );
}

__global__ void gpuBellWarpAffine(
    const void* src, const int src_cols, const int src_rows,
    void* dst, const int dst_cols, const int dst_rows,
    const float3 m0, const float3 m1, const ProcessConfig config) {
    const int element_x = blockDim.x * blockIdx.x + threadIdx.x;
    const int element_y = blockDim.y * blockIdx.y + threadIdx.y;
    
    // Call generic implementation with Bell filter
    warp_affine_generic<true>(
        static_cast<const uint8_t*>(src), src_cols, src_rows, src_cols * 3,
        static_cast<float*>(dst), dst_cols, dst_rows, dst_cols * 3,
        m0, m1, config, element_x, element_y
    );
}

__global__ void gpuMultiBilinearWarpAffine(
    const void* src, const int src_cols, const int src_rows,
    void* dst, const int dst_cols, const int dst_rows,
    const float3 m0, const float3 m1, const ProcessConfig config,
    int num_images) {
    const int image_idx = blockIdx.z;
    if (image_idx >= num_images) {
        return;
    }

    const int element_x = blockDim.x * blockIdx.x + threadIdx.x;
    const int element_y = blockDim.y * blockIdx.y + threadIdx.y;

    if (element_x >= dst_cols || element_y >= dst_rows) {
        return;
    }

    const size_t src_pitch_bytes = static_cast<size_t>(src_cols) * 3 * sizeof(uint8_t);
    const int dst_pitch_elements = dst_cols;

    const size_t src_image_byte_offset = static_cast<size_t>(image_idx) * src_rows * src_pitch_bytes;
    const size_t dst_image_byte_offset = static_cast<size_t>(image_idx) * dst_rows * dst_cols * 3 * sizeof(float);

    warp_affine_generic<false>(
        static_cast<const uint8_t*>(src) + src_image_byte_offset,
        src_cols, src_rows, src_pitch_bytes,
        reinterpret_cast<float*>(reinterpret_cast<uint8_t*>(dst) + dst_image_byte_offset),
        dst_cols, dst_rows, dst_pitch_elements,
        m0, m1, config,
        element_x, element_y
    );
}

__global__ void gpuMultiBellWarpAffine(
    const void* src, const int src_cols, const int src_rows,
    void* dst, const int dst_cols, const int dst_rows,
    const float3 m0, const float3 m1, const ProcessConfig config,
    int num_images) {
    const int image_idx = blockIdx.z;
    if (image_idx >= num_images) {
        return;
    }

    const int element_x = blockDim.x * blockIdx.x + threadIdx.x;
    const int element_y = blockDim.y * blockIdx.y + threadIdx.y;

    if (element_x >= dst_cols || element_y >= dst_rows) {
        return;
    }

    const size_t src_pitch_bytes = static_cast<size_t>(src_cols) * 3 * sizeof(uint8_t);
    const int dst_pitch_elements = dst_cols;

    const size_t src_image_byte_offset = static_cast<size_t>(image_idx) * src_rows * src_pitch_bytes;
    const size_t dst_image_byte_offset = static_cast<size_t>(image_idx) * dst_rows * dst_cols * 3 * sizeof(float);

    warp_affine_generic<true>(
        static_cast<const uint8_t*>(src) + src_image_byte_offset,
        src_cols, src_rows, src_pitch_bytes,
        reinterpret_cast<float*>(reinterpret_cast<uint8_t*>(dst) + dst_image_byte_offset),
        dst_cols, dst_rows, dst_pitch_elements,
        m0, m1, config,
        element_x, element_y
    );
}

AffineTransform::AffineTransform() : last_src_width_(0), last_src_height_(0), dst_offset_x(0), dst_offset_y(0) {
    matrix[0] = make_float3(1.0f, 0.0f, 0.0f);
    matrix[1] = make_float3(0.0f, 1.0f, 0.0f);
}

void deploy::AffineTransform::updateMatrix(int src_width, int src_height, int dst_width, int dst_height) {
    if (src_width <= 0 || src_height <= 0 || dst_width <= 0 || dst_height <= 0) {
        throw std::runtime_error("Invalid dimensions in AffineTransform::updateMatrix");
    }

    if (src_width == last_src_width_ && src_height == last_src_height_) return;
    last_src_width_  = src_width;
    last_src_height_ = src_height;

    // Fixed padding of 24px on the larger dimension
    const int PADDING = 24;
    const int TARGET_SIZE = dst_height; // 1024
    const int AVAILABLE_SIZE = TARGET_SIZE - 2 * PADDING; // 1024 - 48 = 976

    bool is_width_larger = src_width >= src_height;
    double scale;
    double new_width, new_height;
    double offset_x, offset_y;

    if (is_width_larger) {
        // Width is the larger dimension
        // Source width already includes 24px padding on each side, so effective width is src_width - 2 * PADDING
        double effective_width = src_width - 2 * PADDING;
        double effective_height = src_height;

        // Scale based on the width fitting into 1024 - 48
        scale = static_cast<double>(AVAILABLE_SIZE) / effective_width;
        new_width = effective_width * scale; // Should be 976
        new_height = effective_height * scale;

        // Center the image
        offset_x = PADDING; // 24px padding on the left
        offset_y = (TARGET_SIZE - new_height) / 2.0; // Center vertically
    } else {
        // Height is the larger dimension
        // Source height already includes 24px padding on each side, so effective height is src_height - 2 * PADDING
        double effective_width = src_width;
        double effective_height = src_height - 2 * PADDING;

        // Scale based on the height fitting into 1024 - 48
        scale = static_cast<double>(AVAILABLE_SIZE) / effective_height;
        new_width = effective_width * scale;
        new_height = effective_height * scale; // Should be 976

        // Center the image
        offset_x = (TARGET_SIZE - new_width) / 2.0; // Center horizontally
        offset_y = PADDING; // 24px padding on the top
    }

    if (std::abs(scale) < 1e-6) {
        throw std::runtime_error("Invalid scale in AffineTransform::updateMatrix");
    }

    // Calculate transformation matrix
    double inv_scale = 1.0 / scale;
    matrix[0] = make_float3(static_cast<float>(inv_scale), 0.0f, static_cast<float>(-offset_x * inv_scale));
    matrix[1] = make_float3(0.0f, static_cast<float>(inv_scale), static_cast<float>(-offset_y * inv_scale));

    // Store offset values for potential use elsewhere
    dst_offset_x = static_cast<int>(round(offset_x));
    dst_offset_y = static_cast<int>(round(offset_y));
    
    // Debug information
    std::cout << "AffineTransform: original=" << src_width << "x" << src_height
              << ", effective=" << (is_width_larger ? (src_width - 2 * PADDING) : src_width) << "x" 
              << (is_width_larger ? src_height : (src_height - 2 * PADDING))
              << ", new=" << new_width << "x" << new_height 
              << ", scale=" << scale
              << ", offsets=(" << dst_offset_x << "," << dst_offset_y << ")"
              << ", padding=" << PADDING << std::endl;
}

void AffineTransform::applyTransform(float x, float y, float* transformed_x, float* transformed_y) const {
    *transformed_x = matrix[0].x * x + matrix[0].y * y + matrix[0].z;
    *transformed_y = matrix[1].x * x + matrix[1].y * y + matrix[1].z;
}

void cudaWarpAffine(
    const void* src, const int src_cols, const int src_rows,
    void* dst, const int dst_cols, const int dst_rows,
    const float3 matrix[2], const ProcessConfig& config, cudaStream_t stream, bool use_bell_filter) {
    if (!src || !dst) {
        throw std::runtime_error("Null buffer in cudaWarpAffine");
    }
    if (src_cols <= 0 || src_rows <= 0 || dst_cols <= 0 || dst_rows <= 0) {
        throw std::runtime_error("Invalid dimensions in cudaWarpAffine");
    }
    
    // Sanity check for config values
    if (config.alpha.x <= 0 || config.alpha.y <= 0 || config.alpha.z <= 0) {
        throw std::runtime_error("Invalid alpha values in ProcessConfig");
    }
    if (std::isnan(config.beta.x) || std::isnan(config.beta.y) || std::isnan(config.beta.z)) {
        throw std::runtime_error("Invalid beta values in ProcessConfig");
    }

    const dim3 blockDim(32, 8); // Optimized block size
    const dim3 gridDim(iDivUp(dst_cols, blockDim.x), iDivUp(dst_rows, blockDim.y));

    if (use_bell_filter) {
        gpuBellWarpAffine<<<gridDim, blockDim, 0, stream>>>(
            src, src_cols, src_rows, dst, dst_cols, dst_rows, matrix[0], matrix[1], config);
    } else {
        gpuBilinearWarpAffine<<<gridDim, blockDim, 0, stream>>>(
            src, src_cols, src_rows, dst, dst_cols, dst_rows, matrix[0], matrix[1], config);
    }
    CUDA_CHECK(cudaGetLastError());
}

void cudaMultiWarpAffine(
    const void* src, const int src_cols, const int src_rows,
    void* dst, const int dst_cols, int dst_rows,
    const float3 matrix[2], const ProcessConfig& config, int num_images, cudaStream_t stream, bool use_bell_filter) {
    if (!src || !dst) {
        throw std::runtime_error("Null buffer in cudaMultiWarpAffine");
    }
    if (num_images <= 0 || src_cols <= 0 || src_rows <= 0 || dst_cols <= 0 || dst_rows <= 0) {
        throw std::runtime_error("Invalid dimensions in cudaMultiWarpAffine");
    }

    const dim3 blockDim(32, 8); // Optimized block size
    const dim3 gridDim(iDivUp(dst_cols, blockDim.x), iDivUp(dst_rows, blockDim.y), num_images);

    gpuMultiBilinearWarpAffine<<<gridDim, blockDim, 0, stream>>>(
        src, src_cols, src_rows, dst, dst_cols, dst_rows, matrix[0], matrix[1], config, num_images);
    CUDA_CHECK(cudaGetLastError());
}

} // namespace deploy
} // namespace document_layout