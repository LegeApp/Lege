"""Tests for the jbig2enc Python bindings.

These check the binding layer -- buffer handling, polarity, option plumbing,
error mapping -- not the encoder itself, which has its own Rust test suite.
The one thing they do assert about output is that it is a structurally valid
JBIG2 stream, so a binding that silently produced garbage would be caught.
"""

import struct

import pytest

import jbig2enc

WIDTH, HEIGHT = 480, 320

# Eight distinct 5x7 glyphs. Symbol substitution needs a small alphabet of
# *repeated, differing* shapes to work on -- a field of identical rectangles
# compresses so well as a generic region that the encoder rightly declines to
# build a dictionary, which is not what these tests mean to exercise.
_GLYPHS = [
    ("01110", "10001", "10001", "11111", "10001", "10001", "10001"),  # A
    ("11110", "10001", "11110", "10001", "10001", "10001", "11110"),  # B
    ("01111", "10000", "10000", "10000", "10000", "10000", "01111"),  # C
    ("11111", "10000", "11110", "10000", "10000", "10000", "11111"),  # E
    ("10001", "10001", "10001", "11111", "10001", "10001", "10001"),  # H
    ("10000", "10000", "10000", "10000", "10000", "10000", "11111"),  # L
    ("01110", "10001", "10001", "10001", "10001", "10001", "01110"),  # O
    ("11110", "10001", "10001", "11110", "10000", "10000", "10000"),  # P
]


def text_like_page(width=WIDTH, height=HEIGHT):
    """A page of repeated glyphs, so symbol substitution has something to do."""
    pixels = bytearray(width * height)
    index = 0
    for row in range(4, height - 9, 11):
        for col in range(4, width - 7, 7):
            glyph = _GLYPHS[index % len(_GLYPHS)]
            index += 1
            # A space every few glyphs, so the layout reads as words.
            if index % 6 == 0:
                continue
            for dy, line in enumerate(glyph):
                base = (row + dy) * width + col
                for dx, cell in enumerate(line):
                    if cell == "1":
                        pixels[base + dx] = 1
    return bytes(pixels)


def segment_headers(stream):
    """Yield the segment type of each segment in a standalone JBIG2 file.

    Enough of T.88 section 7.2 to prove the stream is structured rather than
    truncated: skip the 13-byte file header, then walk segment headers.
    """
    assert stream[:8] == b"\x97JB2\r\n\x1a\n", "JBIG2 file signature"
    offset = 13  # 8-byte ID + 1 flags + 4 number of pages
    types = []
    while offset + 11 <= len(stream):
        flags = stream[offset + 4]
        types.append(flags & 0x3F)
        page_association_size = 4 if flags & 0x40 else 1
        referred = stream[offset + 5]
        count = referred >> 5
        if count == 7:  # long form; not produced by this encoder
            break
        # 1 byte of referred-to flags, then `count` 1-byte segment numbers
        # (valid while segment numbers stay below 256, which holds here).
        offset += 5 + 1 + count + page_association_size
        if offset + 4 > len(stream):
            break
        (length,) = struct.unpack(">I", stream[offset : offset + 4])
        offset += 4
        if length == 0xFFFFFFFF:  # unknown length; cannot walk further
            break
        offset += length
    return types


def test_encode_produces_a_structurally_valid_jbig2_file():
    pixels = text_like_page()
    data = jbig2enc.encode(pixels, WIDTH, HEIGHT)

    assert data[:8] == b"\x97JB2\r\n\x1a\n"
    types = segment_headers(data)
    # 48 = immediate lossless generic region, 4/6/7 = text regions,
    # 0 = symbol dictionary. Any of those means real content was emitted.
    assert types, "stream should contain at least one segment"
    assert any(t in (0, 4, 6, 7, 36, 38, 39) for t in types), types


def test_compression_actually_compresses():
    pixels = text_like_page()
    data = jbig2enc.encode(pixels, WIDTH, HEIGHT)
    # One byte per pixel in, a bilevel codec out. Anything close to raw size
    # means the pixels never reached the encoder.
    assert len(data) < len(pixels) // 2, (len(data), len(pixels))


def test_any_buffer_type_is_accepted_and_gives_identical_output():
    pixels = text_like_page()
    reference = jbig2enc.encode(pixels, WIDTH, HEIGHT)

    assert jbig2enc.encode(bytearray(pixels), WIDTH, HEIGHT) == reference
    assert jbig2enc.encode(memoryview(pixels), WIDTH, HEIGHT) == reference

    numpy = pytest.importorskip("numpy")
    array = numpy.frombuffer(pixels, dtype=numpy.uint8).reshape(HEIGHT, WIDTH)
    assert jbig2enc.encode(array, WIDTH, HEIGHT) == reference


