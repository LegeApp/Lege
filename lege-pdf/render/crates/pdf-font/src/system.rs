//! System font providers (fonts.md Font Phase 7).
//!
//! The bundled standard 14 cover Latin text in the standard families. They
//! cannot cover a document that names *Cambria* or *DengXian*, and they have
//! no CJK at all — so a viewer that must render "whatever the document
//! asks for" has to look at the fonts the machine has, which is what PDFium
//! does through `SystemFontInfoIface` / `CFX_FolderFontInfo`.
//!
//! # This is opt-in, and that is deliberate
//! Every other resolution step in this engine is a pure function of the PDF.
//! System fonts are not: they depend on what happens to be installed, so
//! enabling them **trades determinism for coverage**. The provider is
//! therefore injected (`PageCompiler::with_system_fonts`), never global and
//! never on by default — an engine built without one still renders, falling
//! back to the bundled faces, and two machines still agree byte for byte.
//! Callers that want PDFium's coverage opt in explicitly.
//!
//! # Matching order (mirrors `CFX_LinuxFontInfo::MapFont`)
//! 1. Exact family name, normalized (`DengXian`, `Cambria`, …).
//! 2. For a CJK charset, a preference list of families known to cover it.
//! 3. A generic match on serif/fixed-pitch and style.
//!
//! Anything not found falls back to the bundled standard 14, so text never
//! disappears.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// The character repertoire a PDF font needs, derived from its CMap or
/// `/CIDSystemInfo` (PDFium's `FX_Charset`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Charset {
    /// Latin / unspecified.
    #[default]
    Ansi,
    ShiftJis,
    ChineseSimplified,
    ChineseTraditional,
    Hangul,
    Symbol,
}

impl Charset {
    pub fn is_cjk(self) -> bool {
        matches!(
            self,
            Charset::ShiftJis
                | Charset::ChineseSimplified
                | Charset::ChineseTraditional
                | Charset::Hangul
        )
    }

    /// Classify a CID font from its `/CIDSystemInfo` registry+ordering, or a
    /// simple font from its `/Encoding` CMap name.
    ///
    /// `Adobe-Identity-0` says nothing about the repertoire — the CIDs are
    /// the original font's glyph ids — so it maps to `Ansi` and only an
    /// exact family match can help. Producers often leave a hint in the
    /// `/BaseFont` name (`DengXian-GBK-EUC-H-Identity-H`), which
    /// [`Self::from_font_name`] picks up.
    pub fn from_ordering(ordering: &[u8]) -> Charset {
        match ordering {
            b"Japan1" => Charset::ShiftJis,
            b"GB1" => Charset::ChineseSimplified,
            b"CNS1" => Charset::ChineseTraditional,
            b"Korea1" | b"KR" => Charset::Hangul,
            _ => Charset::Ansi,
        }
    }

    /// Classify from a CMap name (`GBK-EUC-H`, `90ms-RKSJ-H`, …) or a
    /// `/BaseFont` that embeds one.
    pub fn from_font_name(name: &[u8]) -> Charset {
        let lower = name.to_ascii_lowercase();
        let has = |n: &[u8]| lower.windows(n.len()).any(|w| w == n);
        if has(b"rksj") || has(b"90ms") || has(b"90pv") || has(b"euc-h") && has(b"jis") {
            Charset::ShiftJis
        } else if has(b"gbk") || has(b"gb-euc") || has(b"gbpc") || has(b"unigb") {
            Charset::ChineseSimplified
        } else if has(b"b5") || has(b"eten") || has(b"unicns") || has(b"cns") {
            Charset::ChineseTraditional
        } else if has(b"uhc") || has(b"ksc") || has(b"unikc") {
            Charset::Hangul
        } else {
            Charset::Ansi
        }
    }
}

/// What the renderer is looking for.
#[derive(Debug, Clone, Copy)]
pub struct SystemFontRequest<'a> {
    /// `/BaseFont` with any subset tag already stripped.
    pub family: &'a [u8],
    pub bold: bool,
    pub italic: bool,
    pub serif: bool,
    pub fixed_pitch: bool,
    pub charset: Charset,
}

