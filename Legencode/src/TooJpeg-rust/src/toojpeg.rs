// This is a Rust port of TooJpeg (https://create.stephan-brumme.com/toojpeg/), originally written in C++ by Stephan Brumme.

/// 8-bit unsigned integer
pub type U8 = u8;
/// 16-bit unsigned integer
pub type U16 = u16;
/// 16-bit signed integer
pub type I16 = i16;
/// 32-bit signed integer
pub type I32 = i32;

const DEFAULT_QUANT_LUMINANCE: [U8; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56,
    14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113,
    92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];
const DEFAULT_QUANT_CHROMINANCE: [U8; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99, 99,
    47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
];
const ZIGZAG_INV: [U8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];
const DC_LUMINANCE_CODES_PER_BITSIZE: [U8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
const DC_LUMINANCE_VALUES: [U8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const AC_LUMINANCE_CODES_PER_BITSIZE: [U8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 125];
const AC_LUMINANCE_VALUES: [U8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xA1, 0x08, 0x23, 0x42, 0xB1, 0xC1, 0x15, 0x52, 0xD1, 0xF0,
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0A, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2A, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    0x4A, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6A, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7,
    0xA8, 0xA9, 0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xC2, 0xC3, 0xC4, 0xC5,
    0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8, 0xD9, 0xDA, 0xE1, 0xE2,
    0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA, 0xF1, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8,
    0xF9, 0xFA,
];
const DC_CHROMINANCE_CODES_PER_BITSIZE: [U8; 16] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
const DC_CHROMINANCE_VALUES: [U8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const AC_CHROMINANCE_CODES_PER_BITSIZE: [U8; 16] =
    [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 119];
const AC_CHROMINANCE_VALUES: [U8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71,
    0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xA1, 0xB1, 0xC1, 0x09, 0x23, 0x33, 0x52, 0xF0,
    0x15, 0x62, 0x72, 0xD1, 0x0A, 0x16, 0x24, 0x34, 0xE1, 0x25, 0xF1, 0x17, 0x18, 0x19, 0x1A, 0x26,
    0x27, 0x28, 0x29, 0x2A, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48,
    0x49, 0x4A, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68,
    0x69, 0x6A, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87,
    0x88, 0x89, 0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5,
    0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xC2, 0xC3,
    0xC4, 0xC5, 0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8, 0xD9, 0xDA,
    0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8,
    0xF9, 0xFA,
];
const CODE_WORD_LIMIT: I16 = 2048;

#[derive(Copy, Clone, Debug)]
pub struct BitCode {
    code: U16,
    num_bits: U8,
}

impl BitCode {
    const fn new(code: U16, num_bits: U8) -> Self {
        Self { code, num_bits }
    }
}

/// A bit writer for JPEG encoding
pub struct BitWriter<W: FnMut(U8) -> Result<(), &'static str>> {
    output: W,
    buffer: BitBuffer,
}

struct BitBuffer {
    data: u32,
    num_bits: u8,
}

impl<W: FnMut(U8) -> Result<(), &'static str>> BitWriter<W> {
    pub fn new(output: W) -> Self {
        Self {
            output,
            buffer: BitBuffer {
                data: 0,
                num_bits: 0,
            },
        }
    }

    pub fn write_byte(&mut self, byte: U8) -> Result<(), &'static str> {
        (self.output)(byte)
    }

    // Writes a byte and performs byte stuffing if necessary
    pub fn write_stuffed_byte(&mut self, byte: U8) -> Result<(), &'static str> {
        (self.output)(byte)?;
        if byte == 0xFF {
            (self.output)(0x00)?;
        }
        Ok(())
    }

    pub fn write_bytes(&mut self, bytes: &[U8]) -> Result<(), &'static str> {
        for &byte in bytes {
            self.write_byte(byte)?;
        }
        Ok(())
    }

    pub fn write_bits(&mut self, code: U16, num_bits: U8) -> Result<(), &'static str> {
        // 1. grow buffer first
        self.buffer.num_bits += num_bits;
        // 2. push old bits to the left
        self.buffer.data <<= num_bits as i32;
        // 3. drop the new bits into the now-free lsb positions
        self.buffer.data |= code as u32;

        while self.buffer.num_bits >= 8 {
            self.buffer.num_bits -= 8;
            let byte = ((self.buffer.data >> self.buffer.num_bits) & 0xFF) as U8;
            self.write_stuffed_byte(byte)?;
        }
        Ok(())
    }

    pub fn flush(&mut self) -> Result<(), &'static str> {
        // pad the buffer with 1-bits until we can produce one more byte
        if self.buffer.num_bits > 0 {
            let padding = 8 - self.buffer.num_bits;
            // shift old bits left
            self.buffer.data <<= padding;
            // OR in padding bits (all 1s)
            self.buffer.data |= (1u32 << padding) - 1;
            // we now have at least 8 bits, extract exactly one byte
            self.buffer.num_bits += padding as u8;
            self.buffer.num_bits -= 8;
            let byte = ((self.buffer.data >> self.buffer.num_bits) & 0xFF) as u8;
            self.write_stuffed_byte(byte)?;
        }
        Ok(())
    }

    pub fn add_marker(&mut self, marker: U8, length: U16) -> Result<(), &'static str> {
        self.write_byte(0xFF)?;
        self.write_byte(marker)?;
        self.write_byte((length >> 8) as U8)?;
        self.write_byte((length & 0xFF) as U8)
    }
}

