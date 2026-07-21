# Lege - Release Notes

Lege is a document/PDF processing application. This file preserves the
historical release notes. Downloadable builds remain on the Releases page:
https://github.com/LegeApp/Lege/releases

## Linux universal install (linux-universal)
_Published: 10/11/2025 04:40:17_

current version - 1.4.61 (july 9 2026) 

libtesseract required for ocr features. can be detected but not auto-installed. djvulibre no longer required.

## Windows local installer (Windows-local)
_Published: 10/10/2025 16:37:46_

7/14/2026 - 1.4.63 - smaller and faster. new layout model. GUI tweaks.  grayscale MRC. fixed layout mode cropping. performance tweaks. 

7/9/2026 - 1.4.61 - revamped gui theme system, improved software gui renderer and lege-gpu, fixed heavy model usage. fable assisted bugfixes. see commits.

7/6/2026 - 1.4.6 - SIMD optimization to 3 out of 5 encoders, GUI bugfixes, crucial bugfixes identified by Fable affecting outputs. binarization default k_factor changed to .05 also.

6/28/2026 - 1.4.53 - fixes margin cropping, various GUI fixes including EPUB addition, stability fixes and debug improvements.

## Linux debian installer (Linux-deb)
_Published: 10/09/2025 11:26:02_

June 26 2026 - 1.4.52 - minor updates

June 6 2026 - 1.4.5 - big update - Onnxruntime replaced by custom WGPU onnx inference; compiles to dx12, vulkan and metal so the program is now almost completely OS agnostic. Slightly faster overall. Added a higher quality OCR mode with EPUB output. GUI now calls CLI as subprocess, no code duplication. Freya GUI now custom software rendered - 10x less memory usage, 15mb smaller binary. lots of other minor bugfixes and stability improvements. Total program size cut in half, with better performance.

May 25 2026 - Bugfixes and GUI updates.

May 6 2026 - Hotfix; GPU binarization only for no-layout paths now and is otherwise improved. GUI text updates, processing bar updates. Not sure what 1.4.4 will have if released, this patch stays within 1.4.3. Late addition May 7 2026 - DJVU optimization.  

May 1 2026 - 
1.4.3 - biggest addition is jp2 in-memory encoding and decoding. Binarization on GPU now, Freya GUI, archive.org originated zip and jp2 support, fixed folder mode, fixed processing bar, more

March 19 2026
-1.4.1, see linux universal install notes. biggest addition is jbig2 symbol mode. various debug modes and flags added to CLI also. 

March 6 2026 

 -1.4.0, improvements to CLI, WGPU shaders for hardware accelerated resizing on Linux, concurrent pipeline improvements, jbig2 improvements, high quality setting added explicitly where before it was a hidden feature, dithered DJVU mode that only uses JB2, warning cleanups. 

Feb 21 2026

- 1.3.7, updated to fix Linux Tesseract issues mostly. Other small fixes.

Nov 6 2025

- See Windows notes. Windows and Linux versions should be effectively the same now.

Nov 1 2025-
- See Nov 1 Windows notes. Improvements across the board. However most inference speed related improvements are Windows specific.

Oct 24 2025-

- no-layout mode faster now. layout mode about the same
- refined the new GUI theme


Oct 21 2025 - 

- New GUI theme
- Many small improvements and fixes

