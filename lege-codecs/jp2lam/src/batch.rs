use crate::error::{Jp2LamError, Result};
use crate::model::{ColorSpace, Component, EncodeOptions, Image, OutputFormat, RateControl};
use crate::{decode_jp2, encode};

#[derive(Debug, Clone, PartialEq)]
pub struct BatchProfile {
    pub width: u32,
    pub height: u32,
    pub colorspace: ColorSpace,
    pub components: Vec<BatchComponentProfile>,
    pub quality: u8,
    pub rate_control: Option<RateControl>,
    pub format: OutputFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchComponentProfile {
    pub width: u32,
    pub height: u32,
    pub precision: u32,
    pub signed: bool,
    pub dx: u32,
    pub dy: u32,
}

#[derive(Debug, Clone)]
pub struct BatchEncoder {
    options: EncodeOptions,
    profile: Option<BatchProfile>,
    encoded_count: usize,
}

#[derive(Debug, Clone, Default)]
pub struct BatchDecoder {
    profile: Option<BatchProfile>,
    decoded_count: usize,
}

impl BatchEncoder {
    pub fn new(options: EncodeOptions) -> Self {
        Self {
            options,
            profile: None,
            encoded_count: 0,
        }
    }

    pub fn profile(&self) -> Option<&BatchProfile> {
        self.profile.as_ref()
    }

    pub fn encoded_count(&self) -> usize {
        self.encoded_count
    }

    pub fn encode_one(&mut self, image: &Image) -> Result<Vec<u8>> {
        self.validate_or_set_profile(image)?;
        let encoded = encode(image, &self.options)?;
        self.encoded_count += 1;
        Ok(encoded)
    }

    pub fn encode_all<'a, I>(&mut self, images: I) -> Result<Vec<Vec<u8>>>
    where
        I: IntoIterator<Item = &'a Image>,
    {
        let mut encoded = Vec::new();
        for image in images {
            encoded.push(self.encode_one(image)?);
        }
        Ok(encoded)
    }

    fn validate_or_set_profile(&mut self, image: &Image) -> Result<()> {
        let profile = BatchProfile::from_image(image, &self.options);
        match &self.profile {
            Some(expected) if expected != &profile => Err(profile_mismatch(expected, &profile)),
            Some(_) => Ok(()),
            None => {
                self.profile = Some(profile);
                Ok(())
            }
        }
    }
}

impl BatchDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn profile(&self) -> Option<&BatchProfile> {
        self.profile.as_ref()
    }

    pub fn decoded_count(&self) -> usize {
        self.decoded_count
    }

    pub fn decode_one(&mut self, bytes: &[u8]) -> Result<Image> {
        let image = decode_jp2(bytes)?;
        self.validate_or_set_profile(&image)?;
        self.decoded_count += 1;
        Ok(image)
    }

    pub fn decode_all<'a, I>(&mut self, streams: I) -> Result<Vec<Image>>
    where
        I: IntoIterator<Item = &'a [u8]>,
    {
        let mut decoded = Vec::new();
        for bytes in streams {
            decoded.push(self.decode_one(bytes)?);
        }
        Ok(decoded)
    }

    fn validate_or_set_profile(&mut self, image: &Image) -> Result<()> {
        let profile = BatchProfile::for_decode(image);
        match &self.profile {
            Some(expected) if !expected.same_image_profile(&profile) => {
                Err(profile_mismatch(expected, &profile))
            }
            Some(_) => Ok(()),
            None => {
                self.profile = Some(profile);
                Ok(())
            }
        }
    }
}

impl BatchProfile {
    pub fn from_image(image: &Image, options: &EncodeOptions) -> Self {
        Self {
            width: image.width,
            height: image.height,
            colorspace: image.colorspace,
            components: image
                .components
                .iter()
                .map(BatchComponentProfile::from)
                .collect(),
            quality: options.quality,
            rate_control: options.rate_control,
            format: options.format,
        }
    }

    fn for_decode(image: &Image) -> Self {
        Self::from_image(
            image,
            &EncodeOptions {
                quality: 0,
                format: OutputFormat::Jp2,
                profile: Default::default(),
                ..Default::default()
            },
        )
    }

    fn same_image_profile(&self, other: &Self) -> bool {
        self.width == other.width
            && self.height == other.height
            && self.colorspace == other.colorspace
            && self.components == other.components
    }
}

impl From<&Component> for BatchComponentProfile {
    fn from(component: &Component) -> Self {
        Self {
            width: component.width,
            height: component.height,
            precision: component.precision,
            signed: component.signed,
            dx: component.dx,
            dy: component.dy,
        }
    }
}

pub fn encode_batch<'a, I>(images: I, options: &EncodeOptions) -> Result<Vec<Vec<u8>>>
where
    I: IntoIterator<Item = &'a Image>,
{
    BatchEncoder::new(options.clone()).encode_all(images)
}

pub fn decode_batch<'a, I>(streams: I) -> Result<Vec<Image>>
where
    I: IntoIterator<Item = &'a [u8]>,
{
    BatchDecoder::new().decode_all(streams)
}

fn profile_mismatch(expected: &BatchProfile, found: &BatchProfile) -> Jp2LamError {
    Jp2LamError::InvalidInput(format!(
        "batch item profile mismatch: expected {expected:?}, found {found:?}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gray_image(width: u32, height: u32) -> Image {
        Image::from_gray_bytes(width, height, &vec![128; (width * height) as usize])
            .expect("gray image")
    }

    #[test]
    fn batch_encoder_accepts_matching_profiles() {
        let options = EncodeOptions::default();
        let mut encoder = BatchEncoder::new(options);
        encoder
            .validate_or_set_profile(&gray_image(2, 2))
            .expect("first profile");
        encoder
            .validate_or_set_profile(&gray_image(2, 2))
            .expect("matching profile");
    }

    #[test]
    fn batch_encoder_rejects_mismatched_dimensions() {
        let options = EncodeOptions::default();
        let mut encoder = BatchEncoder::new(options);
        encoder
            .validate_or_set_profile(&gray_image(2, 2))
            .expect("first profile");
        assert!(encoder.validate_or_set_profile(&gray_image(3, 2)).is_err());
    }
}
