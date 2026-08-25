mod irrev97;
pub(crate) mod norms;
pub(crate) mod pcrd;
mod rev53;

#[derive(Debug, Default)]
pub(crate) struct InverseDwtTiming {
    pub(crate) horizontal_ns: u64,
    pub(crate) vertical_ns: u64,
    pub(crate) level_ns: Vec<u64>,
}

impl InverseDwtTiming {
    pub(crate) fn record_level(
        &mut self,
        horizontal: std::time::Duration,
        vertical: std::time::Duration,
    ) {
        let horizontal = u64::try_from(horizontal.as_nanos()).unwrap_or(u64::MAX);
        let vertical = u64::try_from(vertical.as_nanos()).unwrap_or(u64::MAX);
        self.horizontal_ns = self.horizontal_ns.saturating_add(horizontal);
        self.vertical_ns = self.vertical_ns.saturating_add(vertical);
        self.level_ns.push(horizontal.saturating_add(vertical));
    }
}

// `forward_*_2d_in_place`/`forward_*_2d_in_place_wide` are only reached by
// `simd::scalar`/`simd::wide`'s `#[cfg(test)]` cross-check functions
// (production always goes through the tile-origin `_at` variants below) — see
// `simd::scalar::forward_97_2d`'s doc comment. `inverse_*_2d_in_place[_wide]`
// stay unconditional: they back the live `PRIMITIVES.dwt.inverse_*` dispatch
// entries used by decode.
#[cfg(all(test, feature = "simd"))]
pub(crate) use irrev97::forward_97_2d_in_place;
pub(crate) use irrev97::forward_97_2d_in_place_at;
#[cfg(all(test, feature = "simd"))]
pub(crate) use irrev97::forward_97_2d_in_place_wide;
pub(crate) use irrev97::inverse_97_2d_in_place;
pub(crate) use irrev97::inverse_97_2d_in_place_at;
pub(crate) use irrev97::inverse_97_2d_in_place_profiled;
#[cfg(feature = "simd")]
pub(crate) use irrev97::inverse_97_2d_in_place_wide;
#[cfg(all(test, feature = "simd"))]
pub(crate) use rev53::forward_53_2d_in_place;
pub(crate) use rev53::forward_53_2d_in_place_at;
#[cfg(all(test, feature = "simd"))]
pub(crate) use rev53::forward_53_2d_in_place_wide;
pub(crate) use rev53::inverse_53_2d_in_place;
pub(crate) use rev53::inverse_53_2d_in_place_at;
pub(crate) use rev53::inverse_53_2d_in_place_profiled;
#[cfg(feature = "simd")]
pub(crate) use rev53::inverse_53_2d_in_place_wide;