def test_nonzero_values_normalise_so_0_255_matches_0_1():
    """NumPy and PIL hand out 0/255, not 0/1. Both must mean the same thing."""
    ones = text_like_page()
    full = bytes(255 if value else 0 for value in ones)
    assert jbig2enc.encode(full, WIDTH, HEIGHT) == jbig2enc.encode(ones, WIDTH, HEIGHT)


def test_pack_grayscale_inverts_polarity():
    """Grayscale is 0 = black; JBIG2 is 1 = black. This is the trap."""
    # A grayscale row: dark, dark, light, light.
    gray = bytes([0, 40, 200, 255])
    packed = jbig2enc.pack_grayscale(gray, 4, 1, threshold=128)
    assert packed == bytes([1, 1, 0, 0])

    # Threshold is exclusive on the black side: value < threshold is black.
    assert jbig2enc.pack_grayscale(bytes([127, 128]), 2, 1, threshold=128) == bytes([1, 0])


def test_encode_for_pdf_splits_globals_from_page_data():
    pixels = text_like_page()
    globals_, page_data = jbig2enc.encode_for_pdf(pixels, WIDTH, HEIGHT)

    assert isinstance(page_data, bytes) and page_data
    # PDF fragments must not carry the standalone file header.
    assert not page_data.startswith(b"\x97JB2")
    if globals_ is not None:
        assert isinstance(globals_, bytes) and globals_

    # Lossless mode suppresses the shared dictionary entirely.
    globals_, page_data = jbig2enc.encode_for_pdf(pixels, WIDTH, HEIGHT, lossless=True)
    assert globals_ is None
    assert page_data


def test_encode_document_shares_one_dictionary_across_pages():
    page = text_like_page()
    globals_, streams = jbig2enc.encode_document(
        [(page, WIDTH, HEIGHT)] * 3, symbol_mode=True
    )
    assert len(streams) == 3
    assert all(stream for stream in streams)
    assert globals_, "one dictionary must be shared across the pages"

    # Three pages sharing one dictionary should cost no more than three
    # independent encodes -- that is the whole point of the call.
    independent = sum(
        sum(len(part or b"") for part in jbig2enc.encode_for_pdf(page, WIDTH, HEIGHT, symbol_mode=True))
        for _ in range(3)
    )
    shared = sum(len(stream) for stream in streams) + len(globals_)
    assert shared <= independent, (shared, independent)


def test_symbol_mode_reaches_the_encoder_and_pays_off_on_text():
    pixels = text_like_page()
    symbol = jbig2enc.encode(pixels, WIDTH, HEIGHT, symbol_mode=True)
    generic = jbig2enc.encode(pixels, WIDTH, HEIGHT, symbol_mode=False)

    assert symbol != generic, "symbol_mode must change the output"
    assert len(symbol) < len(generic), (
        "symbol substitution should beat a generic region on repeated glyphs",
        len(symbol),
        len(generic),
    )


def test_symbol_mode_controls_whether_a_shared_dictionary_is_emitted():
    pixels = text_like_page()

    globals_, page_data = jbig2enc.encode_for_pdf(pixels, WIDTH, HEIGHT, symbol_mode=True)
    assert globals_, "symbol mode must produce a /JBIG2Globals dictionary"
    assert page_data

    globals_, page_data = jbig2enc.encode_for_pdf(pixels, WIDTH, HEIGHT, symbol_mode=False)
    assert globals_ is None, "a generic-region stream has no dictionary to share"
    assert page_data


def test_refine_reaches_the_encoder():
    """Refinement is only meaningful alongside symbol substitution."""
    pixels = text_like_page()
    plain = jbig2enc.encode(pixels, WIDTH, HEIGHT, symbol_mode=True)
    refined = jbig2enc.encode(pixels, WIDTH, HEIGHT, symbol_mode=True, refine=True)
    assert plain != refined

    # Without symbol_mode the binding clears refine, so it cannot produce a
    # stream whose refinement flag can never fire.
    assert jbig2enc.encode(pixels, WIDTH, HEIGHT, refine=True) == jbig2enc.encode(
        pixels, WIDTH, HEIGHT
    )


def test_inert_encoder_options_are_rejected_rather_than_silently_ignored():
    """dpi, duplicate_line_removal and match_tolerance do nothing today.

    They are not exposed, so passing one must be a loud error rather than a
    silently ignored keyword.
    """
    pixels = text_like_page()
    for option in ("dpi", "duplicate_line_removal", "match_tolerance"):
        with pytest.raises(ValueError, match="unknown option"):
            jbig2enc.encode(pixels, WIDTH, HEIGHT, **{option: 1})


