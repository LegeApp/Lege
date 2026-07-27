# Linux packaging inputs

The default layout, PaddleOCR, and heavy Sauvola models are embedded in the
executables. The `.deb` and AppImage builds no longer require an external model
staging directory. Keep this directory only for platform packaging notes and
optional runtime model overrides used during development.