/// A located system font: its file bytes plus which face to use.
///
/// The index matters — the big CJK families ship as `.ttc` collections
/// holding one face per language, and a collection's bytes are not a font
/// on their own.
#[derive(Debug, Clone)]
pub struct SystemFont {
    pub data: Arc<[u8]>,
    /// Face index within a collection; 0 for a plain font file.
    pub index: u32,
}

/// A source of installed font programs.
///
/// Implementations must be pure with respect to `&self` (safe for concurrent
/// lookups from every worker) and are injected, never global.
pub trait SystemFontProvider: Send + Sync + std::fmt::Debug {
    /// The font best matching `request`, or `None` to fall back to the
    /// bundled faces.
    fn lookup(&self, request: &SystemFontRequest<'_>) -> Option<SystemFont>;
}

/// Families PDFium prefers per CJK charset (`core/fxge/linux/fx_linux_impl.cpp`),
/// followed by modern additions.
///
/// PDFium's lists predate Noto by years, so on a current Linux install its
/// Japanese/Korean lists usually match nothing. Keeping PDFium's names first
/// preserves its choice where those fonts exist, and the Noto/Droid entries
/// then cover the machines PDFium would have failed on — strictly more
/// coverage, never a different answer when PDFium would have found one.
const JP_FONTS: &[&str] = &[
    // Windows / macOS system faces first (where a Windows/mac sweep runs).
    "Yu Gothic",
    "Meiryo",
    "MS Gothic",
    "MS PGothic",
    "MS Mincho",
    "Hiragino Sans",
    "Hiragino Kaku Gothic Pro",
    "Hiragino Mincho ProN",
    // PDFium's Linux list.
    "TakaoGothic",
    "VL Gothic",
    "IPAGothic",
    "Kochi Gothic",
    "TakaoPGothic",
    "VL PGothic",
    "IPAPGothic",
    "TakaoMincho",
    "IPAMincho",
    "Kochi Mincho",
    "Noto Sans CJK JP",
    "Noto Serif CJK JP",
    "Droid Sans Fallback",
];
const GB_FONTS: &[&str] = &[
    // Windows / macOS.
    "SimSun",
    "NSimSun",
    "SimHei",
    "Microsoft YaHei",
    "DengXian",
    "FangSong",
    "KaiTi",
    "PingFang SC",
    "STHeiti",
    "STSong",
    "Songti SC",
    // Linux.
    "AR PL UMing CN Light",
    "WenQuanYi Micro Hei",
    "AR PL UKai CN",
    "Noto Sans CJK SC",
    "Noto Serif CJK SC",
    "Droid Sans Fallback",
];
const B5_FONTS: &[&str] = &[
    // Windows / macOS.
    "MingLiU",
    "PMingLiU",
    "Microsoft JhengHei",
    "DFKai-SB",
    "PingFang TC",
    "Heiti TC",
    "Songti TC",
    // Linux.
    "AR PL UMing TW Light",
    "WenQuanYi Micro Hei",
    "AR PL UKai TW",
    "Noto Sans CJK TC",
    "Noto Serif CJK TC",
    "Droid Sans Fallback",
];
const HANGUL_FONTS: &[&str] = &[
    // Windows / macOS.
    "Malgun Gothic",
    "Gulim",
    "Batang",
    "Dotum",
    "Apple SD Gothic Neo",
    // Linux.
    "UnDotum",
    "Noto Sans CJK KR",
    "Noto Serif CJK KR",
    "Droid Sans Fallback",
];

/// The platform font directories to scan. Covers Windows, macOS and Linux —
/// a non-existent path is simply skipped, so listing all three is harmless and
/// keeps a single cross-platform build. Windows/macOS matter for CJK: their
/// system faces (SimSun, MS Gothic, PingFang, …) are what a non-embedded CID
/// font substitutes against, exactly as PDFium does per platform.
pub fn default_font_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    // Windows: %WINDIR%\Fonts plus the per-user store (Win10 1809+).
    if let Some(windir) = std::env::var_os("WINDIR") {
        paths.push(Path::new(&windir).join("Fonts"));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        paths.push(
            Path::new(&local)
                .join("Microsoft")
                .join("Windows")
                .join("Fonts"),
        );
    }
    // macOS.
    paths.push(PathBuf::from("/System/Library/Fonts"));
    paths.push(PathBuf::from("/Library/Fonts"));
    // Linux (PDFium's `CreateDefaultSystemFontInfo`).
    paths.push(PathBuf::from("/usr/share/fonts"));
    paths.push(PathBuf::from("/usr/share/X11/fonts/Type1"));
    paths.push(PathBuf::from("/usr/share/X11/fonts/TTF"));
    paths.push(PathBuf::from("/usr/local/share/fonts"));
    // Android. Flat directory, no fontconfig, none of the Debian layout above.
    paths.push(PathBuf::from("/system/fonts"));
    // Per-user locations.
    if let Some(home) = std::env::var_os("HOME") {
        paths.push(Path::new(&home).join(".fonts"));
        paths.push(Path::new(&home).join(".local/share/fonts"));
        paths.push(Path::new(&home).join("Library").join("Fonts"));
    }
    paths
}