fn minimum<T: PartialOrd>(value: T, maximum: T) -> T {
    if value <= maximum {
        value
    } else {
        maximum
    }
}

fn clamp<T: PartialOrd>(value: T, min_value: T, max_value: T) -> T {
    if value <= min_value {
        min_value
    } else if value >= max_value {
        max_value
    } else {
        value
    }
}

#[inline(always)]
pub fn rgb2y(r: u8, g: u8, b: u8) -> f32 {
    0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32
}

#[inline(always)]
fn rgb2cb(r: U8, g: U8, b: U8) -> f32 {
    -0.1687 * r as f32 - 0.3313 * g as f32 + 0.500 * b as f32
}

#[inline(always)]
fn rgb2cr(r: U8, g: U8, b: U8) -> f32 {
    0.500 * r as f32 - 0.4187 * g as f32 - 0.0813 * b as f32
}

fn dct(block: &mut [f32], stride: usize) {
    const SQRT_HALF_SQRT: f32 = 1.306562965;
    const INV_SQRT: f32 = 0.707106781;
    const HALF_SQRT_SQRT: f32 = 0.382683432;
    const INV_SQRT_SQRT: f32 = 0.541196100;

    let block0 = block[0];
    let block1 = block[1 * stride];
    let block2 = block[2 * stride];
    let block3 = block[3 * stride];
    let block4 = block[4 * stride];
    let block5 = block[5 * stride];
    let block6 = block[6 * stride];
    let block7 = block[7 * stride];

    let add07 = block0 + block7;
    let sub07 = block0 - block7;
    let add16 = block1 + block6;
    let sub16 = block1 - block6;
    let add25 = block2 + block5;
    let sub25 = block2 - block5;
    let add34 = block3 + block4;
    let sub34 = block3 - block4;

    let add0347 = add07 + add34;
    let sub07_34 = add07 - add34;
    let add1256 = add16 + add25;
    let sub16_25 = add16 - add25;

    block[0] = add0347 + add1256;
    block[4 * stride] = add0347 - add1256;

    let z1 = (sub16_25 + sub07_34) * INV_SQRT;
    block[2 * stride] = sub07_34 + z1;
    block[6 * stride] = sub07_34 - z1;

    let sub23_45 = sub25 + sub34;
    let sub12_56 = sub16 + sub25;
    let sub01_67 = sub16 + sub07;

    let z5 = (sub23_45 - sub01_67) * HALF_SQRT_SQRT;
    let z2 = sub23_45 * INV_SQRT_SQRT + z5;
    let z3 = sub12_56 * INV_SQRT;
    let z4 = sub01_67 * SQRT_HALF_SQRT + z5;
    let z6 = sub07 + z3;
    let z7 = sub07 - z3;
    block[1 * stride] = z6 + z4;
    block[7 * stride] = z6 - z4;
    block[5 * stride] = z7 + z2;
    block[3 * stride] = z7 - z2;
}

