//! Encryption and permissions (`/Encrypt`).
//!
//! Established once at document open; afterwards a `SecurityContext` is an
//! immutable decrypt oracle that any worker can call concurrently. Key
//! derivation state never mutates after construction.

use pdf_object::ObjectId;

mod aes;
mod md5;
mod rc4;
mod sha2;
mod standard;

pub use md5::{Md5, md5};
pub use rc4::rc4_in_place;
pub use standard::{Cipher, EncryptDict, PasswordRole, StandardHandler};

/// Standard security handler schemes supported: the RC4/AES-128 family and
/// AES-256 (V5, revisions 5/6) with the empty user password.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncryptionScheme {
    None,
    Rc4 { key_bits: u16 },
    Aes128,
    Aes256,
}

/// Document permissions from /P, exposed for API completeness. The engine
/// is read-only and renders regardless (matching PDFium's rendering path);
/// embedders decide what to honor.
#[derive(Debug, Clone, Copy)]
pub struct Permissions(pub u32);

/// Immutable decryption context shared by all workers.
///
/// Built once at document open from the parsed `/Encrypt` dictionary. After
/// construction it is a pure `&self` decrypt oracle: `handler` holds the
/// derived file key, and every worker calls [`Self::decrypt_in_place`]
/// concurrently.
#[derive(Debug)]
pub struct SecurityContext {
    pub scheme: EncryptionScheme,
    pub permissions: Permissions,
    /// The standard-handler oracle. `None` iff `scheme == None` (an
    /// unencrypted document that still carries a `SecurityContext`), in which
    /// case decryption is the identity.
    handler: Option<StandardHandler>,
    /// Which password authenticated this context (`None` when the document
    /// opened without password validation, e.g. an Identity crypt filter).
    password_role: Option<PasswordRole>,
}

impl SecurityContext {
    /// A context for an unencrypted document: decryption is the identity.
    pub fn unencrypted() -> Self {
        Self {
            scheme: EncryptionScheme::None,
            permissions: Permissions(0xFFFF_FFFF),
            handler: None,
            password_role: None,
        }
    }

    /// A context backed by the standard security handler.
    pub fn standard(
        scheme: EncryptionScheme,
        permissions: Permissions,
        handler: StandardHandler,
    ) -> Self {
        Self {
            scheme,
            permissions,
            handler: Some(handler),
            password_role: None,
        }
    }

    /// Record which password (user or owner) authenticated this context.
    pub fn with_password_role(mut self, role: PasswordRole) -> Self {
        self.password_role = Some(role);
        self
    }

    /// Which password authenticated the open, when password validation ran.
    pub fn password_role(&self) -> Option<PasswordRole> {
        self.password_role
    }

    /// Whether this context actually decrypts (i.e. the document is encrypted).
    pub fn is_encrypted(&self) -> bool {
        self.handler.is_some()
    }

    /// Decrypt a string or stream body belonging to indirect object `id` in
    /// place. AES strips its IV and padding, so the buffer may shrink — hence
    /// `&mut Vec<u8>` rather than `&mut [u8]`. `&self` only, safe for unlimited
    /// concurrent use.
    ///
    /// The caller is responsible for the standard exemptions (objects inside an
    /// object stream, cross-reference streams, and the `/Encrypt` dictionary
    /// and `/ID` themselves are never routed here).
    pub fn decrypt_in_place(&self, id: ObjectId, buf: &mut Vec<u8>) -> Result<(), SecurityError> {
        match &self.handler {
            None => Ok(()),
            Some(h) => {
                h.decrypt(id.number, id.generation, buf);
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum SecurityError {
    #[error("unsupported encryption scheme {0:?}")]
    UnsupportedScheme(EncryptionScheme),
    #[error("password required")]
    PasswordRequired,
    #[error("incorrect password")]
    IncorrectPassword,
    #[error("malformed /Encrypt dictionary: {0}")]
    MalformedEncryptDict(&'static str),
}
