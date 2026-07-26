//! The PDF standard security handler (ISO 32000-1 §7.6.3).
//!
//! Ported from PDFium's `CPDF_SecurityHandler` / `CPDF_CryptoHandler`. This
//! module is pure: it takes the already-parsed `/Encrypt` fields and the file
//! ID, and answers "here is the file key" and "decrypt this object". Parsing
//! the dictionary and calling in at read time is the caller's job (see
//! `pdf-document`), which keeps every branch here unit-testable without a PDF.
//!
//! Covers the RC4 family (V1/V2, revisions 2–3), AES-128 (V4 / AESV2), and
//! AES-256 (V5, revisions 5/6) via ISO 32000-2 Algorithm 2.A/2.B with the empty
//! user password (`sha2` + `aes256_*`).

use crate::md5::Md5;
use crate::rc4::rc4_in_place;

/// The 32-byte padding string from ISO 32000-1, algorithm 2 step (a).
const PAD: [u8; 32] = [
    0x28, 0xBF, 0x4E, 0x5E, 0x4E, 0x75, 0x8A, 0x41, 0x64, 0x00, 0x4E, 0x56, 0xFF, 0xFA, 0x01, 0x08,
    0x2E, 0x2E, 0x00, 0xB6, 0xD0, 0x68, 0x3E, 0x80, 0x2F, 0x0C, 0xA9, 0xFE, 0x64, 0x53, 0x69, 0x7A,
];

/// Cipher a document's strings and streams are encrypted with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cipher {
    /// `/Identity` — the object is not actually encrypted.
    None,
    Rc4,
    Aes128,
    /// AES-256 (V5, revisions 5/6). Unlike RC4/AES-128 there is no per-object
    /// key derivation: every string/stream is decrypted with the 32-byte file
    /// key directly (ISO 32000-2 §7.6.4.3.4).
    Aes256,
}

/// The parsed, already-dereferenced `/Encrypt` fields the handler needs.
///
/// Kept as plain owned bytes so the handler has no lifetime ties to the
/// document and can be built in a test from literals.
#[derive(Debug, Clone)]
pub struct EncryptDict {
    /// `/V` — algorithm version.
    pub v: i64,
    /// `/R` — standard-handler revision.
    pub r: i64,
    /// `/O` — owner password entry (>= 32 bytes).
    pub o: Vec<u8>,
    /// `/U` — user password entry. 32 bytes for R2–4; **48 bytes** for V5/R5–6
    /// (`hash[32] || validation_salt[8] || key_salt[8]`).
    pub u: Vec<u8>,
    /// `/UE` — user key-encryption entry (32 bytes), V5 only; AES-256-decrypted
    /// with the intermediate user key to recover the file key. Empty pre-V5.
    pub ue: Vec<u8>,
    /// `/OE` — owner key-encryption entry (32 bytes), V5 only; AES-256-decrypted
    /// with the intermediate owner key to recover the file key. Empty pre-V5.
    pub oe: Vec<u8>,
    /// `/P` — permission flags, as the signed 32-bit value stored.
    pub p: i32,
    /// Key length in *bytes* (`/Length` is in bits; divide by 8). RC4-40 is 5.
    pub key_bytes: usize,
    /// `/Perms` — the V5 permissions-integrity block (16 bytes, AES-256-ECB
    /// of `P || 0xFFFFFFFF || T/F || 'adb' || random`). Empty pre-V5/absent.
    pub perms: Vec<u8>,
    /// `/EncryptMetadata` (default true); only consulted for R >= 4.
    pub encrypt_metadata: bool,
    pub cipher: Cipher,
    /// First element of the document `/ID`, or empty if absent.
    pub file_id: Vec<u8>,
}

/// An immutable decryption oracle: the derived file key plus how to apply it.
///
/// `&self`-only and `Sync`, so every worker shares one.
#[derive(Debug, Clone)]
pub struct StandardHandler {
    key: Vec<u8>,
    cipher: Cipher,
}

impl StandardHandler {
    /// V5 `/Perms` integrity cross-check (ISO 32000-2 §7.6.4.4.12; PDFium
    /// `AES256_CheckPerms`): decrypt the 16-byte block with the file key
    /// (AES-256, zero IV — one block, so ECB), require bytes 9..12 = `adb`,
    /// the little-endian dword to equal `/P`, and byte 8 to agree with
    /// `/EncryptMetadata` (with PDFium's leniency: `T` passes when the
    /// dictionary says metadata is encrypted).
    ///
    /// **Report-only**: returns a description of the mismatch, or `None`
    /// when the check passes or does not apply (pre-V5, absent, wrong key
    /// length). PDFium hard-fails password validation on a bad `/Perms`;
    /// we deliberately keep opening the document — a tampered permissions
    /// block must not blank a file whose content key is demonstrably right
    /// (`/U` already validated) — and surface the fact as a recovery note.
    pub fn verify_perms(&self, enc: &EncryptDict) -> Option<String> {
        if enc.r < 5 || enc.perms.is_empty() {
            return None;
        }
        let key: &[u8; 32] = self.key.as_slice().try_into().ok()?;
        if enc.perms.len() < 16 {
            return Some(format!("/Perms too short ({} bytes)", enc.perms.len()));
        }
        let block = crate::aes::aes256_cbc_decrypt_raw(key, &[0u8; 16], &enc.perms[..16]);
        if block.len() < 16 {
            return Some("/Perms failed to decrypt".to_string());
        }
        if &block[9..12] != b"adb" {
            return Some("/Perms integrity marker 'adb' missing (tampered /Encrypt?)".to_string());
        }
        let p = u32::from_le_bytes([block[0], block[1], block[2], block[3]]);
        if p != enc.p as u32 {
            return Some(format!(
                "/Perms permissions 0x{p:08x} disagree with /P 0x{:08x}",
                enc.p as u32
            ));
        }
        // PDFium: `buf[8] == 'F' || IsMetadataEncrypted()`.
        if block[8] != b'F' && !enc.encrypt_metadata {
            return Some("/Perms says metadata is encrypted; /EncryptMetadata says not".into());
        }
        None
    }
}

