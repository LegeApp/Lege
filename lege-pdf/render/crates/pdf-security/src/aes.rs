//! AES in CBC mode, FIPS-197 block cipher, for the PDF standard security
//! handler's crypt filters (ISO 32000-1 §7.6.2, ISO 32000-2 §7.6.4).
//!
//! - AES-128 / AES-256 CBC decrypt with an IV prefix and PKCS#7 padding, for
//!   AESV2 (128) and AESV3 (256) string/stream content.
//! - AES-256 CBC decrypt with an explicit IV and no padding, for the 32-byte
//!   `/UE` / `/OE` key blobs (decrypted under a zero IV).
//! - AES-128 CBC *encrypt* with an explicit IV and no padding, the forward
//!   direction the revision-6 key derivation ("algorithm 2.B") relies on.
//!
//! Not constant-time: this recovers content the reader is authorised to see
//! from a key derived from public document data, so there is no secret for
//! timing to leak. Do not lift this for anything security-bearing.

/// AES S-box inverse (FIPS-197 figure 14).
const INV_SBOX: [u8; 256] = [
    0x52, 0x09, 0x6a, 0xd5, 0x30, 0x36, 0xa5, 0x38, 0xbf, 0x40, 0xa3, 0x9e, 0x81, 0xf3, 0xd7, 0xfb,
    0x7c, 0xe3, 0x39, 0x82, 0x9b, 0x2f, 0xff, 0x87, 0x34, 0x8e, 0x43, 0x44, 0xc4, 0xde, 0xe9, 0xcb,
    0x54, 0x7b, 0x94, 0x32, 0xa6, 0xc2, 0x23, 0x3d, 0xee, 0x4c, 0x95, 0x0b, 0x42, 0xfa, 0xc3, 0x4e,
    0x08, 0x2e, 0xa1, 0x66, 0x28, 0xd9, 0x24, 0xb2, 0x76, 0x5b, 0xa2, 0x49, 0x6d, 0x8b, 0xd1, 0x25,
    0x72, 0xf8, 0xf6, 0x64, 0x86, 0x68, 0x98, 0x16, 0xd4, 0xa4, 0x5c, 0xcc, 0x5d, 0x65, 0xb6, 0x92,
    0x6c, 0x70, 0x48, 0x50, 0xfd, 0xed, 0xb9, 0xda, 0x5e, 0x15, 0x46, 0x57, 0xa7, 0x8d, 0x9d, 0x84,
    0x90, 0xd8, 0xab, 0x00, 0x8c, 0xbc, 0xd3, 0x0a, 0xf7, 0xe4, 0x58, 0x05, 0xb8, 0xb3, 0x45, 0x06,
    0xd0, 0x2c, 0x1e, 0x8f, 0xca, 0x3f, 0x0f, 0x02, 0xc1, 0xaf, 0xbd, 0x03, 0x01, 0x13, 0x8a, 0x6b,
    0x3a, 0x91, 0x11, 0x41, 0x4f, 0x67, 0xdc, 0xea, 0x97, 0xf2, 0xcf, 0xce, 0xf0, 0xb4, 0xe6, 0x73,
    0x96, 0xac, 0x74, 0x22, 0xe7, 0xad, 0x35, 0x85, 0xe2, 0xf9, 0x37, 0xe8, 0x1c, 0x75, 0xdf, 0x6e,
    0x47, 0xf1, 0x1a, 0x71, 0x1d, 0x29, 0xc5, 0x89, 0x6f, 0xb7, 0x62, 0x0e, 0xaa, 0x18, 0xbe, 0x1b,
    0xfc, 0x56, 0x3e, 0x4b, 0xc6, 0xd2, 0x79, 0x20, 0x9a, 0xdb, 0xc0, 0xfe, 0x78, 0xcd, 0x5a, 0xf4,
    0x1f, 0xdd, 0xa8, 0x33, 0x88, 0x07, 0xc7, 0x31, 0xb1, 0x12, 0x10, 0x59, 0x27, 0x80, 0xec, 0x5f,
    0x60, 0x51, 0x7f, 0xa9, 0x19, 0xb5, 0x4a, 0x0d, 0x2d, 0xe5, 0x7a, 0x9f, 0x93, 0xc9, 0x9c, 0xef,
    0xa0, 0xe0, 0x3b, 0x4d, 0xae, 0x2a, 0xf5, 0xb0, 0xc8, 0xeb, 0xbb, 0x3c, 0x83, 0x53, 0x99, 0x61,
    0x17, 0x2b, 0x04, 0x7e, 0xba, 0x77, 0xd6, 0x26, 0xe1, 0x69, 0x14, 0x63, 0x55, 0x21, 0x0c, 0x7d,
];

