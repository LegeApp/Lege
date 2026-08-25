//! Process-backed adapters for independent PDF rendering implementations.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RendererId {
    Pdfium,
    Hayro,
    Poppler,
    Mupdf,
    Ghostscript,
    Pdfjs,
}

impl RendererId {
    pub const ALL: [Self; 6] = [
        Self::Pdfium,
        Self::Hayro,
        Self::Poppler,
        Self::Mupdf,
        Self::Ghostscript,
        Self::Pdfjs,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Pdfium => "pdfium",
            Self::Hayro => "hayro",
            Self::Poppler => "poppler",
            Self::Mupdf => "mupdf",
            Self::Ghostscript => "ghostscript",
            Self::Pdfjs => "pdfjs",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "pdfium" => Some(Self::Pdfium),
            "hayro" => Some(Self::Hayro),
            "poppler" => Some(Self::Poppler),
            "mupdf" => Some(Self::Mupdf),
            "ghostscript" | "gs" => Some(Self::Ghostscript),
            "pdfjs" | "pdf.js" => Some(Self::Pdfjs),
            _ => None,
        }
    }

    pub fn compiled(self) -> bool {
        match self {
            Self::Pdfium => cfg!(feature = "renderer-pdfium"),
            Self::Hayro => cfg!(feature = "renderer-hayro"),
            Self::Poppler => cfg!(feature = "renderer-poppler"),
            Self::Mupdf => cfg!(feature = "renderer-mupdf"),
            Self::Ghostscript => cfg!(feature = "renderer-ghostscript"),
            Self::Pdfjs => cfg!(feature = "renderer-pdfjs"),
        }
    }

    fn env_name(self) -> &'static str {
        match self {
            Self::Pdfium => "PDF_RENDERER_PDFIUM_LIB",
            Self::Hayro => "PDF_RENDERER_HAYRO_BIN",
            Self::Poppler => "PDF_RENDERER_POPPLER_BIN",
            Self::Mupdf => "PDF_RENDERER_MUPDF_BIN",
            Self::Ghostscript => "PDF_RENDERER_GHOSTSCRIPT_BIN",
            Self::Pdfjs => "PDF_RENDERER_PDFJS_BIN",
        }
    }

    fn default_relative(self) -> &'static str {
        match self {
            Self::Pdfium => ".renderer-bin/pdfium/libpdfium.so",
            Self::Hayro => ".renderer-bin/hayro/hayro-render",
            Self::Poppler => ".renderer-bin/poppler/pdftoppm",
            Self::Mupdf => ".renderer-bin/mupdf/mutool",
            Self::Ghostscript => ".renderer-bin/ghostscript/gs",
            Self::Pdfjs => ".renderer-bin/pdfjs/render",
        }
    }
}

impl fmt::Display for RendererId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Debug, Clone)]
pub struct RenderRequest<'a> {
    pub pdf: &'a Path,
    pub pages: &'a [u32],
    pub scale: f64,
}

#[derive(Debug, Clone)]
pub struct RenderedPage {
    pub page: u32,
    pub width: u32,
    pub height: u32,
    pub png: Vec<u8>,
    pub elapsed: Duration,
}

pub trait PdfRenderer {
    fn id(&self) -> RendererId;
    fn render_pages(&self, request: &RenderRequest<'_>) -> Result<Vec<RenderedPage>, String>;
}

#[derive(Debug, Clone)]
pub struct ProcessRenderer {
    id: RendererId,
    program: PathBuf,
    pdfium_lib: Option<PathBuf>,
    timeout: Duration,
}

impl ProcessRenderer {
    pub fn discover(
        id: RendererId,
        overrides: &BTreeMap<RendererId, PathBuf>,
    ) -> Result<Self, String> {
        if !id.compiled() {
            return Err(format!(
                "renderer {id} is disabled; rebuild with --features renderer-{id}"
            ));
        }
        let configured = overrides
            .get(&id)
            .cloned()
            .or_else(|| std::env::var_os(id.env_name()).map(PathBuf::from))
            .unwrap_or_else(|| tools_root().join(id.default_relative()));
        let (program, pdfium_lib) = if id == RendererId::Pdfium {
            let exe = std::env::current_exe()
                .map_err(|e| format!("cannot locate pdfium-diff executable: {e}"))?;
            (exe, Some(configured))
        } else {
            (configured, None)
        };
        if id == RendererId::Pdfium {
            if !pdfium_lib.as_ref().is_some_and(|p| p.is_file()) {
                return Err(format!(
                    "pdfium library not found; set {} or --renderer-path pdfium=/path/to/libpdfium.so",
                    id.env_name()
                ));
            }
        } else if !program.is_file() {
            return Err(format!(
                "{id} helper not found at {}; run ./setup-renderers.sh --engines {id} or set {}",
                program.display(),
                id.env_name()
            ));
        }
        let timeout = std::env::var("PDF_RENDERER_ENGINE_TIMEOUT")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(180));
        Ok(Self {
            id,
            program,
            pdfium_lib,
            timeout,
        })
    }

