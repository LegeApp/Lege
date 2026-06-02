use crate::jbig2sym::BitImage;

/// Estimated decoder-side storage for a single JBIG2 dictionary symbol entry.
///
/// This follows the MSD accounting used in Figuera et al.:
/// 4 bytes of per-symbol overhead plus the bitmap rounded up to 32-bit words.
#[inline]
pub fn symbol_dictionary_entry_bytes(symbol: &BitImage) -> usize {
    let bits = symbol.width.saturating_mul(symbol.height);
    let words32 = bits.div_ceil(32);
    4 + words32.saturating_mul(4)
}

#[inline]
pub fn symbol_dictionary_entries_bytes<'a, I>(symbols: I) -> usize
where
    I: IntoIterator<Item = &'a BitImage>,
{
    symbols.into_iter().map(symbol_dictionary_entry_bytes).sum()
}

#[cfg(test)]
mod tests {
    use super::{symbol_dictionary_entries_bytes, symbol_dictionary_entry_bytes};
    use crate::jbig2sym::BitImage;

    #[test]
    fn entry_bytes_round_to_32_bit_words() {
        let one = BitImage::new(1, 1).expect("1x1");
        let thirty_two = BitImage::new(32, 1).expect("32x1");
        let thirty_three = BitImage::new(33, 1).expect("33x1");
        let thirty_nine = BitImage::new(3, 13).expect("3x13");

        assert_eq!(symbol_dictionary_entry_bytes(&one), 8);
        assert_eq!(symbol_dictionary_entry_bytes(&thirty_two), 8);
        assert_eq!(symbol_dictionary_entry_bytes(&thirty_three), 12);
        assert_eq!(symbol_dictionary_entry_bytes(&thirty_nine), 12);
    }

    #[test]
    fn total_bytes_sum_entries() {
        let first = BitImage::new(10, 10).expect("10x10");
        let second = BitImage::new(20, 2).expect("20x2");
        let symbols = [&first, &second];

        assert_eq!(
            symbol_dictionary_entries_bytes(symbols),
            symbol_dictionary_entry_bytes(&first) + symbol_dictionary_entry_bytes(&second)
        );
    }
}