/// AES S-box (forward) — needed only for the key expansion.
const SBOX: [u8; 256] = [
    0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5, 0x30, 0x01, 0x67, 0x2b, 0xfe, 0xd7, 0xab, 0x76,
    0xca, 0x82, 0xc9, 0x7d, 0xfa, 0x59, 0x47, 0xf0, 0xad, 0xd4, 0xa2, 0xaf, 0x9c, 0xa4, 0x72, 0xc0,
    0xb7, 0xfd, 0x93, 0x26, 0x36, 0x3f, 0xf7, 0xcc, 0x34, 0xa5, 0xe5, 0xf1, 0x71, 0xd8, 0x31, 0x15,
    0x04, 0xc7, 0x23, 0xc3, 0x18, 0x96, 0x05, 0x9a, 0x07, 0x12, 0x80, 0xe2, 0xeb, 0x27, 0xb2, 0x75,
    0x09, 0x83, 0x2c, 0x1a, 0x1b, 0x6e, 0x5a, 0xa0, 0x52, 0x3b, 0xd6, 0xb3, 0x29, 0xe3, 0x2f, 0x84,
    0x53, 0xd1, 0x00, 0xed, 0x20, 0xfc, 0xb1, 0x5b, 0x6a, 0xcb, 0xbe, 0x39, 0x4a, 0x4c, 0x58, 0xcf,
    0xd0, 0xef, 0xaa, 0xfb, 0x43, 0x4d, 0x33, 0x85, 0x45, 0xf9, 0x02, 0x7f, 0x50, 0x3c, 0x9f, 0xa8,
    0x51, 0xa3, 0x40, 0x8f, 0x92, 0x9d, 0x38, 0xf5, 0xbc, 0xb6, 0xda, 0x21, 0x10, 0xff, 0xf3, 0xd2,
    0xcd, 0x0c, 0x13, 0xec, 0x5f, 0x97, 0x44, 0x17, 0xc4, 0xa7, 0x7e, 0x3d, 0x64, 0x5d, 0x19, 0x73,
    0x60, 0x81, 0x4f, 0xdc, 0x22, 0x2a, 0x90, 0x88, 0x46, 0xee, 0xb8, 0x14, 0xde, 0x5e, 0x0b, 0xdb,
    0xe0, 0x32, 0x3a, 0x0a, 0x49, 0x06, 0x24, 0x5c, 0xc2, 0xd3, 0xac, 0x62, 0x91, 0x95, 0xe4, 0x79,
    0xe7, 0xc8, 0x37, 0x6d, 0x8d, 0xd5, 0x4e, 0xa9, 0x6c, 0x56, 0xf4, 0xea, 0x65, 0x7a, 0xae, 0x08,
    0xba, 0x78, 0x25, 0x2e, 0x1c, 0xa6, 0xb4, 0xc6, 0xe8, 0xdd, 0x74, 0x1f, 0x4b, 0xbd, 0x8b, 0x8a,
    0x70, 0x3e, 0xb5, 0x66, 0x48, 0x03, 0xf6, 0x0e, 0x61, 0x35, 0x57, 0xb9, 0x86, 0xc1, 0x1d, 0x9e,
    0xe1, 0xf8, 0x98, 0x11, 0x69, 0xd9, 0x8e, 0x94, 0x9b, 0x1e, 0x87, 0xe9, 0xce, 0x55, 0x28, 0xdf,
    0x8c, 0xa1, 0x89, 0x0d, 0xbf, 0xe6, 0x42, 0x68, 0x41, 0x99, 0x2d, 0x0f, 0xb0, 0x54, 0xbb, 0x16,
];

