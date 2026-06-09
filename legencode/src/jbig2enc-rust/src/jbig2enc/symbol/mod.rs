// Submodule declarations for the symbol-dictionary/text-region machinery.
// No logic here — see REFACTOR_JBIG2ENC.md for the file map.

pub(crate) mod clustering;
pub(crate) mod dictionary;
pub(crate) mod extraction;
pub(crate) mod planning;
pub(crate) mod serialize;
pub(crate) mod text_region;
pub(crate) mod text_region_refine;
pub(crate) mod types;
pub(crate) mod unify_apply;
pub(crate) mod unify_match;

#[cfg(test)]
pub(crate) mod tests;
