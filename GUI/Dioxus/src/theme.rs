use std::fmt::Display;

#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub name: String,
    pub primary_background: String,
    pub secondary_background: String,
    pub gui_buttons: String,
    pub message_box: String,
    pub text_color: String,
    pub close_button: String,
    pub minimize_button: String,
    pub start_processing_button: String,
}

impl Theme {
    #[cfg(feature = "cotinga-theme")]
    pub fn cotinga_mystique() -> Self {
        Self {
            name: "Cotinga Mystique".to_string(),
            primary_background: "#ffffff".to_string(),
            secondary_background: "#ffffff".to_string(),
            gui_buttons: "#2c214a".to_string(),
            message_box: "#a2250a".to_string(),
            text_color: "#0c030aff".to_string(),
            close_button: "#2c214a".to_string(),
            minimize_button: "#2c214a".to_string(),
            start_processing_button: "#a2250a".to_string(),
        }
    }

    pub fn classic_windows() -> Self {
        Self {
            name: "Classic Windows".to_string(),
            primary_background: "#E2E8EB".to_string(), // Classic dialog chrome (COLOR_3DFACE)
            secondary_background: "#FFFAFA".to_string(), // Content panels stay bright and modern
            gui_buttons: "#E2E8EB".to_string(),        // Buttons inherit styling from CSS borders
            message_box: "#ffffe1".to_string(),        // Light amber info panels for contrast
            text_color: "#000000".to_string(),         // Crisp text
            close_button: "#E2E8EB".to_string(), // Neutral controls (glyph-only styling in CSS)
            minimize_button: "#E2E8EB".to_string(),
            start_processing_button: "#E2E8EB".to_string(),
        }
    }

