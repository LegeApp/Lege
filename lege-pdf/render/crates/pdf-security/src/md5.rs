//! MD5 (RFC 1321), for the standard security handler's key derivation.
//!
//! Present only because the PDF standard security handler is specified in
//! terms of it (ISO 32000-1 §7.6.3, algorithms 2 through 5). It is not a
//! secure digest and must never be used as one; nothing here is
//! constant-time, because none of these inputs are secret in a way that
//! timing could betray — the file key is derived from data the attacker
//! already has.

/// Streaming MD5 state.
#[derive(Debug, Clone)]
pub struct Md5 {
    state: [u32; 4],
    /// Bytes hashed so far; the length padding needs the true total.
    len: u64,
    buf: [u8; 64],
    /// Bytes currently held in `buf`, always < 64.
    buf_len: usize,
}

impl Default for Md5 {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-round left-rotation amounts (RFC 1321 §3.4).
const SHIFTS: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, //
    5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, //
    4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, //
    6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// `K[i] = floor(2^32 * abs(sin(i + 1)))`, the RFC's table.
const K: [u32; 64] = [
    0xd76a_a478,
    0xe8c7_b756,
    0x2420_70db,
    0xc1bd_ceee,
    0xf57c_0faf,
    0x4787_c62a,
    0xa830_4613,
    0xfd46_9501,
    0x6980_98d8,
    0x8b44_f7af,
    0xffff_5bb1,
    0x895c_d7be,
    0x6b90_1122,
    0xfd98_7193,
    0xa679_438e,
    0x49b4_0821,
    0xf61e_2562,
    0xc040_b340,
    0x265e_5a51,
    0xe9b6_c7aa,
    0xd62f_105d,
    0x0244_1453,
    0xd8a1_e681,
    0xe7d3_fbc8,
    0x21e1_cde6,
    0xc337_07d6,
    0xf4d5_0d87,
    0x455a_14ed,
    0xa9e3_e905,
    0xfcef_a3f8,
    0x676f_02d9,
    0x8d2a_4c8a,
    0xfffa_3942,
    0x8771_f681,
    0x6d9d_6122,
    0xfde5_380c,
    0xa4be_ea44,
    0x4bde_cfa9,
    0xf6bb_4b60,
    0xbebf_bc70,
    0x289b_7ec6,
    0xeaa1_27fa,
    0xd4ef_3085,
    0x0488_1d05,
    0xd9d4_d039,
    0xe6db_99e5,
    0x1fa2_7cf8,
    0xc4ac_5665,
    0xf429_2244,
    0x432a_ff97,
    0xab94_23a7,
    0xfc93_a039,
    0x655b_59c3,
    0x8f0c_cc92,
    0xffef_f47d,
    0x8584_5dd1,
    0x6fa8_7e4f,
    0xfe2c_e6e0,
    0xa301_4314,
    0x4e08_11a1,
    0xf753_7e82,
    0xbd3a_f235,
    0x2ad7_d2bb,
    0xeb86_d391,
];

impl Md5 {
    pub fn new() -> Self {
        Self {
            state: [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476],
            len: 0,
            buf: [0u8; 64],
            buf_len: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.len = self.len.wrapping_add(data.len() as u64);
        // Top up a partial block first.
        if self.buf_len > 0 {
            let take = (64 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buf_len = 0;
            }
        }
        let mut chunks = data.chunks_exact(64);
        for block in &mut chunks {
            let mut b = [0u8; 64];
            b.copy_from_slice(block);
            self.compress(&b);
        }
        // Append the sub-block remainder to whatever is already buffered.
        // Overwriting `buf_len` here instead would clobber a partial buffer
        // left by the top-up above when this call added no complete block
        // (the exact case `finish()`'s padding loop hits every call) — buf_len
        // would reset to 0 and the loop would spin forever.
        let rest = chunks.remainder();
        self.buf[self.buf_len..self.buf_len + rest.len()].copy_from_slice(rest);
        self.buf_len += rest.len();
    }

    pub fn finish(mut self) -> [u8; 16] {
        // Append 0x80, zero-pad so the total length ≡ 56 (mod 64), then the
        // 64-bit little-endian bit length. Build the whole tail and feed it
        // through `update` once, so the block boundary (including the awkward
        // `buf_len == 56` case, where the length spills into a second block)
        // is handled by the same buffering logic rather than a bespoke loop.
        let bit_len = self.len.wrapping_mul(8);
        let after = (self.buf_len + 1) % 64;
        let zeros = if after <= 56 { 56 - after } else { 120 - after };
        let mut tail = Vec::with_capacity(1 + zeros + 8);
        tail.push(0x80);
        tail.resize(1 + zeros, 0);
        tail.extend_from_slice(&bit_len.to_le_bytes());
        self.update(&tail);
        debug_assert_eq!(self.buf_len, 0, "padding must land on a block boundary");

        let mut out = [0u8; 16];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut m = [0u32; 16];
        for (i, word) in m.iter_mut().enumerate() {
            *word = u32::from_le_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        let [mut a, mut b, mut c, mut d] = self.state;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let tmp = d;
            d = c;
            c = b;
            let sum = a.wrapping_add(f).wrapping_add(K[i]).wrapping_add(m[g]);
            b = b.wrapping_add(sum.rotate_left(SHIFTS[i]));
            a = tmp;
        }
        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
    }
}

/// One-shot MD5.
pub fn md5(data: &[u8]) -> [u8; 16] {
    let mut h = Md5::new();
    h.update(data);
    h.finish()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn rfc1321_test_suite() {
        // The RFC's own vectors. If these pass, key derivation has a
        // trustworthy floor to stand on.
        assert_eq!(hex(&md5(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(&md5(b"a")), "0cc175b9c0f1b6a831c399e269772661");
        assert_eq!(hex(&md5(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            hex(&md5(b"message digest")),
            "f96b697d7cb7938d525a2f31aaf161d0"
        );
        assert_eq!(
            hex(&md5(b"abcdefghijklmnopqrstuvwxyz")),
            "c3fcd3d76192e4007dfb496cca67e13b"
        );
        assert_eq!(
            hex(&md5(
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"
            )),
            "d174ab98d277d9f5a5611c2c9f419d9f"
        );
        assert_eq!(
            hex(&md5(
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"
            )),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
    }

    #[test]
    fn streaming_matches_one_shot_across_block_boundaries() {
        // Key derivation feeds MD5 in several small pieces, so the buffering
        // path must agree with the one-shot path at every split -- including
        // exactly on the 64-byte block edge and the 56-byte padding edge.
        let data: Vec<u8> = (0..200u32).map(|i| (i * 7 % 251) as u8).collect();
        for split in [0, 1, 55, 56, 57, 63, 64, 65, 127, 128, 129, 199, 200] {
            let mut h = Md5::new();
            h.update(&data[..split]);
            h.update(&data[split..]);
            assert_eq!(h.finish(), md5(&data), "split at {split}");
        }
    }

    #[test]
    fn length_padding_uses_the_pre_padding_length() {
        // A message of exactly 56 bytes forces a second block purely for the
        // length field -- the classic place to get padding wrong.
        for n in [55usize, 56, 57, 63, 64, 119, 120] {
            let data = vec![0x61u8; n];
            let mut h = Md5::new();
            h.update(&data);
            assert_eq!(h.finish(), md5(&data), "length {n}");
        }
        assert_eq!(hex(&md5(&[0x61u8; 56])), "3b0c8ac703f828b04c6c197006d17218");
    }
}
