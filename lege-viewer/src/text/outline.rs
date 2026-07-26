use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use crate::document::{OutlineNode, OutlineSource, PageIndex};

use super::TextSubstrate;

#[derive(Debug, Clone)]
struct HeadingCandidate {
    page: PageIndex,
    title: Arc<str>,
    font_size: f64,
    bold_fraction: f64,
}

#[derive(Debug, Clone, Default)]
pub struct OutlineSynthesizer {
    pages: BTreeMap<PageIndex, Vec<HeadingCandidate>>,
    body_samples: Vec<f64>,
}

impl OutlineSynthesizer {
    pub fn insert(&mut self, page: PageIndex, substrate: &TextSubstrate) {
        let mut candidates = Vec::new();
        for line in substrate.lines.lines.iter() {
            let start = line.char_range.0.min(substrate.utf16.len());
            let end = line.char_range.1.min(substrate.utf16.len());
            if start >= end {
                continue;
            }
            let title = normalize_title(&String::from_utf16_lossy(&substrate.utf16[start..end]));
            if title.is_empty() || title.chars().count() > 120 {
                continue;
            }
            let characters: Vec<_> = substrate
                .characters
                .iter()
                .filter(|character| (start..end).contains(&character.char_index))
                .collect();
            if characters.is_empty() {
                continue;
            }
            let font_size = median(
                characters
                    .iter()
                    .map(|character| character.font_size.max(1.0))
                    .collect(),
            )
            .unwrap_or(line.bounds.height.max(1.0));
            self.body_samples.push(font_size);
            let bold_fraction = characters.iter().filter(|character| character.bold).count() as f64
                / characters.len() as f64;
            if plausible_title(&title) {
                candidates.push(HeadingCandidate {
                    page,
                    title: Arc::from(title),
                    font_size,
                    bold_fraction,
                });
            }
        }
        self.pages.insert(page, candidates);
    }

    pub fn finish(&self, page_count: u32) -> Arc<[OutlineNode]> {
        let body = median(self.body_samples.clone()).unwrap_or(12.0).max(1.0);
        let mut repetitions: HashMap<String, HashSet<PageIndex>> = HashMap::new();
        for candidate in self.pages.values().flatten() {
            repetitions
                .entry(repetition_key(&candidate.title))
                .or_default()
                .insert(candidate.page);
        }
        let mut accepted: Vec<&HeadingCandidate> = self
            .pages
            .values()
            .flatten()
            .filter(|candidate| {
                let ratio = candidate.font_size / body;
                (ratio >= 1.30 || (ratio >= 1.15 && candidate.bold_fraction >= 0.5))
                    && repetitions
                        .get(&repetition_key(&candidate.title))
                        .is_none_or(|pages| pages.len() <= 2)
            })
            .collect();
        accepted.sort_by_key(|candidate| candidate.page);
        let outline: Vec<OutlineNode> = accepted
            .into_iter()
            .map(|candidate| {
                let ratio = candidate.font_size / body;
                let depth = if ratio >= 1.8 {
                    0
                } else if ratio >= 1.45 {
                    1
                } else {
                    2
                };
                OutlineNode {
                    title: Arc::clone(&candidate.title),
                    page: candidate.page,
                    target_region: None,
                    depth,
                    source: OutlineSource::Synthesized,
                }
            })
            .collect();
        if !outline.is_empty() {
            return outline.into();
        }
        (0..page_count)
            .map(|page| OutlineNode {
                title: Arc::from(format!("Page {}", page + 1)),
                page: PageIndex(page),
                target_region: None,
                depth: 0,
                source: OutlineSource::Synthesized,
            })
            .collect::<Vec<_>>()
            .into()
    }
}

fn normalize_title(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn plausible_title(title: &str) -> bool {
    let trimmed = title.trim();
    if trimmed.chars().count() < 2 || trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        return false;
    }
    if trimmed.chars().count() > 72 && trimmed.ends_with(['.', ',', ';', ':', '!', '?']) {
        return false;
    }
    trimmed.chars().any(char::is_alphabetic)
}

fn repetition_key(title: &str) -> String {
    title
        .chars()
        .filter(|character| !character.is_ascii_digit())
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|left, right| left.total_cmp(right));
    Some(values[values.len() / 2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_list_is_the_no_text_fallback() {
        let outline = OutlineSynthesizer::default().finish(3);
        assert_eq!(outline.len(), 3);
        assert_eq!(&*outline[1].title, "Page 2");
    }

    #[test]
    fn repetition_key_ignores_page_numbers() {
        assert_eq!(repetition_key("Chapter 12"), repetition_key("Chapter 13"));
    }
}
