#[cfg(all(
    not(feature = "cpu-renderer"),
    any(target_os = "linux", target_os = "windows", target_os = "android")
))]
mod gl;
#[cfg(all(not(feature = "cpu-renderer"), target_os = "macos"))]
mod metal;
#[cfg(feature = "cpu-renderer")]
mod software;
#[cfg(all(
    not(feature = "cpu-renderer"),
    feature = "vulkan",
    any(target_os = "linux", target_os = "windows")
))]
mod vulkan;

#[cfg(not(feature = "cpu-renderer"))]
use freya_engine::prelude::Surface as SkiaSurface;
#[cfg(feature = "cpu-renderer")]
use freya_render_tiny_skia::prelude::TinySkiaRenderer;
use winit::{
    dpi::PhysicalSize,
    event_loop::ActiveEventLoop,
    window::{Window, WindowAttributes},
};

#[allow(clippy::large_enum_variant)]
pub enum GraphicsDriver {
    #[cfg(feature = "cpu-renderer")]
    Software(software::SoftwareDriver),
    #[cfg(all(
        not(feature = "cpu-renderer"),
        any(target_os = "linux", target_os = "windows", target_os = "android")
    ))]
    OpenGl(gl::OpenGLDriver),
    #[cfg(all(not(feature = "cpu-renderer"), target_os = "macos"))]
    Metal(metal::MetalDriver),
    #[cfg(all(
        not(feature = "cpu-renderer"),
        feature = "vulkan",
        any(target_os = "linux", target_os = "windows")
    ))]
    Vulkan(vulkan::VulkanDriver),
}

impl GraphicsDriver {
    #[allow(clippy::needless_return)]
    pub fn new(
        event_loop: &ActiveEventLoop,
        window_attributes: WindowAttributes,
    ) -> (Self, Window) {
        #[cfg(feature = "cpu-renderer")]
        {
            let window = event_loop
                .create_window(window_attributes)
                .expect("Could not create window for software renderer");

            return (Self::Software(software::SoftwareDriver::new()), window);
        }

        // Metal (macOS)
        #[cfg(all(not(feature = "cpu-renderer"), target_os = "macos"))]
        {
            let (driver, window) = metal::MetalDriver::new(event_loop, window_attributes);

            return (Self::Metal(driver), window);
        }

        // OpenGL only on Android.
        #[cfg(all(not(feature = "cpu-renderer"), target_os = "android"))]
        {
            let (driver, window) = gl::OpenGLDriver::new(event_loop, window_attributes);

            return (Self::OpenGl(driver), window);
        }

        #[cfg(all(
            feature = "vulkan",
            not(feature = "cpu-renderer"),
            not(target_os = "macos"),
            not(target_os = "android")
        ))]
        {
            let force_opengl =
                std::env::var("FREYA_RENDERER").is_ok_and(|v| v.eq_ignore_ascii_case("opengl"));

            if !force_opengl {
                let vk_attrs = window_attributes.clone();
                match vulkan::VulkanDriver::new(event_loop, vk_attrs) {
                    Ok((driver, window)) => return (Self::Vulkan(driver), window),
                    Err(err) => {
                        tracing::warn!(
                            "Vulkan initialization failed, falling back to OpenGL: {err}"
                        );
                    }
                }
            }

            let (driver, window) = gl::OpenGLDriver::new(event_loop, window_attributes);

            return (Self::OpenGl(driver), window);
        }

        #[cfg(all(
            not(feature = "cpu-renderer"),
            not(feature = "vulkan"),
            not(target_os = "macos"),
            not(target_os = "android")
        ))]
        {
            let (driver, window) = gl::OpenGLDriver::new(event_loop, window_attributes);

            return (Self::OpenGl(driver), window);
        }
    }

    #[cfg(not(feature = "cpu-renderer"))]
    pub fn present(
        &mut self,
        _size: PhysicalSize<u32>,
        window: &Window,
        render: impl FnOnce(&mut SkiaSurface),
    ) {
        match self {
            #[cfg(any(target_os = "linux", target_os = "windows", target_os = "android"))]
            Self::OpenGl(gl) => gl.present(window, render),
            #[cfg(target_os = "macos")]
            Self::Metal(mtl) => mtl.present(_size, window, render),
            #[cfg(all(feature = "vulkan", any(target_os = "linux", target_os = "windows")))]
            Self::Vulkan(vk) => vk.present(_size, window, render),
        }
    }

    #[cfg(feature = "cpu-renderer")]
    pub fn present(
        &mut self,
        size: PhysicalSize<u32>,
        window: &Window,
        render: impl FnOnce(&mut TinySkiaRenderer),
    ) {
        match self {
            Self::Software(software) => {
                if let Err(err) = software.present(size, window, render) {
                    tracing::error!("Software renderer presentation failed: {err:?}");
                }
            }
        }
    }

    /// The name of the active graphics driver.
    pub fn name(&self) -> &'static str {
        match self {
            #[cfg(feature = "cpu-renderer")]
            Self::Software(_) => "Software",
            #[cfg(all(
                not(feature = "cpu-renderer"),
                any(target_os = "linux", target_os = "windows", target_os = "android")
            ))]
            Self::OpenGl(_) => "OpenGL",
            #[cfg(all(not(feature = "cpu-renderer"), target_os = "macos"))]
            Self::Metal(_) => "Metal",
            #[cfg(all(
                not(feature = "cpu-renderer"),
                feature = "vulkan",
                any(target_os = "linux", target_os = "windows")
            ))]
            Self::Vulkan(_) => "Vulkan",
        }
    }

    pub fn resize(&mut self, size: PhysicalSize<u32>) {
        match self {
            #[cfg(feature = "cpu-renderer")]
            Self::Software(software) => software.resize(size),
            #[cfg(all(
                not(feature = "cpu-renderer"),
                any(target_os = "linux", target_os = "windows", target_os = "android")
            ))]
            Self::OpenGl(gl) => gl.resize(size),
            #[cfg(all(not(feature = "cpu-renderer"), target_os = "macos"))]
            Self::Metal(mtl) => mtl.resize(size),
            #[cfg(all(
                not(feature = "cpu-renderer"),
                feature = "vulkan",
                any(target_os = "linux", target_os = "windows")
            ))]
            Self::Vulkan(vk) => vk.resize(size),
        }
    }
}
