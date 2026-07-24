//! PDF lexical analysis and primitive-object parsing (ISO 32000-1 §7.2–7.3).
//!
//! This crate knows *tokens and primitive values only*. It has no notion of
//! cross-reference tables, indirect-object resolution, or document structure
//! — that lives in `pdf-structure`/`pdf-document`. Keeping the lexer below
//! the xref layer means recovery scans (reading objects out of a damaged
//! file) reuse exactly the same tokenizer as the happy path.
//!
//! The lexer implementation is Phase 1 work; this crate currently fixes the
//! token vocabulary and error taxonomy it must produce.

pub mod classify;
mod lexer;

pub use lexer::Lexer;

/// Byte offset into the source. PDF files can exceed 4 GiB.
pub type Offset = u64;

/// A lexical token with its source span.
#[derive(Debug, Clone, PartialEq)]
pub struct Spanned<T> {
    pub value: T,
    pub start: Offset,
    pub end: Offset,
}

/// The PDF token vocabulary.
///
/// String/name tokens carry *decoded* bytes (escapes and `#xx` resolved);
/// the raw span is available via [`Spanned`].
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// `true` / `false`
    Boolean(bool),
    /// Integer literal. Out-of-range integers degrade to `Real` per common
    /// viewer behavior (compatibility decision — matches PDFium).
    Integer(i64),
    /// Real literal.
    Real(f64),
    /// `(...)` literal or `<...>` hex string, decoded to bytes.
    String(Vec<u8>),
    /// `/Name`, decoded (`#xx` resolved).
    Name(Vec<u8>),
    /// `[`
    ArrayOpen,
    /// `]`
    ArrayClose,
    /// `<<`
    DictOpen,
    /// `>>`
    DictClose,
    /// `null`
    Null,
    /// Any other regular-character run: operators in content streams,
    /// keywords (`obj`, `endobj`, `stream`, `R`, `xref`, ...) at file level.
    Keyword(Vec<u8>),
    /// End of input.
    Eof,
}

/// Lexer/parser errors. Distinct from *recovery events*: an error here means
/// the caller could not proceed; tolerated deviations are recorded on the
/// document's recovery log instead (blueprint §4.5).
#[derive(Debug, Clone, thiserror::Error)]
pub enum SyntaxError {
    #[error("unexpected byte 0x{byte:02x} at offset {offset}")]
    UnexpectedByte { byte: u8, offset: Offset },
    #[error("unterminated string starting at offset {offset}")]
    UnterminatedString { offset: Offset },
    #[error("malformed number at offset {offset}")]
    MalformedNumber { offset: Offset },
    #[error("token exceeds length limit at offset {offset}")]
    TokenTooLong { offset: Offset },
    #[error("nesting depth limit exceeded at offset {offset}")]
    TooDeep { offset: Offset },
    #[error("unexpected end of input at offset {offset}")]
    UnexpectedEof { offset: Offset },
    /// Object-level parse error: the token stream did not form a valid
    /// object where one was required (e.g. a value keyword in an array, a
    /// missing `obj` after `N G`).
    #[error("expected {expected} at offset {offset}")]
    UnexpectedToken { expected: &'static str, offset: Offset },
    #[error(transparent)]
    Source(#[from] pdf_source::SourceError),
}

/// Budgets applied while lexing/parsing a region. Owned by the worker's
/// parse context, threaded by reference — never global (blueprint §4.4).
#[derive(Debug, Clone)]
pub struct SyntaxLimits {
    /// Maximum decoded length of a single string/name token.
    pub max_token_bytes: usize,
    /// Maximum array/dictionary nesting depth.
    pub max_nesting_depth: usize,
}

impl Default for SyntaxLimits {
    fn default() -> Self {
        Self { max_token_bytes: 32 * 1024 * 1024, max_nesting_depth: 512 }
    }
}

// The primitive-object parser (tokens → `pdf_object::PdfObject`) lives in
// `pdf_object::parser`, per the workspace dependency direction
// (source ← syntax ← object): this crate cannot name the object model.
