"""Type stubs for the jbig2enc extension module."""

from collections.abc import Buffer, Sequence

__version__: str

def encode(
    pixels: Buffer,
    width: int,
    height: int,
    *,
    symbol_mode: bool = False,
    refine: bool = False,
    lossless: bool = False,
) -> bytes:
    """Encode one bilevel image as a standalone JBIG2 file.

    `pixels` is one byte per pixel, row-major; any non-zero value is black.
    """

def encode_for_pdf(
    pixels: Buffer,
    width: int,
    height: int,
    *,
    symbol_mode: bool = False,
    refine: bool = False,
    lossless: bool = False,
) -> tuple[bytes | None, bytes]:
    """Encode one image as a PDF fragment: `(globals, page_data)`."""

def encode_document(
    pages: Sequence[tuple[Buffer, int, int]],
    *,
    symbol_mode: bool = False,
    refine: bool = False,
    lossless: bool = False,
) -> tuple[bytes | None, list[bytes]]:
    """Encode several pages against one shared symbol dictionary."""

def decode(data: Buffer) -> tuple[bytes, int, int]:
    """Decode a standalone JBIG2 file's first page.

    Returns `(pixels, width, height)`, one byte per pixel, 1 = black --
    the same layout the encode functions accept.
    """

def decode_document(data: Buffer) -> list[tuple[bytes, int, int]]:
    """Decode every page of a standalone JBIG2 file, in stream order."""

def decode_pdf_stream(
    page_data: Buffer, globals: Buffer | None = None
) -> tuple[bytes, int, int]:
    """Decode a PDF-embedded page stream against its optional globals."""

def pack_grayscale(
    pixels: Buffer, width: int, height: int, threshold: int = 128
) -> bytes:
    """Threshold 8-bit grayscale into encoder input, inverting polarity."""

def version() -> str: ...
def build_info() -> str: ...
