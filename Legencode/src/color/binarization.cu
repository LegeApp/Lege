/********************************************************************
*  binarization.cu  –  CUDA back-end for adaptive document binariser
*  (fixed bypass-initialization errors)
********************************************************************/

#include <cuda_runtime.h>
#include <device_launch_parameters.h>
#include <cmath>
#include "binarization.h"
#include "logging.h"

/* ----------------------------------------------------------------- */
/*  Constants                                                        */
/* ----------------------------------------------------------------- */

#define MAX_WINDOW_RADIUS 64

/* ----------------------------------------------------------------- */
/*  RGB → linear-luma                                                */
/* ----------------------------------------------------------------- */
__global__ void rgb_to_luma_kernel(const uchar3 *rgb,
                                   unsigned char *y,
                                   size_t        n)
{
    size_t i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i >= n) return;
    uchar3 c = rgb[i];
    float l = 0.2126f*(c.x/255.f)
            + 0.7152f*(c.y/255.f)
            + 0.0722f*(c.z/255.f);
    y[i] = static_cast<unsigned char>(255.f * l + 0.5f);
}

/* ----------------------------------------------------------------- */
/*  Integral-image pass A: row prefix (1 thread per row)             */
/* ----------------------------------------------------------------- */
__global__ void integral_row_kernel(const unsigned char *src,
                                    unsigned int       *dst_sum,
                                    unsigned long long *dst_sq,
                                    int width, int height,
                                    size_t pitch_sum, size_t pitch_sq)
{
    int y = blockIdx.x;
    if (y >= height + 1) return;
    
    // Initialize the first column of the integral image to 0
    if (y == 0) {
        for (int x = 0; x < width + 1; ++x) {
            *(unsigned int*)((char*)dst_sum + x * sizeof(unsigned int)) = 0;
            *(unsigned long long*)((char*)dst_sq + x * sizeof(unsigned long long)) = 0ULL;
        }
    } else {
        unsigned int       s  = 0;
        unsigned long long s2 = 0ULL;
        // The first element of each row (at x=0) is 0
        *(unsigned int*)((char*)dst_sum + y * pitch_sum) = 0;
        *(unsigned long long*)((char*)dst_sq + y * pitch_sq) = 0ULL;

        for (int x = 0; x < width; ++x) {
            unsigned int p = src[(y - 1) * width + x]; // src is not pitched
            s  += p;
            s2 += 1ULL * p * p;
            *(unsigned int*)((char*)dst_sum + y * pitch_sum + (x + 1) * sizeof(unsigned int)) = s;
            *(unsigned long long*)((char*)dst_sq + y * pitch_sq + (x + 1) * sizeof(unsigned long long)) = s2;
        }
    }
}

/* ----------------------------------------------------------------- */
/*  Integral-image pass B: column prefix (1 thread per col)          */
/* ----------------------------------------------------------------- */
__global__ void integral_col_kernel(unsigned int       *dst_sum,
                                    unsigned long long *dst_sq,
                                    int width, int height,
                                    size_t pitch_sum, size_t pitch_sq)
{
    int x = blockIdx.x;
    if (x >= width + 1) return;
    
    unsigned int       s  = *(unsigned int*)((char*)dst_sum + x * sizeof(unsigned int));
    unsigned long long s2 = *(unsigned long long*)((char*)dst_sq + x * sizeof(unsigned long long));

    for (int y = 1; y < height + 1; ++y) {
        s  += *(unsigned int*)((char*)dst_sum + y * pitch_sum + x * sizeof(unsigned int));
        s2 += *(unsigned long long*)((char*)dst_sq + y * pitch_sq + x * sizeof(unsigned long long));
        *(unsigned int*)((char*)dst_sum + y * pitch_sum + x * sizeof(unsigned int)) = s;
        *(unsigned long long*)((char*)dst_sq + y * pitch_sq + x * sizeof(unsigned long long)) = s2;
    }
}

/* ----------------------------------------------------------------- */
/*  Integral-image kernel (unsigned int version)                     */
/* ----------------------------------------------------------------- */
__global__ void integral_img_kernel(const uint8_t* src,
                                    unsigned int* intImg,
                                    int width, int height,
                                    int pitch)
{
    int x = blockIdx.x * blockDim.x + threadIdx.x;
    int y = blockIdx.y * blockDim.y + threadIdx.y;

    // Add bounds checking for integral image dimensions
    if (x >= width + 1 || y >= height + 1) return;

    // Calculate the 1D index
    int idx = y * (width + 1) + x;

    // Initialize the integral image with 0s
    intImg[idx] = 0;

    // If within image bounds, copy the source pixel value
    if (x < width && y < height) {
        intImg[idx] = src[y * width + x];
    }
}

