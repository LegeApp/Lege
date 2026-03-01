// src/jb2/context.rs

use crate::encode::zc::ZEncoder;
use crate::encode::jb2::error::Jb2Error;
use crate::encode::jb2::symbol_dict::BitImage;
use std::io::Write;

//-----------------------------------------------------------------------------
// DIRECT CODING (for dictionary symbols)
//-----------------------------------------------------------------------------

/// Compute the direct context for a pixel in a `BitImage`.
///
/// This is a 10-bit context used for encoding new symbols into the dictionary.
/// It only considers pixels from the image being encoded.
/// This function safely handles boundary conditions by treating any pixel
/// outside the image as white (false).
#[inline]
fn get_direct_context_image(image: &BitImage, x: i32, y: i32) -> usize {
    let get_pixel = |x: i32, y: i32| -> usize {
        if x < 0 || y < 0 || x >= image.width as i32 || y >= image.height as i32 {
            0 // Pixels outside the boundary are considered white (0).
        } else {
            image.get_pixel_unchecked(x as usize, y as usize) as usize
        }
    };

    (get_pixel(x - 1, y - 2) << 9)
        | (get_pixel(x, y - 2) << 8)
        | (get_pixel(x + 1, y - 2) << 7)
        | (get_pixel(x - 2, y - 1) << 6)
        | (get_pixel(x - 1, y - 1) << 5)
        | (get_pixel(x, y - 1) << 4)
        | (get_pixel(x + 1, y - 1) << 3)
        | (get_pixel(x + 2, y - 1) << 2)
        | (get_pixel(x - 2, y) << 1)
        | (get_pixel(x - 1, y) << 0)
}

//-----------------------------------------------------------------------------
// REFINEMENT CODING (for symbol instances)
//-----------------------------------------------------------------------------

/// Compute the refinement context for a pixel in `current` image, using `reference`
/// as the predictor.
///
/// This is a 13-bit context used for refinement coding. It combines information
/// from a 3x3 window in the reference symbol and a causal region of 4 pixels
/// in the symbol instance being coded.
///
/// This function safely handles boundary conditions by treating any pixel
/// outside the image as white (false).
#[inline]
fn _get_refinement_context(
    current: &BitImage,
    reference: &BitImage,
    x: i32,
    y: i32,
    cx_offset: i32,
    cy_offset: i32,
) -> usize {
    let get_current_pixel = |x: i32, y: i32| -> usize {
        if x < 0 || y < 0 || x >= current.width as i32 || y >= current.height as i32 {
            0
        } else {
            current.get_pixel_unchecked(x as usize, y as usize) as usize
        }
    };

    let get_ref_pixel = |x: i32, y: i32| -> usize {
        let rx = x + cx_offset;
        let ry = y + cy_offset;
        if rx < 0 || ry < 0 || rx >= reference.width as i32 || ry >= reference.height as i32 {
            0
        } else {
            reference.get_pixel_unchecked(rx as usize, ry as usize) as usize
        }
    };

    // 9 bits from the reference image (3x3 neighborhood)
    (get_ref_pixel(x - 1, y - 1) << 0) |
    (get_ref_pixel(x,     y - 1) << 1) |
    (get_ref_pixel(x + 1, y - 1) << 2) |
    (get_ref_pixel(x - 1, y)     << 3) |
    (get_ref_pixel(x,     y)     << 4) |
    (get_ref_pixel(x + 1, y)     << 5) |
    (get_ref_pixel(x - 1, y + 1) << 6) |
    (get_ref_pixel(x,     y + 1) << 7) |
    (get_ref_pixel(x + 1, y + 1) << 8) |
    // 4 bits from the already-coded part of the current image
    (get_current_pixel(x - 1, y)       << 9) |
    (get_current_pixel(x, y - 1)       << 10) |
    (get_current_pixel(x - 1, y - 1)   << 11) |
    (get_current_pixel(x - 2, y - 1)   << 12)
}