/// Round constants for the key schedule.
const RCON: [u8; 10] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1b, 0x36];

/// Multiply in GF(2^8) with the AES reduction polynomial.
fn gmul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    for _ in 0..8 {
        if b & 1 != 0 {
            p ^= a;
        }
        let hi = a & 0x80;
        a <<= 1;
        if hi != 0 {
            a ^= 0x1b;
        }
        b >>= 1;
    }
    p
}

/// Expand a 16-byte key into 11 round keys (176 bytes).
fn expand_key(key: &[u8; 16]) -> [[u8; 16]; 11] {
    let mut w = [[0u8; 4]; 44];
    for i in 0..4 {
        w[i] = [key[4 * i], key[4 * i + 1], key[4 * i + 2], key[4 * i + 3]];
    }
    for i in 4..44 {
        let mut t = w[i - 1];
        if i % 4 == 0 {
            // RotWord + SubWord + Rcon.
            t = [t[1], t[2], t[3], t[0]];
            for b in &mut t {
                *b = SBOX[*b as usize];
            }
            t[0] ^= RCON[i / 4 - 1];
        }
        for j in 0..4 {
            w[i][j] = w[i - 4][j] ^ t[j];
        }
    }
    let mut round_keys = [[0u8; 16]; 11];
    for (r, rk) in round_keys.iter_mut().enumerate() {
        for c in 0..4 {
            rk[c * 4..c * 4 + 4].copy_from_slice(&w[r * 4 + c]);
        }
    }
    round_keys
}

/// Expand a 32-byte key into 15 round keys (240 bytes). AES-256 differs from
/// AES-128 in the schedule (Nk=8, Nr=14) and adds an extra SubWord step on the
/// word four positions into each 8-word run (`i % 8 == 4`); the RotWord +
/// SubWord + Rcon step still falls on `i % 8 == 0` (FIPS-197 §5.2).
fn expand_key_256(key: &[u8; 32]) -> [[u8; 16]; 15] {
    let mut w = [[0u8; 4]; 60];
    for i in 0..8 {
        w[i] = [key[4 * i], key[4 * i + 1], key[4 * i + 2], key[4 * i + 3]];
    }
    for i in 8..60 {
        let mut t = w[i - 1];
        if i % 8 == 0 {
            // RotWord + SubWord + Rcon.
            t = [t[1], t[2], t[3], t[0]];
            for b in &mut t {
                *b = SBOX[*b as usize];
            }
            t[0] ^= RCON[i / 8 - 1];
        } else if i % 8 == 4 {
            // SubWord only.
            for b in &mut t {
                *b = SBOX[*b as usize];
            }
        }
        for j in 0..4 {
            w[i][j] = w[i - 8][j] ^ t[j];
        }
    }
    let mut round_keys = [[0u8; 16]; 15];
    for (r, rk) in round_keys.iter_mut().enumerate() {
        for c in 0..4 {
            rk[c * 4..c * 4 + 4].copy_from_slice(&w[r * 4 + c]);
        }
    }
    round_keys
}

fn add_round_key(state: &mut [u8; 16], rk: &[u8; 16]) {
    for i in 0..16 {
        state[i] ^= rk[i];
    }
}

fn inv_sub_bytes(state: &mut [u8; 16]) {
    for b in state.iter_mut() {
        *b = INV_SBOX[*b as usize];
    }
}

/// Inverse ShiftRows: row `r` rotates right by `r` (state is column-major, so
/// byte at column c, row r is index c*4+r).
fn inv_shift_rows(state: &mut [u8; 16]) {
    let s = *state;
    for c in 0..4 {
        for r in 0..4 {
            state[c * 4 + r] = s[((c + 4 - r) % 4) * 4 + r];
        }
    }
}