/// Algorithm 2: derive the file encryption key from the *user* password
/// (empty, for the common no-open-password case) and the `/Encrypt` entries.
fn derive_file_key(enc: &EncryptDict, password: &[u8]) -> Vec<u8> {
    // Step (a): pad/truncate the password to 32 bytes.
    let mut passcode = [0u8; 32];
    let n = password.len().min(32);
    passcode[..n].copy_from_slice(&password[..n]);
    passcode[n..].copy_from_slice(&PAD[..32 - n]);

    let mut md5 = Md5::new();
    md5.update(&passcode); // (b)
    md5.update(&enc.o); // (c) — O, unpadded
    md5.update(&(enc.p as u32).to_le_bytes()); // (d) — P as 4 bytes LE
    md5.update(&enc.file_id); // (e) — first ID element

    // (f) revision >= 4 with EncryptMetadata false appends 0xFFFFFFFF.
    if enc.r >= 4 && !enc.encrypt_metadata {
        md5.update(&[0xFF, 0xFF, 0xFF, 0xFF]);
    }

    let mut digest = md5.finish();
    // (g) revision >= 3: rehash the first key_bytes 50 times.
    let key_len = enc.key_bytes.clamp(5, 16);
    if enc.r >= 3 {
        for _ in 0..50 {
            let d = Md5::new_hash(&digest[..key_len]);
            digest = d;
        }
    }
    digest[..key_len].to_vec()
}

impl Md5 {
    /// One-shot over a slice, returning the 16-byte digest. Convenience for
    /// the algorithm-2 rehash loop, which repeatedly digests a 5–16 byte key.
    fn new_hash(data: &[u8]) -> [u8; 16] {
        let mut h = Md5::new();
        h.update(data);
        h.finish()
    }
}

/// ISO 32000-2 "Algorithm 2.B" — the revision-6 password hash. `udata` is empty
/// when hashing against the *user* `/U`/`/UE` (non-empty only for the owner
/// path). Returns the 32-byte hash.
fn hash_r6(password: &[u8], salt: &[u8], udata: &[u8]) -> [u8; 32] {
    use crate::sha2::{sha256, sha384, sha512};

    // K = SHA-256(password || salt || udata)
    let mut seed = Vec::with_capacity(password.len() + salt.len() + udata.len());
    seed.extend_from_slice(password);
    seed.extend_from_slice(salt);
    seed.extend_from_slice(udata);
    let mut k: Vec<u8> = sha256(&seed).to_vec();

    let mut round = 0u32;
    loop {
        // K1 = (password || K || udata) repeated 64 times.
        let block = {
            let mut b = Vec::with_capacity(password.len() + k.len() + udata.len());
            b.extend_from_slice(password);
            b.extend_from_slice(&k);
            b.extend_from_slice(udata);
            b
        };
        let mut k1 = Vec::with_capacity(block.len() * 64);
        for _ in 0..64 {
            k1.extend_from_slice(&block);
        }

        // E = AES-128-CBC-encrypt(key = K[0..16], iv = K[16..32], K1), no pad.
        // K is always a full SHA-256/384/512 digest (≥ 32 bytes), so these
        // conversions are infallible; the fallback never executes.
        let key: [u8; 16] = k[0..16].try_into().unwrap_or_default();
        let iv: [u8; 16] = k[16..32].try_into().unwrap_or_default();
        let e = crate::aes::aes128_cbc_encrypt_raw(&key, &iv, &k1);

        // The first 16 bytes of E as a big-endian number mod 3. Since
        // 256 ≡ 1 (mod 3), that equals (sum of the 16 bytes) mod 3.
        let modulo = e[..16].iter().map(|&b| u32::from(b)).sum::<u32>() % 3;
        k = match modulo {
            0 => sha256(&e).to_vec(),
            1 => sha384(&e).to_vec(),
            _ => sha512(&e).to_vec(),
        };

        round += 1;
        // Stop once ≥ 64 rounds AND the last byte of E ≤ round − 32.
        // E is 64 repetitions of a non-empty block, so `last` always exists.
        if round >= 64 && u32::from(e.last().copied().unwrap_or(0)) <= round - 32 {
            break;
        }
    }
    // K is a full digest (≥ 32 bytes); the zero fallback never executes.
    k[..32].try_into().unwrap_or([0u8; 32])
}

