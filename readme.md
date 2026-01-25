Latest version - 1.31 - December 2025

January 15 2026 - Full source code uploaded.

Need to know - 

Ort local folder is needed because it is forked to allow WebGPU to work in the Linux version.

WinRS-71 local folder is needed because it is forked to improve OCR support in the Windows version.

Legencode is the self enclosed encoding module. OpenJP2 is not used in the program anymore but is still included.

Now that the program is open source, the layout detection model could be switched to YOLO but the performance benefits are not definite. Also the DJVU module could be improved to be a direct FFI to a DJVU DLL instead of using the executables. The only reason these weren't done is because both YOLO and DJVUlibre are GPL. And now Lege is GPL too.

OS specific files and instructions will be added later. MacOS needs a different cargo.toml and engine.rs along with its different ORT library to function.

Otherwise it's just cargo build --release and you'll get two executables.

Features - 

 Automatic per-page rendering, binarization, re-encoding, and re-concatenation of any raster book scan.

- Fast precise per-page layout detection with hardware acceleration via DirectML to preserve image areas from binarization, so that they are either dithered or encoded in original color.

- Adaptive binarization, a fixed binary threshold, and a heavier neural GPU accelerated Sauvola model are the 3 binarization options.

- per-page HOCR layer is an option. If OCR is in the document already, it is detected and transferred to the output.

- JBIG2 with custom halftone dithering, CCITT4, and JPEG for images as the formats for PDF.

- Optional automatic margin centering or cropping. Deskewing and de-rotating available with an ONNX model.

- DJVU output supported, which is a great format for E-ink readers if you use opensource KoReader as the software for your reader.
