#![cfg_attr(
    feature = "cpu-renderer",
    allow(
        dead_code,
        clippy::default_constructed_unit_structs,
        clippy::large_enum_variant,
    )
)]

pub mod reexports {
    pub use winit;
}

use std::sync::Arc;

#[cfg(target_os = "linux")]
use std::{
    ffi::OsString,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
};

use crate::{
    config::LaunchConfig,
    renderer::{LaunchProxy, NativeEvent, NativeGenericEvent, WinitRenderer},
};
mod accessibility;
pub mod config;
mod drivers;
pub mod extensions;
pub mod integration;
pub mod plugins;
pub mod renderer;
#[cfg(feature = "tray")]
mod tray_icon;
mod window;
mod winit_mappings;

pub use extensions::*;
use futures_util::task::{ArcWake, waker};

use crate::winit::event_loop::EventLoopProxy;

pub mod winit {
    pub use winit::*;
}

#[cfg(feature = "tray")]
pub mod tray {
    pub use tray_icon::*;

    pub use crate::tray_icon::*;
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinuxDisplayBackend {
    Wayland,
    X11,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Default)]
struct LinuxDisplayEnvironment {
    wayland_display: Option<OsString>,
    wayland_socket: Option<OsString>,
    xdg_runtime_dir: Option<PathBuf>,
    x11_display: Option<OsString>,
}

#[cfg(target_os = "linux")]
impl LinuxDisplayEnvironment {
    fn current() -> Self {
        Self {
            wayland_display: non_empty_env("WAYLAND_DISPLAY"),
            wayland_socket: non_empty_env("WAYLAND_SOCKET"),
            xdg_runtime_dir: non_empty_env("XDG_RUNTIME_DIR").map(PathBuf::from),
            x11_display: non_empty_env("DISPLAY"),
        }
    }

    fn wayland_is_advertised(&self) -> bool {
        self.wayland_display.is_some() || self.wayland_socket.is_some()
    }

    fn x11_is_advertised(&self) -> bool {
        self.x11_display.is_some()
    }

    fn wayland_socket_path(&self) -> Option<PathBuf> {
        let display = Path::new(self.wayland_display.as_ref()?);
        if display.is_absolute() {
            return Some(display.to_owned());
        }

        let runtime_dir = self.xdg_runtime_dir.as_deref()?;
        runtime_dir.is_absolute().then(|| runtime_dir.join(display))
    }

    fn wayland_is_reachable(&self) -> bool {
        // WAYLAND_SOCKET is an inherited, already-connected file descriptor. Probing it
        // would consume ownership before winit can use it, so leave validation to winit.
        self.wayland_socket.is_some()
            || self
                .wayland_socket_path()
                .is_some_and(|path| UnixStream::connect(path).is_ok())
    }
}

#[cfg(target_os = "linux")]
fn non_empty_env(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

#[cfg(target_os = "linux")]
fn select_linux_display_backend(
    wayland_advertised: bool,
    wayland_reachable: bool,
    x11_advertised: bool,
) -> Result<LinuxDisplayBackend, &'static str> {
    match (wayland_advertised, wayland_reachable, x11_advertised) {
        (true, true, _) => Ok(LinuxDisplayBackend::Wayland),
        (true, false, true) | (false, _, true) => Ok(LinuxDisplayBackend::X11),
        (true, false, false) => Err(
            "Wayland is advertised but its compositor socket is unreachable, and DISPLAY is not set for an X11 fallback",
        ),
        (false, _, false) => {
            Err("neither a Wayland compositor socket nor an X11 DISPLAY is available")
        }
    }
}

fn build_default_event_loop() -> Result<winit::event_loop::EventLoop<NativeEvent>, String> {
    let mut builder = winit::event_loop::EventLoop::<NativeEvent>::with_user_event();

    #[cfg(target_os = "linux")]
    {
        use winit::{
            platform::wayland::EventLoopBuilderExtWayland, platform::x11::EventLoopBuilderExtX11,
        };

        let environment = LinuxDisplayEnvironment::current();
        let wayland_advertised = environment.wayland_is_advertised();
        let wayland_reachable = wayland_advertised && environment.wayland_is_reachable();
        let backend = select_linux_display_backend(
            wayland_advertised,
            wayland_reachable,
            environment.x11_is_advertised(),
        )
        .map_err(str::to_owned)?;

        match backend {
            LinuxDisplayBackend::Wayland => {
                builder.with_wayland();
            }
            LinuxDisplayBackend::X11 => {
                if wayland_advertised && !wayland_reachable {
                    tracing::warn!(
                        "Wayland is advertised but no compositor is reachable; falling back to X11"
                    );
                }
                builder.with_x11();
            }
        }
    }

    builder
        .build()
        .map_err(|error| format!("winit could not initialize a display backend: {error}"))
}