    fn command(&self, request: &RenderRequest<'_>, dir: &Path) -> Result<Command, String> {
        let dpi = request.scale * 72.0;
        let pages_one_based = request
            .pages
            .iter()
            .map(|p| (p + 1).to_string())
            .collect::<Vec<_>>()
            .join(",");
        let mut command = Command::new(&self.program);
        match self.id {
            RendererId::Pdfium => {
                command
                    .arg("--reference-worker")
                    .arg(self.pdfium_lib.as_ref().unwrap())
                    .arg(request.scale.to_string())
                    .arg(request.pdf)
                    .arg(pages_csv(request.pages))
                    .arg(dir);
            }
            RendererId::Hayro => {
                command
                    .arg(request.pdf)
                    .arg(dir)
                    .arg(request.scale.to_string())
                    .arg(pages_csv(request.pages));
            }
            RendererId::Poppler => {
                // pdftoppm has no arbitrary page-list option, so the caller
                // invokes it once per requested page (still process isolated).
                return Err("poppler-single-page".into());
            }
            RendererId::Mupdf => {
                command
                    .args([
                        "draw", "-q", "-F", "png", "-c", "rgb", "-b", "CropBox", "-r",
                    ])
                    .arg(dpi.to_string())
                    .arg("-o")
                    .arg(dir.join("page-%d.png"))
                    .arg(request.pdf)
                    .arg(pages_one_based);
            }
            RendererId::Ghostscript => {
                command
                    .args([
                        "-dSAFER",
                        "-dBATCH",
                        "-dNOPAUSE",
                        "-dUseCropBox",
                        "-sDEVICE=png16m",
                        "-dGraphicsAlphaBits=4",
                        "-dTextAlphaBits=4",
                    ])
                    .arg(format!("-r{dpi}"))
                    .arg(format!("-sPageList={pages_one_based}"))
                    .arg(format!(
                        "-sOutputFile={}",
                        dir.join("page-%d.png").display()
                    ))
                    .arg(request.pdf);
            }
            RendererId::Pdfjs => {
                command
                    .arg(request.pdf)
                    .arg(pages_csv(request.pages))
                    .arg(request.scale.to_string())
                    .arg(dir);
            }
        }
        Ok(command)
    }

    fn render_poppler(&self, request: &RenderRequest<'_>, dir: &Path) -> Result<(), String> {
        for page in request.pages {
            let start = Instant::now();
            let prefix = dir.join(format!("page-{page}"));
            let mut command = Command::new(&self.program);
            command
                .args(["-q", "-png", "-singlefile", "-cropbox", "-f"])
                .arg((page + 1).to_string())
                .arg("-l")
                .arg((page + 1).to_string())
                .arg("-r")
                .arg((request.scale * 72.0).to_string())
                .arg(request.pdf)
                .arg(&prefix);
            run_command(command, self.timeout)?;
            let generated = prefix.with_extension("png");
            let canonical = dir.join(format!("page-{page}.png"));
            if generated != canonical {
                std::fs::rename(&generated, &canonical)
                    .map_err(|e| format!("rename {}: {e}", generated.display()))?;
            }
            let _ = start;
        }
        Ok(())
    }
}

impl PdfRenderer for ProcessRenderer {
    fn id(&self) -> RendererId {
        self.id
    }