/// Encodes a `BitImage` using refinement/cross-coding against a reference bitmap.
///
/// This is used to encode a symbol instance that is a refinement of a symbol
/// from the dictionary.
pub fn encode_bitmap_refine<W: Write>(

    ac: &mut ZEncoder<W>,
    image: &BitImage,
    reference: &BitImage,
    cx_offset: i32, // relative offset of `image` from `reference`
    cy_offset: i32,
    base_context_index: usize,
) -> Result<(), Jb2Error> {
    // We need a temporary image to store the pixels we've already coded
    let mut temp_image = BitImage::new(
        image
            .width
            .try_into()
            .map_err(|_| Jb2Error::InvalidData("Width too large".to_string()))?,
        image
            .height
            .try_into()
            .map_err(|_| Jb2Error::InvalidData("Height too large".to_string()))?,
    )
    .map_err(|e| Jb2Error::InvalidData(e.to_string()))?;

    for y in 0..image.height as i32 {
        for x in 0..image.width as i32 {
            // Get the context for this pixel using both the reference and already-coded pixels
            let context = get_refinement_context_with_base(
                &temp_image,
                reference,
                x,
                y,
                cx_offset,
                cy_offset,
            );

            // Get the pixel value and encode it
            let pixel = image.get_pixel_unchecked(x as usize, y as usize);
            ac.encode(pixel, &mut ((context + base_context_index) as u8))?;

            // Update the temporary image with the pixel we just coded
            if pixel {
                temp_image.set_usize(x as usize, y as usize, true);
            }
        }
    }
    Ok(())
}

/// Encodes a full `BitImage` using the 10-bit direct coding context.
///
/// This function uses an efficient, row-based approach to minimize redundant
/// calculations and boundary checks, making it suitable for encoding entire symbols.
pub fn encode_bitmap_direct<W: Write>(

    ac: &mut ZEncoder<W>,
    image: &BitImage,
    base_context_index: usize,
) -> Result<(), Jb2Error> {
    // Process the image row by row
    for y in 0..image.height as i32 {
        for x in 0..image.width as i32 {
            // Get the context for this pixel
            let context = get_direct_context_image(image, x, y);
            let final_context = base_context_index + context;

            // Get the pixel value and encode it
            let pixel = image.get_pixel_unchecked(x as usize, y as usize);
            ac.encode(pixel, &mut (final_context as u8))?

        }
    }
    Ok(())
}

/// Gets the context for a pixel in the current image, using a reference image
/// for prediction. This is used during refinement coding.
fn get_refinement_context_with_base(
    current: &BitImage,
    reference: &BitImage,
    x: i32,
    y: i32,
    cx_offset: i32,
    cy_offset: i32,
) -> usize {
    let get_current_pixel = |x: i32, y: i32| -> usize {
        if x < 0 || y < 0 || x >= current.width as i32 || y >= current.height as i32 {
            0
        } else {
            current.get_pixel_unchecked(x as usize, y as usize) as usize
        }
    };

    let get_ref_pixel = |x: i32, y: i32| -> usize {
        let rx = x + cx_offset;
        let ry = y + cy_offset;
        if rx < 0 || ry < 0 || rx >= reference.width as i32 || ry >= reference.height as i32 {
            0
        } else {
            reference.get_pixel_unchecked(rx as usize, ry as usize) as usize
        }
    };

    (get_current_pixel(x - 1, y - 1) << 10)
        | (get_current_pixel(x, y - 1) << 9)
        | (get_current_pixel(x + 1, y - 1) << 8)
        | (get_current_pixel(x - 1, y) << 7)
        | (get_ref_pixel(x, y - 1) << 6)
        | (get_ref_pixel(x - 1, y) << 5)
        | (get_ref_pixel(x, y) << 4)
        | (get_ref_pixel(x + 1, y) << 3)
        | (get_ref_pixel(x - 1, y + 1) << 2)
        | (get_ref_pixel(x, y + 1) << 1)
        | (get_ref_pixel(x + 1, y + 1) << 0)
}
