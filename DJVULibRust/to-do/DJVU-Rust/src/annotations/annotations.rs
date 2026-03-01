// src/annotations.rs

use std::fmt;
use std::io::Write;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AnnotationError {
    #[error("I/O error during annotation encoding")]
    Io(#[from] std::io::Error),
    #[error("Invalid shape coordinates for annotation: {0}")]
    InvalidShape(&'static str),
}

/// Represents the shape of a hyperlink area.
pub enum AnnotationShape {
    Rect { x: u32, y: u32, w: u32, h: u32 },
    Oval { x: u32, y: u32, w: u32, h: u32 },
    Polygon { points: Vec<(u32, u32)> },
}

impl fmt::Display for AnnotationShape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rect { x, y, w, h } => write!(f, "(rect {} {} {} {})", x, y, w, h),
            Self::Oval { x, y, w, h } => write!(f, "(oval {} {} {} {})", x, y, w, h),
            Self::Polygon { points } => {
                let points_str = points
                    .iter()
                    .map(|(x, y)| format!("{} {}", x, y))
                    .collect::<Vec<_>>()
                    .join(" ");
                write!(f, "(poly {})", points_str)
            }
        }
    }
}

/// Represents a single hyperlink or clickable map area.
pub struct Hyperlink {
    pub shape: AnnotationShape,
    pub url: String,
    pub comment: String,
    pub target: String,
    // Note: Border and highlight options are omitted for simplicity but can be added here.
}

/// Represents the full set of annotations for a page.
#[derive(Default)]
pub struct Annotations {
    pub hyperlinks: Vec<Hyperlink>,
    pub metadata: Vec<(String, String)>,
}

impl Annotations {
    pub fn new() -> Self {
        Default::default()
    }

    /// Encodes the annotations into the LISP-like format required for an ANTa/ANTz chunk.
    /// The output of this function should be compressed (e.g., with bzip2) before
    /// being stored in a final DjVu file as an 'ANTz' chunk.
    pub fn encode(&self, writer: &mut impl Write) -> Result<(), AnnotationError> {
        for link in &self.hyperlinks {
            let url_part = format!(
                "(url \"{}\" \"{}\")",
                escape_str(&link.url),
                escape_str(&link.target)
            );
            let comment_part = format!("\"{}\"", escape_str(&link.comment));
            let shape_part = format!("{}", link.shape);

            // The full format is `(maparea <url> <comment> <shape> <options...>)`
            let maparea = format!(
                "(maparea {} {} {} (none))",
                url_part, comment_part, shape_part
            );
            writer.write_all(maparea.as_bytes())?;
        }

        if !self.metadata.is_empty() {
            let mut meta_str = String::from("(metadata");
            for (key, value) in &self.metadata {
                meta_str.push_str(&format!(" ({} \"{}\")", escape_str(key), escape_str(value)));
            }
            meta_str.push(')');
            writer.write_all(meta_str.as_bytes())?;
        }

        Ok(())
    }
}

/// Escapes a string for use inside the LISP-like annotation format.
fn escape_str(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}
