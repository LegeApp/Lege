// Static CSS styles - computed at compile time to reduce startup overhead

pub const BASE_STYLES: &str = r#"
    * { margin: 0; padding: 0; box-sizing: border-box; }
    body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif; background-color: var(--app-bg, #f5f5f5); }
    html, body { height: 100%; overflow: hidden; }

    :root {
        --control-font-size: 15px;
        --control-text-color: #555;
        --control-font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, sans-serif;
        --statusbar-height: 50px;
    }

    .app-shell {
        min-height: 100vh;
        display: flex;
        flex-direction: column;
        background: var(--app-bg, #f5f5f5);
        padding: 4px;
        gap: 8px;
    }

    .app-content-shell {
        flex: 1;
        display: flex;
        flex-direction: column;
        background: var(--panel-bg, #ffffff);
        border: 1px solid var(--outline-color, rgba(0, 0, 0, 0.12));
        border-radius: 8px;
        overflow: hidden;
    }

    .app-content-inner {
        padding: 8px;
        display: flex;
        flex-direction: column;
        gap: 6px;
        flex: 1;
        min-width: 650px;
        overflow: hidden;
        background: var(--app-bg, #f5f5f5);
    }
    
    /* Settings Panel Styles */
    .settings-container {
        display: flex;
        justify-content: space-between;
        gap: 20px;
        align-items: flex-start;
    }

    .settings-panel-left,
    .settings-panel-center,
    .settings-panel-right {
        flex: 0 0 280px; /* fixed column width to guarantee 3-panel layout */
        width: 280px;
    }

    .settings-card {
        display: flex;
        flex-direction: column;
        gap: 8px;
        padding: 12px;
        border: 1px solid var(--panel-border-color, var(--outline-color, rgba(0, 0, 0, 0.12)));
        border-radius: 4px;
        background-color: var(--panel-bg, #fdfdfd);
        /* Fixed height to accommodate maximum possible content in either panel
           Right panel when Sauvola mode is expanded is the tallest scenario:
           Page Range + Target Height + 3 binarization radio buttons + 3 Sauvola inputs = 8 items
           Left panel maximum: 7 items (when PDF compatibility is shown)
           ~340px accommodates the right panel's maximum content and prevents intersection */
        height: 340px;
        min-height: 340px;
        max-height: 340px;
    }

    /* Right card can scroll to avoid overflow when Sauvola expands */
    .settings-card-right {
        overflow-y: auto;
    }

    /* Ensure center panel adopts the same constraints */
    .settings-card-center { overflow-y: auto; }  /* Add overflow like right panel for longer content */

    .settings-heading {
        margin: 0 0 8px 0;
        font-size: 16px;
        font-weight: 600;
        color: #333;
        border-bottom: 1px solid #eee;
        padding-bottom: 6px;
        text-align: center;
    }

    .setting-item {
        display: flex;
        align-items: center;
        justify-content: space-between;
        gap: 8px;
    }

    .setting-label {
        font-family: var(--control-font-family);
        font-size: var(--control-font-size);
        color: var(--control-text-color);
        white-space: nowrap;
    }

    .setting-input {
        padding: 4px 6px;
        border: 1px solid #ccc;
        border-radius: 3px;
        font-family: var(--control-font-family);
        font-size: var(--control-font-size);
        color: var(--control-text-color);
        width: 80px;
        box-sizing: border-box;
    }

    select.setting-input {
        appearance: none;
        background-color: var(--panel-bg, #fdfdfd) !important;
        color: var(--text-color, #333) !important;
        padding-right: 28px;
        border: 1px solid var(--panel-border-color, var(--outline-color, rgba(0, 0, 0, 0.12))) !important;
        scrollbar-color: var(--info-bg, #ffffe1) var(--panel-bg, #fdfdfd);
    }

    select.setting-input option {
        background-color: var(--panel-bg, #fdfdfd) !important;
        color: var(--text-color, #333) !important;
    }

    select.setting-input option:hover,
    select.setting-input option:checked {
        background-color: var(--info-bg, #ffffe1) !important;
        color: var(--text-color, #333) !important;
    }

    select.setting-input::-webkit-scrollbar {
        width: 10px;
        background-color: var(--panel-bg, #fdfdfd);
    }

    select.setting-input::-webkit-scrollbar-track {
        background-color: var(--panel-bg, #fdfdfd);
    }

    select.setting-input::-webkit-scrollbar-thumb {
        background-color: var(--info-bg, #ffffe1);
        border-radius: 8px;
        border: 2px solid var(--panel-bg, #fdfdfd);
    }

    select.setting-input::-webkit-scrollbar-thumb:hover {
        background-color: var(--info-bg, #f7d97a);
    }
    .radio-label {
        display: flex;
        align-items: center;
        gap: 8px; /* increased from 6px for better text-checkbox spacing */
        margin: 4px 0;
        font-family: var(--control-font-family);
        font-size: var(--control-font-size);
        color: var(--control-text-color);
        cursor: pointer;
    }

    .checkbox-label {
        display: flex;
        align-items: center;
        gap: 8px;
        margin: 4px 0;
        cursor: pointer;
        font-family: var(--control-font-family);
        font-size: var(--control-font-size);
        color: var(--control-text-color);
    }

    .checkbox-text {
        font-family: var(--control-font-family);
        font-size: var(--control-font-size);
        color: var(--control-text-color);
    }

    .setting-indented-group {
        margin-left: 20px;
        padding-left: 10px;
        border-left: 2px solid #f0f0f0;
        display: flex;
        flex-direction: column;
        gap: 6px;
    }

    /* Popup messages alongside the start button */
    .popup-rail {
        position: absolute;
        left: 0;
        top: 50%;
        transform: translateY(-50%);
        display: flex;
        flex-direction: column;
        align-items: flex-start;
        gap: 6px;
        max-width: 400px;
        width: auto;
        z-index: 100;
        pointer-events: none;
    }

    .popup-chip {
        pointer-events: auto;
        display: flex;
        align-items: flex-start;
        gap: 8px;
        background: var(--info-bg, #ffffe1);
        color: var(--info-text, #333);
        border: 1px solid var(--outline-color, rgba(0, 0, 0, 0.2));
        border-radius: 4px;
        padding: 10px 14px;
        max-width: 100%;
        width: auto;
        font-size: 13px;
        line-height: 1.5;
        box-shadow: 0 2px 8px rgba(0, 0, 0, 0.15);
        word-break: break-word;
        overflow-wrap: break-word;
    }

    .popup-chip__text {
        flex: 1;
        line-height: 1.5;
        word-wrap: break-word;
        overflow-wrap: break-word;
        white-space: normal;
        text-align: left;
        max-width: 100%;
        overflow: hidden;
    }

    .popup-chip__close {
        pointer-events: auto;
        background: transparent;
        border: none;
        color: inherit;
        font-size: 14px;
        cursor: pointer;
        width: 20px;
        height: 20px;
        display: flex;
        align-items: center;
        justify-content: center;
        padding: 0;
    }
    /* Base button styles - enhanced 3D pressable effect */
    .raised-btn {
        position: relative;
        padding: 12px 20px;
        border: none;
        border-radius: 8px;
        cursor: pointer;
        font-size: 14px;
        font-weight: 600;
        box-shadow: 
            0 4px 8px rgba(0, 0, 0, 0.15),
            0 2px 4px rgba(0, 0, 0, 0.1),
            inset 0 1px 0 rgba(255, 255, 255, 0.4);
        transition: all 0.1s ease-out;
        transform: translateY(0);
        outline: none;
    }
    
    .raised-btn:active {
        transform: translateY(2px);
        box-shadow: 
            0 1px 2px rgba(0, 0, 0, 0.2),
            inset 0 2px 4px rgba(0, 0, 0, 0.15);
    }

    .raised-btn.toggle-btn {
        padding: 4px 8px;
        font-size: 14px;
        min-width: 90px;
        text-align: center;
    }

    /* Left panel button - subtle click lightening effect */
    .raised-btn.left-panel-btn {
        background: linear-gradient(to bottom, #f8f9fa, #e9ecef);
        color: #495057;
        animation: none;
    }

    .raised-btn.left-panel-btn:active {
        background: linear-gradient(to bottom, #ffffff, #f8f9fa);
        animation: click-lighten 0.2s ease-out;
        transform: translateY(2px);
    }

    @keyframes click-lighten {
        0% {
            background: linear-gradient(to bottom, #ffffff, #f8f9fa);
        }
        100% {
            background: linear-gradient(to bottom, #f8f9fa, #e9ecef);
        }
    }

    .raised-btn.primary { 
        background: linear-gradient(to bottom, #4a86e8, #3a76d8);
        color: white;
        text-shadow: 0 1px 0 rgba(0, 0, 0, 0.2);
    }
    .raised-btn.primary:hover {
        background: linear-gradient(to bottom, #3a76d8, #2a66c8);
    }
    .raised-btn.primary:active {
        background: linear-gradient(to bottom, #2a66c8, #1a56b8);
    }

    /* Global spinner animation used in StatusBar */
    @keyframes spin {
        from { transform: rotate(0deg); }
        to { transform: rotate(360deg); }
    }
    
    .raised-btn.danger {
        background: linear-gradient(to bottom, #f44336, #d32f2f);
        color: white;
        text-shadow: 0 1px 0 rgba(0, 0, 0, 0.2);
    }
    .raised-btn.danger:hover {
        background: linear-gradient(to bottom, #d32f2f, #c62828);
    }
    .raised-btn.danger:active {
        background: linear-gradient(to bottom, #c62828, #b71c1c);
    }

    /* Keep start/cancel button red for consistency */
    #start-processing-btn.danger {
        background: linear-gradient(to bottom, #a2250a, #a2250add) !important;
        color: white !important;
    }
    #start-processing-btn.danger:hover {
        background: linear-gradient(to bottom, #a2250a, #a2250add) !important;
        filter: brightness(1.1) !important;
    }
    #start-processing-btn.danger:active {
        background: linear-gradient(to bottom, #a2250a, #a2250add) !important;
        filter: brightness(0.9) !important;
    }
    
    .raised-btn.secondary { 
        background: linear-gradient(to bottom, #9e9e9e, #757575);
        color: white;
        text-shadow: 0 1px 0 rgba(0, 0, 0, 0.2);
    }
    .raised-btn.secondary:hover {
        background: linear-gradient(to bottom, #757575, #616161);
    }
    .raised-btn.secondary:active {
        background: linear-gradient(to bottom, #616161, #424242);
    }

    .raised-btn.success {
        background: linear-gradient(to bottom, #28a745, #218838);
        color: white;
        text-shadow: 0 1px 0 rgba(0, 0, 0, 0.2);
    }
    .raised-btn.success:hover {
        background: linear-gradient(to bottom, #218838, #1e7e34);
    }
    .raised-btn.success:active {
        background: linear-gradient(to bottom, #1e7e34, #1c7430);
    }

    .raised-btn.warning {
        background: linear-gradient(to bottom, #ffc107, #ffb300);
        color: #212529;
        text-shadow: 0 1px 0 rgba(255, 255, 255, 0.5);
    }
    .raised-btn.warning:hover {
        background: linear-gradient(to bottom, #ffb300, #ffa000);
    }
    .raised-btn.warning:active {
        background: linear-gradient(to bottom, #ffa000, #ff8f00);
    }
    
    .raised-btn:disabled {
        background: linear-gradient(to bottom, #e9ecef, #dee2e6);
        color: #6c757d;
        cursor: not-allowed;
        transform: none;
        box-shadow: 
            0 2px 4px rgba(0, 0, 0, 0.05),
            inset 0 1px 0 rgba(255, 255, 255, 0.2);
        text-shadow: none;
    }
    
    /* Custom radio button styles */
    .custom-radio {
        display: flex;
        align-items: center;
        gap: 8px;
        padding: 8px 12px;
        margin: 2px 0;
        border-radius: 4px;
        cursor: pointer;
        transition: background-color 0.2s ease;
    }
    
    .custom-radio:hover {
        background-color: #f8f9fa;
    }
    
    .custom-radio input[type="radio"] {
        width: 16px;
        height: 16px;
        cursor: pointer;
    }
    
    /* Window controls */
    .window-controls {
        display: flex;
        gap: 4px;
        z-index: 1000;
        position: relative;
    }
    
    .window-control-btn {
        width: 28px;
        height: 28px;
        border: none;
        border-radius: 50%;
        cursor: pointer;
        font-size: 14px;
        font-weight: bold;
        display: flex;
        align-items: center;
        justify-content: center;
        position: relative;
        z-index: 1001;
        pointer-events: auto;
    }
    
    .window-control-btn:active {
        transform: scale(0.95);
    }
    
    /* Prevent text selection on window controls */
    .window-control-btn {
        user-select: none;
        -webkit-user-select: none;
        -moz-user-select: none;
        -ms-user-select: none;
    }
    
    /* Pulse animation for ONNX provider status */
    @keyframes pulse {
        0% {
            opacity: 1;
            transform: scale(1);
        }
        50% {
            opacity: 0.7;
            transform: scale(1.1);
        }
        100% {
            opacity: 1;
            transform: scale(1);
        }
    }

    /* Themed custom dropdown (to avoid native white/blue styling) */
    .themed-select { position: relative; min-width: 240px; }
    .themed-select-display {
        background: var(--panel-bg, #fdfdfd);
        color: var(--text-color, #333);
        border: 1px solid var(--panel-border-color, var(--outline-color, rgba(0,0,0,0.12)));
        border-radius: 3px;
        padding: 6px 28px 6px 8px;
        font-size: 14px;
        cursor: pointer;
        position: relative;
        user-select: none;
    }
    .themed-select-display::after {
        content: "\25BE"; /* U+25BE: BLACK DOWN-POINTING SMALL TRIANGLE */
        position: absolute;
        right: 8px;
        top: 50%;
        transform: translateY(-50%);
        color: var(--text-color, #333);
        opacity: 0.8;
        pointer-events: none;
    }
    .themed-select-list {
        position: absolute;
        top: calc(100% + 4px);
        left: 0;
        right: 0;
        background: var(--panel-bg, #fdfdfd);
        color: var(--text-color, #333);
        border: 1px solid var(--panel-border-color, var(--outline-color, rgba(0,0,0,0.12)));
        border-radius: 4px;
        max-height: 240px;
        overflow-y: auto;
        z-index: 1000;
        box-shadow: 0 2px 6px rgba(15,23,42,0.12);
        scrollbar-color: var(--app-bg, #f5f5f5) var(--panel-bg, #fdfdfd);
    }
    .themed-select-list::-webkit-scrollbar { width: 10px; }
    .themed-select-list::-webkit-scrollbar-track {
        background: var(--panel-bg, #fdfdfd);
        border-left: 1px solid var(--outline-color, rgba(0,0,0,0.12));
    }
    .themed-select-list::-webkit-scrollbar-thumb {
        background: var(--app-bg, #f5f5f5);
        border-radius: 8px;
        border: 2px solid var(--panel-bg, #fdfdfd);
    }
    .themed-select-list::-webkit-scrollbar-thumb:hover { background: var(--app-bg, #e0e0e0); }
    .themed-select-option {
        padding: 6px 8px;
        cursor: pointer;
        white-space: nowrap;
        text-overflow: ellipsis;
        overflow: hidden;
    }
    .themed-select-option:hover, .themed-select-option[aria-selected="true"] {
        background: var(--info-bg, #ffffe1);
    }
"#;
