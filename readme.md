Latest version - 1.31 - December 2025

January 15 2026 - Full source code uploaded.

Need to know - 
Ort local folder is needed because it is forked to allow WebGPU to work in the program.
WinRS-71 local folder is needed because it is forked to improve OCR support in the program.
Legencode is the self enclosed encoding module. OpenJP2 is not used in the program anymore but is still included.

Now that the program is open source, the layout detection model could be switched to YOLO but the performance benefits are not definite. Also the DJVU module could be improved to be a direct FFI to a DJVU DLL instead of using the executables.

OS specific files and instructions will be added later. Otherwise it's just cargo build --release and you'll get two executables.