/// One installed face.
#[derive(Debug, Clone)]
struct FaceEntry {
    path: PathBuf,
    /// Face index within a collection.
    index: u32,
    bold: bool,
    italic: bool,
}

/// A [`SystemFontProvider`] backed by scanning font directories — the
/// analogue of PDFium's `CFX_FolderFontInfo`.
///
/// The index (family → faces) is built once at construction from the fonts'
/// `name` tables; file *contents* are read lazily on a hit and then cached,
/// so an engine that never needs a system font never reads one.
#[derive(Debug)]
pub struct FolderFontProvider {
    /// Normalized family name → the faces under it.
    families: HashMap<String, Vec<FaceEntry>>,
    /// Loaded programs, keyed by path.
    cache: RwLock<HashMap<PathBuf, Option<Arc<[u8]>>>>,
    /// Cap on a single font file, so a hostile file cannot exhaust memory.
    max_font_bytes: u64,
}

/// Fonts above this are not plausible faces and are skipped.
const DEFAULT_MAX_FONT_BYTES: u64 = 64 << 20;

impl FolderFontProvider {
    /// Scan the platform's font directories.
    pub fn system() -> FolderFontProvider {
        Self::with_paths(&default_font_paths())
    }

    /// Scan explicit directories (tests, sandboxes, an app's own font dir).
    pub fn with_paths(paths: &[PathBuf]) -> FolderFontProvider {
        let mut families: HashMap<String, Vec<FaceEntry>> = HashMap::new();
        for root in paths {
            scan_dir(root, 0, &mut families);
        }
        // Deterministic order within a family, so repeated runs on one
        // machine pick the same face.
        for faces in families.values_mut() {
            faces.sort_by(|a, b| (&a.path, a.index).cmp(&(&b.path, b.index)));
        }
        FolderFontProvider {
            families,
            cache: RwLock::new(HashMap::new()),
            max_font_bytes: DEFAULT_MAX_FONT_BYTES,
        }
    }

    /// Number of indexed families (diagnostics/tests).
    pub fn family_count(&self) -> usize {
        self.families.len()
    }

    /// True if a family is present, by its normalized name.
    pub fn has_family(&self, name: &str) -> bool {
        self.families.contains_key(&normalize(name.as_bytes()))
    }

    fn pick(&self, family: &str, bold: bool, italic: bool) -> Option<SystemFont> {
        let faces = self.families.get(&normalize(family.as_bytes()))?;
        // Prefer an exact style, then anything in the family: a wrong weight
        // beats no glyphs at all.
        let best = faces
            .iter()
            .find(|f| f.bold == bold && f.italic == italic)
            .or_else(|| faces.iter().find(|f| f.italic == italic))
            .or_else(|| faces.iter().find(|f| f.bold == bold))
            .or_else(|| faces.first())?;
        Some(SystemFont {
            data: self.load(&best.path)?,
            index: best.index,
        })
    }

