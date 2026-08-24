#!/usr/bin/env python3
"""Transcribe PDFium's predefined CJK CMap tables into compact binary blobs +
a generated Rust registry, the way this repo ported the CMYK grid / CCITT
tables (a one-shot script; the *generated* artefact is what is committed).

Source of truth (BSD-licensed, Foxit/PDFium):
  core/fpdfapi/cmaps/<charset>/cmaps_<charset>.inc   — per-charset CMap registry
  core/fpdfapi/cmaps/<charset>/<Name>_<n>.cpp        — uint16 record arrays
  core/fpdfapi/font/cpdf_cmap.cpp  (kPredefinedCMaps) — coding scheme + leading
                                                        bytes per base name

Emits, under crates/pdf-font/src/:
  cmap_data/<charset>.bin   — concatenated u16 LE code->CID records (dedup'd)
  cmap_tables.rs            — the static registry (name, wmode, coding scheme,
                              leading bytes, record offset/len, usecmap link)

Also emits the four CID->Unicode (UCS-2) tables (Adobe-*-UCS2_*.inc,
registered by cpdf_fontglobals.cpp SetEmbeddedToUnicode): flat u16 LE arrays
indexed by CID, as cmap_data/<charset>_uni.bin + a `cid_to_unicode` accessor.
These power CJK fallback-face glyph mapping (fonts.md), not just extraction.

We deliberately DO NOT port:
  * DWordCIDMap tables — only reachable via 4-byte codes, which the predefined
    coding schemes (OneByte/TwoBytes/MixedTwoBytes) never assemble; PDFium's
    own MixedTwoBytes decoder cannot reach them either.
  * CMaps whose base name has no kPredefinedCMaps coding scheme (e.g. CNS-EUC):
    PDFium's GetPredefinedCMap returns null for them, so they are unusable.
"""
import os, re, struct, sys

REF = os.environ.get(
    "PDFIUM_CMAPS",
    "/mnt/Samsung980_1TB/Rust-projects/pdfium-port-plan/pdfium-reference-source/core/fpdfapi/cmaps",
)
OUT_DATA = os.path.join(os.path.dirname(__file__), "..", "src", "cmap_data")
OUT_RS = os.path.join(os.path.dirname(__file__), "..", "src", "cmap_tables.rs")

# Coding schemes.
ONE_BYTE, TWO_BYTES, MIXED_TWO = 0, 1, 2

# kPredefinedCMaps transcribed from core/fpdfapi/font/cpdf_cmap.cpp: base name
# (full name minus its trailing 2 chars) -> (scheme, [leading byte ranges]).
# Leading ranges only matter for MixedTwoBytes (which byte starts a 2-byte code).
PREDEF = {
    # GB1
    "GB-EUC":     (MIXED_TWO, [(0xa1, 0xfe)]),
    "GBpc-EUC":   (MIXED_TWO, [(0xa1, 0xfc)]),
    "GBK-EUC":    (MIXED_TWO, [(0x81, 0xfe)]),
    "GBKp-EUC":   (MIXED_TWO, [(0x81, 0xfe)]),
    "GBK2K-EUC":  (MIXED_TWO, [(0x81, 0xfe)]),
    "GBK2K":      (MIXED_TWO, [(0x81, 0xfe)]),
    "UniGB-UCS2":  (TWO_BYTES, []),
    "UniGB-UTF16": (TWO_BYTES, []),
    # CNS1
    "B5pc":       (MIXED_TWO, [(0xa1, 0xfc)]),
    "HKscs-B5":   (MIXED_TWO, [(0x88, 0xfe)]),
    "ETen-B5":    (MIXED_TWO, [(0xa1, 0xfe)]),
    "ETenms-B5":  (MIXED_TWO, [(0xa1, 0xfe)]),
    "UniCNS-UCS2":  (TWO_BYTES, []),
    "UniCNS-UTF16": (TWO_BYTES, []),
    # Japan1
    "83pv-RKSJ":  (MIXED_TWO, [(0x81, 0x9f), (0xe0, 0xfc)]),
    "90ms-RKSJ":  (MIXED_TWO, [(0x81, 0x9f), (0xe0, 0xfc)]),
    "90msp-RKSJ": (MIXED_TWO, [(0x81, 0x9f), (0xe0, 0xfc)]),
    "90pv-RKSJ":  (MIXED_TWO, [(0x81, 0x9f), (0xe0, 0xfc)]),
    "Add-RKSJ":   (MIXED_TWO, [(0x81, 0x9f), (0xe0, 0xfc)]),
    "EUC":        (MIXED_TWO, [(0x8e, 0x8e), (0xa1, 0xfe)]),
    "H":          (TWO_BYTES, []),
    "V":          (TWO_BYTES, []),
    "Ext-RKSJ":   (MIXED_TWO, [(0x81, 0x9f), (0xe0, 0xfc)]),
    "UniJIS-UCS2":    (TWO_BYTES, []),
    "UniJIS-UCS2-HW": (TWO_BYTES, []),
    "UniJIS-UTF16":   (TWO_BYTES, []),
    # Korea1
    "KSC-EUC":     (MIXED_TWO, [(0xa1, 0xfe)]),
    "KSCms-UHC":   (MIXED_TWO, [(0x81, 0xfe)]),
    "KSCms-UHC-HW":(MIXED_TWO, [(0x81, 0xfe)]),
    "KSCpc-EUC":   (MIXED_TWO, [(0xa1, 0xfd)]),
    "UniKS-UCS2":  (TWO_BYTES, []),
    "UniKS-UTF16": (TWO_BYTES, []),
}

