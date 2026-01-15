use super::{BELL_SHADER, BILINEAR_SHADER, LANCZOS3_SHADER};

/// Image filtering algorithms supported by the resizer
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FilterType {
    /// Bilinear interpolation - fast, good quality for most cases
    Bilinear,
    /// Bell filter - higher quality than bilinear, good balance
    Bell,
    /// Lanczos-3 filter - highest quality, slower
    Lanczos3,
}

impl FilterType {
    /// Get the shader bytecode for this filter type
    pub fn shader_bytecode(&self) -> &'static [u8] {
        match self {
              FilterType::Bilinear => BILINEAR_SHADER,
              FilterType::Bell => BELL_SHADER,
              FilterType::Lanczos3 => LANCZOS3_SHADER,
        }
    }

    /// Get the thread group dimensions for this filter
    pub fn thread_group_size(&self) -> (u32, u32, u32) {
        match self {
            FilterType::Bilinear => (16, 16, 1),
            FilterType::Bell => (16, 16, 1),
            FilterType::Lanczos3 => (8, 8, 1), // Smaller groups due to higher computational load
        }
    }

    /// Get recommended filter for different use cases
    pub fn recommended_for_use_case(use_case: super::UseCase) -> Self {
        match use_case {
            super::UseCase::PaddleX => FilterType::Bilinear,  // Speed is important
            super::UseCase::OCR => FilterType::Bell,          // Good quality for text
            super::UseCase::MarginCalculation => FilterType::Bilinear, // Speed over quality
        }
    }
}

impl Default for FilterType {
    fn default() -> Self {
        FilterType::Bilinear
    }
}

impl std::fmt::Display for FilterType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FilterType::Bilinear => write!(f, "Bilinear"),
            FilterType::Bell => write!(f, "Bell"),
            FilterType::Lanczos3 => write!(f, "Lanczos3"),
        }
    }
}

/// Border handling modes for pixels outside the image boundary
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderMode {
    /// Clamp to edge pixels
    Clamp = 0,
    /// Reflect across the boundary
    Reflect = 1,
    /// Wrap around to the opposite edge
    Wrap = 2,
    /// Use constant border value
    Constant = 3,
}

impl Default for BorderMode {
    fn default() -> Self {
        BorderMode::Clamp
    }
}

impl std::fmt::Display for BorderMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BorderMode::Clamp => write!(f, "Clamp"),
            BorderMode::Reflect => write!(f, "Reflect"),
            BorderMode::Wrap => write!(f, "Wrap"),
            BorderMode::Constant => write!(f, "Constant"),
        }
    }
}