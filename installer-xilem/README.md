# Lege Installer (Xilem)

Windows-only Xilem-based installer with real backend (zstd payload extraction) that mirrors the general presentation of the existing egui installer:

- branded two-pane layout
- install path input and shortcut toggles
- progress bar + activity log
- license drawer panel
- real backend install flow:
  - embedded payload from project root (`Lege-win64.tar.zst`)
  - zstd decompress + tar unpack
  - recursive file copy to install path
  - install manifest + user data dir setup
  - desktop/start-menu shortcut creation via Windows Shell COM
  - uninstaller staging (`install_dir\\uninstall.exe`) when `uninstaller` binary is built

## Run

```bash
cargo run
```

## Next integration step

Drop your updated payload in project root as `Lege-win64.tar.zst` and run the installer.

## Uninstaller

Build the CLI uninstaller first so installer can stage it:

```bash
cd uninstaller
cargo build --release
```

Then run installer from project root (`cargo run`). If the uninstaller binary is found at
`uninstaller/target/release/lege-uninstaller.exe`, the installer copies it to `install_dir\\uninstall.exe`
and creates `Uninstall Lege.lnk` in the Start Menu folder.
