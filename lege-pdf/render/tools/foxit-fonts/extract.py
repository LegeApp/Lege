#!/usr/bin/env python3
"""Convert PDFium's bundled Foxit fonts (bare CFF, emitted as C arrays) into
standalone OpenType/CFF files.

PDFium hands bare CFF to FreeType, which accepts it; Skrifa (our outline
engine) only reads SFNT-wrapped fonts, so we wrap each CFF in a minimal OTF
exactly once, here, and commit the result. Metrics come from the CFF itself
(charstring widths) so the wrapper adds no information of its own.

Run:  python3 tools/foxit-fonts/extract.py <pdfium-src> <out-dir>
"""
import re, sys, io
from pathlib import Path
from fontTools.cffLib import CFFFontSet
from fontTools.ttLib import TTFont, newTable
from fontTools.misc.timeTools import timestampNow

def parse_c_array(path: Path):
    text = path.read_text()
    m = re.search(r"std::array<uint8_t,\s*(\d+)>\s+(\w+)\s*=\s*\{\{(.*?)\}\};", text, re.S)
    if not m:
        raise SystemExit(f"no array in {path}")
    size, name, body = int(m.group(1)), m.group(2), m.group(3)
    data = bytes(int(t, 0) for t in re.findall(r"0x[0-9a-fA-F]+|\b\d+\b", body))
    if len(data) != size:
        raise SystemExit(f"{name}: parsed {len(data)} != declared {size}")
    return name, data