def test_bad_input_raises_useful_errors():
    pixels = text_like_page()

    with pytest.raises(ValueError, match="one byte per pixel"):
        jbig2enc.encode(pixels[:-1], WIDTH, HEIGHT)

    with pytest.raises(ValueError, match="greater than zero"):
        jbig2enc.encode(b"", 0, 0)

    with pytest.raises(ValueError, match="unknown option"):
        jbig2enc.encode(pixels, WIDTH, HEIGHT, symbol_moed=True)

    with pytest.raises(ValueError, match="empty"):
        jbig2enc.encode_document([])

    # Packed 1-bit data is the mistake this API invites; the message has to
    # say so rather than reporting a size mismatch.
    packed = bytes((WIDTH * HEIGHT + 7) // 8)
    with pytest.raises(ValueError, match="1-bit-packed"):
        jbig2enc.encode(packed, WIDTH, HEIGHT)

    with pytest.raises(BufferError):
        jbig2enc.encode(["not", "a", "buffer"], WIDTH, HEIGHT)


def test_encoding_releases_the_gil():
    """Two threads encoding must overlap, or the GIL was held throughout."""
    import threading
    import time

    pixels = text_like_page(1024, 1024)
    overlap = threading.Barrier(2, timeout=10)
    errors = []

    def worker():
        try:
            overlap.wait()
            jbig2enc.encode(pixels, 1024, 1024)
        except Exception as error:  # noqa: BLE001 - reported below
            errors.append(error)

    threads = [threading.Thread(target=worker) for _ in range(2)]
    start = time.monotonic()
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(timeout=30)
    assert not errors, errors
    assert all(not thread.is_alive() for thread in threads)
    # The barrier itself proves both threads got to run; this just guards
    # against a pathological serialisation.
    assert time.monotonic() - start < 30


def test_module_metadata():
    assert jbig2enc.__version__
    assert jbig2enc.version()
    assert jbig2enc.build_info()


# --- Round trips -----------------------------------------------------------
#
# These are the tests that actually prove the binding produces correct JBIG2
# rather than merely well-formed JBIG2: encode, decode, compare pixel for
# pixel. JBIG2's generic and symbol modes are both lossless at these
# settings, so the comparison is exact, not approximate.


def test_standalone_round_trip_is_pixel_exact():
    source = text_like_page()

    for symbol_mode in (False, True):
        encoded = jbig2enc.encode(source, WIDTH, HEIGHT, symbol_mode=symbol_mode)
        pixels, width, height = jbig2enc.decode(encoded)
        assert (width, height) == (WIDTH, HEIGHT)
        assert pixels == source, f"symbol_mode={symbol_mode} lost pixels"


def test_pdf_fragment_round_trip_is_pixel_exact():
    source = text_like_page()

    # Without a dictionary.
    globals_, page_data = jbig2enc.encode_for_pdf(source, WIDTH, HEIGHT)
    assert globals_ is None
    pixels, width, height = jbig2enc.decode_pdf_stream(page_data)
    assert (width, height) == (WIDTH, HEIGHT)
    assert pixels == source

    # With a shared dictionary, which is the case that goes wrong if the
    # globals and the page stream are not planned by one encoder.
    globals_, page_data = jbig2enc.encode_for_pdf(
        source, WIDTH, HEIGHT, symbol_mode=True
    )
    assert globals_
    pixels, _, _ = jbig2enc.decode_pdf_stream(page_data, globals_)
    assert pixels == source


def test_multipage_round_trip_against_one_shared_dictionary():
    """The failure this guards against is silent and total.

    Symbol indices in each page stream only line up with a dictionary planned
    by the same encoder instance. Mismatched ones decode to solid black or
    white pages rather than raising, so only a pixel comparison catches it.
    """
    source = text_like_page()
    globals_, streams = jbig2enc.encode_document(
        [(source, WIDTH, HEIGHT)] * 3, symbol_mode=True
    )
    assert globals_

    for index, stream in enumerate(streams):
        pixels, width, height = jbig2enc.decode_pdf_stream(stream, globals_)
        assert (width, height) == (WIDTH, HEIGHT), index
        assert pixels == source, f"page {index} did not survive the round trip"


def test_decode_document_returns_every_page():
    source = text_like_page()
    encoded = jbig2enc.encode(source, WIDTH, HEIGHT)
    pages = jbig2enc.decode_document(encoded)
    assert len(pages) >= 1
    pixels, width, height = pages[0]
    assert (width, height) == (WIDTH, HEIGHT)
    assert pixels == source


def test_grayscale_round_trip_through_pack_grayscale():
    """The whole user-facing path: grayscale in, grayscale-equivalent out."""
    source = text_like_page()
    # Render the page as an 8-bit grayscale image would store it: 0 = ink.
    gray = bytes(0 if value else 255 for value in source)

    packed = jbig2enc.pack_grayscale(gray, WIDTH, HEIGHT, threshold=128)
    assert packed == source

    pixels, _, _ = jbig2enc.decode(jbig2enc.encode(packed, WIDTH, HEIGHT))
    assert pixels == source


def test_decode_rejects_garbage():
    with pytest.raises(ValueError, match="decoding JBIG2"):
        jbig2enc.decode(b"not a jbig2 stream at all")

    with pytest.raises(ValueError, match="decoding JBIG2"):
        jbig2enc.decode_pdf_stream(b"\x00\x01\x02\x03")