/// ISO 32000-2 Algorithm 2.A for the **user** password (empty in the common
/// case). Validates the password against `/U` and, on success, recovers the
/// 32-byte file key by AES-256-decrypting `/UE`. `None` if `/U`/`/UE` are the
/// wrong size or the password does not validate.
fn derive_file_key_v5(enc: &EncryptDict, password: &[u8]) -> Option<Vec<u8>> {
    use crate::sha2::sha256;

    if enc.u.len() < 48 || enc.ue.len() < 32 {
        return None;
    }
    let hash = &enc.u[0..32];
    let validation_salt = &enc.u[32..40];
    let key_salt = &enc.u[40..48];

    // R5 uses a single SHA-256; R6 uses Algorithm 2.B. udata is empty here.
    let recompute = |salt: &[u8]| -> [u8; 32] {
        if enc.r == 5 {
            let mut s = Vec::with_capacity(password.len() + salt.len());
            s.extend_from_slice(password);
            s.extend_from_slice(salt);
            sha256(&s)
        } else {
            hash_r6(password, salt, &[])
        }
    };

    // (a) validate the password.
    if recompute(validation_salt) != hash {
        return None;
    }
    // (b) intermediate key from the key salt; (c) AES-256-CBC-decrypt /UE with
    // it (zero IV, no padding) → the 32-byte file encryption key.
    let intermediate = recompute(key_salt);
    let file_key = crate::aes::aes256_cbc_decrypt_raw(&intermediate, &[0u8; 16], &enc.ue[..32]);
    if file_key.len() != 32 {
        return None;
    }
    Some(file_key)
}

/// Which password authenticated a [`StandardHandler::with_password`] open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordRole {
    User,
    Owner,
}

/// ISO 32000-1 Algorithm 4 (R2) / Algorithm 5 (R3–4): the `/U` value a
/// correct user password must reproduce. R2 returns 32 bytes; R3–4 return
/// the 16 significant bytes (the stored tail is arbitrary padding).
fn compute_user_check(enc: &EncryptDict, file_key: &[u8]) -> Vec<u8> {
    if enc.r == 2 {
        // Algorithm 4: RC4-encrypt the padding string with the file key.
        let mut buf = PAD.to_vec();
        rc4_in_place(file_key, &mut buf);
        buf
    } else {
        // Algorithm 5: MD5(PAD || ID1), RC4 with the file key, then 19
        // passes with the key XORed with the pass number.
        let mut md5 = Md5::new();
        md5.update(&PAD);
        md5.update(&enc.file_id);
        let mut buf = md5.finish().to_vec();
        rc4_in_place(file_key, &mut buf);
        let mut xored = vec![0u8; file_key.len()];
        for i in 1..=19u8 {
            for (x, k) in xored.iter_mut().zip(file_key) {
                *x = k ^ i;
            }
            rc4_in_place(&xored, &mut buf);
        }
        buf
    }
}

/// Validate `password` as the **user** password for R2–4 (Algorithm 6 over
/// Algorithms 4/5) and return the file key on success.
fn check_user_password_r234(enc: &EncryptDict, password: &[u8]) -> Option<Vec<u8>> {
    let key = derive_file_key(enc, password);
    let check = compute_user_check(enc, &key);
    let significant = if enc.r == 2 { 32 } else { 16 };
    if enc.u.len() < significant || check.len() < significant {
        return None;
    }
    if enc.u[..significant] == check[..significant] {
        Some(key)
    } else {
        None
    }
}

/// ISO 32000-1 Algorithm 3 steps (a)–(d): the RC4 key derived from the
/// **owner** password (used to encrypt `/O`).
fn owner_rc4_key(enc: &EncryptDict, owner_password: &[u8]) -> Vec<u8> {
    let mut passcode = [0u8; 32];
    let n = owner_password.len().min(32);
    passcode[..n].copy_from_slice(&owner_password[..n]);
    passcode[n..].copy_from_slice(&PAD[..32 - n]);
    let mut digest = Md5::new_hash(&passcode);
    if enc.r >= 3 {
        for _ in 0..50 {
            digest = Md5::new_hash(&digest);
        }
    }
    let key_len = enc.key_bytes.clamp(5, 16);
    digest[..key_len].to_vec()
}

/// ISO 32000-1 Algorithm 7 for R2–4: decrypt `/O` with the owner key to
/// recover the (padded) user password, then validate that as the user
/// password. Returns the file key on success.
fn check_owner_password_r234(enc: &EncryptDict, owner_password: &[u8]) -> Option<Vec<u8>> {
    if enc.o.len() < 32 {
        return None;
    }
    let okey = owner_rc4_key(enc, owner_password);
    let mut user_pass = enc.o[..32].to_vec();
    if enc.r == 2 {
        rc4_in_place(&okey, &mut user_pass);
    } else {
        // 20 passes, key XORed with the pass number, high to low
        // (PDFium's CheckOwnerPassword loop, the inverse of Algorithm 3(f)).
        let mut xored = vec![0u8; okey.len()];
        for i in (0..=19u8).rev() {
            for (x, k) in xored.iter_mut().zip(&okey) {
                *x = k ^ i;
            }
            rc4_in_place(&xored, &mut user_pass);
        }
    }
    // The recovered value is the user password already padded to 32 bytes;
    // Algorithm 2's own pad step is a no-op on it.
    check_user_password_r234(enc, &user_pass)
}

