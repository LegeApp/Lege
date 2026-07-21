#include <cuda_runtime.h>
#include <cstddef>
#include <cstdio>
#include <stdexcept>
#include <iostream>
#include "cuda_utils.h"
#include "reformat.h"

using std::cout;
using std::endl;

namespace document_layout {
namespace deploy {

inline __host__ __device__ dim3 iDivUpGrid(int total_x, int total_y, int total_z, const dim3& block) {
    return dim3(
        (total_x + block.x - 1) / block.x,
        (total_y + block.y - 1) / block.y,
        (total_z + block.z - 1) / block.z
    );
}

__global__ void kernel_reformat_hwc_to_nchw_float(
    const float* __restrict__ src_hwc,
    float* __restrict__ dst_nchw,
    int H, int W, int C,
    int batch_stride_in_elements,
    int batch_stride_out_elements)
{
    const int n = blockIdx.z;
    const int base_x = blockIdx.x * blockDim.x + threadIdx.x;
    const int base_y = blockIdx.y * blockDim.y + threadIdx.y;

    if (base_x >= W || base_y >= H || n >= gridDim.z) return; // Early termination for out-of-bounds threads

    for (int y = base_y; y < H; y += blockDim.y * gridDim.y) {
        for (int x = base_x; x < W; x += blockDim.x * gridDim.x) {
            if (x >= W || y >= H) break; // Skip out-of-bounds pixels

            const float* img_in = src_hwc + n * batch_stride_in_elements;
            float* img_out = dst_nchw + n * batch_stride_out_elements;

            const size_t in_offset = (y * W + x) * C;
            const size_t out_base = y * W + x;
            const size_t plane_size = static_cast<size_t>(H) * W;

            if (C == 3) {
                // Safe pixel access - avoid potential misaligned memory access
                float r = img_in[in_offset];
                float g = img_in[in_offset + 1];
                float b = img_in[in_offset + 2];
                
                img_out[out_base] = r;
                img_out[plane_size + out_base] = g;
                img_out[2 * plane_size + out_base] = b;
            } else {
                for (int c = 0; c < C; ++c) {
                    img_out[c * plane_size + out_base] = img_in[in_offset + c];
                }
            }
        }
    }
}

void launch_reformat_kernel_float(const void* src_hwc_buffer, void* dst_nchw_buffer,
                                 int N, int C, int H, int W, cudaStream_t stream) {
    if (N <= 0 || C <= 0 || H <= 0 || W <= 0) {
        fprintf(stderr, "[ERROR] Invalid dimensions: N=%d, C=%d, H=%d, W=%d\n", N, C, H, W);
        throw std::runtime_error("Invalid dimensions in launch_reformat_kernel_float");
    }
    if (!src_hwc_buffer || !dst_nchw_buffer) {
        fprintf(stderr, "[ERROR] Null buffer: src=%p, dst=%p\n", src_hwc_buffer, dst_nchw_buffer);
        throw std::runtime_error("Null buffer in launch_reformat_kernel_float");
    }

    cout << "launch_reformat_kernel_float: N=" << N << ", C=" << C
              << ", H=" << H << ", W=" << W
              << ", src=" << src_hwc_buffer << ", dst=" << dst_nchw_buffer << endl;

    const dim3 block(32, 8, 1);
    const dim3 grid = iDivUpGrid(W, H, N, block);

    const size_t batch_stride_in = static_cast<size_t>(H) * W * C;
    const size_t batch_stride_out = static_cast<size_t>(C) * H * W;

    const float* src = static_cast<const float*>(src_hwc_buffer);
    float* dst = static_cast<float*>(dst_nchw_buffer);    kernel_reformat_hwc_to_nchw_float<<<grid, block, 0, stream>>>(
        src, dst, H, W, C,
        static_cast<int>(batch_stride_in),
        static_cast<int>(batch_stride_out)
    );
    CUDA_CHECK(cudaGetLastError());
    cout << "launch_reformat_kernel_float launched" << endl;
}

} // namespace deploy
} // namespace document_layout