# Icon Requirements for lege

To ensure your application icon displays correctly on all platforms, include the following icon files and sizes in this folder:

## Windows (.ico)
- **File:** `icon.ico`
- **Sizes to include (all in one .ico file):**
  - 256x256 px (required for Windows 10/11 high-DPI)
  - 128x128 px
  - 64x64 px
  - 48x48 px
  - 32x32 px
  - 16x16 px
- You can use tools like [IcoConvert](https://icoconvert.com/) or GIMP to generate a multi-size .ico file.

## Linux (.png)
- **File:** `icon.png`
- **Recommended size:** 256x256 px (used by most modern Linux desktop environments)
- Optionally, you may include other sizes for compatibility:
  - 128x128 px
  - 64x64 px
  - 48x48 px
  - 32x32 px
  - 16x16 px
- Only `icon.png` (256x256) is required for this project; others are optional.

---
- Place your icon files in this `assets` folder before building or running the application.
- The build system will automatically use these files for embedding or installation.