CHARSETS = ["GB1", "CNS1", "Japan1", "Korea1"]

# CID->Unicode UCS-2 tables (one flat array per charset, index = CID; 0x0000 /
# 0xFFFD mark unmapped CIDs). File + symbol per cpdf_fontglobals.cpp.
CID2UNI = {
    "GB1":    ("Adobe-GB1-UCS2_5.inc",    "kGB1CID2Unicode_5"),
    "CNS1":   ("Adobe-CNS1-UCS2_5.inc",   "kCNS1CID2Unicode_5"),
    "Japan1": ("Adobe-Japan1-UCS2_4.inc", "kJapan1CID2Unicode_4"),
    "Korea1": ("Adobe-Korea1-UCS2_2.inc", "kKorea1CID2Unicode_2"),
}

def load_cid2unicode(charset):
    """The flat u16 list of a charset's Adobe-*-UCS2 array."""
    fn, symbol = CID2UNI[charset]
    txt = open(os.path.join(REF, charset, fn), encoding="latin1").read()
    m = re.search(r"\b" + re.escape(symbol) + r"\s*\[\s*\]\s*=\s*\{", txt)
    if not m:
        raise SystemExit(f"symbol {symbol} not found in {charset}/{fn}")
    body = txt[m.end():]
    body = body[:body.index("};")]
    return [int(x, 16) for x in re.findall(r"0x[0-9A-Fa-f]+", body)]

def base_name(full):
    return full[:-2] if len(full) > 2 else full

def load_symbol_data(charset, symbol):
    """Return the flat list of u16 ints for a `const uint16_t kSymbol[..]` array,
    searching every .cpp in the charset dir."""
    dirp = os.path.join(REF, charset)
    for fn in os.listdir(dirp):
        if not fn.endswith(".cpp"):
            continue
        txt = open(os.path.join(dirp, fn), encoding="latin1").read()
        m = re.search(r"\b" + re.escape(symbol) + r"\s*\[[^\]]*\]\s*=\s*\{", txt)
        if not m:
            continue
        body = txt[m.end():]
        end = body.index("};")
        body = body[:end]
        return [int(x, 16) for x in re.findall(r"0x[0-9A-Fa-f]+", body)]
    raise SystemExit(f"symbol {symbol} not found in {charset}")

# One registry tuple per .inc row:
#   {"Name", kSymbol, dword_or_nullptr, word_count, dword_count, Type, use_offset}
ROW = re.compile(
    r'\{\s*"([^"]+)"\s*,\s*(\w+)\s*,\s*(\w+|nullptr)\s*,\s*(\d+)\s*,\s*(\d+)\s*,'
    r'\s*CMap::Type::(kSingle|kRange)\s*,\s*(-?\d+)\s*\}',
    re.S)

