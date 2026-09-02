# Lege Android app

This directory is a self-contained Android Gradle project whose `app` module
hosts the Rust JNI library. It is currently an arm64-v8a Android 9+ app
(`minSdk 26`), appropriate for the intended E-Ink readers.

## Build on Linux

Install the Rust target and cargo-ndk once:

```bash
rustup target add aarch64-linux-android
cargo install cargo-ndk --version 4.1.2 --locked
```

Set the SDK location for the current shell (or create Android Studio's ignored
`local.properties` with `sdk.dir=/path/to/Android/Sdk`):

```bash
export ANDROID_HOME="$HOME/Android/Sdk"
cd lege-android
./gradlew :app:assembleDebug
```

`buildRustAndroid` runs automatically before the Android build. It obtains the
NDK version selected in `app/build.gradle.kts`, compiles the JNI cdylib with
the workspace's `android` Cargo profile, and places it under `jniLibs` for APK
packaging. The resulting APK is at `app/build/outputs/apk/debug/app-debug.apk`.

The shipped host is intentionally small but functional: choose a PDF through
the Storage Access Framework, choose an output location, and it invokes the
native pipeline off the UI thread while showing native progress. Inputs and
outputs are copied through the app cache because JNI accepts filesystem paths,
not `content://` URIs.

## Output formats

The OUTPUT section's `PDF` / `DJVU` control mirrors the desktop GUI's format
choice. It sets `LegeParams.outputFormat`, which `src/config.rs` turns into the
pipeline's `text_format`; `"djvu"` routes the job to `djvu_pipeline` and the
linked `djvu_encoder` crate. Both formats are compiled into
`liblege_android.so` and run in-process — DjVu needs no helper executable, so
there is nothing here that a JNI host cannot call.

Selecting DJVU hides the "Compatibility (CCITT + JPEG)" toggle, which only
applies to PDF encoders. The optional EPUB companion works with either format.
