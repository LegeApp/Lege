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
    pub const fn light() -> Self {
        Self {
            metrics: ThemeMetrics {
                toolbar_height: 42.0,
                status_height: 24.0,
                scrollbar_width: 14.0,
                sidebar_width: 260.0,
                page_gap: 8.0,
                canvas_margin: 12.0,
                page_border: 1.0,
            },
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
}

impl Default for Theme {
    fn default() -> Self {
        Self::light()
    }
}