/* ----------------------------------------------------------------- */
/*  Integral-image kernel (unsigned long long version)               */
/* ----------------------------------------------------------------- */
__global__ void integral_img_kernel_ull(const uint8_t* src,
                                        unsigned long long* intImg,
                                        int width, int height,
                                        int pitch)
{
    int x = blockIdx.x * blockDim.x + threadIdx.x;
    int y = blockIdx.y * blockDim.y + threadIdx.y;

    // Add bounds checking for integral image dimensions
    if (x >= width + 1 || y >= height + 1) return;

    // Add bounds checking for integral image dimensions
    if (x >= width + 1 || y >= height + 1) return;

    // Calculate the 1D index
    int idx = y * (width + 1) + x;

    // Initialize the integral image with 0s
    intImg[idx] = 0;

    // If within image bounds, copy the source pixel value
    if (x < width && y < height) {
        intImg[idx] = src[y * width + x];
    }
}

/* ----------------------------------------------------------------- */
/*  Sauvola pixel kernel (unchanged)                                 */
/* ----------------------------------------------------------------- */
__device__ __forceinline__
void region_stats(const unsigned int *S,
                  const unsigned long long *Q,
                  int x1,int y1,int x2,int y2,int W_orig, size_t pitch_S, size_t pitch_Q,
                  unsigned int &sum,
                  unsigned long long &sq)
{
    // Adjust coordinates for the +1 padded integral image
    x1 += 1; y1 += 1;
    x2 += 1; y2 += 1;

    // Get values from pitched memory
    unsigned int SA = *(unsigned int*)((char*)S + y2 * pitch_S + x2 * sizeof(unsigned int));
    unsigned int SB = (y1 > 0) ? *(unsigned int*)((char*)S + (y1 - 1) * pitch_S + x2 * sizeof(unsigned int)) : 0;
    unsigned int SC = (x1 > 0) ? *(unsigned int*)((char*)S + y2 * pitch_S + (x1 - 1) * sizeof(unsigned int)) : 0;
    unsigned int SD = (x1 > 0 && y1 > 0) ? *(unsigned int*)((char*)S + (y1 - 1) * pitch_S + (x1 - 1) * sizeof(unsigned int)) : 0;

    unsigned long long QA = *(unsigned long long*)((char*)Q + y2 * pitch_Q + x2 * sizeof(unsigned long long));
    unsigned long long QB = (y1 > 0) ? *(unsigned long long*)((char*)Q + (y1 - 1) * pitch_Q + x2 * sizeof(unsigned long long)) : 0ULL;
    unsigned long long QC = (x1 > 0) ? *(unsigned long long*)((char*)Q + y2 * pitch_Q + (x1 - 1) * sizeof(unsigned long long)) : 0ULL;
    unsigned long long QD = (x1 > 0 && y1 > 0) ? *(unsigned long long*)((char*)Q + (y1 - 1) * pitch_Q + (x1 - 1) * sizeof(unsigned long long)) : 0ULL;

    sum = SA - SB - SC + SD;
    sq  = QA - QB - QC + QD;
}

__global__ void sauvola_kernel(const unsigned char *gray,
                               unsigned char       *out,
                               const unsigned int  *S,
                               const unsigned long long *Q,
                               int W, int H,
                               int r, float k, float R,
                               int inv,
                               size_t pitch_S, size_t pitch_Q)
{
    int x = blockIdx.x * blockDim.x + threadIdx.x;
    int y = blockIdx.y * blockDim.y + threadIdx.y;
    if (x >= W || y >= H) return;

    int x1 = max(0, x - r), y1 = max(0, y - r);
    int x2 = min(W-1, x + r), y2 = min(H-1, y + r);
    int N  = (x2 - x1 + 1) * (y2 - y1 + 1);

    unsigned int       sum;
    unsigned long long sq;
    region_stats(S, Q, x1, y1, x2, y2, W, pitch_S, pitch_Q, sum, sq);

    float mean = float(sum) / N;
    float var  = float(sq)/N - mean*mean; if (var < 0) var = 0;
    float thr  = mean * (1.f + k * (sqrtf(var)/R - 1.f));

    unsigned char p = gray[y*W + x];
    out[y*W + x] = ((p > thr) ^ inv) ? 255 : 0;
}

