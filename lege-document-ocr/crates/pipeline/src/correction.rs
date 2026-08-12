//! Conservative English OCR correction with raw-evidence preservation.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, BufReader};
use std::path::Path;

use lege_docir::{Correction, Document, RegionContent, TextEvidence};

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CorrectionMode {
    Disabled,
    Suggest,
    Conservative,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CorrectionSummary {
    pub examined: u64,
    pub suggested: u64,
    pub applied: u64,
}

#[derive(Debug, Clone)]
struct Entry {
    word: String,
    frequency: u64,
}

#[derive(Debug, Clone)]
pub struct EnglishCorrector {
    entries: Vec<Entry>,
    exact: HashSet<String>,
    deletes: HashMap<String, Vec<usize>>,
    allowlist: HashSet<String>,
    max_distance: usize,
    minimum_frequency: u64,
    minimum_margin_ratio: u64,
}

impl EnglishCorrector {
    /// Load `word<TAB>frequency` rows. Blank lines and `#` comments are ignored.
    pub fn from_frequency_file(path: &Path) -> Result<Self, CorrectionError> {
        Self::from_reader(BufReader::new(std::fs::File::open(path)?))
    }

    pub fn from_reader(reader: impl BufRead) -> Result<Self, CorrectionError> {
        let mut frequencies = HashMap::<String, u64>::new();
        for (line_index, line) in reader.lines().enumerate() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let mut fields = trimmed.split_whitespace();
            let word = fields.next().unwrap_or_default().to_lowercase();
            if word.len() < 2
                || !word
                    .chars()
                    .all(|character| character.is_alphabetic() || character == '\'')
            {
                return Err(CorrectionError::InvalidRow(line_index + 1));
            }
            let frequency = fields
                .next()
                .map(str::parse)
                .transpose()
                .map_err(|_| CorrectionError::InvalidRow(line_index + 1))?
                .unwrap_or(1);
            frequencies
                .entry(word)
                .and_modify(|current| *current = (*current).max(frequency))
                .or_insert(frequency);
        }
        if frequencies.is_empty() {
            return Err(CorrectionError::EmptyDictionary);
        }
        let mut entries = frequencies
            .into_iter()
            .map(|(word, frequency)| Entry { word, frequency })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.word.cmp(&right.word));
        let exact = entries.iter().map(|entry| entry.word.clone()).collect();
        let mut deletes = HashMap::<String, Vec<usize>>::new();
        for (index, entry) in entries.iter().enumerate() {
            for deletion in deletion_keys(&entry.word, 2) {
                deletes.entry(deletion).or_default().push(index);
            }
        }
        Ok(Self {
            entries,
            exact,
            deletes,
            allowlist: HashSet::new(),
            max_distance: 2,
            minimum_frequency: 2,
            minimum_margin_ratio: 4,
        })
    }

    pub fn add_allowlist<I, S>(&mut self, words: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.allowlist
            .extend(words.into_iter().map(|word| word.as_ref().to_lowercase()));
    }

    pub fn correct_document(
        &self,
        document: &mut Document,
        mode: CorrectionMode,
    ) -> CorrectionSummary {
        let mut summary = CorrectionSummary {
            examined: 0,
            suggested: 0,
            applied: 0,
        };
        if mode == CorrectionMode::Disabled {
            return summary;
        }
        for page in &mut document.pages {
            for region in &mut page.regions {
                match &mut region.content {
                    RegionContent::Text(block) => {
                        for line in &mut block.lines {
                            self.correct_evidence(&mut line.text, mode, &mut summary);
                            for word in &mut line.words {
                                self.correct_evidence(&mut word.text, mode, &mut summary);
                            }
                        }
                    }
                    RegionContent::Table(table) => {
                        for cell in &mut table.cells {
                            for block in &mut cell.blocks {
                                for line in &mut block.lines {
                                    self.correct_evidence(&mut line.text, mode, &mut summary);
                                    for word in &mut line.words {
                                        self.correct_evidence(&mut word.text, mode, &mut summary);
                                    }
                                }
                            }
                        }
                    }
                    RegionContent::Figure(figure) => {
                        if let Some(caption) = &mut figure.caption {
                            for line in &mut caption.lines {
                                self.correct_evidence(&mut line.text, mode, &mut summary);
                            }
                        }
                    }
                    RegionContent::Formula(formula) => {
                        if let Some(block) = &mut formula.raw_ocr {
                            for line in &mut block.lines {
                                self.correct_evidence(&mut line.text, mode, &mut summary);
                                for word in &mut line.words {
                                    self.correct_evidence(&mut word.text, mode, &mut summary);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        summary
    }

    fn correct_evidence(
        &self,
        evidence: &mut TextEvidence,
        mode: CorrectionMode,
        summary: &mut CorrectionSummary,
    ) {
        let source = evidence
            .normalized
            .clone()
            .unwrap_or_else(|| evidence.raw.clone());
        let mut output = String::with_capacity(source.len());
        let mut token = String::new();
        for character in source.chars().chain(std::iter::once(' ')) {
            if character.is_alphabetic() || character == '\'' {
                token.push(character);
                continue;
            }
            self.finish_token(&token, mode, evidence, &mut output, summary);
            token.clear();
            if character != ' ' || output.len() < source.len() {
                output.push(character);
            }
        }
        if output.ends_with(' ') && !source.ends_with(' ') {
            output.pop();
        }
        if output != source && mode == CorrectionMode::Conservative {
            evidence.corrected = Some(output);
        }
    }

    fn finish_token(
        &self,
        token: &str,
        mode: CorrectionMode,
        evidence: &mut TextEvidence,
        output: &mut String,
        summary: &mut CorrectionSummary,
    ) {
        if token.is_empty() {
            return;
        }
        summary.examined += 1;
        let lower = token.to_lowercase();
        if protected_token(token, &lower, &self.exact, &self.allowlist) {
            output.push_str(token);
            return;
        }
        let Some((replacement, margin)) = self.best_candidate(&lower) else {
            output.push_str(token);
            return;
        };
        let replacement = preserve_case(token, replacement);
        let applied = mode == CorrectionMode::Conservative;
        summary.suggested += 1;
        if applied {
            summary.applied += 1;
            output.push_str(&replacement);
        } else {
            output.push_str(token);
        }
        evidence.corrections.push(Correction {
            original: token.to_string(),
            replacement,
            applied,
            reason: "frequency-weighted OCR edit candidate".to_string(),
            score_margin_micros: Some(margin.min(u32::MAX as u64) as u32),
        });
    }

    fn best_candidate(&self, token: &str) -> Option<(&str, u64)> {
        let mut indices = HashSet::new();
        for key in deletion_keys(token, self.max_distance) {
            if let Some(values) = self.deletes.get(&key) {
                indices.extend(values.iter().copied());
            }
        }
        let mut candidates = indices
            .into_iter()
            .filter_map(|index| {
                let entry = &self.entries[index];
                let distance = edit_distance(token, &entry.word);
                (distance > 0
                    && distance <= self.max_distance
                    && entry.frequency >= self.minimum_frequency)
                    .then_some((index, distance, entry.frequency / distance as u64))
            })
            .collect::<Vec<_>>();
        // Frequency is the language-model prior.  Distance is already part of
        // the score, so sorting by distance first would make a rare one-edit
        // word beat an overwhelmingly common two-edit OCR correction (for
        // example `teh` -> `ten` instead of `the`).
        candidates.sort_by(|left, right| {
            right
                .2
                .cmp(&left.2)
                .then_with(|| left.1.cmp(&right.1))
                .then_with(|| self.entries[left.0].word.cmp(&self.entries[right.0].word))
        });
        let (best_index, _, best_score) = *candidates.first()?;
        let runner_score = candidates
            .iter()
            .skip(1)
            .map(|(_, _, score)| *score)
            .max()
            .unwrap_or(1);
        if best_score < runner_score.saturating_mul(self.minimum_margin_ratio) {
            return None;
        }
        Some((
            &self.entries[best_index].word,
            best_score
                .saturating_sub(runner_score)
                .saturating_mul(1_000_000)
                / best_score.max(1),
        ))
    }
}

fn protected_token(
    token: &str,
    lower: &str,
    exact: &HashSet<String>,
    allowlist: &HashSet<String>,
) -> bool {
    token.chars().count() < 3
        || exact.contains(lower)
        || allowlist.contains(lower)
        || token.chars().next().is_some_and(char::is_uppercase)
        || token.chars().skip(1).any(char::is_uppercase)
}
fn preserve_case(original: &str, replacement: &str) -> String {
    if original.chars().all(char::is_uppercase) {
        replacement.to_uppercase()
    } else {
        replacement.to_string()
    }
}
fn deletion_keys(word: &str, distance: usize) -> HashSet<String> {
    let mut seen = HashSet::from([word.to_string()]);
    let mut frontier = vec![word.to_string()];
    for _ in 0..distance {
        let mut next = Vec::new();
        for value in frontier {
            let chars = value.chars().collect::<Vec<_>>();
            for index in 0..chars.len() {
                let deletion = chars
                    .iter()
                    .enumerate()
                    .filter_map(|(current, character)| (current != index).then_some(*character))
                    .collect::<String>();
                if seen.insert(deletion.clone()) {
                    next.push(deletion);
                }
            }
        }
        frontier = next;
    }
    seen
}
fn edit_distance(left: &str, right: &str) -> usize {
    let right = right.chars().collect::<Vec<_>>();
    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    for (left_index, left_character) in left.chars().enumerate() {
        let mut current = vec![left_index + 1; right.len() + 1];
        for (right_index, right_character) in right.iter().enumerate() {
            current[right_index + 1] = (current[right_index] + 1)
                .min(previous[right_index + 1] + 1)
                .min(previous[right_index] + usize::from(left_character != *right_character));
        }
        previous = current;
    }
    previous[right.len()]
}

#[derive(Debug, thiserror::Error)]
pub enum CorrectionError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid dictionary row {0}")]
    InvalidRow(usize),
    #[error("dictionary is empty")]
    EmptyDictionary,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn corrects_only_decisive_lowercase_candidates() {
        let corrector =
            EnglishCorrector::from_reader("the\t1000000\nten\t10\nlege\t100\n".as_bytes()).unwrap();
        let mut evidence = TextEvidence::raw("teh Lege");
        let mut summary = CorrectionSummary {
            examined: 0,
            suggested: 0,
            applied: 0,
        };
        corrector.correct_evidence(&mut evidence, CorrectionMode::Conservative, &mut summary);
        assert_eq!(evidence.corrected.as_deref(), Some("the Lege"));
        assert_eq!(summary.applied, 1);
        assert_eq!(evidence.raw, "teh Lege");
    }
}
