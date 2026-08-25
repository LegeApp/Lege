use crate::error::{Jp2LamError, Result};
#[cfg(test)]
use crate::model::Image;
use crate::model::ImageView;

/// Only used by the `#[cfg(test)]`-gated `EncodingPlan::build`; live callers
/// go through `validate_image_view` via `EncodingPlan::build_view` instead.
#[cfg(test)]
pub(super) fn validate_image(image: &Image) -> Result<()> {
    if image.width == 0 || image.height == 0 {
        return Err(Jp2LamError::InvalidInput(
            "image dimensions must be non-zero".to_string(),
        ));
    }

    if image.components.len() != image.colorspace.component_count() {
        return Err(Jp2LamError::InvalidInput(format!(
            "{:?} images must have exactly {} component(s)",
            image.colorspace,
            image.colorspace.component_count()
        )));
    }

    for (idx, component) in image.components.iter().enumerate() {
        if component.width != image.width || component.height != image.height {
            return Err(Jp2LamError::InvalidInput(format!(
                "component {idx} dimensions {}x{} do not match image {}x{}",
                component.width, component.height, image.width, image.height
            )));
        }
        if !(8..=16).contains(&component.precision) {
            return Err(Jp2LamError::InvalidInput(format!(
                "component {idx} precision {} is outside the supported 8..=16 range",
                component.precision
            )));
        }
        if component.signed {
            return Err(Jp2LamError::InvalidInput(format!(
                "component {idx} must be unsigned"
            )));
        }
        if component.dx != 1 || component.dy != 1 {
            return Err(Jp2LamError::InvalidInput(format!(
                "component {idx} subsampling {}x{} is unsupported",
                component.dx, component.dy
            )));
        }
        let expected_len = (component.width as usize) * (component.height as usize);
        if component.data.len() != expected_len {
            return Err(Jp2LamError::InvalidInput(format!(
                "component {idx} has {} samples, expected {expected_len}",
                component.data.len()
            )));
        }
        let max_sample = (1i32 << component.precision) - 1;
        if component
            .data
            .iter()
            .any(|&sample| !(0..=max_sample).contains(&sample))
        {
            return Err(Jp2LamError::InvalidInput(format!(
                "component {idx} contains samples outside 0..={max_sample}"
            )));
        }
    }

    Ok(())
}

pub(super) fn validate_image_view(image: &ImageView<'_>) -> Result<()> {
    if image.width == 0 || image.height == 0 {
        return Err(Jp2LamError::InvalidInput(
            "image dimensions must be non-zero".to_string(),
        ));
    }

    if image.components.len() != image.colorspace.component_count() {
        return Err(Jp2LamError::InvalidInput(format!(
            "{:?} images must have exactly {} component(s)",
            image.colorspace,
            image.colorspace.component_count()
        )));
    }

    for (idx, component) in image.components.iter().enumerate() {
        if component.width != image.width || component.height != image.height {
            return Err(Jp2LamError::InvalidInput(format!(
                "component {idx} dimensions {}x{} do not match image {}x{}",
                component.width, component.height, image.width, image.height
            )));
        }
        if !(8..=16).contains(&component.precision) {
            return Err(Jp2LamError::InvalidInput(format!(
                "component {idx} precision {} is outside the supported 8..=16 range",
                component.precision
            )));
        }
        if component.signed {
            return Err(Jp2LamError::InvalidInput(format!(
                "component {idx} must be unsigned"
            )));
        }
        if component.dx != 1 || component.dy != 1 {
            return Err(Jp2LamError::InvalidInput(format!(
                "component {idx} subsampling {}x{} is unsupported",
                component.dx, component.dy
            )));
        }
    }

    if image.components.len() == 3
        && image
            .components
            .iter()
            .any(|component| component.precision != image.components[0].precision)
    {
        return Err(Jp2LamError::InvalidInput(
            "RGB component transformation requires equal component precision".into(),
        ));
    }

    Ok(())
}