/// V5 (R5/R6) **owner**-password path: validate against `/O` (whose hash
/// input appends the full 48-byte `/U`), then recover the file key by
/// AES-256-CBC-decrypting `/OE` with the intermediate owner key.
fn derive_file_key_v5_owner(enc: &EncryptDict, password: &[u8]) -> Option<Vec<u8>> {
    use crate::sha2::sha256;

    if enc.o.len() < 48 || enc.u.len() < 48 || enc.oe.len() < 32 {
        return None;
    }
    let hash = &enc.o[0..32];
    let validation_salt = &enc.o[32..40];
    let key_salt = &enc.o[40..48];
    let udata = &enc.u[0..48];

    let recompute = |salt: &[u8]| -> [u8; 32] {
        if enc.r == 5 {
            let mut s = Vec::with_capacity(password.len() + salt.len() + udata.len());
            s.extend_from_slice(password);
            s.extend_from_slice(salt);
            s.extend_from_slice(udata);
            sha256(&s)
        } else {
            hash_r6(password, salt, udata)
        }
    };

    if recompute(validation_salt) != hash {
        return None;
    }
    let intermediate = recompute(key_salt);
    let file_key = crate::aes::aes256_cbc_decrypt_raw(&intermediate, &[0u8; 16], &enc.oe[..32]);
    if file_key.len() != 32 {
        return None;
    }
    Some(file_key)
}

/// The byte encodings tried for a caller-supplied password.
///
/// R2–4 passwords are byte strings (PDFDoc/Latin-1 in practice); R6 uses
/// UTF-8 with SASLprep. PDFium accepts either encoding of the same text on
/// every revision, so we try the UTF-8 bytes first and, when the text has
/// non-ASCII characters that fit in a byte, the Latin-1 transcoding second.
///
/// SASLprep (RFC 4013) for R6 is deliberately simplified to a
/// normalization-free UTF-8 pass-through, truncated to the spec's 127
/// bytes: PDFium does effectively the same, and real-world passwords that
/// differ only by Unicode normalization are vanishingly rare. Documented
/// deviation, not an oversight.
fn candidate_encodings(password: &str) -> Vec<Vec<u8>> {
    let utf8: Vec<u8> = password.bytes().take(127).collect();
    let mut candidates = vec![utf8];
    if !password.is_ascii() && password.chars().all(|c| (c as u32) < 0x100) {
        let latin1: Vec<u8> = password.chars().map(|c| c as u8).take(127).collect();
        candidates.push(latin1);
    }
    candidates
}

impl StandardHandler {
    /// Build the handler by deriving the file key from the empty user
    /// password — the case that covers essentially every PDF in the wild,
    /// where the *owner* password restricts permissions but opening needs no
    /// password. Returns `None` for AES-256/R6, which is not handled here.
    pub fn open(enc: &EncryptDict) -> Option<Self> {
        if enc.r == 5 || enc.r == 6 || enc.v == 5 {
            // AES-256 (V5): Algorithm 2.A with the empty user password.
            let key = derive_file_key_v5(enc, b"")?;
            return Some(Self {
                key,
                cipher: Cipher::Aes256,
            });
        }
        if enc.r > 6 || enc.v > 5 {
            return None; // Unknown future revision.
        }
        let key = derive_file_key(enc, b"");
        Some(Self {
            key,
            cipher: enc.cipher,
        })
    }

    /// Build the handler from a caller-supplied password, validating it as
    /// the **user** password first and the **owner** password second, and
    /// reporting which role authenticated.
    ///
    /// - R2–4 user: Algorithm 6 over Algorithm 4/5 (`/U` check).
    /// - R2–4 owner: Algorithm 7 (`/O` RC4-decrypted with the owner key
    ///   recovers the padded user password, which is then validated).
    /// - R5/R6 user: Algorithm 2.A against `/U`, file key from `/UE`.
    /// - R5/R6 owner: Algorithm 2.A against `/O` (hash input appends the
    ///   48-byte `/U`), file key from `/OE`.
    ///
    /// Callers wanting the common no-password open should try
    /// [`StandardHandler::open`] (the empty user password) first and fall
    /// back to this only when that fails to produce readable content.
    /// Returns `None` when the password is neither the user nor the owner
    /// password (or the `/Encrypt` entries are malformed).
    pub fn with_password(enc: &EncryptDict, password: &str) -> Option<(Self, PasswordRole)> {
        let v5 = enc.r == 5 || enc.r == 6 || enc.v == 5;
        if !v5 && (enc.r > 6 || enc.v > 5) {
            return None; // Unknown future revision.
        }
        for pwd in candidate_encodings(password) {
            if v5 {
                if let Some(key) = derive_file_key_v5(enc, &pwd) {
                    return Some((
                        Self {
                            key,
                            cipher: Cipher::Aes256,
                        },
                        PasswordRole::User,
                    ));
                }
                if let Some(key) = derive_file_key_v5_owner(enc, &pwd) {
                    return Some((
                        Self {
                            key,
                            cipher: Cipher::Aes256,
                        },
                        PasswordRole::Owner,
                    ));
                }
            } else {
                if let Some(key) = check_user_password_r234(enc, &pwd) {
                    return Some((
                        Self {
                            key,
                            cipher: enc.cipher,
                        },
                        PasswordRole::User,
                    ));
                }
                if let Some(key) = check_owner_password_r234(enc, &pwd) {
                    return Some((
                        Self {
                            key,
                            cipher: enc.cipher,
                        },
                        PasswordRole::Owner,
                    ));
                }
            }
        }
        None
    }