    fn load(&self, path: &Path) -> Option<Arc<[u8]>> {
        if let Some(hit) = self.cache.read().ok()?.get(path) {
            return hit.clone();
        }
        let bytes = std::fs::metadata(path)
            .ok()
            .filter(|m| m.len() <= self.max_font_bytes)
            .and_then(|_| std::fs::read(path).ok())
            .map(Arc::<[u8]>::from);
        if let Ok(mut cache) = self.cache.write() {
            cache.insert(path.to_path_buf(), bytes.clone());
        }
        bytes
    }
}

impl SystemFontProvider for FolderFontProvider {
    fn lookup(&self, request: &SystemFontRequest<'_>) -> Option<SystemFont> {
        // 1. The document's own family name.
        let family = String::from_utf8_lossy(request.family).to_string();
        if let Some(bytes) = self.pick(&family, request.bold, request.italic) {
            return Some(bytes);
        }
        // A `Family,Bold` / `Family-BoldMT` spelling: retry on the stem.
        if let Some(stem) = style_stem(request.family)
            && let Some(bytes) = self.pick(&stem, request.bold, request.italic)
        {
            return Some(bytes);
        }
        // 2. A charset preference list (PDFium's CJK tables).
        let list = match request.charset {
            Charset::ShiftJis => JP_FONTS,
            Charset::ChineseSimplified => GB_FONTS,
            Charset::ChineseTraditional => B5_FONTS,
            Charset::Hangul => HANGUL_FONTS,
            _ => &[],
        };
        for name in list {
            if let Some(bytes) = self.pick(name, request.bold, request.italic) {
                return Some(bytes);
            }
        }
        // 3. No generic serif/sans guess: the bundled standard 14 already
        //    answer that, deterministically, and are metric-compatible.
        None
    }
}

/// Fold case, spaces and separators so `Arial-BoldMT`, `Arial,Bold` and
/// `arial bold` all key alike.
fn normalize(name: &[u8]) -> String {
    name.iter()
        .filter(|b| !b" -_,".contains(b))
        .map(|b| b.to_ascii_lowercase() as char)
        .collect()
}

/// `Cambria,Bold` / `Cambria-BoldItalic` → `Cambria`.
fn style_stem(name: &[u8]) -> Option<String> {
    let cut = name.iter().position(|b| *b == b',' || *b == b'-')?;
    if cut == 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&name[..cut]).to_string())
}

/// Recursively index font files. Bounded depth: font trees are shallow, and
/// a symlink loop must not hang startup.
fn scan_dir(dir: &Path, depth: u32, out: &mut HashMap<String, Vec<FaceEntry>>) {
    const MAX_DEPTH: u32 = 8;
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            scan_dir(&path, depth + 1, out);
            continue;
        }
        let is_font = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| {
                let e = e.to_ascii_lowercase();
                matches!(e.as_str(), "ttf" | "otf" | "ttc" | "otc" | "pfb")
            })
            .unwrap_or(false);
        if !is_font {
            continue;
        }
        index_font(&path, out);
    }
}

