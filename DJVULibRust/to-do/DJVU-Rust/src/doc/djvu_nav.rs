// src/nav.rs

use std::io::{self, Write};

/// Represents a single bookmark entry.
#[derive(Debug, Clone)]
pub struct Bookmark {
    pub title: String,
    /// Destination URL, typically a page ID like "#1".
    pub dest: String,
    /// Nested bookmarks.
    pub children: Vec<Bookmark>,
}

/// Represents the entire navigation/bookmark structure (`NAVM` chunk).
#[derive(Debug, Clone, Default)]
pub struct DjVmNav {
    pub bookmarks: Vec<Bookmark>,
}

impl DjVmNav {
    /// Creates a new, empty navigation structure.
    pub fn new() -> Self {
        Self::default()
    }

    /// Encodes the navigation data into the S-expression format required for a `NAVM` chunk.
    pub fn encode<W: Write>(&self, writer: &mut W) -> Result<(), io::Error> {
        if self.bookmarks.is_empty() {
            return Ok(());
        }

        writer.write_all(b"(bookmarks\n")?;
        for bookmark in &self.bookmarks {
            self.encode_bookmark(bookmark, writer, 1)?;
        }
        writer.write_all(b")\n")?;
        Ok(())
    }

    fn encode_bookmark<W: Write>(
        &self,
        bookmark: &Bookmark,
        writer: &mut W,
        indent_level: usize,
    ) -> Result<(), io::Error> {
        let indent = " ".repeat(indent_level * 2);

        // Escape quotes and backslashes in title and destination
        let safe_title = bookmark.title.replace('\\', "\\\\").replace('"', "\\\"");
        let safe_dest = bookmark.dest.replace('\\', "\\\\").replace('"', "\\\"");

        writer.write_all(indent.as_bytes())?;
        writer.write_all(b"(\"")?;
        writer.write_all(safe_title.as_bytes())?;
        writer.write_all(b"\" \"")?;
        writer.write_all(safe_dest.as_bytes())?;
        writer.write_all(b"\"")?;

        if bookmark.children.is_empty() {
            writer.write_all(b")\n")?;
        } else {
            writer.write_all(b"\n")?;
            for child in &bookmark.children {
                self.encode_bookmark(child, writer, indent_level + 1)?;
            }
            writer.write_all(indent.as_bytes())?;
            writer.write_all(b")\n")?;
        }
        Ok(())
    }
}
