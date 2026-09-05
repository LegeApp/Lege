use std::error::Error;
use std::fmt::{Display, Formatter};

pub type Result<T> = std::result::Result<T, Jp2LamError>;

#[derive(Debug)]
pub enum Jp2LamError {
    InvalidInput(String),
    EncodeFailed(String),
    DecodeFailed(String),
    UnsupportedFeature(String),
    Io(String),
    /// Historical variant from Session 2; the controller is now wired.
    #[allow(dead_code)]
    PerceptualNotImplemented,
    /// Bounded search finished without a stream at or above the SSIMULACRA2 floor.
    PerceptualTargetMissed {
        achieved: f64,
        target: f64,
        status: &'static str,
    },
}

impl Display for Jp2LamError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidInput(msg) => write!(f, "invalid input: {msg}"),
            Self::EncodeFailed(msg) => write!(f, "encode failed: {msg}"),
            Self::DecodeFailed(msg) => write!(f, "decode failed: {msg}"),
            Self::UnsupportedFeature(msg) => write!(f, "unsupported feature: {msg}"),
            Self::Io(msg) => write!(f, "I/O error: {msg}"),
            Self::PerceptualNotImplemented => {
                write!(f, "perceptual SSIMULACRA2 targeting is not implemented yet")
            }
            Self::PerceptualTargetMissed {
                achieved,
                target,
                status,
            } => write!(
                f,
                "perceptual encode missed SSIMULACRA2 floor {target}: achieved {achieved} ({status})"
            ),
        }
    }
}

impl Error for Jp2LamError {}

impl Jp2LamError {
    pub fn is_decode_failure(&self) -> bool {
        matches!(self, Self::DecodeFailed(_) | Self::UnsupportedFeature(_))
    }

    pub fn is_unsupported_feature(&self) -> bool {
        matches!(self, Self::UnsupportedFeature(_))
    }

    #[must_use]
    pub fn is_perceptual_not_implemented(&self) -> bool {
        matches!(self, Self::PerceptualNotImplemented)
    }

    #[must_use]
    pub fn is_perceptual_target_missed(&self) -> bool {
        matches!(self, Self::PerceptualTargetMissed { .. })
    }

    pub fn message(&self) -> &str {
        match self {
            Self::InvalidInput(msg)
            | Self::EncodeFailed(msg)
            | Self::DecodeFailed(msg)
            | Self::UnsupportedFeature(msg)
            | Self::Io(msg) => msg,
            Self::PerceptualNotImplemented => {
                "perceptual SSIMULACRA2 targeting is not implemented yet"
            }
            Self::PerceptualTargetMissed { .. } => {
                "perceptual encode missed the requested SSIMULACRA2 floor"
            }
        }
    }
}
