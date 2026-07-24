//! Optional content (OCG) default-configuration visibility (ISO 32000-1
//! §8.11; PDFium `core/fpdfapi/page/cpdf_occontext.cpp`).
//!
//! Only the catalog's `/OCProperties /D` *default* configuration is modeled —
//! `/BaseState`, `/ON`, `/OFF` — with PDFium's default-visibility semantics:
//! a group is **visible unless configured off**. Viewer UI state and `/AS`
//! usage-application dictionaries are out of scope. Groups are identified by
//! object id, matching how `/ON`//`/OFF` reference them.

use std::collections::HashSet;

use pdf_object::{ObjectId, PdfObject};
use pdf_structure::RecoveryEvent;

use crate::resolve::Resolver;
use crate::ParseContext;

/// The `/D` default optional-content configuration, frozen at open.
#[derive(Debug, Clone, Default)]
pub struct OcgConfig {
    /// `/BaseState /OFF` flips the default (rare; default is `/ON`).
    base_off: bool,
    /// Groups the config turns on (only meaningful under `/BaseState /OFF`).
    on: HashSet<ObjectId>,
    /// Groups the config turns off. Applied after `/ON`, so it wins ties —
    /// the order `cpdf_occontext.cpp` applies them in.
    off: HashSet<ObjectId>,
}

impl OcgConfig {
    /// Whether the OCG with object id `id` is visible under this
    /// configuration. Default-visibility: visible unless configured off.
    pub fn visible(&self, id: ObjectId) -> bool {
        if self.off.contains(&id) {
            return false;
        }
        !self.base_off || self.on.contains(&id)
    }
}

/// Parse the catalog's `/OCProperties /D` config. Recovery-driven: anything
/// malformed degrades to the default (everything visible), never fatal.
pub(crate) fn parse_ocproperties(
    resolver: &Resolver<'_>,
    ctx: &mut ParseContext,
) -> OcgConfig {
    let names = resolver.names;
    let mut config = OcgConfig::default();

    let Ok(catalog) = resolver.resolve(resolver.structure.trailer.root, ctx) else {
        return config;
    };
    let Some(catalog_dict) = catalog.as_dict() else {
        return config;
    };
    let Some(ocprops_raw) = catalog_dict.get(names.intern(b"OCProperties")) else {
        return config;
    };
    let Ok(ocprops) = resolver.resolve_value(ocprops_raw, ctx) else {
        ctx.recovery.push(RecoveryEvent::Other(
            "unreadable /OCProperties; all optional content visible".into(),
        ));
        return config;
    };
    let Some(d_raw) = ocprops.as_dict().and_then(|d| d.get(names.intern(b"D"))) else {
        return config;
    };
    let Ok(d) = resolver.resolve_value(d_raw, ctx) else {
        ctx.recovery.push(RecoveryEvent::Other(
            "unreadable /OCProperties /D; all optional content visible".into(),
        ));
        return config;
    };
    let Some(d) = d.as_dict() else {
        return config;
    };

    if let Some(state) = d.get(names.intern(b"BaseState")).and_then(PdfObject::as_name) {
        config.base_off = names.resolve(state).as_ref() == b"OFF";
    }
    config.on = ref_set(d.get(names.intern(b"ON")), resolver, ctx);
    config.off = ref_set(d.get(names.intern(b"OFF")), resolver, ctx);
    config
}

/// Collect the object ids referenced by a (possibly indirect) array value.
/// Non-reference entries are skipped — group identity is by reference.
fn ref_set(
    value: Option<&PdfObject>,
    resolver: &Resolver<'_>,
    ctx: &mut ParseContext,
) -> HashSet<ObjectId> {
    let mut out = HashSet::new();
    let Some(value) = value else { return out };
    let Ok(resolved) = resolver.resolve_value(value, ctx) else {
        return out;
    };
    if let PdfObject::Array(items) = &*resolved {
        for item in items.iter() {
            if let Some(id) = item.as_reference() {
                out.insert(id);
            }
        }
    }
    out
}