/// Launch the application.
///
/// If a custom event loop was provided via [`LaunchConfig::with_event_loop`], it will be used.
/// Otherwise a default one is created.
pub fn launch(mut launch_config: LaunchConfig) {
    use std::collections::HashMap;

    use freya_core::integration::*;
    use freya_engine::prelude::{FontCollection, FontMgr, TypefaceFontProvider};
    #[cfg(all(not(debug_assertions), not(target_os = "android")))]
    {
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic_info| {
            rfd::MessageDialog::new()
                .set_title("Fatal Error")
                .set_description(&panic_info.to_string())
                .set_level(rfd::MessageLevel::Error)
                .show();
            previous_hook(panic_info);
            std::process::exit(1);
        }));
    }

    let event_loop = launch_config.event_loop.take().unwrap_or_else(|| {
        build_default_event_loop()
            .unwrap_or_else(|error| panic!("Failed to create event loop: {error}"))
    });

    let proxy = event_loop.create_proxy();

    let mut font_collection = FontCollection::new();
    let def_mgr = FontMgr::default();
    let mut provider = TypefaceFontProvider::new();
    for (font_name, font_data) in launch_config.embedded_fonts {
        let ft_type = def_mgr
            .new_from_data(&font_data, None)
            .unwrap_or_else(|| panic!("Failed to load font {font_name}."));
        provider.register_typeface(ft_type, Some(font_name.as_ref()));
    }
    let font_mgr: FontMgr = provider.into();
    font_collection.set_default_font_manager(def_mgr, None);
    font_collection.set_dynamic_font_manager(font_mgr.clone());
    font_collection.paragraph_cache_mut().turn_on(false);

    let screen_reader = ScreenReader::new();

    struct FuturesWaker(EventLoopProxy<NativeEvent>);

    impl ArcWake for FuturesWaker {
        fn wake_by_ref(arc_self: &Arc<Self>) {
            _ = arc_self
                .0
                .send_event(NativeEvent::Generic(NativeGenericEvent::PollFutures));
        }
    }

    let waker = waker(Arc::new(FuturesWaker(proxy.clone())));

    let mut renderer = WinitRenderer {
        windows: HashMap::default(),
        #[cfg(feature = "tray")]
        tray: launch_config.tray,
        #[cfg(all(feature = "tray", not(target_os = "linux")))]
        tray_icon: None,
        resumed: false,
        futures: launch_config
            .tasks
            .into_iter()
            .map(|task| task(LaunchProxy(proxy.clone())))
            .collect::<Vec<_>>(),
        proxy,
        font_manager: font_mgr,
        font_collection,
        windows_configs: launch_config.windows_configs,
        plugins: launch_config.plugins,
        fallback_fonts: launch_config.fallback_fonts,
        screen_reader,
        waker,
        exit_on_close: launch_config.exit_on_close,
    };

    #[cfg(feature = "tray")]
    {
        use crate::{
            renderer::{NativeTrayEvent, NativeTrayEventAction},
            tray::{TrayIconEvent, menu::MenuEvent},
        };

        let proxy = renderer.proxy.clone();
        MenuEvent::set_event_handler(Some(move |event| {
            let _ = proxy.send_event(NativeEvent::Tray(NativeTrayEvent {
                action: NativeTrayEventAction::MenuEvent(event),
            }));
        }));
        let proxy = renderer.proxy.clone();
        TrayIconEvent::set_event_handler(Some(move |event| {
            let _ = proxy.send_event(NativeEvent::Tray(NativeTrayEvent {
                action: NativeTrayEventAction::TrayEvent(event),
            }));
        }));

        #[cfg(target_os = "linux")]
        if let Some(tray_icon) = renderer.tray.0.take() {
            std::thread::spawn(move || {
                if !gtk::is_initialized() {
                    if gtk::init().is_ok() {
                        tracing::debug!("Tray: GTK initialized");
                    } else {
                        tracing::error!("Tray: Failed to initialize GTK");
                    }
                }

                let _tray_icon = (tray_icon)();

                gtk::main();
            });
        }
    }

    event_loop.run_app(&mut renderer).unwrap();
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::{LinuxDisplayBackend, select_linux_display_backend};

    #[test]
    fn reachable_wayland_is_preferred_over_x11() {
        assert_eq!(
            select_linux_display_backend(true, true, true),
            Ok(LinuxDisplayBackend::Wayland)
        );
    }

    #[test]
    fn unreachable_wayland_falls_back_to_x11() {
        assert_eq!(
            select_linux_display_backend(true, false, true),
            Ok(LinuxDisplayBackend::X11)
        );
    }

    #[test]
    fn a_single_advertised_backend_is_selected() {
        assert_eq!(
            select_linux_display_backend(true, true, false),
            Ok(LinuxDisplayBackend::Wayland)
        );
        assert_eq!(
            select_linux_display_backend(false, false, true),
            Ok(LinuxDisplayBackend::X11)
        );
    }

    #[test]
    fn missing_or_unreachable_backends_return_a_diagnostic() {
        assert!(select_linux_display_backend(false, false, false).is_err());
        assert!(select_linux_display_backend(true, false, false).is_err());
    }
}