fn inv_mix_columns(state: &mut [u8; 16]) {
    for c in 0..4 {
        let col = [
            state[c * 4],
            state[c * 4 + 1],
            state[c * 4 + 2],
            state[c * 4 + 3],
        ];
        state[c * 4] = gmul(col[0], 14) ^ gmul(col[1], 11) ^ gmul(col[2], 13) ^ gmul(col[3], 9);
        state[c * 4 + 1] = gmul(col[0], 9) ^ gmul(col[1], 14) ^ gmul(col[2], 11) ^ gmul(col[3], 13);
        state[c * 4 + 2] = gmul(col[0], 13) ^ gmul(col[1], 9) ^ gmul(col[2], 14) ^ gmul(col[3], 11);
        state[c * 4 + 3] = gmul(col[0], 11) ^ gmul(col[1], 13) ^ gmul(col[2], 9) ^ gmul(col[3], 14);
    }
}

/// Decrypt one 16-byte block in place. The number of rounds follows the number
/// of round keys, so this serves both AES-128 (11 keys) and AES-256 (15).
fn decrypt_block(block: &mut [u8; 16], round_keys: &[[u8; 16]]) {
    let nr = round_keys.len() - 1;
    add_round_key(block, &round_keys[nr]);
    for round in (1..nr).rev() {
        inv_shift_rows(block);
        inv_sub_bytes(block);
        add_round_key(block, &round_keys[round]);
        inv_mix_columns(block);
    }
    inv_shift_rows(block);
    inv_sub_bytes(block);
    add_round_key(block, &round_keys[0]);
}

fn sub_bytes(state: &mut [u8; 16]) {
    for b in state.iter_mut() {
        *b = SBOX[*b as usize];
    }
}

/// ShiftRows: row `r` rotates left by `r` (inverse of `inv_shift_rows`).
fn shift_rows(state: &mut [u8; 16]) {
    let s = *state;
    for c in 0..4 {
        for r in 0..4 {
            state[c * 4 + r] = s[((c + r) % 4) * 4 + r];
        }
    }
}

fn mix_columns(state: &mut [u8; 16]) {
    for c in 0..4 {
        let col = [
            state[c * 4],
            state[c * 4 + 1],
            state[c * 4 + 2],
            state[c * 4 + 3],
        ];
        state[c * 4] = gmul(col[0], 2) ^ gmul(col[1], 3) ^ col[2] ^ col[3];
        state[c * 4 + 1] = col[0] ^ gmul(col[1], 2) ^ gmul(col[2], 3) ^ col[3];
        state[c * 4 + 2] = col[0] ^ col[1] ^ gmul(col[2], 2) ^ gmul(col[3], 3);
        state[c * 4 + 3] = gmul(col[0], 3) ^ col[1] ^ col[2] ^ gmul(col[3], 2);
    }
}

/// Encrypt one 16-byte block in place. Rounds follow the round-key count, as in
/// `decrypt_block`.
fn encrypt_block(block: &mut [u8; 16], round_keys: &[[u8; 16]]) {
    let nr = round_keys.len() - 1;
    add_round_key(block, &round_keys[0]);
    for round in 1..nr {
        sub_bytes(block);
        shift_rows(block);
        mix_columns(block);
        add_round_key(block, &round_keys[round]);
    }
    sub_bytes(block);
    shift_rows(block);
    add_round_key(block, &round_keys[nr]);
}