    fn render_pages(&self, request: &RenderRequest<'_>) -> Result<Vec<RenderedPage>, String> {
        let temp = TempDir::new(self.id.name())?;
        let start = Instant::now();
        if self.id == RendererId::Poppler {
            self.render_poppler(request, temp.path())?;
        } else {
            let command = self.command(request, temp.path())?;
            run_command(command, self.timeout)?;
        }
        let elapsed = start.elapsed();
        let mut result = Vec::with_capacity(request.pages.len());
        for page in request.pages {
            let candidates = match self.id {
                RendererId::Hayro => vec![temp.path().join(format!("rendered_{page}.png"))],
                RendererId::Mupdf | RendererId::Ghostscript => vec![
                    temp.path().join(format!("page-{}.png", page + 1)),
                    temp.path().join(format!("page-{}.png", result.len() + 1)),
                ],
                _ => vec![temp.path().join(format!("page-{page}.png"))],
            };
            let path = candidates
                .iter()
                .find(|p| p.is_file())
                .ok_or_else(|| format!("{} produced no PNG for zero-based page {page}", self.id))?;
            let png = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
            let (width, height) = png_dimensions(&png)?;
            result.push(RenderedPage {
                page: *page,
                width,
                height,
                png,
                elapsed,
            });
        }
        Ok(result)
    }
}

fn pages_csv(pages: &[u32]) -> String {
    pages
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn tools_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

fn run_command(mut command: Command, timeout: Duration) -> Result<ExitStatus, String> {
    // Do not pipe stderr without draining it while polling: a noisy malformed
    // document could fill the pipe and deadlock the renderer before timeout.
    command.stdout(Stdio::null()).stderr(Stdio::inherit());
    let display = format!("{command:?}");
    let mut child = command
        .spawn()
        .map_err(|e| format!("spawn {display}: {e}"))?;
    let status = wait_child(&mut child, timeout)?;
    if status.success() {
        Ok(status)
    } else {
        Err(format!("{display} exited {status}"))
    }
}

fn wait_child(child: &mut Child, timeout: Duration) -> Result<ExitStatus, String> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "renderer timed out after {} seconds",
                    timeout.as_secs()
                ));
            }
            Err(e) => return Err(format!("wait for renderer: {e}")),
        }
    }
}

pub fn png_dimensions(bytes: &[u8]) -> Result<(u32, u32), String> {
    // `png` 0.18's `Decoder<R>` requires `R: Read + Seek`; a bare `&[u8]` only
    // implements `Read`, so the slice is wrapped rather than passed directly.
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let reader = decoder
        .read_info()
        .map_err(|e| format!("decode PNG header: {e}"))?;
    let info = reader.info();
    Ok((info.width, info.height))
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Result<Self, String> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("pdfium-diff-{tag}-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&path).map_err(|e| format!("create {}: {e}", path.display()))?;
        Ok(Self(path))
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn parse_references(values: &[String]) -> Result<Vec<RendererId>, String> {
    let mut ids = Vec::new();
    for value in values {
        for token in value.split(',').filter(|s| !s.is_empty()) {
            if token.eq_ignore_ascii_case("all") {
                ids.extend(RendererId::ALL.into_iter().filter(|id| id.compiled()));
            } else {
                ids.push(
                    RendererId::parse(token)
                        .ok_or_else(|| format!("unknown renderer {token:?}"))?,
                );
            }
        }
    }
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        return Err("select at least one renderer with --reference".into());
    }
    Ok(ids)
}

pub fn parse_override(value: &str) -> Result<(RendererId, PathBuf), String> {
    let (name, path) = value
        .split_once('=')
        .ok_or("--renderer-path requires NAME=PATH")?;
    let id = RendererId::parse(name).ok_or_else(|| format!("unknown renderer {name:?}"))?;
    Ok((id, PathBuf::from(OsString::from(path))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_lists_are_deduplicated() {
        let got = parse_references(&["mupdf,pdfjs".into(), "mupdf".into()]).unwrap();
        assert_eq!(got, vec![RendererId::Mupdf, RendererId::Pdfjs]);
    }

    #[test]
    fn override_requires_known_name() {
        assert_eq!(
            parse_override("mupdf=/tmp/mutool").unwrap().0,
            RendererId::Mupdf
        );
        assert!(parse_override("wat=/tmp/x").is_err());
    }
}
