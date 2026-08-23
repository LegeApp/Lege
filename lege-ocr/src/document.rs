/// Document model for EPUB output — stubs, not yet wired into the pipeline.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockKind {
    Title,
    SectionHeading,
    Paragraph,
    Footnote,
    Header,
    Footer,
    Caption,
    Table,
    Formula,
    Other(String),
}

impl BlockKind {
    pub fn from_class_name(name: &str) -> Self {
        match name {
            "title" | "doc_title" => BlockKind::Title,
            "paragraph_title" => BlockKind::SectionHeading,
            "plain_text" | "text" | "content" | "abstract" | "reference" | "aside_text" => {
                BlockKind::Paragraph
            }
            "table_footnote" | "footnote" => BlockKind::Footnote,
            "header" => BlockKind::Header,
            "footer" => BlockKind::Footer,
            "figure_caption" | "table_caption" | "formula_caption" | "figure_title"
            | "chart_title" | "table_title" => BlockKind::Caption,
            "table" => BlockKind::Table,
            "isolate_formula" | "formula" | "formula_number" | "algorithm" => BlockKind::Formula,
            other => BlockKind::Other(other.to_string()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TextWord {
    pub text: String,
    /// [x1, y1, x2, y2] in page-global high-res pixel space
    pub bbox: [u32; 4],
    pub confidence: Option<f32>,
}

#[derive(Debug, Clone)]
pub struct TextLine {
    pub text: String,
    /// [x1, y1, x2, y2] in page-global high-res pixel space
    pub bbox: [u32; 4],
    pub words: Vec<TextWord>,
}

#[derive(Debug, Clone)]
pub struct TextBlock {
    pub kind: BlockKind,
    /// [x1, y1, x2, y2] in page-global high-res pixel space
    pub bbox: [u32; 4],
    pub lines: Vec<TextLine>,
    pub indent_px: u32,
    pub spacing_before_px: u32,
    pub spacing_after_px: u32,
}

impl TextBlock {
    /// Join the block's lines into a single paragraph string. A line ending in a
    /// hyphen is treated as a soft line-break inside a word and is joined to the
    /// next line without a space (the hyphen removed); otherwise lines are joined
    /// with a single space.
    pub fn paragraph_text(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            let trimmed = line.text.trim();
            if trimmed.is_empty() {
                continue;
            }
            if out.is_empty() {
                out.push_str(trimmed);
                continue;
            }
            // De-hyphenate: a trailing hyphen before another line usually splits a
            // single word. Only repair when the preceding char is alphabetic so we
            // do not eat intentional dashes (e.g. ranges, em-dash-as-hyphen).
            if should_dehyphenate(&out, line) {
                out.pop();
                out.push_str(trimmed);
            } else {
                out.push(' ');
                out.push_str(trimmed);
            }
        }
        out
    }

    /// True when the block carries no recognizable text.
    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(|l| l.text.trim().is_empty())
    }
}

/// True when `current` (the paragraph built so far) ends in a soft hyphen that
/// `next_line` continues, so the two should be joined without a space.
fn should_dehyphenate(current: &str, next_line: &TextLine) -> bool {
    if !current.ends_with('-') {
        return false;
    }
    let prev_is_alpha = current
        .chars()
        .rev()
        .nth(1)
        .is_some_and(|c| c.is_alphabetic());
    if !prev_is_alpha {
        return false;
    }

    let first_next = next_line
        .words
        .first()
        .and_then(|word| word.text.chars().next())
        .or_else(|| next_line.text.trim().chars().next());
    first_next.is_some_and(|c| c.is_alphabetic() && c.is_lowercase())
}

#[derive(Debug, Clone)]
pub struct PageText {
    pub page_index: usize,
    pub blocks: Vec<TextBlock>,
    /// Page dimensions
    pub width_px: u32,
    pub height_px: u32,
}

impl PageText {
    pub fn plain_text(&self) -> String {
        let mut out = String::new();
        for block in &self.blocks {
            for line in &block.lines {
                out.push_str(&line.text);
                out.push('\n');
            }
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(lines: &[&str]) -> TextBlock {
        TextBlock {
            kind: BlockKind::Paragraph,
            bbox: [0, 0, 0, 0],
            lines: lines
                .iter()
                .map(|t| TextLine {
                    text: t.to_string(),
                    bbox: [0, 0, 0, 0],
                    words: Vec::new(),
                })
                .collect(),
            indent_px: 0,
            spacing_before_px: 0,
            spacing_after_px: 0,
        }
    }

    #[test]
    fn paragraph_joins_lines_with_spaces() {
        let b = block(&["The quick brown", "fox jumps over"]);
        assert_eq!(b.paragraph_text(), "The quick brown fox jumps over");
    }

    #[test]
    fn paragraph_repairs_hyphenated_word_break() {
        let b = block(&["the walled to-", "wn was quiet"]);
        assert_eq!(b.paragraph_text(), "the walled town was quiet");
    }

    #[test]
    fn paragraph_keeps_trailing_hyphen_on_last_line() {
        let b = block(&["a range 10-"]);
        assert_eq!(b.paragraph_text(), "a range 10-");
    }

    #[test]
    fn paragraph_skips_blank_lines() {
        let b = block(&["alpha", "   ", "beta"]);
        assert_eq!(b.paragraph_text(), "alpha beta");
    }

    #[test]
    fn from_class_name_maps_headings() {
        assert_eq!(BlockKind::from_class_name("title"), BlockKind::Title);
        assert_eq!(
            BlockKind::from_class_name("paragraph_title"),
            BlockKind::SectionHeading
        );
        assert_eq!(
            BlockKind::from_class_name("plain_text"),
            BlockKind::Paragraph
        );
        assert_eq!(
            BlockKind::from_class_name("table_footnote"),
            BlockKind::Footnote
        );
    }
}
