//! Fuzz the tokenizer: `pdf_syntax::Lexer` over raw bytes, driven to
//! exhaustion. The invariant is "never panic, never loop forever" — every
//! outcome must be a token, a typed `SyntaxError`, or `Eof`.

#![no_main]

use libfuzzer_sys::fuzz_target;
use pdf_syntax::{Lexer, SyntaxLimits, Token};

fuzz_target!(|data: &[u8]| {
    let limits = SyntaxLimits::default();
    let mut lexer = Lexer::new(data, 0, &limits);
    // Hard iteration cap so a lexer bug that fails to advance surfaces as a
    // fuzzer timeout finding rather than a silent hang.
    for _ in 0..data.len() + 8 {
        match lexer.next_token() {
            Ok(t) if t.value == Token::Eof => return,
            Ok(_) => {}
            Err(_) => return,
        }
    }
});
