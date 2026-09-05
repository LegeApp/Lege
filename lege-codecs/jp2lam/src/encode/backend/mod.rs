pub(crate) mod native;

use super::context::EncodeContext;
use crate::error::Result;

pub(crate) trait CodestreamBackend {
    fn supports(&self, context: &EncodeContext<'_>) -> bool;
    fn encode_codestream(&self, context: &EncodeContext<'_>) -> Result<Vec<u8>>;
}

pub(crate) use native::{NativeBackend, NativeEncodedTier1Layout, NativeEncodedTier1Pass};
