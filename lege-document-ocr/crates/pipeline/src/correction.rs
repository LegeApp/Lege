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
    /// Apply exact-letter spacing repairs; retain spelling edits as suggestions.
    Conservative,
    /// Apply both spacing repairs and frequency-weighted spelling edits.
    Aggressive,
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
    frequencies: HashMap<String, u64>,
    deletes: HashMap<String, Vec<usize>>,
    allowlist: HashSet<String>,
    maximum_frequency: u64,
    max_distance: usize,
    minimum_frequency: u64,
    minimum_split_frequency: u64,
    minimum_margin_ratio: u64,
}

impl EnglishCorrector {
    /// Load `word frequency` rows. Blank lines and `#` comments are ignored.
    /// Spaces or tabs may separate fields, and a UTF-8 BOM is accepted.
    pub fn from_frequency_file(path: &Path) -> Result<Self, CorrectionError> {
        Self::from_reader(BufReader::new(std::fs::File::open(path)?))
    }

    pub fn from_frequency_file_for_mode(
        path: &Path,
        mode: CorrectionMode,
    ) -> Result<Self, CorrectionError> {
        Self::from_reader_with_spelling(
            BufReader::new(std::fs::File::open(path)?),
            matches!(mode, CorrectionMode::Suggest | CorrectionMode::Aggressive),
        )
    }

    pub fn from_reader(reader: impl BufRead) -> Result<Self, CorrectionError> {
        Self::from_reader_with_spelling(reader, true)
    }

