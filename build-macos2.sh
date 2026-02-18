#!/bin/bash
# Script to set up a robust macOS cross-compilation for ONNX Runtime + CoreML.
# This version pre-fetches dependencies to avoid network issues during the build
# and applies targeted patches for a smoother cross-compilation process.

set -euo pipefail  # Exit on any error, undefined var, or failed pipe
set -x             # Trace commands for easier debugging

# Remember repo root to restore later
SCRIPT_ROOT=$(pwd)
# Use an absolute path for the wrapper directory to avoid issues with `cd`
WRAP_DIR="$SCRIPT_ROOT/.ort-wrap"

# --- 1. Install Dependencies ---
echo "Installing build dependencies..."
apt-get update -y
apt-get install -y wget cmake protobuf-compiler git git-lfs build-essential python3 python3-pip python3-dev ninja-build libeigen3-dev

# Install iconv library for cross-compilation (libc6-dev usually provides this)
apt-get install -y libiconv-hook-dev || apt-get install -y libc6-dev

# --- 2. Clone ONNX Runtime and Dependencies ---

# Clone ONNX Runtime if not present
if [ ! -d "onnxruntime" ]; then
    echo "Cloning ONNX Runtime source..."
    git clone --recursive https://github.com/Microsoft/onnxruntime.git
fi
cd onnxruntime
# Use a stable release tag and ensure all submodules are present
git checkout v1.18.1
git submodule update --init --recursive

# --- 3. Pre-fetch and Patch the Problematic `google_nsync` Dependency ---

# The Problem: google_nsync is fetched via CMake's FetchContent, which can fail due to network issues.
# It also incorrectly detects the host (Linux) instead of the target (Darwin) during cross-compilation.
# The Solution: We clone it ourselves and patch ORT's build system to use our local copy.

NSYNC_VERSION="1.26.0"
NSYNC_DIR="cmake/external/nsync_local_source"

if [ ! -d "$NSYNC_DIR" ]; then
    echo "Cloning google_nsync locally to ensure a hermetic build..."
    git clone https://github.com/google/nsync.git "$NSYNC_DIR"
    (cd "$NSYNC_DIR" && git checkout "$NSYNC_VERSION")
fi

