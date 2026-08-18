//! One-based page ranges for the public CLI boundary.

use anyhow::{Context, Result, bail};

/// A zero-based page index resolved against a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PageZero(pub u32);

impl PageZero {
    pub fn one_based(self) -> u32 {
        self.0 + 1
    }
}

/// Parse a one-based page number from the CLI (rejects 0).
pub fn parse_one_based(raw: &str) -> Result<u32> {
    let n: u32 = raw
        .trim()
        .parse()
        .with_context(|| format!("invalid page number {raw:?}"))?;
    if n == 0 {
        bail!("page numbers are one-based; got 0");
    }
    Ok(n)
}

/// Convert a one-based page number to a zero-based index, checking range.
pub fn to_zero_based(page_one: u32, page_count: u32) -> Result<PageZero> {
    if page_one == 0 {
        bail!("page numbers are one-based; got 0");
    }
    let index = page_one - 1;
    if page_count == 0 || index >= page_count {
        bail!("page {page_one} out of range (document has {page_count} pages)");
    }
    Ok(PageZero(index))
}

/// Parse a page range expression against `page_count`.
///
/// Accepted forms: `all`, `1`, `1-3`, `1,3,5-7` (one-based, inclusive).
/// Returns unique zero-based indices in ascending order, capped by `max_pages`.
pub fn parse_page_range(
    spec: Option<&str>,
    page_count: u32,
    max_pages: u32,
) -> Result<(Vec<PageZero>, Vec<String>)> {
    let mut warnings = Vec::new();
    if page_count == 0 {
        return Ok((Vec::new(), warnings));
    }

    let mut indices: Vec<u32> = match spec.map(str::trim).filter(|s| !s.is_empty()) {
        None | Some("all") => (0..page_count).collect(),
        Some(raw) => {
            let mut out = Vec::new();
            for part in raw.split(',') {
                let part = part.trim();
                if part.is_empty() {
                    continue;
                }
                if let Some((a, b)) = part.split_once('-') {
                    let start = parse_one_based(a)?;
                    let end = parse_one_based(b)?;
                    if start > end {
                        bail!("invalid page range {part:?}: start > end");
                    }
                    for page in start..=end {
                        let z = to_zero_based(page, page_count)?;
                        out.push(z.0);
                    }
                } else {
                    let z = to_zero_based(parse_one_based(part)?, page_count)?;
                    out.push(z.0);
                }
            }
            out.sort_unstable();
            out.dedup();
            out
        }
    };

    if max_pages > 0 && indices.len() as u32 > max_pages {
        warnings.push(format!(
            "page selection truncated from {} to max-pages={max_pages}",
            indices.len()
        ));
        indices.truncate(max_pages as usize);
    }

    Ok((indices.into_iter().map(PageZero).collect(), warnings))
}

/// Parse `X0,Y0,X1,Y1` into a PDF-space rectangle.
pub fn parse_bbox(raw: &str) -> Result<[f64; 4]> {
    let parts: Vec<&str> = raw.split(',').map(str::trim).collect();
    if parts.len() != 4 {
        bail!("bbox must be X0,Y0,X1,Y1; got {raw:?}");
    }
    let mut vals = [0.0; 4];
    for (i, part) in parts.iter().enumerate() {
        vals[i] = part
            .parse::<f64>()
            .with_context(|| format!("invalid bbox component {part:?}"))?;
        if !vals[i].is_finite() {
            bail!("bbox components must be finite");
        }
    }
    Ok(vals)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_range() {
        let (pages, warnings) = parse_page_range(Some("1,3-4"), 5, 50).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(pages.iter().map(|p| p.0).collect::<Vec<_>>(), vec![0, 2, 3]);
    }

    #[test]
    fn rejects_zero() {
        assert!(parse_one_based("0").is_err());
        assert!(parse_page_range(Some("0"), 3, 50).is_err());
    }

    #[test]
    fn truncates_max_pages() {
        let (pages, warnings) = parse_page_range(Some("all"), 10, 3).unwrap();
        assert_eq!(pages.len(), 3);
        assert!(!warnings.is_empty());
    }

    #[test]
    fn parses_bbox() {
        assert_eq!(parse_bbox("0,0,100,200").unwrap(), [0.0, 0.0, 100.0, 200.0]);
    }
}