    fn from_reader_with_spelling(
        reader: impl BufRead,
        build_spelling_index: bool,
    ) -> Result<Self, CorrectionError> {
        let mut frequencies = HashMap::<String, u64>::new();
        for (line_index, line) in reader.lines().enumerate() {
            let line = line?;
            let trimmed = line.trim().trim_start_matches('\u{feff}');
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let mut fields = trimmed.split_whitespace();
            let word = fields.next().unwrap_or_default().to_lowercase();
            if word.is_empty()
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
        let frequencies = entries
            .iter()
            .map(|entry| (entry.word.clone(), entry.frequency))
            .collect::<HashMap<_, _>>();
        let exact = frequencies.keys().cloned().collect();
        let maximum_frequency = frequencies.values().copied().max().unwrap_or(1);
        let mut deletes = HashMap::<String, Vec<usize>>::new();
        if build_spelling_index {
            for (index, entry) in entries.iter().enumerate() {
                for deletion in deletion_keys(&entry.word, 2) {
                    deletes.entry(deletion).or_default().push(index);
                }
            }
        }
        Ok(Self {
            entries,
            exact,
            frequencies,
            deletes,
            allowlist: HashSet::new(),
            maximum_frequency,
            max_distance: 2,
            minimum_frequency: 2,
            minimum_split_frequency: 10,
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
        if output != source
            && matches!(
                mode,
                CorrectionMode::Conservative | CorrectionMode::Aggressive
            )
        {
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
        if token.chars().count() < 3
            || self.exact.contains(&lower)
            || self.allowlist.contains(&lower)
        {
            output.push_str(token);
            return;
        }
        if let Some((replacement, margin)) = self.best_split_candidate(token) {
            let applied = matches!(
                mode,
                CorrectionMode::Conservative | CorrectionMode::Aggressive
            );
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
                reason: "frequency-weighted missing-space segmentation".to_string(),
                score_margin_micros: Some(margin),
            });
            return;
        }
        if protected_token(token, &lower, &self.exact, &self.allowlist) {
            output.push_str(token);
            return;
        }
        if self.deletes.is_empty() {
            output.push_str(token);
            return;
        }
        let Some((replacement, margin)) = self.best_candidate(&lower) else {
            output.push_str(token);
            return;
        };
        let replacement = preserve_case(token, replacement);
        let applied = mode == CorrectionMode::Aggressive;
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

    /// Find one exact two-word segmentation without changing any letters.
    /// Existing dictionary words, proper-name-like title case, acronyms, and
    /// ambiguous segmentations are rejected before this is called or here.
    fn best_split_candidate(&self, token: &str) -> Option<(String, u32)> {
        if token.contains('\'') {
            return None;
        }
        let characters = token.chars().collect::<Vec<_>>();
        if characters.len() < 6 {
            return None;
        }

        let internal_uppercase = characters
            .iter()
            .enumerate()
            .skip(1)
            .filter_map(|(index, character)| character.is_uppercase().then_some(index))
            .collect::<Vec<_>>();
        let lowercase_token = internal_uppercase.is_empty()
            && characters.iter().all(|character| !character.is_uppercase());
        let camel_case_boundary = match internal_uppercase.as_slice() {
            [boundary] => Some(*boundary),
            [] if lowercase_token => None,
            _ => return None,
        };
        if lowercase_token && has_known_derived_base(&token.to_lowercase(), &self.exact) {
            return None;
        }

        let mut candidates = Vec::<(String, u128)>::new();
        for index in 1..characters.len() {
            if camel_case_boundary.is_some_and(|boundary| boundary != index) {
                continue;
            }
            let left_source = characters[..index].iter().collect::<String>();
            let right_source = characters[index..].iter().collect::<String>();
            let left = left_source.to_lowercase();
            let right = right_source.to_lowercase();
            let Some(left_frequency) = self.frequencies.get(&left).copied() else {
                continue;
            };
            let Some(right_frequency) = self.frequencies.get(&right).copied() else {
                continue;
            };
            if left_frequency < self.minimum_split_frequency
                || right_frequency < self.minimum_split_frequency
            {
                continue;
            }
            if !plausible_split_word(&left, left_frequency, self.maximum_frequency)
                || !plausible_split_word(&right, right_frequency, self.maximum_frequency)
            {
                continue;
            }
            if lowercase_token && productive_compound_prefix(&left) {
                continue;
            }
            candidates.push((
                format!("{left_source} {right_source}"),
                u128::from(left_frequency) * u128::from(right_frequency),
            ));
        }
        candidates.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        let (replacement, best_score) = candidates.first()?;
        let runner_score = candidates.get(1).map(|candidate| candidate.1).unwrap_or(0);
        if runner_score > 0
            && *best_score < runner_score.saturating_mul(u128::from(self.minimum_margin_ratio))
        {
            return None;
        }
        let margin = if runner_score == 0 {
            1_000_000
        } else {
            best_score
                .saturating_sub(runner_score)
                .saturating_mul(1_000_000)
                .checked_div(*best_score)
                .unwrap_or(0)
                .min(u128::from(u32::MAX)) as u32
        };
        Some((replacement.clone(), margin))
    }
}

fn plausible_split_word(word: &str, frequency: u64, maximum_frequency: u64) -> bool {
    const CLOSED_CLASS_SHORT_WORDS: &[&str] = &[
        "a", "i", "am", "an", "as", "at", "be", "by", "do", "go", "he", "if", "in", "is", "it",
        "me", "my", "no", "of", "oh", "on", "or", "so", "to", "up", "us", "we",
    ];
    match word.chars().count() {
        0 => false,
        1 | 2 => CLOSED_CLASS_SHORT_WORDS.contains(&word),
        3 => frequency >= maximum_frequency / 200,
        _ => true,
    }
}

fn has_known_derived_base(word: &str, exact: &HashSet<String>) -> bool {
    const DERIVATIONAL_SUFFIXES: &[&str] = &[
        "tion", "sion", "ness", "ment", "ity", "ism", "ous", "ive", "ful", "less",
    ];
    DERIVATIONAL_SUFFIXES
        .iter()
        .any(|suffix| word.ends_with(suffix))
        || word
            .strip_suffix("iness")
            .map(|stem| format!("{stem}y"))
            .is_some_and(|base| exact.contains(&base))
        || word
            .strip_suffix("ness")
            .is_some_and(|base| exact.contains(base))
}

fn productive_compound_prefix(word: &str) -> bool {
    matches!(word, "over" | "under")
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
    fn aggressive_mode_applies_only_decisive_lowercase_candidates() {
        let corrector =
            EnglishCorrector::from_reader("the\t1000000\nten\t10\nlege\t100\n".as_bytes()).unwrap();
        let mut evidence = TextEvidence::raw("teh Lege");
        let mut summary = CorrectionSummary {
            examined: 0,
            suggested: 0,
            applied: 0,
        };
        corrector.correct_evidence(&mut evidence, CorrectionMode::Aggressive, &mut summary);
        assert_eq!(evidence.corrected.as_deref(), Some("the Lege"));
        assert_eq!(summary.applied, 1);
        assert_eq!(evidence.raw, "teh Lege");
    }

    #[test]
    fn conservative_mode_only_suggests_spelling_edits() {
        let corrector = EnglishCorrector::from_reader("the 1000000\nten 10\n".as_bytes()).unwrap();
        let mut evidence = TextEvidence::raw("teh");
        let mut summary = CorrectionSummary {
            examined: 0,
            suggested: 0,
            applied: 0,
        };

        corrector.correct_evidence(&mut evidence, CorrectionMode::Conservative, &mut summary);

        assert_eq!(evidence.corrected, None);
        assert_eq!(summary.suggested, 1);
        assert_eq!(summary.applied, 0);
        assert_eq!(evidence.corrections[0].replacement, "the");
        assert!(!evidence.corrections[0].applied);
    }

    #[test]
    fn conservative_index_can_omit_spelling_candidates() {
        let corrector =
            EnglishCorrector::from_reader_with_spelling("the 1000000\nten 10\n".as_bytes(), false)
                .unwrap();
        let mut evidence = TextEvidence::raw("teh");
        let mut summary = CorrectionSummary {
            examined: 0,
            suggested: 0,
            applied: 0,
        };

        corrector.correct_evidence(&mut evidence, CorrectionMode::Conservative, &mut summary);

        assert!(corrector.deletes.is_empty());
        assert!(evidence.corrections.is_empty());
        assert_eq!(evidence.corrected, None);
    }

    #[test]
    fn accepts_utf8_bom_and_space_separated_frequencies() {
        let corrector =
            EnglishCorrector::from_reader("\u{feff}the 1000000\na 900000\nten 10\n".as_bytes())
                .unwrap();

        assert!(corrector.exact.contains("the"));
        assert_eq!(corrector.frequencies.get("the"), Some(&1_000_000));
        assert_eq!(corrector.frequencies.get("a"), Some(&900_000));
    }

    #[test]
    fn inserts_only_missing_spaces_for_decisive_compounds() {
        let corrector = EnglishCorrector::from_reader(
            "much\t10000\ngreater\t9000\nrelate\t8000\ngeneral\t7000\nsocial\t6000\njustice\t5000\n"
                .as_bytes(),
        )
        .unwrap();
        let mut evidence = TextEvidence::raw("muchgreater relategeneral SocialJustice");
        let mut summary = CorrectionSummary {
            examined: 0,
            suggested: 0,
            applied: 0,
        };

        corrector.correct_evidence(&mut evidence, CorrectionMode::Conservative, &mut summary);

        assert_eq!(
            evidence.corrected.as_deref(),
            Some("much greater relate general Social Justice")
        );
        assert_eq!(summary.applied, 3);
        assert_eq!(evidence.corrections.len(), 3);
        assert!(evidence.corrections.iter().all(|correction| {
            correction.reason == "frequency-weighted missing-space segmentation"
        }));
        assert_eq!(evidence.raw, "muchgreater relategeneral SocialJustice");
    }

    #[test]
    fn preserves_known_words_and_rejects_ambiguous_splits() {
        let corrector = EnglishCorrector::from_reader(
            "choices\t1000\nch\t1000\noices\t1000\nthere\t100\nin\t100\nthe\t100\nrein\t100\n"
                .as_bytes(),
        )
        .unwrap();
        assert!(corrector.best_split_candidate("therein").is_none());

        let mut evidence = TextEvidence::raw("choices");
        let mut summary = CorrectionSummary {
            examined: 0,
            suggested: 0,
            applied: 0,
        };

        corrector.correct_evidence(&mut evidence, CorrectionMode::Conservative, &mut summary);

        assert_eq!(evidence.corrected, None);
        assert_eq!(summary.applied, 0);
        assert!(evidence.corrections.is_empty());
    }

    #[test]
    fn rejects_short_fragments_and_plausible_unsplit_derived_words() {
        let corrector = EnglishCorrector::from_reader(
            "the 1000000\ntat 100\nion 100\ncom 100\neth 100\nlati 10000\non 100000\nother 50000\nworldliness 1000\notherworldly 500\nover 50000\ngarment 1000\n"
                .as_bytes(),
        )
        .unwrap();

        assert!(corrector.best_split_candidate("tation").is_none());
        assert!(corrector.best_split_candidate("lation").is_none());
        assert!(corrector.best_split_candidate("cometh").is_none());
        assert!(corrector.best_split_candidate("otherworldliness").is_none());
        assert!(corrector.best_split_candidate("overgarment").is_none());
    }

    #[test]
    fn accepts_very_common_three_letter_split_words() {
        let corrector =
            EnglishCorrector::from_reader("the 1000000\nall 900000\ngross 1000\n".as_bytes())
                .unwrap();

        assert_eq!(
            corrector
                .best_split_candidate("allgross")
                .map(|candidate| candidate.0),
            Some("all gross".to_string())
        );
    }

    #[test]
    fn suggest_mode_records_but_does_not_apply_missing_space() {
        let corrector =
            EnglishCorrector::from_reader("much\t100\ngreater\t100\n".as_bytes()).unwrap();
        let mut evidence = TextEvidence::raw("muchgreater");
        let mut summary = CorrectionSummary {
            examined: 0,
            suggested: 0,
            applied: 0,
        };

        corrector.correct_evidence(&mut evidence, CorrectionMode::Suggest, &mut summary);

        assert_eq!(evidence.corrected, None);
        assert_eq!(summary.suggested, 1);
        assert_eq!(summary.applied, 0);
        assert_eq!(evidence.corrections[0].replacement, "much greater");
        assert!(!evidence.corrections[0].applied);
    }
}