def parse_registry(charset):
    inc = open(os.path.join(REF, charset, f"cmaps_{charset.lower()}.inc"),
               encoding="latin1").read()
    rows = []
    for m in ROW.finditer(inc):
        name, sym, _dword, wcount, _dcount, typ, use_off = m.groups()
        rows.append(dict(name=name, sym=sym, wcount=int(wcount),
                         kind=typ, use_off=int(use_off)))
    return rows

def main():
    os.makedirs(OUT_DATA, exist_ok=True)
    rs = []
    rs.append("// @generated by crates/pdf-font/tools/gen_cmaps.py — DO NOT EDIT.")
    rs.append("//")
    rs.append("// Predefined CJK CMap tables transcribed from PDFium (BSD-licensed,")
    rs.append("// Foxit Software): core/fpdfapi/cmaps/<charset>/ record arrays +")
    rs.append("// core/fpdfapi/font/cpdf_cmap.cpp coding schemes. Records are code->CID")
    rs.append("// singles/ranges as u16 LE in the per-charset `*.bin` blobs; this table")
    rs.append("// carries only the small per-CMap header. See gen_cmaps.py for the port.")
    rs.append("")
    rs.append("use super::cmap::RawCMap;")
    rs.append("")

    all_entries = []  # (name, charset_idx, wmode, scheme, lead bytes, kind, off, len_records, use_index)
    charset_consts = []
    for cs_idx, charset in enumerate(CHARSETS):
        rows = parse_registry(charset)
        # Keep only rows whose base name has a coding scheme.
        kept = []
        for r in rows:
            b = base_name(r["name"])
            if b in PREDEF:
                kept.append(r)
        # Map self index (within kept) for use_offset resolution. PDFium's
        # use_offset is relative to the *full* .inc array; recompute against the
        # kept subset by name. Since we only drop CNS-EUC (which nothing chains
        # to), index deltas are preserved for the kept rows — but resolve by
        # walking the ORIGINAL array to be safe.
        name_to_kept_idx = {r["name"]: i for i, r in enumerate(kept)}
        orig_idx = {r["name"]: i for i, r in enumerate(rows)}

        # Dedup symbol data into the blob.
        blob = bytearray()
        sym_off = {}   # symbol -> record offset (in records, not bytes)
        def intern(sym, kind):
            if sym in sym_off:
                return sym_off[sym]
            data = load_symbol_data(charset, sym)
            stride = 2 if kind == "kSingle" else 3
            assert len(data) % stride == 0, (sym, len(data), stride)
            off = len(blob) // 2
            for v in data:
                blob.extend(struct.pack("<H", v))
            sym_off[sym] = off
            return off

        rows_rs = []
        for r in kept:
            off = intern(r["sym"], r["kind"])
            b = base_name(r["name"])
            scheme, leads = PREDEF[b]
            l0 = leads[0] if len(leads) > 0 else (0, 0)
            l1 = leads[1] if len(leads) > 1 else (0, 0)
            wmode = 1 if r["name"].endswith("V") else 0
            # Resolve usecmap: PDFium `cmap + use_offset` in the full array.
            use_index = -1
            if r["use_off"] != 0:
                target_orig = orig_idx[r["name"]] + r["use_off"]
                target_name = rows[target_orig]["name"] if 0 <= target_orig < len(rows) else None
                if target_name in name_to_kept_idx:
                    use_index = name_to_kept_idx[target_name]
            kind_variant = "Single" if r["kind"] == "kSingle" else "Range"
            scheme_variant = {ONE_BYTE: "OneByte", TWO_BYTES: "TwoBytes",
                              MIXED_TWO: "MixedTwoBytes"}[scheme]
            rows_rs.append(
                '    RawCMap {{ name: "{name}", wmode: {wmode}, '
                'scheme: Scheme::{scheme}, lead: [({a0}, {b0}), ({a1}, {b1})], '
                'kind: Kind::{kind}, off: {off}, records: {rec}, use_index: {ui} }},'
                .format(name=r["name"], wmode=wmode, scheme=scheme_variant,
                        a0=l0[0], b0=l0[1], a1=l1[0], b1=l1[1],
                        kind=kind_variant, off=off, rec=r["wcount"], ui=use_index))

        binname = f"{charset.lower()}.bin"
        open(os.path.join(OUT_DATA, binname), "wb").write(bytes(blob))
        const = f"{charset.upper()}"
        rs.append(f'static {const}_BLOB: &[u8] = include_bytes!("cmap_data/{binname}");')
        rs.append(f"static {const}: &[RawCMap] = &[")
        rs.extend(rows_rs)
        rs.append("];")
        rs.append("")
        charset_consts.append((const, len(rows_rs), len(blob)))
        print(f"{charset}: {len(rows_rs)} cmaps, blob {len(blob)} bytes")

    # A flat registry the runtime searches by name: (records, blob) per charset.
    rs.append("/// All predefined CMap charsets: `(entries, blob)`.")
    rs.append("pub(crate) static CHARSETS: &[(&[RawCMap], &[u8])] = &[")
    for const, _, _ in charset_consts:
        rs.append(f"    ({const}, {const}_BLOB),")
    rs.append("];")
    rs.append("")

    # CID->Unicode blobs + accessor (Adobe-*-UCS2 tables).
    for charset in CHARSETS:
        vals = load_cid2unicode(charset)
        binname = f"{charset.lower()}_uni.bin"
        with open(os.path.join(OUT_DATA, binname), "wb") as f:
            f.write(b"".join(struct.pack("<H", v) for v in vals))
        rs.append(
            f'static {charset.upper()}_UNI: &[u8] = '
            f'include_bytes!("cmap_data/{binname}");')
        print(f"{charset}: cid->unicode {len(vals)} entries")
    rs.append("")
    rs.extend([
        "/// The raw CID→Unicode blob for a charset (u16 LE, index = CID), or",
        "/// `None` for non-CJK charsets. `len() / 2` is the charset's CID count.",
        "pub(crate) fn cid2uni_table(charset: crate::system::Charset) -> Option<&'static [u8]> {",
        "    use crate::system::Charset;",
        "    match charset {",
        "        Charset::ChineseSimplified => Some(GB1_UNI),",
        "        Charset::ChineseTraditional => Some(CNS1_UNI),",
        "        Charset::ShiftJis => Some(JAPAN1_UNI),",
        "        Charset::Hangul => Some(KOREA1_UNI),",
        "        _ => None,",
        "    }",
        "}",
        "",
        "/// CID → Unicode for the four Adobe CJK charsets, from PDFium's",
        "/// `Adobe-<charset>-UCS2` tables (cpdf_fontglobals.cpp",
        "/// `SetEmbeddedToUnicode`): flat u16 LE arrays indexed by CID.",
        "/// `None` for out-of-range CIDs and unmapped slots (`0x0000` and",
        "/// `0xFFFD` — the tables' fillers, including index 0 = `.notdef`).",
        "pub fn cid_to_unicode(charset: crate::system::Charset, cid: u32) -> Option<char> {",
        "    let table = cid2uni_table(charset)?;",
        "    let i = (cid as usize).checked_mul(2)?;",
        "    let pair = table.get(i..i + 2)?;",
        "    let u = u16::from_le_bytes([pair[0], pair[1]]);",
        "    if u == 0x0000 || u == 0xFFFD {",
        "        return None;",
        "    }",
        "    char::from_u32(u as u32)",
        "}",
        "",
    ])
    # Emit the Scheme/Kind enums referenced above (kept next to the data users).
    header = [
        "#[derive(Debug, Clone, Copy, PartialEq, Eq)]",
        "// `OneByte` mirrors PDFium's full set of coding schemes; none of the",
        "// predefined CMaps transcribed here happens to use it.",
        "#[allow(dead_code)]",
        "pub(crate) enum Scheme { OneByte, TwoBytes, MixedTwoBytes }",
        "",
        "#[derive(Debug, Clone, Copy, PartialEq, Eq)]",
        "pub(crate) enum Kind { Single, Range }",
        "",
    ]
    # Insert enums right after the `use` line.
    idx = rs.index("use super::cmap::RawCMap;") + 2
    rs[idx:idx] = header

    open(OUT_RS, "w", encoding="utf-8", newline="\n").write("\n".join(rs) + "\n")
    total = sum(b for _, _, b in charset_consts)
    print(f"total blob {total} bytes across {len(CHARSETS)} charsets")

if __name__ == "__main__":
    main()