    pub fn generate_css(&self) -> String {
        let is_classic = self.name == "Classic Windows";

        if is_classic {
            format!(
                r#"
                /* Theme: {} - Modernized Windows Classic */
                
                :root {{
                    --app-bg: {};
                    --panel-bg: {};
                    --bg-color: {};
                    --content-bg: {};
                    --text-color: {};
                    --btn-face: {};
                    --info-bg: {};
                    --info-text: {};
                    --outline-color: var(--border-shadow);
                    --panel-border-color: var(--border-shadow);
                    --active-caption: #0053e1;
                    --caption-text: #ffffff;
                    --border-light: #ffffff;
                    --border-shadow: #808080;
                    --border-dark: #404040;
                    --control-font-size: 15px;
                    --control-text-color: {};
                    --control-font-family: Tahoma, 'Segoe UI', system-ui, sans-serif;
                    --statusbar-height: 38px;
                }}
                
                body {{
                    background-color: var(--bg-color) !important;
                    font-family: Tahoma, 'Segoe UI', system-ui, sans-serif !important;
                    font-size: 11px !important;
                    color: var(--text-color) !important;
                }}

                .text-on-paper,
                .heading-on-paper {{
                    color: var(--text-color) !important;
                    text-shadow: none !important;
                }}

                .heading-on-paper {{
                    font-weight: 700 !important;
                }}

                .settings-card {{
                    background-color: var(--content-bg) !important;
                    border: 1px solid var(--border-shadow);
                    box-shadow: inset 1px 1px 0 var(--border-dark);
                    padding: 12px;
                    border-radius: 0 !important;
                }}

                .raised-btn {{
                    background: var(--btn-face) !important;
                    color: var(--text-color) !important;
                    border-style: solid;
                    border-width: 1px;
                    border-color: var(--border-light) var(--border-shadow) var(--border-shadow) var(--border-light);
                    box-shadow: none !important;
                    border-radius: 0 !important;
                    padding: 4px 12px !important;
                    font-size: 11px !important;
                    text-shadow: none !important;
                    transition: none !important;
                }}
                
                .raised-btn.primary-action-btn {{
                    padding: 8px 14px !important;
                    font-weight: 600 !important;
                }}
                
                .raised-btn:active {{
                    border-color: var(--border-dark) var(--border-light) var(--border-light) var(--border-dark);
                    transform: none !important;
                }}

                #start-processing-btn {{
                    border: 2px solid var(--border-shadow);
                    border-color: var(--border-light) var(--border-shadow) var(--border-shadow) var(--border-light);
                    width: 120px !important;
                    height: 45px !important;
                    padding: 0 !important;
                }}

                #start-processing-btn:active {{
                    border-color: var(--border-dark) var(--border-light) var(--border-light) var(--border-dark);
                }}

                .raised-btn.danger,
                .raised-btn.success,
                .raised-btn.secondary,
                .raised-btn.warning {{
                    background: var(--btn-face) !important;
                    color: var(--text-color) !important;
                    border-color: var(--border-light) var(--border-shadow) var(--border-shadow) var(--border-light);
                }}

                .setting-input,
                input[type="text"],
                input[type="number"] {{
                    background: var(--content-bg) !important;
                    color: var(--text-color) !important;
                    border: 1px solid var(--border-shadow) !important;
                    box-shadow: inset 1px 1px 0 var(--border-dark) !important;
                    border-radius: 0 !important;
                    padding: 3px 4px !important;
                    font-size: 11px !important;
                }}
                
                .setting-input:focus,
                input[type="text"]:focus,
                input[type="number"]:focus {{
                    outline: 1px dotted var(--text-color) !important;
                    outline-offset: -2px !important;
                }}

                .title-bar {{
                    background-color: var(--active-caption) !important;
                    color: var(--caption-text) !important;
                    padding: 4px;
                    display: flex;
                    justify-content: space-between;
                    align-items: center;
                    font-weight: bold;
                }}

                .window-controls {{
                    display: flex;
                    gap: 2px;
                }}

                .window-control-btn {{
                    background: var(--btn-face) !important;
                    border-color: var(--border-light) var(--border-shadow) var(--border-shadow) var(--border-light) !important;
                    border-width: 1px !important;
                    border-style: solid !important;
                    width: 20px !important;
                    height: 20px !important;
                    font-family: 'Marlett', 'Segoe UI Symbol', sans-serif;
                    font-size: 12px;
                    color: black !important;
                    border-radius: 0 !important;
                }}

                .window-control-btn:active {{
                    border-color: var(--border-dark) var(--border-light) var(--border-light) var(--border-dark) !important;
                }}
                
                .status-bar-root {{
                    background: var(--bg-color);
                    padding: 4px 8px;
                    min-height: var(--statusbar-height);
                    border-top: 1px solid var(--border-shadow);
                    box-shadow: inset 0px 1px 0px var(--border-light);
                }}

                .message-box,
                .provider-status {{
                    background: var(--info-bg) !important;
                    color: var(--text-color) !important;
                    border-style: solid;
                    border-width: 1px;
                    border-color: var(--border-dark) var(--border-light) var(--border-light) var(--border-dark);
                }}

                .settings-container {{
                    gap: 8px !important;
                    display: flex;
                    height: calc(70% + 10px);
                    min-height: 320px;
                    position: relative;
                }}

                .setting-item {{
                    margin: 4px 0 !important;
                }}

                .start-button-container {{
                    position: absolute;
                    bottom: 20px !important;
                    left: 50%;
                    transform: translate(-50%, 28px);
                    z-index: 10;
                }}

                .settings-panel-left,
                .settings-panel-center,
                .settings-panel-right {{
                    position: relative;
                }}
                "#,
                self.name,
                self.primary_background,   // --app-bg
                self.secondary_background, // --panel-bg
                self.primary_background,   // --bg-color
                self.secondary_background, // --content-bg
                self.text_color,           // --text-color
                self.gui_buttons,          // --btn-face
                self.message_box,          // --info-bg
                self.text_color,           // --info-text
                self.text_color            // --control-text-color
            )
        } else {
            // Original Cotinga Mystique theme with gradients and shadows
            format!(
                r#"
                /* Theme: {} */
                
                :root {{
                    --app-bg: {};
                    --panel-bg: {};
                    --info-bg: {};
                    --info-text: #ffffff;
                    --outline-color: {}AA;
                    --panel-border-color: {}66;
                    --control-font-size: 15px;
                    --control-text-color: {};
                    --control-font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
                    --statusbar-height: 38px;
                }}

                /* Text colors */
                .text-on-paper {{
                    color: {} !important;
                    text-shadow: 
                        0 1px 0 rgba(255, 255, 255, 0.1),
                        0 0 1px rgba(0, 0, 0, 0.3);
                    font-weight: 500;
                }}
                
                .heading-on-paper {{
                    color: {} !important;
                    text-shadow: 
                        0 2px 3px rgba(255, 255, 255, 0.1),
                        0 1px 0 rgba(0, 0, 0, 0.3);
                    font-weight: 600;
                }}

                /* Button base styling with theme colors */
                .raised-btn {{
                    background: linear-gradient(to bottom, {}, {}) !important;
                    color: white !important;
                    text-shadow: 0 1px 0 rgba(0, 0, 0, 0.3);
                }}
                
                .raised-btn.primary-action-btn {{
                    padding: 8px 14px !important;
                    font-weight: 600 !important;
                }}

                /* Start processing button - carmine red */
                .raised-btn.success {{
                    background: linear-gradient(to bottom, {}, {}dd) !important;
                    color: white !important;
                    text-shadow: 0 1px 0 rgba(0, 0, 0, 0.3);
                    transition: filter 0.2s ease;
                }}
                
                .raised-btn.success:hover {{
                    filter: brightness(1.1) !important;
                }}
                
                .raised-btn.success:active {{
                    filter: brightness(0.9) !important;
                }}

                /* Window control buttons */
                .minimize-btn {{
                    background: {} !important;
                    color: white !important;
                }}
                
                .close-btn {{
                    background: {} !important;
                    color: white !important;
                }}


                /* Settings cards - keep secondary background */
                .settings-card {{
                    background-color: {} !important;
                    border: 1px solid {}44;
                }}

                /* Header bar and status bar - secondary background */
                div[style*="background: white"] {{
                    background: {} !important;
                }}

                /* Ensure status bar remains anchored at the bottom */
                .status-bar-root {{
                    position: sticky;
                    bottom: 0;
                    width: 100%;
                }}

                /* Grabbing state on header drag */
                .header-draggable:active {{
                    cursor: grabbing !important;
                }}

                /* Message box styling */
                .message-box,
                .provider-status {{
                    border-color: {} !important;
                    border-left-color: {} !important;
                }}

                /* --- Compact panels with optimized height for popup space --- */
                .settings-container {{
                    display: flex;
                    gap: 20px;
                    height: calc(70% + 10px);
                    min-height: 320px;
                    position: relative;
                }}

                .start-button-container {{
                    position: absolute;
                    bottom: 20px !important; /* base offset above status bar */
                    left: 50%;
                    transform: translate(-50%, 28px); /* move ~28px downward */
                    z-index: 10;
                }}

                .settings-panel-left, .settings-panel-center, .settings-panel-right {{
                    position: relative;
                }}

                "#,
                self.name,
                self.primary_background,   // app background
                self.secondary_background, // panel background
                self.message_box,          // info background
                self.gui_buttons,          // outline color base
                self.gui_buttons,          // panel border base
                self.text_color,           // control text color
                self.text_color,           // text-on-paper color
                self.text_color,           // heading-on-paper color
                self.gui_buttons,
                self.gui_buttons, // button base colors
                self.start_processing_button,
                self.start_processing_button, // success button
                self.minimize_button,         // minimize button
                self.close_button,            // close button
                self.secondary_background,    // settings card background
                self.gui_buttons,             // settings card border
                self.secondary_background,    // header/status background
                self.message_box,
                self.message_box // message box colors
            )
        }
    }
}

impl Display for Theme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
    }
}

pub struct ThemeManager {
    current_theme: Theme,
}

impl ThemeManager {
    pub fn new() -> Self {
        Self {
            #[cfg(feature = "cotinga-theme")]
            current_theme: Theme::cotinga_mystique(),
            #[cfg(not(feature = "cotinga-theme"))]
            current_theme: Theme::classic_windows(),
        }
    }

    pub fn get_css(&self) -> String {
        self.current_theme.generate_css()
    }
}

impl Default for ThemeManager {
    fn default() -> Self {
        Self::new()
    }
}
