//! CTC greedy decoding for PP-OCR text recognition.
//!
//! The recognition head emits per-timestep logits `[1, T, C]` over a character
//! set. Greedy CTC: take the argmax at each timestep, collapse runs of the same
//! index, and drop the blank (index 0). The character set follows the PP-OCR
//! convention: index 0 is the CTC blank, indices `1..=n` are the dictionary
//! lines in order, and the final index is a space.

use crate::vision::reference::Tensor;
use std::ops::Range;

/// Character set for a CTC recognizer.
pub(crate) struct CtcDict {
    text: String,
    entries: Vec<Range<usize>>,
}

impl CtcDict {
    /// Build from raw dictionary text (one character per line), adding the CTC
    /// blank at index 0 and a trailing space (PP-OCRv5 layout: for an N-line
    /// dictionary the head has N+2 classes).
    pub(crate) fn from_dict_text(text: &str) -> Self {
        // Keep one owned dictionary buffer plus byte ranges instead of allocating
        // a separate String for each of the 18k+ glyphs.
        let text = text.to_owned();
        let base = text.as_ptr() as usize;
        let entries = text
            .lines()
            .map(|line| {
                let start = line.as_ptr() as usize - base;
                start..start + line.len()
            })
            .collect();
        Self { text, entries }
    }

    /// Number of classes (dictionary lines + blank + space).
    pub(crate) fn num_classes(&self) -> usize {
        self.entries.len() + 2
    }

    pub(crate) fn char_at(&self, index: usize) -> &str {
        if index == 0 {
            ""
        } else if let Some(range) = self.entries.get(index - 1) {
            &self.text[range.clone()]
        } else if index == self.entries.len() + 1 {
            " "
        } else {
            ""
        }
    }
}

/// One decoded character and the timestep at which it was emitted.
pub(crate) struct CharSpan {
    pub(crate) class_index: usize,
    /// Timestep index (0..T) of the argmax that produced this character.
    pub(crate) timestep: usize,
    /// Recognition confidence derived from the emitted class probability or,
    /// for raw logits, the top-two margin.
    pub(crate) confidence: f32,
}

fn for_each_emission(
    logits: &Tensor,
    dict: &CtcDict,
    mut emit: impl FnMut(usize, usize, f32),
) -> usize {
    let (timesteps, classes) = match logits.shape.as_slice() {
        [1, t, c] => (*t, *c),
        [t, c] => (*t, *c),
        _ => return 0,
    };
    if classes != dict.num_classes() || timesteps.checked_mul(classes) != Some(logits.data.len()) {
        return 0;
    }

    let mut prev = usize::MAX;
    for t in 0..timesteps {
        let base = t * classes;
        let mut best = 0usize;
        let mut best_val = f32::NEG_INFINITY;
        let mut second_val = f32::NEG_INFINITY;
        for c in 0..classes {
            let value = logits.data[base + c];
            if value > best_val {
                second_val = best_val;
                best_val = value;
                best = c;
            } else if value > second_val {
                second_val = value;
            }
        }
        if best != prev && best != 0 {
            let confidence = if (0.0..=1.0).contains(&best_val) {
                best_val
            } else {
                1.0 / (1.0 + (second_val - best_val).exp())
            };
            emit(best, t, confidence.clamp(0.0, 1.0));
        }
        prev = best;
    }
    timesteps
}

/// Greedy CTC decode returning per-character spans plus the total timestep count
/// `T`. The caller maps `timestep/T` to a horizontal position to reconstruct
/// word boxes. Same collapse rule as [`ctc_greedy_decode`].
pub(crate) fn ctc_greedy_decode_spans(logits: &Tensor, dict: &CtcDict) -> (Vec<CharSpan>, usize) {
    let mut spans = Vec::new();
    let timesteps = for_each_emission(logits, dict, |class_index, timestep, confidence| {
        spans.push(CharSpan {
            class_index,
            timestep,
            confidence,
        });
    });
    (spans, timesteps)
}

/// Greedy CTC decode of recognition logits shaped `[1, T, C]` or `[T, C]`.
pub(crate) fn ctc_greedy_decode(logits: &Tensor, dict: &CtcDict) -> String {
    let mut out = String::new();
    for_each_emission(logits, dict, |class_index, _, _| {
        out.push_str(dict.char_at(class_index));
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logits(indices: &[usize], classes: usize) -> Tensor {
        let mut data = vec![-1.0; indices.len() * classes];
        for (timestep, &class) in indices.iter().enumerate() {
            data[timestep * classes + class] = 1.0;
        }
        Tensor::new(vec![1, indices.len(), classes], data).unwrap()
    }

    #[test]
    fn greedy_decode_collapses_repeats_but_not_across_blank() {
        let dict = CtcDict::from_dict_text("a\nb\n");
        let output = logits(&[1, 1, 0, 1, 3, 2], dict.num_classes());
        assert_eq!(ctc_greedy_decode(&output, &dict), "aa b");

        let (spans, timesteps) = ctc_greedy_decode_spans(&output, &dict);
        assert_eq!(timesteps, 6);
        assert_eq!(
            spans
                .iter()
                .map(|span| (dict.char_at(span.class_index), span.timestep))
                .collect::<Vec<_>>(),
            vec![("a", 0), ("a", 3), (" ", 4), ("b", 5)]
        );
    }

    #[test]
    fn mismatched_dictionary_is_rejected_without_indexing_past_logits() {
        let dict = CtcDict::from_dict_text("a\n");
        let output = logits(&[1], 4);
        assert!(ctc_greedy_decode(&output, &dict).is_empty());
    }
}
