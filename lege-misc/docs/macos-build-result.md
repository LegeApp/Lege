 The hardware and macOS cross-build paths now pass.

  ### Test results

  - Paddle OCR used the real NVIDIA RTX 4060 through Vulkan.
      - 1200 px grayscale page: 22 lines recovered.
      - First inference: 2.05 s.
      - Repeated cached inference: 1.74 s.
      - The repeat returned the same line count. The opt-in regression/benchmark is in lege-ocr/src/engine_paddle.rs:263.

  - lege-gpu release tests: 36/36 passed on the physical GPU under the normal parallel runner.
  - Full Intel macOS cross-link succeeded:
      - Paddle-enabled lege: Mach-O x86_64.
      - Sheet Music GUI: Mach-O x86_64.
      - No Tesseract or Linux libraries were linked; dependencies are Apple system frameworks including Metal.

  - PDFium, CLI, and GUI all have verified ADHOC | RUNTIME signatures.
  - ZIP integrity passed.

  The resulting Intel package is target/macos/Lege-Sheet-Music-Edition-1.4.63-x86_64.zip.

  ### macOS packaging changes

  scripts/build-macos-app.sh:83 now:

  - Builds the CLI with Paddle OCR and no Tesseract dependency.
  - Rejects mismatched PDFium architectures.
  - Places PDFium in Contents/Frameworks and Sauvola in Contents/Resources.
  - Supports inexpensive repackaging through SKIP_BUILD=1.
  - Signs nested Mach-O files before the app.
  - Enables hardened runtime for native codesign and Linux rcodesign.
  - Signs and verifies the DMG when run on macOS.
  - Uses tar.gz when a cross-build container lacks zip.
  - Avoids declaring a missing icon and fixes the iconutil iconset layout.
  - Has clean Bash syntax and no whitespace errors.

  rcodesign is now installed at ~/.cargo/bin/rcodesign.

  ### Without an Apple developer account

  You can compile, bundle, ad-hoc sign, enable hardened runtime, verify integrity, and distribute an archive. You cannot obtain a trusted Developer ID certificate or notarize the application. Apple issues Developer ID certificates only to Apple
  Developer Program members, and notarization requires Developer ID signing, hardened runtime, and a secure timestamp. Developer ID requirements (https://developer.apple.com/help/glossary/developer-id-certificate/), Apple notarization requirements
  (https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution).

  Consequently, Gatekeeper will not trust this ad-hoc signature automatically. On the receiving Mac:

  unzip Lege-Sheet-Music-Edition-1.4.63-x86_64.zip

  codesign --verify --deep --strict --verbose=2 \
    "Lege Sheet Music Edition.app"

  codesign -dv --verbose=4 \
    "Lege Sheet Music Edition.app"

  open "Lege Sheet Music Edition.app"

  If macOS blocks it, first attempt the launch, then open System Settings → Privacy & Security → Open Anyway. That is Apple’s current supported procedure. Apple first-launch instructions
  (https://support.apple.com/guide/mac-help/open-a-mac-app-from-an-unknown-developer-mh40616/26/mac/26). On macOS Sequoia, the old Control-click override should not be relied upon. Apple’s Sequoia Gatekeeper notice
  (https://developer.apple.com/news/?id=saqachfa).

  spctl --assess may still report rejection for the ad-hoc build; that is expected and does not mean its integrity signature is broken.

  ### Apple Silicon

  The bundled PDFium is x86_64-only. The script deliberately refuses to put it in an ARM bundle. For Apple Silicon, obtain a matching mac-arm64 libpdfium.dylib, then build:

  TARGET=aarch64-apple-darwin \
  PDFIUM_DYLIB=/path/to/arm64/libpdfium.dylib \
  scripts/build-macos-app.sh

  I recommend separate x86_64 and arm64 artifacts initially. A universal build requires combining both CLI binaries, both GUI binaries, and both PDFium builds with lipo.

  The only remaining validation that cannot be done here is native launch testing on a Mac: GUI startup, PDFium opening a real PDF, Paddle OCR through Metal, and the actual Gatekeeper approval flow.
