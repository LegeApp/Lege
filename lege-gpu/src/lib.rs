// Android's shared fallbacks compile cleanly and then misbehave silently, so
// the platform feature must not be optional. See `src/android.rs`.
#[cfg(all(target_os = "android", not(feature = "android")))]
compile_error!(
    "building lege-gpu for Android requires the `android` feature \
     (e.g. --features android, or lege/android which enables it)"
);

#[cfg(all(feature = "android", not(target_os = "android")))]
compile_error!("the `android` feature is only valid for Android targets");

#[cfg(feature = "android")]
pub mod android;

pub mod binarization;
pub mod compute;
#[cfg(feature = "presentation")]
pub mod presentation;
pub mod resize;
pub mod wgpu_setup;

pub mod vision;