/// Read a font's family/style out of its `name` table and index it.
fn index_font(path: &Path, out: &mut HashMap<String, Vec<FaceEntry>>) {
    use skrifa::MetadataProvider;
    use skrifa::raw::TableProvider;
    use skrifa::raw::types::NameId;

    let Ok(data) = std::fs::read(path) else {
        return;
    };
    // A collection holds several faces; index each family it names.
    let Ok(file) = skrifa::raw::FileRef::new(&data) else {
        return;
    };
    let fonts: Vec<(u32, skrifa::FontRef)> = match file {
        skrifa::raw::FileRef::Font(f) => vec![(0, f)],
        skrifa::raw::FileRef::Collection(c) => (0..c.len())
            .filter_map(|i| c.get(i).ok().map(|f| (i, f)))
            .collect(),
    };
    for (face_index, font) in fonts {
        let name_of = |id: NameId| -> Option<String> {
            font.localized_strings(id)
                .english_or_first()
                .map(|s| s.to_string())
        };
        // Prefer the typographic family, so "Noto Sans CJK JP Bold" indexes
        // under "Noto Sans CJK JP" rather than as its own family.
        let family =
            name_of(NameId::TYPOGRAPHIC_FAMILY_NAME).or_else(|| name_of(NameId::FAMILY_NAME));
        let Some(family) = family else { continue };
        let subfamily = name_of(NameId::TYPOGRAPHIC_SUBFAMILY_NAME)
            .or_else(|| name_of(NameId::SUBFAMILY_NAME))
            .unwrap_or_default()
            .to_ascii_lowercase();

        // OS/2 is authoritative for style; the subfamily name is the backup.
        let (mut bold, mut italic) = (false, false);
        if let Ok(os2) = font.os2() {
            let sel = os2.fs_selection().bits();
            italic = sel & 0x01 != 0;
            bold = sel & 0x20 != 0;
        }
        bold |= subfamily.contains("bold");
        italic |= subfamily.contains("italic") || subfamily.contains("oblique");

        out.entry(normalize(family.as_bytes()))
            .or_default()
            .push(FaceEntry {
                path: path.to_path_buf(),
                index: face_index,
                bold,
                italic,
            });
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn names_normalize_across_spellings() {
        for n in [
            "Arial-BoldMT",
            "Arial,BoldMT",
            "arial bold mt",
            "ARIAL_BOLDMT",
        ] {
            assert_eq!(normalize(n.as_bytes()), "arialboldmt", "{n}");
        }
    }

    #[test]
    fn style_stems_are_extracted() {
        assert_eq!(style_stem(b"Cambria,Bold").as_deref(), Some("Cambria"));
        assert_eq!(
            style_stem(b"Cambria-BoldItalic").as_deref(),
            Some("Cambria")
        );
        assert_eq!(style_stem(b"Cambria"), None);
    }

    #[test]
    fn charsets_come_from_ordering_and_names() {
        assert_eq!(Charset::from_ordering(b"Japan1"), Charset::ShiftJis);
        assert_eq!(Charset::from_ordering(b"GB1"), Charset::ChineseSimplified);
        assert_eq!(Charset::from_ordering(b"CNS1"), Charset::ChineseTraditional);
        assert_eq!(Charset::from_ordering(b"Korea1"), Charset::Hangul);
        // Identity says nothing about the repertoire.
        assert_eq!(Charset::from_ordering(b"Identity"), Charset::Ansi);
        assert!(!Charset::Ansi.is_cjk());
        assert!(Charset::Hangul.is_cjk());

        // Producers leave the original CMap in the /BaseFont name.
        assert_eq!(
            Charset::from_font_name(b"DengXian-GBK-EUC-H-Identity-H"),
            Charset::ChineseSimplified
        );
        assert_eq!(
            Charset::from_font_name(b"CambriaMath-KSCms-UHC-H-Identity-H"),
            Charset::Hangul
        );
        assert_eq!(
            Charset::from_font_name(b"KozMinPro-90ms-RKSJ-H"),
            Charset::ShiftJis
        );
        assert_eq!(
            Charset::from_font_name(b"MSung-B5pc-H"),
            Charset::ChineseTraditional
        );
        assert_eq!(Charset::from_font_name(b"TimesNewRoman"), Charset::Ansi);
    }

    #[test]
    fn an_empty_provider_finds_nothing_and_does_not_panic() {
        let p = FolderFontProvider::with_paths(&[PathBuf::from("/nonexistent/font/dir")]);
        assert_eq!(p.family_count(), 0);
        let r = SystemFontRequest {
            family: b"Arial",
            bold: false,
            italic: false,
            serif: false,
            fixed_pitch: false,
            charset: Charset::Ansi,
        };
        assert!(p.lookup(&r).is_none());
    }
}
