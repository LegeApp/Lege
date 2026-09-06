# jbig2enc

Python bindings for [`jbig2enc-rust`](https://github.com/liminalism/lege/tree/main/lege-codecs/jbig2enc-rust),
a pure-Rust JBIG2 encoder. JBIG2 is the bilevel image codec used by scanned
PDFs; on clean text it typically beats CCITT Group 4 by 2-4x.

Requires CPython 3.11 or newer.

## Install

```sh
pip install jbig2enc
```

Building from a checkout needs a Rust toolchain and `maturin`:

```sh
pip install maturin
maturin develop --release
```

## Use

Input is **one byte per pixel**, row-major, where any non-zero value is
black. That is not what an 8-bit grayscale image gives you — grayscale
stores 0 as black — so convert with `pack_grayscale`, which thresholds and
inverts in one step:

```python
import jbig2enc
from PIL import Image

page = Image.open("scan.png").convert("L")
pixels = jbig2enc.pack_grayscale(page.tobytes(), page.width, page.height, threshold=128)

with open("scan.jb2", "wb") as handle:
    handle.write(jbig2enc.encode(pixels, page.width, page.height))
```

Any buffer works — `bytes`, `bytearray`, `memoryview`, or a contiguous
`uint8` NumPy array.

### Embedding in a PDF

`encode_for_pdf` splits the shared symbol dictionary from the page data, as
PDF requires:

```python
globals_, page_data = jbig2enc.encode_for_pdf(pixels, width, height)
```

Store `globals_` as its own PDF object and reference it from the image
stream's `/DecodeParms << /JBIG2Globals ... >>`.

For a multi-page document, use `encode_document` rather than calling
`encode_for_pdf` per page. One shared dictionary has to be planned across
every page for the symbol indices to line up; independently encoded pages
stitched together are undecodable.

```python
globals_, streams = jbig2enc.encode_document([
    (page_one, width, height),
    (page_two, width, height),
])
```

## Decoding

The encoder ships with a decoder, so round trips are checkable and PDFs
containing JBIG2 images can be read back:

```python
pixels, width, height = jbig2enc.decode(open("scan.jb2", "rb").read())
Image.frombytes("L", (width, height), bytes(255 - p * 255 for p in pixels))
```

`decode` returns the first page; `decode_document` returns every page. For a
PDF-embedded image, pass the page stream and its globals:

```python
pixels, width, height = jbig2enc.decode_pdf_stream(page_data, globals_)
```

Output is one byte per pixel, 1 = black -- the same layout the encoders
accept, so `decode(encode(pixels, w, h)) == pixels` holds exactly. Both
modes are lossless at the exposed settings.

## Options

Every encode function takes keyword options:

| Option | Default | Effect |
|---|---|---|
| `symbol_mode` | `True` | Symbol substitution. The reason to choose JBIG2 over CCITT G4 on text, and the only way to get a shared `/JBIG2Globals` dictionary. **Lossy** -- see below. Set `False` for a bit-exact generic region. |
| `refine` | `False` | Refinement coding. Ignored unless `symbol_mode` is set, and currently makes output slightly *larger*. |
| `lossless` | `False` | Preset: no symbol dictionary, no refinement. Use when a viewer mishandles shared dictionaries. |

```python
data = jbig2enc.encode(pixels, width, height, symbol_mode=False)
```

### Symbol mode is lossy

This is the one thing to understand before using the defaults. Symbol
substitution replaces near-identical glyphs with a single shared bitmap, so
the decoded page is not guaranteed identical to what you encoded:

```python
>>> data = jbig2enc.encode(pixels, w, h)                    # symbol mode
>>> jbig2enc.decode(data)[0] == pixels
False                                                        # on a real scan
>>> data = jbig2enc.encode(pixels, w, h, symbol_mode=False)  # generic region
>>> jbig2enc.decode(data)[0] == pixels
True
```

On synthetic pages of exactly repeated glyphs the substitution is exact and
the win is large (2876 -> 1027 bytes here). On a real 956x1557 scan it
changed 175 of 185395 ink pixels and the file was *larger* than the generic
region (13211 vs 12941 bytes). Measure on your own material rather than
assuming symbol mode wins.

Use `symbol_mode=False` when exactness matters -- archival masters, line
art, halftones, or anything where a substituted character would be a
correctness bug rather than a compression artefact.

The encoder has many more configuration fields. Only the three above are
exposed, because they are the only ones measured to change the output —
`dpi`, `duplicate_line_removal` and `match_tolerance` are currently inert,
and a binding that accepted them would be lying.

Calls release the GIL, so encoding pages across a thread pool actually runs
in parallel.

## Licence

MIT OR Apache-2.0.
