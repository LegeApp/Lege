#[derive(Debug, Clone, Copy)]
pub struct ThemeMetrics {
    pub toolbar_height: f64,
    pub status_height: f64,
    pub scrollbar_width: f64,
    pub sidebar_width: f64,
    pub page_gap: f64,
    pub canvas_margin: f64,
    pub page_border: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct ThemeColors {
    pub window: u32,
    pub chrome: u32,
    pub canvas: u32,
    pub paper: u32,
    pub page_border: u32,
    pub text: u32,
    pub muted_text: u32,
    pub selection: u32,
    pub error: u32,
    pub scrollbar_track: u32,
    pub scrollbar_thumb: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub metrics: ThemeMetrics,
    pub colors: ThemeColors,
}

impl Theme {
    const METRICS: ThemeMetrics = ThemeMetrics {
        toolbar_height: 42.0,
        status_height: 24.0,
        scrollbar_width: 14.0,
        sidebar_width: 260.0,
        page_gap: 8.0,
        canvas_margin: 12.0,
        page_border: 1.0,
    };

    pub const fn light() -> Self {
        Self {
            metrics: Self::METRICS,
            colors: ThemeColors {
                window: 0x00f0f0f0,
                chrome: 0x00ececec,
                canvas: 0x00c9c9c9,
                paper: 0x00ffffff,
                page_border: 0x00909090,
                text: 0x00181818,
                muted_text: 0x00686868,
                selection: 0x00568bd2,
                error: 0x00b93a32,
                scrollbar_track: 0x00d8d8d8,
                scrollbar_thumb: 0x00888888,
            },
        }
    }

    pub const fn night() -> Self {
        Self {
            metrics: Self::METRICS,
            colors: ThemeColors {
                window: 0x001d1d1d,
                chrome: 0x00212121,
                canvas: 0x00161616,
                paper: 0x00252525,
                page_border: 0x00484848,
                text: 0x00d8d1c4,
                muted_text: 0x009b968e,
                selection: 0x00638bc0,
                error: 0x00dc6b60,
                scrollbar_track: 0x002d2d2d,
                scrollbar_thumb: 0x00686868,
            },
        }
    }

    pub const fn warm() -> Self {
        Self {
            metrics: Self::METRICS,
            colors: ThemeColors {
                paper: 0x00f2e8d2,
                canvas: 0x00c9bfaa,
                ..Self::light().colors
            },
        }
    }

    /// Native, compact descendants of the Freya Sanzo Wada palettes. Keeping
    /// them as theme tokens means the Winit UI retains the visual work without
    /// importing a widget toolkit or a theme browser.
    pub const fn sanzo_earth() -> Self {
        Self {
            metrics: Self::METRICS,
            colors: ThemeColors {
                window: 0x003a_3430,
                chrome: 0x004c_443d,
                canvas: 0x002c_2825,
                paper: 0x00f2_e5d2,
                page_border: 0x0098_765b,
                text: 0x00f7_eee2,
                muted_text: 0x00d1_b9a0,
                selection: 0x00c7_8050,
                error: 0x00e2_7160,
                scrollbar_track: 0x0036_302c,
                scrollbar_thumb: 0x009a_8068,
            },
        }
    }

    pub const fn sanzo_sea() -> Self {
        Self {
            metrics: Self::METRICS,
            colors: ThemeColors {
                window: 0x001d_3035,
                chrome: 0x0026_4147,
                canvas: 0x0017_272c,
                paper: 0x00e9_f0e9,
                page_border: 0x006a_9294,
                text: 0x00e7_f3ef,
                muted_text: 0x00af_cbc5,
                selection: 0x004c_a5a0,
                error: 0x00e7_7770,
                scrollbar_track: 0x0020_373d,
                scrollbar_thumb: 0x006d_9da0,
            },
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::light()
    }
}
