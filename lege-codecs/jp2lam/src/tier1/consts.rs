pub(crate) const T1_SIGMA_0: u32 = 1u32 << 0;
pub(crate) const T1_SIGMA_1: u32 = 1u32 << 1;
pub(crate) const T1_SIGMA_2: u32 = 1u32 << 2;
pub(crate) const T1_SIGMA_3: u32 = 1u32 << 3;
#[allow(dead_code)]
pub(crate) const T1_SIGMA_4: u32 = 1u32 << 4;
pub(crate) const T1_SIGMA_5: u32 = 1u32 << 5;
pub(crate) const T1_SIGMA_6: u32 = 1u32 << 6;
pub(crate) const T1_SIGMA_7: u32 = 1u32 << 7;
pub(crate) const T1_SIGMA_8: u32 = 1u32 << 8;

pub(crate) const T1_SIGMA_NW: u32 = T1_SIGMA_0;
pub(crate) const T1_SIGMA_N: u32 = T1_SIGMA_1;
pub(crate) const T1_SIGMA_NE: u32 = T1_SIGMA_2;
pub(crate) const T1_SIGMA_W: u32 = T1_SIGMA_3;
#[allow(dead_code)]
pub(crate) const T1_SIGMA_THIS: u32 = T1_SIGMA_4;
pub(crate) const T1_SIGMA_E: u32 = T1_SIGMA_5;
pub(crate) const T1_SIGMA_SW: u32 = T1_SIGMA_6;
pub(crate) const T1_SIGMA_S: u32 = T1_SIGMA_7;
pub(crate) const T1_SIGMA_SE: u32 = T1_SIGMA_8;

pub(crate) const T1_SIGMA_NEIGHBOURS: u32 = T1_SIGMA_NW
    | T1_SIGMA_N
    | T1_SIGMA_NE
    | T1_SIGMA_W
    | T1_SIGMA_E
    | T1_SIGMA_SW
    | T1_SIGMA_S
    | T1_SIGMA_SE;

pub(crate) const T1_LUT_SGN_W: u32 = 1u32 << 0;
pub(crate) const T1_LUT_SIG_N: u32 = 1u32 << 1;
pub(crate) const T1_LUT_SGN_E: u32 = 1u32 << 2;
pub(crate) const T1_LUT_SIG_W: u32 = 1u32 << 3;
pub(crate) const T1_LUT_SGN_N: u32 = 1u32 << 4;
pub(crate) const T1_LUT_SIG_E: u32 = 1u32 << 5;
pub(crate) const T1_LUT_SGN_S: u32 = 1u32 << 6;
pub(crate) const T1_LUT_SIG_S: u32 = 1u32 << 7;
