# DjVu File Format Specification

This document outlines the DjVu file format specification based on the official documentation.

## File Structure

### Header
- The first four bytes of a DjVu file must be: `0x41 0x54 0x26 0x54` ("AT&T" in ASCII)
- This preamble is not part of the EA IFF 85 format but is required to identify DjVu files

### IFF Wrapper

An IFF file consists of chunks with the following structure:

| Offset | Size  | Field    | Description                                  |
|--------|-------|----------|----------------------------------------------|
| 0      | 4     | Chunk ID | Identifies the chunk type                   |
| 4      | 4     | Length   | Length of the data (big-endian)              |
| 8      | N     | Data     | The chunk data                               |
| 8+N    | 0 or 1| Padding  | Optional padding byte to align to even boundary |

### Chunk Types

| Chunk ID  | Description |
|-----------|-------------|
| FORM      | Container chunk. First 4 data bytes are a secondary ID |
| FORM:DJVM | Multipage DjVu document |
| FORM:DJVU | Single DjVu page |
| FORM:DJVI | Shared DjVu file (included via INCL) |
| FORM:THUM | Embedded thumbnails |
| DIRM      | Page name information for multi-page documents |
| NAVM      | Bookmark information |
| ANTa, ANTz | Annotations (view settings, hyperlinks, etc.) |
| TXTa, TXTz | Unicode text and layout information |
| Djbz      | Shared shape table |
| Sjbz      | BZZ compressed JB2 bitonal data (mask) |
| FG44      | IW44 data for foreground |
| BG44      | IW44 data for background |
| TH44      | IW44 data for thumbnails |
| WMRM      | JB2 data for watermark removal |
| FGbz      | Color JB2 data (colors for Sjbz chunk) |
| INFO      | Page information |
| INCL      | Reference to included FORM:DJVI chunk |
| BGjp      | JPEG encoded background |
| FGjp      | JPEG encoded foreground |
| Smmr      | G4 encoded mask |

## Document Structure Examples

### Single Page Document
```
0000000: 41 54 26 54  AT&T magic
0000004: 46 4f 52 4d  FORM
0000008: 00 00 68 a6   (0xA668 = 26790, length of FORM)
000000b: 44 4a 56 55  DJVU (FORM:DJVU)
```

### Multi-page Document
```
FORM:DJVM [126475]
├── DIRM [59] Document directory (bundled, 3 files 2 pages)
├── FORM:DJVI [3493] {dict0002.iff}
├── FORM:DJVU [115016] {p0001.djvu}
└── FORM:DJVU [7869] {p0002.djvu}
```

### Page Structure
```
FORM:DJVU [26790]
├── INFO [10] DjVu 2202x967, v26, 300 dpi, gamma=2.2
├── Sjbz [13133] JB2 bilevel data
├── FG44 [185] IW4 data #1, 76 slices, v1.2 (color), 184x81
├── BG44 [935] IW4 data #1, 74 slices, v1.2 (color), 734x323
├── BG44 [1672] IW4 data #2, 10 slices
├── BG44 [815] IW4 data #3, 4 slices
└── BG44 [9976] IW4 data #4, 9 slices
```

## Implementation Notes

1. All chunks must begin on an even byte boundary
2. If necessary, a padding byte (0x00) must be inserted before a chunk to maintain alignment
3. The IFF format allows nesting, but DjVu only uses one level of nesting (FORM chunks containing other chunks)
4. The FORM:DJVU chunk must have an INFO chunk as its first child
5. The order of BG44 chunks is significant when multiple are present
6. Multi-page documents use FORM:DJVM as the root container