#[inline]
fn encode_block<W: FnMut(U8) -> Result<(), &'static str>>(
    writer: &mut BitWriter<W>,
    block: &mut [[f32; 8]; 8],
    scaled: &[f32; 64],
    last_dc: I16,
    huffman_dc: &[BitCode; 256],
    huffman_ac: &[BitCode; 256],
    codewords: &[BitCode; 4096],
) -> Result<I16, &'static str> {
    // Flatten block safely
    let mut block64 = [0.0f32; 64];
    for y in 0..8 {
        for x in 0..8 {
            block64[y * 8 + x] = block[y][x];
        }
    }
    for offset in 0..8 {
        dct(&mut block64[offset * 8..], 1);
    }
    for offset in 0..8 {
        dct(&mut block64[offset..], 8);
    }

    // CORRECT: multiply each coefficient by its matching scale factor
    for (idx, coeff) in block64.iter_mut().enumerate() {
        *coeff *= scaled[idx];
    }
    let dc = block64[0] as I32 + if block64[0] >= 0.0 { 0.5 } else { -0.5 } as I32;
    let dc = dc as I16;

    let mut pos_non_zero = 0;
    let mut quantized = [0; 64];
    for i in 1..64 {
        let value = block64[ZIGZAG_INV[i] as usize];
        quantized[i] = value as I16;
        if quantized[i] != 0 {
            pos_non_zero = i;
        }
    }

    let diff = dc - last_dc;

    // Clamp diff to valid range for codewords indexing
    let min_bound = -(CODE_WORD_LIMIT - 1);
    let max_bound = CODE_WORD_LIMIT - 1;
    let clamped_diff = diff.clamp(min_bound, max_bound);

    if clamped_diff == 0 {
        writer.write_bits(huffman_dc[0].code, huffman_dc[0].num_bits)?;
    } else {
        let index = (clamped_diff + CODE_WORD_LIMIT) as usize;
        let bits = codewords[index];
        writer.write_bits(
            huffman_dc[bits.num_bits as usize].code,
            huffman_dc[bits.num_bits as usize].num_bits,
        )?;
        writer.write_bits(bits.code, bits.num_bits)?;
    }

    let mut run = 0;
    for i in 1..64 {
        let ac = quantized[i];
        // Clamp AC coefficient to valid range for codewords indexing
        let clamped_ac = ac.clamp(-(CODE_WORD_LIMIT - 1), CODE_WORD_LIMIT - 1);
        let encoded = &codewords[(clamped_ac + CODE_WORD_LIMIT) as usize];

        if ac == 0 {
            run += 1;
        } else {
            while run >= 16 {
                writer.write_bits(huffman_ac[0xF0].code, huffman_ac[0xF0].num_bits)?;
                run -= 16;
            }

            let symbol = (run << 4) | encoded.num_bits as u8;

            writer.write_bits(
                huffman_ac[symbol as usize].code,
                huffman_ac[symbol as usize].num_bits,
            )?;
            writer.write_bits(encoded.code, encoded.num_bits)?;
            run = 0;
        }
    }

    if pos_non_zero < 63 {
        writer.write_bits(huffman_ac[0].code, huffman_ac[0].num_bits)?;
    }

    Ok(dc)
}

fn generate_huffman_table(num_codes: &[U8], values: &[U8], result: &mut [BitCode]) {
    let mut huffman_code = 0;
    let mut value_index = 0;
    for num_bits in 1..=16 {
        for _ in 0..num_codes[num_bits as usize - 1] {
            let value = values[value_index];
            result[value as usize] = BitCode::new(huffman_code, num_bits);
            huffman_code += 1;
            value_index += 1;
        }
        huffman_code <<= 1;
    }
}

