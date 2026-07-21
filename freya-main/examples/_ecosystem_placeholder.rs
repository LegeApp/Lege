//! Placeholder target for the vendored `freya-main` workspace root package
//! (`examples`). The upstream example sources are not present in this checkout,
//! which otherwise leaves the root package with no Cargo targets.
//!
//! Lege does not build this target; Cargo only needs it to parse freya-main's
//! workspace manifest while resolving the GUI's local `freya` dependency.
fn main() {}