def wrap_cff(cff_bytes: bytes, out: Path):
    cff = CFFFontSet()
    cff.decompile(io.BytesIO(cff_bytes), None)
    top = cff[cff.fontNames[0]]
    charstrings = top.CharStrings
    order = top.getGlyphOrder()

    fb = TTFont()
    fb.setGlyphOrder(list(order))
    fb["CFF "] = newTable("CFF ")
    fb["CFF "].cff = cff

    # Advance widths straight from the charstrings, so hmtx agrees with the
    # outlines. (The renderer positions with the PDF's /Widths; hmtx only has
    # to be present and self-consistent.)
    from fontTools.pens.basePen import NullPen
    hmtx = {}
    for g in order:
        cs = charstrings[g]
        cs.draw(NullPen())  # populates .width from the charstring
        hmtx[g] = (int(round(cs.width)), 0)
    fb["hmtx"] = newTable("hmtx"); fb["hmtx"].metrics = hmtx

    upem = 1000
    fm = top.FontMatrix
    if fm and fm[0]:
        upem = int(round(1.0 / fm[0]))

    head = fb["head"] = newTable("head")
    head.tableVersion = 1.0; head.fontRevision = 1.0
    head.checkSumAdjustment = 0; head.magicNumber = 0x5F0F3CF5
    head.flags = 3; head.unitsPerEm = upem
    head.created = head.modified = timestampNow()
    head.macStyle = 0; head.lowestRecPPEM = 3; head.fontDirectionHint = 2
    head.indexToLocFormat = 0; head.glyphDataFormat = 0
    bounds = top.FontBBox
    head.xMin, head.yMin, head.xMax, head.yMax = [int(v) for v in bounds]

    hhea = fb["hhea"] = newTable("hhea")
    hhea.tableVersion = 0x00010000
    hhea.ascent = int(bounds[3]); hhea.descent = int(bounds[1]); hhea.lineGap = 0
    hhea.advanceWidthMax = max((w for w, _ in hmtx.values()), default=0)
    hhea.minLeftSideBearing = 0; hhea.minRightSideBearing = 0
    hhea.xMaxExtent = int(bounds[2]); hhea.caretSlopeRise = 1
    hhea.caretSlopeRun = 0; hhea.caretOffset = 0
    hhea.reserved0 = hhea.reserved1 = hhea.reserved2 = hhea.reserved3 = 0
    hhea.metricDataFormat = 0; hhea.numberOfHMetrics = len(order)

    maxp = fb["maxp"] = newTable("maxp")
    maxp.tableVersion = 0x00005000  # 0.5: CFF outlines
    maxp.numGlyphs = len(order)

    # Unicode cmap synthesized from the CFF charset via the AGL: the renderer
    # resolves simple fonts by glyph *name*, but a cmap keeps the symbolic and
    # fallback paths working.
    from fontTools.agl import AGL2UV
    cmap_table = {}
    for gid, g in enumerate(order):
        uv = AGL2UV.get(g)
        if uv is None and re.fullmatch(r"uni[0-9A-Fa-f]{4}", g or ""):
            uv = int(g[3:], 16)
        if uv is not None and uv not in cmap_table:
            cmap_table[uv] = g
    cmap = fb["cmap"] = newTable("cmap")
    cmap.tableVersion = 0
    from fontTools.ttLib.tables._c_m_a_p import CmapSubtable
    st = CmapSubtable.newSubtable(4)
    st.platformID, st.platEncID, st.language = 3, 1, 0
    st.cmap = cmap_table
    cmap.tables = [st]

    name = fb["name"] = newTable("name")
    name.names = []
    family = top.FullName if hasattr(top, "FullName") else cff.fontNames[0]
    for nid, val in ((1, family), (2, "Regular"), (4, family), (6, cff.fontNames[0])):
        name.setName(str(val), nid, 3, 1, 0x409)

    os2 = fb["OS/2"] = newTable("OS/2")
    os2.version = 4
    os2.xAvgCharWidth = 0
    os2.usWeightClass = 700 if "Bold" in cff.fontNames[0] else 400
    os2.usWidthClass = 5; os2.fsType = 0
    for f in ("ySubscriptXSize ySubscriptYSize ySubscriptXOffset ySubscriptYOffset "
              "ySuperscriptXSize ySuperscriptYSize ySuperscriptXOffset ySuperscriptYOffset "
              "yStrikeoutSize yStrikeoutPosition").split():
        setattr(os2, f, 0)
    os2.sFamilyClass = 0
    from fontTools.ttLib.tables.O_S_2f_2 import Panose
    os2.panose = Panose()
    os2.ulUnicodeRange1 = os2.ulUnicodeRange2 = os2.ulUnicodeRange3 = os2.ulUnicodeRange4 = 0
    os2.achVendID = "NONE"; os2.fsSelection = 0x40
    os2.usFirstCharIndex = min(cmap_table) if cmap_table else 0
    os2.usLastCharIndex = max(cmap_table) if cmap_table else 0
    os2.sTypoAscender = int(bounds[3]); os2.sTypoDescender = int(bounds[1]); os2.sTypoLineGap = 0
    os2.usWinAscent = int(bounds[3]); os2.usWinDescent = abs(int(bounds[1]))
    os2.ulCodePageRange1 = os2.ulCodePageRange2 = 0
    os2.sxHeight = 0; os2.sCapHeight = 0
    os2.usDefaultChar = 0; os2.usBreakChar = 32; os2.usMaxContext = 0

    post = fb["post"] = newTable("post")
    # Format 2.0 keeps the glyph names: substituted simple fonts resolve
    # /Differences by name, and the symbolic faces (Symbol, ZapfDingbats)
    # have no useful Unicode cmap.
    post.formatType = 2.0
    post.extraNames = []
    post.mapping = {}
    post.glyphOrder = list(order)
    post.italicAngle = float(getattr(top, "ItalicAngle", 0))
    post.underlinePosition = -100; post.underlineThickness = 50
    post.isFixedPitch = int(bool(getattr(top, "isFixedPitch", False)))
    post.minMemType42 = post.maxMemType42 = post.minMemType1 = post.maxMemType1 = 0

    fb.sfntVersion = "OTTO"
    fb.save(str(out))
    return len(order), upem

def main():
    src = Path(sys.argv[1]) / "core/fxge/fontdata/chromefontdata"
    out = Path(sys.argv[2]); out.mkdir(parents=True, exist_ok=True)
    total = 0
    for cpp in sorted(src.glob("Foxit*.cpp")):
        stem = cpp.stem
        if stem.endswith("MM"):
            # The Multiple Master faces are PFB Type 1, not CFF: PDFium uses
            # them to interpolate a weight/width for *unknown* fonts. Type 1
            # parsing is Font Phase 5, so they are skipped and the fallback
            # picks the nearest of the standard 14 instead.
            print(f"{stem:24} skipped (Type 1 Multiple Master; see DEFERRED.md)")
            continue
        name, data = parse_c_array(cpp)
        dest = out / f"{stem}.otf"
        n, upem = wrap_cff(data, dest)
        total += dest.stat().st_size
        print(f"{stem:24} {len(data):>7} B CFF -> {dest.stat().st_size:>7} B OTF  glyphs={n} upem={upem}")
    print(f"total bundled: {total/1024:.0f} KiB")

main()