/// Decrypt a PDF AESV2 datum in place: the first 16 bytes are the IV, the rest
/// is CBC ciphertext with PKCS#7 padding. On any structural problem (short
/// input, non-block-multiple, bad padding) the buffer is cleared rather than
/// left holding ciphertext — a corrupt encrypted stream should decode to
/// nothing, not to garbage that a later stage might misread.
pub fn aes_cbc_decrypt(key: &[u8], buf: &mut Vec<u8>) {
    if key.len() != 16 || buf.len() < 32 || !(buf.len() - 16).is_multiple_of(16) {
        buf.clear();
        return;
    }
    // Guarded above: `key.len() == 16`, so this conversion is infallible.
    let key16: [u8; 16] = key[..16].try_into().unwrap_or_default();
    let round_keys = expand_key(&key16);

    let mut iv = [0u8; 16];
    iv.copy_from_slice(&buf[..16]);

    let mut out = Vec::with_capacity(buf.len() - 16);
    let mut prev = iv;
    for chunk in buf[16..].chunks_exact(16) {
        let mut block = [0u8; 16];
        block.copy_from_slice(chunk);
        let cipher = block;
        decrypt_block(&mut block, &round_keys);
        for i in 0..16 {
            block[i] ^= prev[i];
        }
        out.extend_from_slice(&block);
        prev = cipher;
    }

    // Strip PKCS#7 padding: last byte is the pad length, 1..=16.
    let pad = *out.last().unwrap_or(&0) as usize;
    if pad == 0 || pad > 16 || pad > out.len() {
        buf.clear();
        return;
    }
    out.truncate(out.len() - pad);
    *buf = out;
}

/// Decrypt a PDF AESV3 datum in place, the AES-256 analogue of
/// [`aes_cbc_decrypt`]: the first 16 bytes are the IV, the rest is AES-256-CBC
/// ciphertext with PKCS#7 padding. Same failure contract — any structural
/// problem clears the buffer rather than leaving ciphertext behind.
pub fn aes256_cbc_decrypt(key: &[u8; 32], buf: &mut Vec<u8>) {
    if buf.len() < 32 || !(buf.len() - 16).is_multiple_of(16) {
        buf.clear();
        return;
    }
    let round_keys = expand_key_256(key);

    let mut iv = [0u8; 16];
    iv.copy_from_slice(&buf[..16]);

    let mut out = Vec::with_capacity(buf.len() - 16);
    let mut prev = iv;
    for chunk in buf[16..].chunks_exact(16) {
        let mut block = [0u8; 16];
        block.copy_from_slice(chunk);
        let cipher = block;
        decrypt_block(&mut block, &round_keys);
        for i in 0..16 {
            block[i] ^= prev[i];
        }
        out.extend_from_slice(&block);
        prev = cipher;
    }

    // Strip PKCS#7 padding: last byte is the pad length, 1..=16.
    let pad = *out.last().unwrap_or(&0) as usize;
    if pad == 0 || pad > 16 || pad > out.len() {
        buf.clear();
        return;
    }
    out.truncate(out.len() - pad);
    *buf = out;
}

/// AES-256-CBC decrypt with an explicit IV and no padding removal: returns the
/// full plaintext for all complete 16-byte blocks of `data` (any trailing
/// partial block is ignored). Used to unwrap the 32-byte `/UE` / `/OE` key
/// under a zero IV (ISO 32000-2 §7.6.4.3.3, algorithm 2.A steps g/h).
/// Encrypt one raw AES-256 block in place (ECB, no chaining). Test support
/// for the `/Perms` round trip; content encryption never uses this.
pub(crate) fn aes256_encrypt_block(key: &[u8; 32], block: &mut [u8; 16]) {
    let round_keys = expand_key_256(key);
    encrypt_block(block, &round_keys);
}

pub fn aes256_cbc_decrypt_raw(key: &[u8; 32], iv: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let round_keys = expand_key_256(key);
    let mut out = Vec::with_capacity(data.len());
    let mut prev = *iv;
    for chunk in data.chunks_exact(16) {
        let mut block = [0u8; 16];
        block.copy_from_slice(chunk);
        let cipher = block;
        decrypt_block(&mut block, &round_keys);
        for i in 0..16 {
            block[i] ^= prev[i];
        }
        out.extend_from_slice(&block);
        prev = cipher;
    }
    out
}