/* ----------------------------------------------------------------- */
/*  Public launcher: RGB→gray                                        */
/* ----------------------------------------------------------------- */
extern "C"
void launch_rgb_to_grayscale_cuda(const unsigned char *h_rgb,
                                  unsigned char       *h_gray,
                                  int W, int H)
{
    size_t np = size_t(W) * H;

    /* declare ALL locals before any CUDA_CHECK / goto */  
    uchar3 *d_rgb   = nullptr;
    unsigned char *d_gray = nullptr;
    int block = 256;
    int grid  = 0;

    CUDA_CHECK(cudaMalloc(&d_rgb, 3*np));
    CUDA_CHECK(cudaMalloc(&d_gray, np));
    CUDA_CHECK(cudaMemcpy(d_rgb, h_rgb, 3*np, cudaMemcpyHostToDevice));

    grid = int((np + block - 1) / block);
    rgb_to_luma_kernel<<<grid, block>>>(d_rgb, d_gray, np);

    CUDA_CHECK(cudaMemcpy(h_gray, d_gray, np, cudaMemcpyDeviceToHost));

    if (d_rgb)   cudaFree(d_rgb);
    if (d_gray)  cudaFree(d_gray);
}

/* ----------------------------------------------------------------- */
/*  Public launcher: Sauvola binarisation                            */
/* ----------------------------------------------------------------- */
extern "C"
void launch_precision_binarize_cuda(const unsigned char *h_gray,
                                    unsigned char       *h_bin,
                                    int W, int H,
                                    const BinarizationOptions &opt)
{
    size_t np = size_t(W) * H;

    printf("DEBUG: Binarization W=%d, H=%d, np=%zu\n", W, H, np);

    // Clear any previous CUDA error
    cudaError_t prevErr = cudaGetLastError();
    if (prevErr != cudaSuccess) {
        printf("Clearing previous CUDA error: %s\n", cudaGetErrorString(prevErr));
        printf("Resetting CUDA device\n");
        cudaDeviceReset();
    }

    /* declare ALL locals up-front */
    unsigned char       *d_g = nullptr, *d_b = nullptr;
    unsigned int        *d_S = nullptr;
    unsigned long long  *d_Q = nullptr;
    
    cudaStream_t         st  = nullptr;
    int                  r;
    dim3                 tb(16,16), gb;
    size_t               pitch_S, pitch_Q;
    dim3                 gridSize; // Moved declaration here

    CUDA_CHECK(cudaStreamCreate(&st));
    CUDA_CHECK(cudaMalloc(&d_g,  np));
    CUDA_CHECK(cudaMalloc(&d_b,  np));
    CUDA_CHECK( cudaMallocPitch(&d_S, &pitch_S,
                                (W + 1) * sizeof(unsigned int),
                                H + 1) );
    CUDA_CHECK( cudaMallocPitch(&d_Q, &pitch_Q,
                                (W + 1) * sizeof(unsigned long long),
                                H + 1) );

    CUDA_CHECK(cudaMemcpyAsync(d_g, h_gray, np, cudaMemcpyHostToDevice, st));

    /* build Σ and Σ² */
    
    integral_row_kernel<<<H + 1, 1, 0, st>>>(d_g, d_S, d_Q, W, H, pitch_S, pitch_Q); // Launch H+1 blocks
    CUDA_CHECK(cudaGetLastError());

    integral_col_kernel<<<W + 1, 1, 0, st>>>(d_S, d_Q, W, H, pitch_S, pitch_Q); // Launch W+1 blocks
    CUDA_CHECK(cudaGetLastError());

    /* Sauvola */
    r = opt.window_size / 2;
    if (r < 1) r = 1;
    if (r > MAX_WINDOW_RADIUS) r = MAX_WINDOW_RADIUS;
    gridSize = dim3((W + tb.x - 1) / tb.x, (H + tb.y - 1) / tb.y); // Assignment here

    sauvola_kernel<<<gridSize, tb, 0, st>>>(
        d_g, d_b, d_S, d_Q, W, H,
        r, opt.k_factor, opt.sauvola_R_param,
        opt.invert ? 1 : 0,
        pitch_S, pitch_Q
    );

    CUDA_CHECK(cudaMemcpyAsync(h_bin, d_b, np, cudaMemcpyDeviceToHost, st));
    CUDA_CHECK(cudaStreamSynchronize(st));


    if (st)         cudaStreamDestroy(st);
    if (d_g)        cudaFree(d_g);
    if (d_b)        cudaFree(d_b);
    if (d_S)        cudaFree(d_S);
    if (d_Q)        cudaFree(d_Q);
}
