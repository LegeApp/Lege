//! `jbig2dec` differential oracle (jbig2dec-gaps-plan.md Gap C).
//!
//! Runs the system `jbig2dec` on a temp file and parses its P4 PBM output.
//! When the binary is absent the helpers return `None` so tests *skip* the
//! oracle comparison rather than fail (CI must have it installed).

#![allow(dead_code)]

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

use super::pbm::Pbm;

/// Locate `jbig2dec`: honour `$JBIG2DEC`, else a repo-local `jbig2dec.exe`,
/// else `jbig2dec` on `PATH`. Returns `None` if none is runnable.
pub fn jbig2dec_path() -> Option<PathBuf> {
    if let Ok(env) = std::env::var("JBIG2DEC") {
        let p = PathBuf::from(env);
        if p.exists() {
            return Some(p);
        }
    }
    let local = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("jbig2dec.exe");
    if local.exists() {
        return Some(local);
    }
    // Probe PATH by attempting `--version`.
    if Command::new("jbig2dec")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return Some(PathBuf::from("jbig2dec"));
    }
    None
}

/// Decode a standalone JBIG2 file with `jbig2dec`. Returns `None` when the
/// binary is absent (skip), `Err` when it runs but fails.
pub fn decode_standalone(file_bytes: &[u8]) -> Option<Result<Pbm, String>> {
    let dec = jbig2dec_path()?;
    Some(run(&dec, &[], file_bytes, false))
}

/// Decode an embedded page stream (with optional globals) with `jbig2dec -e`.
pub fn decode_embedded(globals: Option<&[u8]>, page: &[u8]) -> Option<Result<Pbm, String>> {
    let dec = jbig2dec_path()?;
    Some(run(
        &dec,
        globals.into_iter().collect::<Vec<_>>().as_slice(),
        page,
        true,
    ))
}

fn run(dec: &PathBuf, globals: &[&[u8]], page: &[u8], embedded: bool) -> Result<Pbm, String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let out_path = dir.path().join("out.pbm");
    let page_path = dir.path().join("page.jb2");
    std::fs::File::create(&page_path)
        .and_then(|mut f| f.write_all(page))
        .map_err(|e| e.to_string())?;

    let mut cmd = Command::new(dec);
    if embedded {
        cmd.arg("-e");
    }
    cmd.arg("-o").arg(&out_path);

    let mut globals_paths = Vec::new();
    for (i, g) in globals.iter().enumerate() {
        let gp = dir.path().join(format!("globals{i}.jb2"));
        std::fs::File::create(&gp)
            .and_then(|mut f| f.write_all(g))
            .map_err(|e| e.to_string())?;
        globals_paths.push(gp);
    }
    for gp in &globals_paths {
        cmd.arg(gp);
    }
    cmd.arg(&page_path);

    let output = cmd.output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "jbig2dec exited {:?}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let pbm = std::fs::read(&out_path).map_err(|e| e.to_string())?;
    Pbm::from_p4(&pbm)
}