echo "Patching ONNX Runtime's CMake to use our local nsync clone..."
# Replace the FetchContent block with a simple add_subdirectory pointing to our clone.
# This completely bypasses the network download and gives us full control.
sed -i.bak '
/FetchContent_Declare(google_nsync/,/FetchContent_MakeAvailable(google_nsync)/ {
    /FetchContent_Declare(google_nsync/ {
        i \
# --- PATCHED: Use local nsync source for cross-compilation ---
set(google_nsync_SOURCE_DIR ${CMAKE_CURRENT_SOURCE_DIR}/nsync_local_source)
add_subdirectory(${google_nsync_SOURCE_DIR} ${CMAKE_CURRENT_BINARY_DIR}/nsync)
# --- END PATCH ---
        d
    }
    d
}
' cmake/external/google_nsync.cmake

echo "Forcing nsync's CMakeLists.txt into Darwin mode..."
# Inject flags directly into nsync's CMakeLists to ensure it configures correctly for macOS.
# This prevents it from trying to use Linux futex.
if ! grep -q "PATCHED FOR MACOS CROSS-COMPILE" "$NSYNC_DIR/CMakeLists.txt"; then
    sed -i '1s,^,# PATCHED FOR MACOS CROSS-COMPILE\nset(APPLE 1)\nset(CMAKE_SYSTEM_NAME Darwin)\n,' "$NSYNC_DIR/CMakeLists.txt"
fi


# --- 4. Set up macOS Cross-Compilation Environment ---

echo "Setting up macOS cross-compilation environment..."

# Create wrapper compilers that strip GNU-only linker flags unsupported by macOS linker.
mkdir -p "$WRAP_DIR"
cat >"$WRAP_DIR/x86_64-apple-darwin24-clang" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
real="/osxcross/bin/x86_64-apple-darwin24-clang"
args=()
for a in "$@"; do
    # Drop GNU ld flags (-Wl,--gc-sections) and obsolete -s. macOS uses -Wl,-dead_strip instead.
    if [[ "$a" == *"--gc-sections"* ]] || [ "$a" = "-s" ]; then
        continue
    fi
    args+=("$a")
done
exec "$real" "${args[@]}"
EOF
cat >"$WRAP_DIR/x86_64-apple-darwin24-clang++" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
real="/osxcross/bin/x86_64-apple-darwin24-clang++"
args=()
for a in "$@"; do
    if [[ "$a" == *"--gc-sections"* ]] || [ "$a" = "-s" ]; then
        continue
    fi
    args+=("$a")
done
exec "$real" "${args[@]}"
EOF
chmod +x "$WRAP_DIR/x86_64-apple-darwin24-clang" "$WRAP_DIR/x86_64-apple-darwin24-clang++"

# Detect macOS SDK sysroot from osxcross
SYSROOT=""
for base in /osxcross /opt/osxcross; do
    if [ -d "$base/SDK" ]; then
        candidate=$(ls -d "$base"/SDK/MacOSX*.sdk 2>/dev/null | sort -V | tail -n1)
        if [ -n "$candidate" ]; then
            SYSROOT="$candidate"
            break
        fi
    fi
done

if [ -z "$SYSROOT" ]; then
    echo "FATAL: macOS SDK sysroot not found under /osxcross/SDK or /opt/osxcross/SDK. Cannot proceed."
    exit 1
fi
echo "Using macOS SDK sysroot: $SYSROOT"

# Set common flags
MACOS_VERSION="13.3"
COMMON_FLAGS="-isysroot $SYSROOT -mmacosx-version-min=$MACOS_VERSION -D_LIBCPP_DISABLE_AVAILABILITY -Wno-error"
COMMON_LINKER_FLAGS="-isysroot $SYSROOT -mmacosx-version-min=$MACOS_VERSION -Wl,-dead_strip"
export CFLAGS="$COMMON_FLAGS -Wno-error=deprecated-pragma -Wno-error=deprecated-declarations"
export CXXFLAGS="$COMMON_FLAGS -Wno-deprecated-this-capture -Wno-error=deprecated-this-capture -Wno-error=deprecated-pragma -Wno-error=deprecated-declarations"
export LDFLAGS="$COMMON_LINKER_FLAGS"

# Set environment for CMake/build.sh
export CC="$WRAP_DIR/x86_64-apple-darwin24-clang"
export CXX="$WRAP_DIR/x86_64-apple-darwin24-clang++"
export AR=x86_64-apple-darwin24-ar
export STRIP=x86_64-apple-darwin24-strip
export SDKROOT="$SYSROOT"
export MACOSX_DEPLOYMENT_TARGET="$MACOS_VERSION"

# Parallelism
JOBS=${JOBS:-$(command -v nproc >/dev/null 2>&1 && nproc || echo 4)}

# Clean any previous build artifacts to ensure a fresh start with the new settings
if [ -d build ]; then
    echo "Cleaning previous build directory..."
    rm -rf build
fi


# --- 5. Build ONNX Runtime with CoreML for macOS ---

echo "Building ONNX Runtime with CoreML for macOS..."

./build.sh --config Release \
    --build_shared_lib \
    --use_coreml \
    --allow_running_as_root \
    --use_preinstalled_eigen \
    --eigen_path /usr/include/eigen3 \
    --minimal_build extended \
    --disable_contrib_ops \
    --cmake_generator Ninja \
    --parallel "${JOBS}" \
    --target onnxruntime \
    \
    --cmake_extra_defines CMAKE_SYSTEM_NAME=Darwin \
    --cmake_extra_defines CMAKE_SYSTEM_PROCESSOR=x86_64 \
    --cmake_extra_defines CMAKE_OSX_ARCHITECTURES=x86_64 \
    --cmake_extra_defines CMAKE_OSX_SYSROOT="$SYSROOT" \
    --cmake_extra_defines CMAKE_OSX_DEPLOYMENT_TARGET="$MACOS_VERSION" \
    --cmake_extra_defines CMAKE_CXX_STANDARD=17 \
    --cmake_extra_defines CMAKE_CXX_STANDARD_REQUIRED=ON \
    --cmake_extra_defines CMAKE_CROSSCOMPILING=ON \
    --cmake_extra_defines CMAKE_BUILD_WITH_INSTALL_RPATH=ON \
    --cmake_extra_defines CMAKE_INSTALL_RPATH=@rpath \
    --cmake_extra_defines CMAKE_MACOSX_RPATH=ON \
    --cmake_extra_defines onnxruntime_BUILD_UNIT_TESTS=OFF \
    --cmake_extra_defines onnxruntime_ENABLE_TRAINING=OFF \
    --cmake_extra_defines FLATBUFFERS_BUILD_FLATC=OFF \
    --cmake_extra_defines FLATBUFFERS_BUILD_TESTS=OFF \
    --cmake_extra_defines onnxruntime_BUILD_CUSTOM_OP_LIBRARY=OFF

BUILD_STATUS=$?
cd .. # Return to the original SCRIPT_ROOT

if [ $BUILD_STATUS -ne 0 ]; then
    echo "ONNX Runtime build failed. Check the logs above for errors."
    echo "The most common issues are incorrect sysroot paths or compiler mismatches."
    exit 1
fi

echo "ONNX Runtime build successful!"

# --- 6. Set Environment for Downstream (e.g., Rust) Builds ---

echo "Setting environment variables for the custom-built ONNX Runtime..."

# Auto-detect the built library
BUILD_ROOT="$SCRIPT_ROOT/onnxruntime/build"
ORT_LIB_FILE=$(find "$BUILD_ROOT" -type f -name "libonnxruntime.*.dylib" | head -n1)

if [ -z "${ORT_LIB_FILE:-}" ]; then
    echo "FATAL: Could not find the built onnxruntime dylib in $BUILD_ROOT" >&2
    exit 1
fi

export ORT_LIB_LOCATION="$(dirname "$ORT_LIB_FILE")"
export ORT_INCLUDE_LOCATION="$SCRIPT_ROOT/onnxruntime/include"

# These tell downstream build systems (like the Rust onnxruntime crate) to use our custom build
export ORT_USE_SYSTEM_LIB=1
export ORT_STRATEGY=system

echo "Environment set:"
echo "  ORT_LIB_LOCATION:     $ORT_LIB_LOCATION"
echo "  ORT_INCLUDE_LOCATION: $ORT_INCLUDE_LOCATION"

# Set up cross-compilation environment for Rust/Cargo
export CC_x86_64_apple_darwin=x86_64-apple-darwin24-clang
export CXX_x86_64_apple_darwin=x86_64-apple-darwin24-clang++
export AR_x86_64_apple_darwin=x86_64-apple-darwin24-ar
export CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER="$CC_x86_64_apple_darwin"

# Add the macOS target if not already added
if command -v rustup >/dev/null 2>&1; then
    rustup target add x86_64-apple-darwin
fi

echo "Setup complete. You can now run your macOS-targeted build."

# Example: Build a Rust project for macOS
# if [ -f "Cargo.toml" ] && command -v cargo >/dev/null 2>&1; then
#     echo "Building Rust project for macOS..."
#     cargo build --release --target x86_64-apple-darwin
# else
#     echo "No Cargo.toml found or cargo not installed. Skipping Rust build step."
# fi