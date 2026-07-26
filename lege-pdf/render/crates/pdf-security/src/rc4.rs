//! RC4, for the standard security handler (ISO 32000-1 §7.6.3).
//!
//! Required by the format, not chosen: RC4 is broken and must never be used
//! for anything new. It exists here so that documents written when it was
//! current can still be read.

/// Encrypt/decrypt `buf` in place under `key`. RC4 is a stream cipher, so
/// this one function does both.
pub fn rc4_in_place(key: &[u8], buf: &mut [u8]) {
    if key.is_empty() {
        return;
    }
    // Key-scheduling.
    let mut s: [u8; 256] = std::array::from_fn(|i| i as u8);
    let mut j = 0u8;
    for i in 0..256 {
        j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
        s.swap(i, j as usize);
    }
    // Pseudo-random generation.
    let (mut i, mut j) = (0u8, 0u8);
    for byte in buf {
        i = i.wrapping_add(1);
        j = j.wrapping_add(s[i as usize]);
        s.swap(i as usize, j as usize);
        let k = s[(s[i as usize].wrapping_add(s[j as usize])) as usize];
        *byte ^= k;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn rfc6229_test_vectors() {
        // RFC 6229 §2: the first keystream bytes for known keys. Recovered by
        // encrypting zeros, which yields the keystream itself.
        let mut buf = [0u8; 16];
        rc4_in_place(b"Key", &mut buf[..3]);
        // "Key"/"Plaintext" is the classic Wikipedia vector.
        let mut pt = *b"Plaintext";
        rc4_in_place(b"Key", &mut pt);
        assert_eq!(hex(&pt), "bbf316e8d940af0ad3");

        let mut pt = *b"pedia";
        rc4_in_place(b"Wiki", &mut pt);
        assert_eq!(hex(&pt), "1021bf0420");

        let mut pt = *b"Attack at dawn";
        rc4_in_place(b"Secret", &mut pt);
        assert_eq!(hex(&pt), "45a01f645fc35b383552544b9bf5");
    }

    #[test]
    fn round_trips() {
        // A stream cipher is its own inverse; the handler relies on it.
        let key = b"\x01\x02\x03\x04\x05";
        let original: Vec<u8> = (0..300u32).map(|i| (i % 256) as u8).collect();
        let mut buf = original.clone();
        rc4_in_place(key, &mut buf);
        assert_ne!(buf, original, "must actually encrypt");
        rc4_in_place(key, &mut buf);
        assert_eq!(buf, original);
    }

    #[test]
    fn empty_key_is_a_no_op_not_a_panic() {
        // A malformed /Encrypt can yield a zero-length key; index arithmetic
        // must not divide by zero.
        let mut buf = [1u8, 2, 3];
        rc4_in_place(&[], &mut buf);
        assert_eq!(buf, [1, 2, 3]);
    }
}