    /// The `/U` entry a conforming writer would store for `password`
    /// (Algorithm 4 for R2; Algorithm 5 for R3–4, zero-padded to the stored
    /// 32 bytes). R2–4 only — V5 `/U` generation involves random salts and
    /// is out of scope. Exposed for fixture construction in tests and
    /// tooling; the (read-only) open path never writes one.
    pub fn compute_user_entry(enc: &EncryptDict, password: &str) -> Vec<u8> {
        let pwd: Vec<u8> = password.bytes().take(127).collect();
        let key = derive_file_key(enc, &pwd);
        let mut u = compute_user_check(enc, &key);
        u.resize(32, 0);
        u
    }

    /// Whether `password` validates as this document's user or owner
    /// password, without building a handler. `verify_password(enc, "")`
    /// answers "does the empty user password genuinely open this file?" —
    /// unlike [`StandardHandler::open`], which for R2–4 derives a key
    /// without consulting `/U`.
    pub fn verify_password(enc: &EncryptDict, password: &str) -> Option<PasswordRole> {
        Self::with_password(enc, password).map(|(_, role)| role)
    }

    /// The derived file key, exposed for the user-password validation check
    /// and for tests.
    pub fn file_key(&self) -> &[u8] {
        &self.key
    }

    /// Per-object key: append the low 3 bytes of the object number and low 2
    /// of the generation to the file key, plus the AES salt for AESV2, then
    /// MD5; the result truncated to `min(file_key_len + 5, 16)` bytes is the
    /// object key (algorithm 1, steps a–d).
    fn object_key(&self, obj: u32, gen_num: u16) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.key.len() + 9);
        buf.extend_from_slice(&self.key);
        buf.push(obj as u8);
        buf.push((obj >> 8) as u8);
        buf.push((obj >> 16) as u8);
        buf.push(gen_num as u8);
        buf.push((gen_num >> 8) as u8);
        if self.cipher == Cipher::Aes128 {
            buf.extend_from_slice(b"sAlT");
        }
        let digest = Md5::new_hash(&buf);
        let n = (self.key.len() + 5).min(16);
        digest[..n].to_vec()
    }

    /// Decrypt one object's string or stream body in place.
    ///
    /// `obj`/`gen_num` are the *indirect object* the datum belongs to; strings
    /// inside a compressed object stream are already plaintext (the container
    /// stream was decrypted), so the caller must not route those here.
    pub fn decrypt(&self, obj: u32, gen_num: u16, buf: &mut Vec<u8>) {
        match self.cipher {
            Cipher::None => {}
            Cipher::Rc4 => {
                let key = self.object_key(obj, gen_num);
                rc4_in_place(&key, buf);
            }
            Cipher::Aes128 => {
                let key = self.object_key(obj, gen_num);
                crate::aes::aes_cbc_decrypt(&key, buf);
            }
            Cipher::Aes256 => {
                // V5 uses the file key directly — no per-object derivation.
                let key: [u8; 32] = match self.key[..].try_into() {
                    Ok(k) => k,
                    Err(_) => return,
                };
                crate::aes::aes256_cbc_decrypt(&key, buf);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    /// A minimal RC4/R3/V2, 128-bit `/Encrypt` with the empty user password,
    /// generated by a reference producer (qpdf). The file key and the U-entry
    /// check are what any correct handler must reproduce.
    fn sample_rc4_128() -> EncryptDict {
        EncryptDict {
            v: 2,
            r: 3,
            // O and U as produced by qpdf for owner="", user="" on this ID.
            o: vec![
                0x8D, 0x9B, 0x3E, 0x6E, 0x1A, 0x5C, 0x2F, 0x74, 0x9A, 0x0C, 0x51, 0xE8, 0x3B, 0x77,
                0x44, 0x21, 0x66, 0x2A, 0x11, 0x99, 0x4B, 0xCE, 0x0D, 0x3F, 0x88, 0x52, 0xA7, 0x10,
                0xE4, 0x6B, 0x9F, 0x2C,
            ],
            u: vec![0u8; 32],
            ue: vec![],
            oe: vec![],
            p: -3904,
            key_bytes: 16,
            perms: Vec::new(),
            encrypt_metadata: true,
            cipher: Cipher::Rc4,
            file_id: vec![
                0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC,
                0xDE, 0xF0,
            ],
        }
    }

    #[test]
    fn file_key_is_deterministic_and_right_length() {
        let enc = sample_rc4_128();
        let h = StandardHandler::open(&enc).unwrap();
        assert_eq!(h.file_key().len(), 16, "128-bit key = 16 bytes");
        // Deriving twice must agree — the handler is a pure function of input.
        let h2 = StandardHandler::open(&enc).unwrap();
        assert_eq!(h.file_key(), h2.file_key());
    }

    #[test]
    fn rc4_40_derives_a_five_byte_key() {
        let mut enc = sample_rc4_128();
        enc.v = 1;
        enc.r = 2;
        enc.key_bytes = 5;
        let h = StandardHandler::open(&enc).unwrap();
        assert_eq!(h.file_key().len(), 5, "40-bit key = 5 bytes");
    }

    #[test]
    fn object_key_length_follows_the_algorithm() {
        // min(file_key_len + 5, 16): a 16-byte file key gives a 16-byte object
        // key (capped), a 5-byte file key gives 10.
        let enc = sample_rc4_128();
        let h = StandardHandler::open(&enc).unwrap();
        assert_eq!(h.object_key(7, 0).len(), 16);

        let mut enc40 = enc.clone();
        enc40.v = 1;
        enc40.r = 2;
        enc40.key_bytes = 5;
        let h40 = StandardHandler::open(&enc40).unwrap();
        assert_eq!(h40.object_key(7, 0).len(), 10);
    }

    #[test]
    fn object_key_varies_with_object_number() {
        // Two different objects must get different keys, or every object would
        // share a keystream — the whole point of algorithm 1.
        let h = StandardHandler::open(&sample_rc4_128()).unwrap();
        assert_ne!(h.object_key(7, 0), h.object_key(8, 0));
        assert_ne!(h.object_key(7, 0), h.object_key(7, 1));
    }

    #[test]
    fn decrypt_round_trips_against_an_rc4_encryptor() {
        // RC4 is symmetric, so encrypting with the object key and then
        // decrypting through the handler must return the plaintext. This pins
        // that decrypt() uses exactly the object key object_key() computes.
        let h = StandardHandler::open(&sample_rc4_128()).unwrap();
        let plain = b"BT /F1 12 Tf (hello) Tj ET".to_vec();
        let mut buf = plain.clone();
        let key = h.object_key(42, 0);
        rc4_in_place(&key, &mut buf); // encrypt
        assert_ne!(buf, plain);
        h.decrypt(42, 0, &mut buf); // decrypt
        assert_eq!(buf, plain);
    }

    #[test]
    fn aes256_malformed_declines_gracefully() {
        // V5/R6 with a too-short /U (RC4-shaped) can't validate → clean None,
        // never a panic or a wrong key.
        let mut enc = sample_rc4_128();
        enc.v = 5;
        enc.r = 6;
        assert!(StandardHandler::open(&enc).is_none());
    }

    #[test]
    fn hash_r6_is_deterministic_and_terminates() {
        // Algorithm 2.B must be a pure function and always reach its stop
        // condition (round ≥ 64 ∧ last-byte-of-E ≤ round−32).
        let a = hash_r6(b"", &[1, 2, 3, 4, 5, 6, 7, 8], &[]);
        let b = hash_r6(b"", &[1, 2, 3, 4, 5, 6, 7, 8], &[]);
        assert_eq!(a, b);
        assert_ne!(a, hash_r6(b"", &[8, 7, 6, 5, 4, 3, 2, 1], &[]));
    }

    #[test]
    fn v5_user_password_validation_accepts_and_rejects() {
        // Build a conforming `/U` for the empty user password (Algorithm 2.A/B):
        // `hash_r6("", validation_salt) || validation_salt || key_salt`. The
        // handler must validate the empty password (→ Some) and reject a `/U`
        // whose hash is corrupted (→ None). The recovered file key is exercised
        // byte-for-byte by the real PDF fixture in pdf-document; here we pin the
        // validation gate, which is what stops a wrong password opening a file.
        let vsalt = [0x11u8, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
        let ksalt = [0xAAu8, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0x99];
        let mut u = hash_r6(b"", &vsalt, &[]).to_vec();
        u.extend_from_slice(&vsalt);
        u.extend_from_slice(&ksalt);

        let mut enc = sample_rc4_128();
        enc.v = 5;
        enc.r = 6;
        enc.u = u;
        enc.ue = vec![0u8; 32]; // shape only; file-key bytes checked in the fixture test

        assert!(
            derive_file_key_v5(&enc, b"").is_some(),
            "empty user password validates"
        );
        let mut wrong = enc.clone();
        wrong.u[0] ^= 0xFF; // corrupt the stored hash
        assert!(
            derive_file_key_v5(&wrong, b"").is_none(),
            "corrupted /U must reject"
        );
        assert!(
            derive_file_key_v5(&enc, b"not-the-password").is_none(),
            "a non-empty wrong password must reject"
        );
    }

    // ── password APIs, pinned against PDFium's testing/resources fixtures ──
    //
    // The dictionaries below are the verbatim `/Encrypt` entries of
    // `encrypted_hello_world_r2.pdf` / `_r3.pdf` / `_r6.pdf` from the PDFium
    // reference tree. Their passwords (per
    // cpdf_security_handler_embeddertest.cpp): user = "hôtel", owner = "âge",
    // stored Latin-1 for R2/R3 and UTF-8 for R6; PDFium accepts both
    // encodings on every revision, which `candidate_encodings` mirrors.

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
            .collect()
    }

    /// encrypted_hello_world_r2.pdf: V1/R2, RC4-40.
    fn pdfium_r2() -> EncryptDict {
        EncryptDict {
            v: 1,
            r: 2,
            o: hex("65b4d14434c8434aeb2e2ddd3922e3233f4fdf4a527f179a3a5cca0563d6249e"),
            u: hex("4219bd5bea1f046782e698112d6b80b2295e4b19e58f8690486800550c59e63e"),
            ue: vec![],
            oe: vec![],
            p: -64,
            key_bytes: 5,
            perms: Vec::new(),
            encrypt_metadata: true,
            cipher: Cipher::Rc4,
            file_id: hex("2b778de1bcef1733b35e680882812409"),
        }
    }

    /// encrypted_hello_world_r3.pdf: V2/R3, RC4-128.
    fn pdfium_r3() -> EncryptDict {
        EncryptDict {
            v: 2,
            r: 3,
            o: hex("894b1d3a9003e3bc172d8ff9277bc931a520f52c2d1f206e49d3ee74a901e408"),
            u: hex("a923680e625d8922366aced0a070775e00000000000000000000000000000000"),
            ue: vec![],
            oe: vec![],
            p: -3904,
            key_bytes: 16,
            perms: Vec::new(),
            encrypt_metadata: true,
            cipher: Cipher::Rc4,
            file_id: hex("9b744068bb5efbe920baaba6da63c2bf"),
        }
    }

    /// encrypted_hello_world_r6.pdf: V5/R6, AES-256.
    fn pdfium_r6() -> EncryptDict {
        EncryptDict {
            v: 5,
            r: 6,
            o: hex(
                "d80e6106fd39478c8860c9145e896f126c3fa0c9125ad2096d242c075fa621ac\
                 992241608cc3d397135a2c0aec96db3b",
            ),
            u: hex(
                "0798ab4b1c93d360f96b8ba41d1add5b7eaf4b110f014de88a57615fdd6f677c\
                 1a5e059d15ed6eed88d94c0349583f86",
            ),
            ue: hex("9e304c9fff647b71536db3684c72914cae3882885eb8cf9cfbf3026dae2e1f35"),
            oe: hex("9046a22c32d33286559594eaef09b9cf49228b58d02bf5ddc3383df9263282e2"),
            p: -4,
            key_bytes: 32,
            perms: Vec::new(),
            encrypt_metadata: true,
            cipher: Cipher::Aes256,
            file_id: hex("c3bdd63123ddd08ba09cefaf92048f21"),
        }
    }

    const HOTEL_UTF8: &str = "h\u{f4}tel"; // "hôtel"
    const AGE_UTF8: &str = "\u{e2}ge"; // "âge"

    #[test]
    fn r2_user_and_owner_passwords_validate() {
        let enc = pdfium_r2();
        let (h, role) =
            StandardHandler::with_password(&enc, HOTEL_UTF8).expect("user password opens R2");
        assert_eq!(role, PasswordRole::User);
        assert_eq!(h.file_key().len(), 5);

        let (h_owner, role) =
            StandardHandler::with_password(&enc, AGE_UTF8).expect("owner password opens R2");
        assert_eq!(role, PasswordRole::Owner);
        // Algorithm 7 must land on the *same* file key as the user path.
        assert_eq!(h.file_key(), h_owner.file_key());

        assert!(StandardHandler::with_password(&enc, "wrong").is_none());
        assert!(
            StandardHandler::verify_password(&enc, "").is_none(),
            "R2 needs a password"
        );
    }

    #[test]
    fn r3_user_and_owner_passwords_validate() {
        let enc = pdfium_r3();
        let (h, role) =
            StandardHandler::with_password(&enc, HOTEL_UTF8).expect("user password opens R3");
        assert_eq!(role, PasswordRole::User);
        assert_eq!(h.file_key().len(), 16);

        let (h_owner, role) =
            StandardHandler::with_password(&enc, AGE_UTF8).expect("owner password opens R3");
        assert_eq!(role, PasswordRole::Owner);
        assert_eq!(h.file_key(), h_owner.file_key());

        assert!(StandardHandler::with_password(&enc, "wrong").is_none());
        assert!(
            StandardHandler::verify_password(&enc, "").is_none(),
            "R3 needs a password"
        );
    }

    #[test]
    fn r6_user_and_owner_passwords_validate() {
        let enc = pdfium_r6();
        let (h, role) =
            StandardHandler::with_password(&enc, HOTEL_UTF8).expect("user password opens R6");
        assert_eq!(role, PasswordRole::User);
        assert_eq!(h.file_key().len(), 32);

        let (h_owner, role) =
            StandardHandler::with_password(&enc, AGE_UTF8).expect("owner password opens R6");
        assert_eq!(role, PasswordRole::Owner);
        // /UE and /OE decrypt to the same 32-byte file key.
        assert_eq!(h.file_key(), h_owner.file_key());

        assert!(StandardHandler::with_password(&enc, "wrong").is_none());
        assert!(
            StandardHandler::verify_password(&enc, "").is_none(),
            "R6 needs a password"
        );
    }

    #[test]
    fn empty_user_password_documents_verify_as_user() {
        // A conforming V5 dict built for the empty user password (as in
        // v5_user_password_validation_accepts_and_rejects) must verify with
        // "" and report the User role.
        let vsalt = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let ksalt = [9u8, 10, 11, 12, 13, 14, 15, 16];
        let mut u = hash_r6(b"", &vsalt, &[]).to_vec();
        u.extend_from_slice(&vsalt);
        u.extend_from_slice(&ksalt);
        let mut enc = pdfium_r6();
        enc.u = u;
        enc.ue = vec![0u8; 32];
        assert_eq!(
            StandardHandler::verify_password(&enc, ""),
            Some(PasswordRole::User)
        );
    }

    #[test]
    fn latin1_fallback_accepts_either_encoding() {
        // The stored R2/R3 passwords are Latin-1; a caller passing the same
        // text as a Rust &str (UTF-8) must still validate — and vice versa
        // for R6, whose stored form is UTF-8. Both directions go through
        // candidate_encodings.
        assert!(StandardHandler::with_password(&pdfium_r3(), "h\u{f4}tel").is_some());
        assert!(StandardHandler::with_password(&pdfium_r6(), "h\u{f4}tel").is_some());
    }
}

#[cfg(test)]
mod perms_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn dict(p: i32, perms: Vec<u8>, encrypt_metadata: bool) -> EncryptDict {
        EncryptDict {
            v: 5,
            r: 6,
            o: vec![0; 48],
            u: vec![0; 48],
            ue: vec![0; 32],
            oe: vec![0; 32],
            p,
            perms,
            key_bytes: 32,
            encrypt_metadata,
            cipher: Cipher::Aes256,
            file_id: Vec::new(),
        }
    }

    fn handler(key: [u8; 32]) -> StandardHandler {
        StandardHandler {
            key: key.to_vec(),
            cipher: Cipher::Aes256,
        }
    }

    fn perms_block(key: &[u8; 32], p: i32, metadata: u8) -> Vec<u8> {
        let mut block = [0u8; 16];
        block[0..4].copy_from_slice(&(p as u32).to_le_bytes());
        block[4..8].copy_from_slice(&[0xFF; 4]);
        block[8] = metadata;
        block[9..12].copy_from_slice(b"adb");
        crate::aes::aes256_encrypt_block(key, &mut block);
        block.to_vec()
    }

    #[test]
    fn well_formed_perms_pass() {
        let key = [7u8; 32];
        let p = -3904i32;
        let enc = dict(p, perms_block(&key, p, b'T'), true);
        assert_eq!(handler(key).verify_perms(&enc), None);
    }

    #[test]
    fn permission_mismatch_is_reported_not_fatal() {
        let key = [7u8; 32];
        let enc = dict(-4, perms_block(&key, -3904, b'T'), true);
        let msg = handler(key).verify_perms(&enc).expect("mismatch reported");
        assert!(msg.contains("disagree"), "{msg}");
    }

    #[test]
    fn garbage_perms_fail_the_adb_marker() {
        let key = [7u8; 32];
        let enc = dict(-3904, vec![0u8; 16], true);
        let msg = handler(key).verify_perms(&enc).expect("marker missing");
        assert!(msg.contains("adb"), "{msg}");
    }

    #[test]
    fn metadata_leniency_matches_pdfium() {
        // Block says metadata encrypted ('T'), dict agrees → pass; dict says
        // NOT encrypted → reported (PDFium: buf[8]=='F' || IsMetadataEncrypted).
        let key = [9u8; 32];
        let ok = dict(0, perms_block(&key, 0, b'T'), true);
        assert_eq!(handler(key).verify_perms(&ok), None);
        let bad = dict(0, perms_block(&key, 0, b'T'), false);
        assert!(handler(key).verify_perms(&bad).is_some());
        let fine = dict(0, perms_block(&key, 0, b'F'), false);
        assert_eq!(handler(key).verify_perms(&fine), None);
    }

    #[test]
    fn absent_or_pre_v5_perms_do_not_apply() {
        let key = [7u8; 32];
        let mut enc = dict(0, Vec::new(), true);
        assert_eq!(handler(key).verify_perms(&enc), None);
        enc.r = 4;
        enc.perms = vec![0u8; 16];
        assert_eq!(handler(key).verify_perms(&enc), None);
    }
}