/// Encode an image to JPEG format
///
/// # Arguments
/// * `writer` - Bit writer for output
/// * `pixels` - Image pixel data in either RGB (3 bytes per pixel) or grayscale (1 byte per pixel) format
/// * `width` - Image width in pixels
/// * `height` - Image height in pixels
/// * `is_rgb` - True if the input is in RGB format, false for grayscale
/// * `quality` - Encoding quality (1-100)
/// * `downsample` - Whether to downsample chroma channels (4:2:0 subsampling)
/// * `comment` - Optional comment to include in the JPEG file
///
/// # Returns
/// `Result<(), &'static str>` indicating success or an error message
pub fn write_jpeg<W: FnMut(U8) -> Result<(), &'static str>>(
    writer: &mut BitWriter<W>,
    pixels: &[U8],
    width: U16,
    height: U16,
    is_rgb: bool,
    quality: U8,
    downsample: bool,
    comment: Option<&str>,
) -> Result<(), &'static str> {
    // Use the writer directly instead of creating a new variable
    if width == 0 || height == 0 {
        return Err("Invalid image dimensions");
    }

    let num_components = if is_rgb { 3 } else { 1 };
    let downsample = downsample && is_rgb;
    // let mut bit_writer = BitWriter::new(&mut output); // Remove this line, not used

    writer.write_bytes(&[
        0xFF, 0xD8, 0xFF, 0xE0, 0, 16, b'J', b'F', b'I', b'F', 0, 1, 1, 0, 0, 1, 0, 1, 0, 0,
    ])?;

    if let Some(comment) = comment {
        let length = comment.len();
        writer.add_marker(0xFE, (length + 2) as U16)?;
        for c in comment.bytes() {
            writer.write_byte(c)?
        }
    }

    let quality = clamp(quality, 1, 100) as U16;
    let quality = if quality < 50 {
        5000 / quality
    } else {
        200 - quality * 2
    };

    let mut quant_luminance = [0; 64];
    let mut quant_chrominance = [0; 64];
    for i in 0..64 {
        let luminance =
            (DEFAULT_QUANT_LUMINANCE[ZIGZAG_INV[i] as usize] as U16 * quality + 50) / 100;
        let chrominance =
            (DEFAULT_QUANT_CHROMINANCE[ZIGZAG_INV[i] as usize] as U16 * quality + 50) / 100;
        quant_luminance[i] = clamp(luminance, 1, 255) as U8;
        quant_chrominance[i] = clamp(chrominance, 1, 255) as U8;
    }

    let table_length = 2 + (if is_rgb { 2 } else { 1 }) * (1 + 64);
    writer.add_marker(0xDB, table_length as U16)?;
    writer.write_byte(0)?;
    writer.write_bytes(&quant_luminance)?;
    if is_rgb {
        writer.write_byte(1)?;
        writer.write_bytes(&quant_chrominance)?;
    }

    let frame_length = 2 + 6 + 3 * num_components;
    writer.add_marker(0xC0, frame_length as U16)?;
    writer.write_byte(8)?;
    writer.write_byte((height >> 8) as U8)?;
    writer.write_byte(height as U8)?;
    writer.write_byte((width >> 8) as U8)?;
    writer.write_byte(width as U8)?;
    writer.write_byte(num_components as U8)?;
    for id in 1..=num_components {
        writer.write_byte(id as U8)?;
        writer.write_byte(if id == 1 && downsample { 0x22 } else { 0x11 })?;
        writer.write_byte(if id == 1 { 0 } else { 1 })?;
    }

    let htable_length = if is_rgb { 2 + 208 + 208 } else { 2 + 208 };
    writer.add_marker(0xC4, htable_length as U16)?;
    writer.write_byte(0)?;
    writer.write_bytes(&DC_LUMINANCE_CODES_PER_BITSIZE)?;
    writer.write_bytes(&DC_LUMINANCE_VALUES)?;
    writer.write_byte(0x10)?;
    writer.write_bytes(&AC_LUMINANCE_CODES_PER_BITSIZE)?;
    writer.write_bytes(&AC_LUMINANCE_VALUES)?;

    let mut huffman_luminance_dc = [BitCode::new(0, 0); 256];
    let mut huffman_luminance_ac = [BitCode::new(0, 0); 256];
    generate_huffman_table(
        &DC_LUMINANCE_CODES_PER_BITSIZE,
        &DC_LUMINANCE_VALUES,
        &mut huffman_luminance_dc,
    );
    generate_huffman_table(
        &AC_LUMINANCE_CODES_PER_BITSIZE,
        &AC_LUMINANCE_VALUES,
        &mut huffman_luminance_ac,
    );

    let mut huffman_chrominance_dc = [BitCode::new(0, 0); 256];
    let mut huffman_chrominance_ac = [BitCode::new(0, 0); 256];
    if is_rgb {
        writer.write_byte(1)?;
        writer.write_bytes(&DC_CHROMINANCE_CODES_PER_BITSIZE)?;
        writer.write_bytes(&DC_CHROMINANCE_VALUES)?;
        writer.write_byte(0x11)?;
        writer.write_bytes(&AC_CHROMINANCE_CODES_PER_BITSIZE)?;
        writer.write_bytes(&AC_CHROMINANCE_VALUES)?;
        generate_huffman_table(
            &DC_CHROMINANCE_CODES_PER_BITSIZE,
            &DC_CHROMINANCE_VALUES,
            &mut huffman_chrominance_dc,
        );
        generate_huffman_table(
            &AC_CHROMINANCE_CODES_PER_BITSIZE,
            &AC_CHROMINANCE_VALUES,
            &mut huffman_chrominance_ac,
        );
    }

    let scan_length = 2 + 1 + 2 * num_components + 3;
    writer.add_marker(0xDA, scan_length as U16)?;
    writer.write_byte(num_components as U8)?;
    for id in 1..=num_components {
        writer.write_byte(id as U8)?;
        writer.write_byte(if id == 1 { 0x00 } else { 0x11 })?;
    }
    writer.write_bytes(&[0, 63, 0])?;

    const AAN_SCALE_FACTORS: [f32; 8] = [
        1.0,
        1.387039845,
        1.306562965,
        1.175875602,
        1.0,
        0.785694958,
        0.541196100,
        0.275899379,
    ];
    // Precompute zigzag-scaled quantization tables
    let mut scaled_luminance_zigzag = [0.0; 64];
    let mut scaled_chrominance_zigzag = [0.0; 64];
    for i in 0..64 {
        let row = ZIGZAG_INV[i] as usize / 8;
        let column = ZIGZAG_INV[i] as usize % 8;
        let factor = 1.0 / (AAN_SCALE_FACTORS[row] * AAN_SCALE_FACTORS[column] * 8.0);
        scaled_luminance_zigzag[i] = factor / quant_luminance[i] as f32;
        scaled_chrominance_zigzag[i] = factor / quant_chrominance[i] as f32;
    }

    let mut scaled_lum_row = [0.0f32; 64];
    let mut scaled_ch_row = [0.0f32; 64];
    for zig in 0..64 {
        let pos = ZIGZAG_INV[zig] as usize;
        scaled_lum_row[pos] = scaled_luminance_zigzag[zig];
        scaled_ch_row[pos] = scaled_chrominance_zigzag[zig];
    }

    let mut codewords_array = [BitCode::new(0, 0); 2 * CODE_WORD_LIMIT as usize];
    let _codewords = &mut codewords_array[CODE_WORD_LIMIT as usize..];
    let mut num_bits = 1;
    let mut mask = 1;
    for value in 1..CODE_WORD_LIMIT {
        if value > mask {
            num_bits += 1;
            mask = (mask << 1) | 1;
        }
        codewords_array[(CODE_WORD_LIMIT - value) as usize] =
            BitCode::new((mask - value) as U16, num_bits);
        codewords_array[(CODE_WORD_LIMIT + value) as usize] = BitCode::new(value as U16, num_bits);
    }

    let max_width = width - 1;
    let max_height = height - 1;
    let sampling = if downsample { 2 } else { 1 };
    let mcu_size = 8 * sampling;
    let mut last_y_dc = 0;
    let mut last_cb_dc = 0;
    let mut last_cr_dc = 0;
    let mut y_block = [[0.0; 8]; 8];
    let mut cb_block = [[0.0; 8]; 8];
    let mut cr_block = [[0.0; 8]; 8];

    for mcu_y in (0..height).step_by(mcu_size as usize) {
        for mcu_x in (0..width).step_by(mcu_size as usize) {
            // Y block processing
            for _block_y in (0..mcu_size).step_by(8) {
                for _block_x in (0..mcu_size).step_by(8) {
                    for delta_y in 0..8 {
                        let row = minimum(mcu_y + _block_y + delta_y, max_height);
                        let mut column = minimum(mcu_x + _block_x, max_width);
                        for delta_x in 0..8 {
                            let pixel_pos = (row as usize * width as usize + column as usize)
                                * if is_rgb { 3 } else { 1 };
                            column = if column < max_width {
                                column + 1
                            } else {
                                column
                            };
                            if !is_rgb {
                                y_block[delta_y as usize][delta_x as usize] =
                                    pixels[pixel_pos] as f32 - 128.0;
                            } else {
                                let r = pixels[pixel_pos];
                                let g = pixels[pixel_pos + 1];
                                let b = pixels[pixel_pos + 2];
                                y_block[delta_y as usize][delta_x as usize] =
                                    rgb2y(r, g, b) - 128.0;

                                if !downsample {
                                    cb_block[delta_y as usize][delta_x as usize] = rgb2cb(r, g, b);
                                    cr_block[delta_y as usize][delta_x as usize] = rgb2cr(r, g, b);
                                }
                            }
                        }
                    }
                    last_y_dc = encode_block(
                        writer,
                        &mut y_block,
                        &scaled_lum_row,
                        last_y_dc,
                        &huffman_luminance_dc,
                        &huffman_luminance_ac,
                        &codewords_array,
                    )?;
                }
            }

            // Chroma block processing
            if is_rgb {
                if downsample {
                    for delta_y in (0..8).rev() {
                        let row = minimum(mcu_y + 2 * delta_y, max_height);
                        let mut column = mcu_x;
                        let mut pixel_pos = (row as usize * width as usize + column as usize) * 3;
                        let row_step = if row < max_height {
                            width as usize * 3
                        } else {
                            0
                        };
                        let mut column_step = if column < max_width { 3 } else { 0 };

                        for delta_x in 0..8 {
                            let right = pixel_pos + column_step;
                            let down = pixel_pos + row_step;
                            let down_right = pixel_pos + column_step + row_step;

                            let r_sum = pixels[pixel_pos] as u32
                                + pixels[right] as u32
                                + pixels[down] as u32
                                + pixels[down_right] as u32;
                            let g_sum = pixels[pixel_pos + 1] as u32
                                + pixels[right + 1] as u32
                                + pixels[down + 1] as u32
                                + pixels[down_right + 1] as u32;
                            let b_sum = pixels[pixel_pos + 2] as u32
                                + pixels[right + 2] as u32
                                + pixels[down + 2] as u32
                                + pixels[down_right + 2] as u32;

                            // Compute average with rounding
                            let r_avg = ((r_sum + 2) / 4) as u8;
                            let g_avg = ((g_sum + 2) / 4) as u8;
                            let b_avg = ((b_sum + 2) / 4) as u8;

                            cb_block[delta_y as usize][delta_x as usize] =
                                rgb2cb(r_avg, g_avg, b_avg);
                            cr_block[delta_y as usize][delta_x as usize] =
                                rgb2cr(r_avg, g_avg, b_avg);

                            pixel_pos += 6;
                            column += 2;
                            if column >= max_width {
                                column_step = 0;
                                pixel_pos = ((row as usize + 1) * width as usize - 1) * 3;
                            }
                        }
                    }

                    last_cb_dc = encode_block(
                        writer,
                        &mut cb_block,
                        &scaled_ch_row,
                        last_cb_dc,
                        &huffman_chrominance_dc,
                        &huffman_chrominance_ac,
                        &codewords_array,
                    )?;
                    last_cr_dc = encode_block(
                        writer,
                        &mut cr_block,
                        &scaled_ch_row,
                        last_cr_dc,
                        &huffman_chrominance_dc,
                        &huffman_chrominance_ac,
                        &codewords_array,
                    )?;
                } else {
                    // Non-downsampled chroma (4:4:4)
                    for _block_y in (0..mcu_size).step_by(8) {
                        for _block_x in (0..mcu_size).step_by(8) {
                            // This part of the code needs to be filled based on how 4:4:4 chroma is handled
                            // Currently, the `cb_block` and `cr_block` are filled within the Y loop for 4:4:4
                            // So, we just need to encode them here.
                            last_cb_dc = encode_block(
                                writer,
                                &mut cb_block,
                                &scaled_ch_row,
                                last_cb_dc,
                                &huffman_chrominance_dc,
                                &huffman_chrominance_ac,
                                &codewords_array,
                            )?;
                            last_cr_dc = encode_block(
                                writer,
                                &mut cr_block,
                                &scaled_ch_row,
                                last_cr_dc,
                                &huffman_chrominance_dc,
                                &huffman_chrominance_ac,
                                &codewords_array,
                            )?;
                        }
                    }
                }
            }
        }
    }

    // Write End Of Image
    writer.flush()?;
    writer.write_byte(0xFF)?;
    writer.write_byte(0xD9)?;

    Ok(())
}