/// AES-128-CBC encrypt with an explicit IV and no padding: `data`'s length must
/// be a multiple of 16 (any trailing partial block is dropped). The forward
/// direction the revision-6 hash ("algorithm 2.B", ISO 32000-2 §7.6.4.3.4)
/// needs; not used to protect content.
pub fn aes128_cbc_encrypt_raw(key: &[u8; 16], iv: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let round_keys = expand_key(key);
    let mut out = Vec::with_capacity(data.len());
    let mut prev = *iv;
    for chunk in data.chunks_exact(16) {
        let mut block = [0u8; 16];
        block.copy_from_slice(chunk);
        for i in 0..16 {
            block[i] ^= prev[i];
        }
        encrypt_block(&mut block, &round_keys);
        out.extend_from_slice(&block);
        prev = block;
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn fips197_block_decrypt() {
        // FIPS-197 §C.1 appendix worked example, run backwards: decrypting the
        // known ciphertext under the known key must give the known plaintext.
        let key = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        let cipher = [
            0x69, 0xc4, 0xe0, 0xd8, 0x6a, 0x7b, 0x04, 0x30, 0xd8, 0xcd, 0xb7, 0x80, 0x70, 0xb4,
            0xc5, 0x5a,
        ];
        let plain = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let rk = expand_key(&key);
        let mut block = cipher;
        decrypt_block(&mut block, &rk);
        assert_eq!(block, plain);
    }

    #[test]
    fn cbc_with_iv_and_pkcs7_round_trips() {
        // Encrypt a known message with the real forward cipher, then check the
        // decryptor recovers it including the IV strip and PKCS#7 unpad — for
        // both AES-128 and AES-256.
        let iv = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        let msg = b"Encrypted PDF strings and streams look just like this.".to_vec();

        let key128: [u8; 16] = unhex("2b7e151628aed2a6abf7158809cf4f3c")
            .try_into()
            .unwrap();
        let mut buf = cbc_encrypt_pkcs7(&expand_key(&key128), &iv, &msg);
        aes_cbc_decrypt(&key128, &mut buf);
        assert_eq!(buf, msg);

        let key256: [u8; 32] =
            unhex("603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4")
                .try_into()
                .unwrap();
        let mut buf = cbc_encrypt_pkcs7(&expand_key_256(&key256), &iv, &msg);
        aes256_cbc_decrypt(&key256, &mut buf);
        assert_eq!(buf, msg);
    }

    #[test]
    fn fips197_aes256_block() {
        // FIPS-197 §C.3 known answer for AES-256, both directions.
        let key: [u8; 32] =
            unhex("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f")
                .try_into()
                .unwrap();
        let plain: [u8; 16] = unhex("00112233445566778899aabbccddeeff")
            .try_into()
            .unwrap();
        let cipher: [u8; 16] = unhex("8ea2b7ca516745bfeafc49904b496089")
            .try_into()
            .unwrap();
        let rk = expand_key_256(&key);

        let mut block = cipher;
        decrypt_block(&mut block, &rk);
        assert_eq!(block, plain, "AES-256 decrypt");

        let mut block = plain;
        encrypt_block(&mut block, &rk);
        assert_eq!(block, cipher, "AES-256 encrypt");
    }

    #[test]
    fn aes128_encrypt_raw_matches_fips197_single_block() {
        // One CBC block under a zero IV is ECB, so this must reproduce the
        // FIPS-197 §C.1 AES-128 known answer.
        let key: [u8; 16] = unhex("000102030405060708090a0b0c0d0e0f")
            .try_into()
            .unwrap();
        let pt = unhex("00112233445566778899aabbccddeeff");
        let ct = aes128_cbc_encrypt_raw(&key, &[0u8; 16], &pt);
        assert_eq!(hex(&ct), "69c4e0d86a7b0430d8cdb78070b4c55a");
    }

    #[test]
    fn nist_sp800_38a_cbc_aes128_encrypt() {
        // NIST SP 800-38A §F.2.1 (CBC-AES128.Encrypt), 4 blocks — pins the
        // chaining across blocks, not just a single-block cipher.
        let key: [u8; 16] = unhex("2b7e151628aed2a6abf7158809cf4f3c")
            .try_into()
            .unwrap();
        let iv: [u8; 16] = unhex("000102030405060708090a0b0c0d0e0f")
            .try_into()
            .unwrap();
        let pt = unhex(
            "6bc1bee22e409f96e93d7e117393172a\
             ae2d8a571e03ac9c9eb76fac45af8e51\
             30c81c46a35ce411e5fbc1191a0a52ef\
             f69f2445df4f9b17ad2b417be66c3710",
        );
        let ct = aes128_cbc_encrypt_raw(&key, &iv, &pt);
        assert_eq!(
            hex(&ct),
            "7649abac8119b246cee98e9b12e9197d\
             5086cb9b507219ee95db113a917678b2\
             73bed6b8e3c1743b7116e69e22229516\
             3ff1caa1681fac09120eca307586e1a7"
        );
    }

    #[test]
    fn nist_sp800_38a_cbc_aes256_decrypt_raw() {
        // NIST SP 800-38A §F.2.6 (CBC-AES256.Decrypt), 4 blocks, explicit IV,
        // no padding — exactly the shape of the /UE / /OE key unwrap.
        let key: [u8; 32] =
            unhex("603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4")
                .try_into()
                .unwrap();
        let iv: [u8; 16] = unhex("000102030405060708090a0b0c0d0e0f")
            .try_into()
            .unwrap();
        let ct = unhex(
            "f58c4c04d6e5f1ba779eabfb5f7bfbd6\
             9cfc4e967edb808d679f777bc6702c7d\
             39f23369a9d9bacfa530e26304231461\
             b2eb05e2c39be9fcda6c19078c6a9d1b",
        );
        let pt = aes256_cbc_decrypt_raw(&key, &iv, &ct);
        assert_eq!(
            hex(&pt),
            "6bc1bee22e409f96e93d7e117393172a\
             ae2d8a571e03ac9c9eb76fac45af8e51\
             30c81c46a35ce411e5fbc1191a0a52ef\
             f69f2445df4f9b17ad2b417be66c3710"
        );
    }

    #[test]
    fn malformed_inputs_clear_rather_than_corrupt() {
        let key = [0u8; 16];
        // Too short to hold an IV.
        let mut a = vec![1u8; 10];
        aes_cbc_decrypt(&key, &mut a);
        assert!(a.is_empty());
        // IV present but ciphertext not a block multiple.
        let mut b = vec![0u8; 16 + 20];
        aes_cbc_decrypt(&key, &mut b);
        assert!(b.is_empty());
        // Wrong key length.
        let mut c = vec![0u8; 48];
        aes_cbc_decrypt(&[0u8; 24], &mut c);
        assert!(c.is_empty());
    }

    #[test]
    fn aes256_malformed_inputs_clear_rather_than_corrupt() {
        let key = [0u8; 32];
        // Too short to hold an IV plus a block.
        let mut a = vec![1u8; 10];
        aes256_cbc_decrypt(&key, &mut a);
        assert!(a.is_empty());
        // IV present but ciphertext not a block multiple.
        let mut b = vec![0u8; 16 + 20];
        aes256_cbc_decrypt(&key, &mut b);
        assert!(b.is_empty());
    }

    // --- test helpers ---

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn unhex(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// PKCS#7-pad `msg`, CBC-encrypt it under `round_keys` (11 keys for
    /// AES-128, 15 for AES-256), and prefix the IV — the exact framing the
    /// `aes*_cbc_decrypt` functions consume. Uses the module's real forward
    /// cipher, so a passing round-trip pins encrypt against decrypt.
    fn cbc_encrypt_pkcs7(round_keys: &[[u8; 16]], iv: &[u8; 16], msg: &[u8]) -> Vec<u8> {
        let pad = 16 - (msg.len() % 16);
        let mut padded = msg.to_vec();
        padded.extend(std::iter::repeat_n(pad as u8, pad));
        let mut out = iv.to_vec();
        let mut prev = *iv;
        for chunk in padded.chunks_exact(16) {
            let mut block = [0u8; 16];
            block.copy_from_slice(chunk);
            for i in 0..16 {
                block[i] ^= prev[i];
            }
            encrypt_block(&mut block, round_keys);
            out.extend_from_slice(&block);
            prev = block;
        }
        out
    }
}
