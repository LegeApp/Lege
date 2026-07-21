//! Reference scalar kernels. Always compiled, always correct — every other
//! backend is validated against this one bit-for-bit (see `wide` module tests).

use std::sync::OnceLock;

type YccTables = ([[i32; 256]; 3], [[i32; 256]; 3], [[i32; 256]; 3]);

static YCC_TABLES: OnceLock<YccTables> = OnceLock::new();

pub(super) fn ycc_tables() -> &'static YccTables {
    YCC_TABLES.get_or_init(|| {
        let mut y = [[0; 256]; 3];
        let mut cb = [[0; 256]; 3];
        let mut cr = [[0; 256]; 3];

        const RGB_TO_YCC: [[f32; 3]; 3] = [
            [0.304348, 0.608696, 0.086956],
            [0.463768, -0.405797, -0.057971],
            [-0.173913, -0.347826, 0.521739],
        ];

        for k in 0..256 {
            y[0][k] = (k as f32 * 65536.0 * RGB_TO_YCC[0][0]) as i32;
            y[1][k] = (k as f32 * 65536.0 * RGB_TO_YCC[0][1]) as i32;
            y[2][k] = (k as f32 * 65536.0 * RGB_TO_YCC[0][2]) as i32;

            cb[0][k] = (k as f32 * 65536.0 * RGB_TO_YCC[2][0]) as i32;
            cb[1][k] = (k as f32 * 65536.0 * RGB_TO_YCC[2][1]) as i32;
            cb[2][k] = (k as f32 * 65536.0 * RGB_TO_YCC[2][2]) as i32;

            cr[0][k] = (k as f32 * 65536.0 * RGB_TO_YCC[1][0]) as i32;
            cr[1][k] = (k as f32 * 65536.0 * RGB_TO_YCC[1][1]) as i32;
            cr[2][k] = (k as f32 * 65536.0 * RGB_TO_YCC[1][2]) as i32;
        }
        (y, cb, cr)
    })
}

/// Convert interleaved RGB bytes to planar YCbCr (i8, DjVu IW44 convention).
///
/// Callers must ensure `img_raw.len() == 3 * out_y.len() == 3 * out_cb.len() == 3 * out_cr.len()`;
/// this kernel does not re-validate lengths (the public wrapper in
/// `encode::iw44::encoder` does).
pub fn rgb_to_ycbcr(img_raw: &[u8], out_y: &mut [i8], out_cb: &mut [i8], out_cr: &mut [i8]) {
    let (y_tbl, cb_tbl, cr_tbl) = ycc_tables();

    for (i, chunk) in img_raw.chunks_exact(3).enumerate() {
        let r = chunk[0] as usize;
        let g = chunk[1] as usize;
        let b = chunk[2] as usize;

        let y = y_tbl[0][r] + y_tbl[1][g] + y_tbl[2][b] + 32768;
        out_y[i] = ((y >> 16) - 128) as i8;

        let cb = cb_tbl[0][r] + cb_tbl[1][g] + cb_tbl[2][b] + 32768;
        out_cb[i] = (cb >> 16).clamp(-128, 127) as i8;

        let cr = cr_tbl[0][r] + cr_tbl[1][g] + cr_tbl[2][b] + 32768;
        out_cr[i] = (cr >> 16).clamp(-128, 127) as i8;
    }
}
