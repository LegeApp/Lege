use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use crate::present::PresenterPreference;

pub const USAGE: &str = "Usage: lege-viewer [--presenter auto|gpu|software] <document.pdf>\n       lege-viewer [--presenter auto|gpu|software] --synthetic [PAGE_COUNT]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchMode {
    Pdf(PathBuf),
    Synthetic { page_count: u32 },
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchConfig {
    pub mode: LaunchMode,
    pub presenter: PresenterPreference,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CliError {
    #[error("no document was specified")]
    MissingDocument,
    #[error("unknown option: {0}")]
    UnknownOption(String),
    #[error("--synthetic accepts at most one page count")]
    TooManySyntheticArguments,
    #[error("synthetic page count must be a positive integer")]
    InvalidSyntheticPageCount,
    #[error("only one PDF document can be opened at a time")]
    TooManyDocuments,
    #[error("--presenter requires auto, gpu, or software")]
    MissingPresenter,
    #[error("unknown presenter: {0}")]
    InvalidPresenter(String),
    #[error("--presenter may only be specified once")]
    DuplicatePresenter,
    #[error("--synthetic cannot be combined with a PDF document")]
    ConflictingDocumentModes,
}

pub fn parse_launch_mode<I, S>(arguments: I) -> Result<LaunchMode, CliError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    parse_launch_config(arguments).map(|config| config.mode)
}

pub fn parse_launch_config<I, S>(arguments: I) -> Result<LaunchConfig, CliError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let arguments = arguments.into_iter().map(Into::into).collect::<Vec<_>>();
    if arguments.is_empty() {
        return Err(CliError::MissingDocument);
    }
    let mut presenter = None;
    let mut synthetic = false;
    let mut synthetic_count = None;
    let mut document = None;
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if argument == OsStr::new("-h") || argument == OsStr::new("--help") {
            return Ok(LaunchConfig {
                mode: LaunchMode::Help,
                presenter: presenter.unwrap_or_default(),
            });
        }
        if argument == OsStr::new("--presenter") {
            if presenter.is_some() {
                return Err(CliError::DuplicatePresenter);
            }
            index += 1;
            let value = arguments.get(index).ok_or(CliError::MissingPresenter)?;
            presenter = Some(match value.to_string_lossy().as_ref() {
                "auto" => PresenterPreference::Auto,
                "gpu" => PresenterPreference::Gpu,
                "software" => PresenterPreference::Software,
                _ => {
                    return Err(CliError::InvalidPresenter(
                        value.to_string_lossy().into_owned(),
                    ));
                }
            });
        } else if argument == OsStr::new("--synthetic") {
            if synthetic {
                return Err(CliError::TooManySyntheticArguments);
            }
            if document.is_some() {
                return Err(CliError::ConflictingDocumentModes);
            }
            synthetic = true;
        } else if argument.to_string_lossy().starts_with('-') {
            return Err(CliError::UnknownOption(
                argument.to_string_lossy().into_owned(),
            ));
        } else if synthetic && synthetic_count.is_none() {
            synthetic_count = Some(
                argument
                    .to_string_lossy()
                    .parse::<u32>()
                    .ok()
                    .filter(|count| *count > 0)
                    .ok_or(CliError::InvalidSyntheticPageCount)?,
            );
        } else if document.is_some() {
            return Err(if synthetic {
                CliError::TooManySyntheticArguments
            } else {
                CliError::TooManyDocuments
            });
        } else if synthetic {
            return Err(CliError::TooManySyntheticArguments);
        } else {
            document = Some(PathBuf::from(argument));
        }
        index += 1;
    }
    let mode = if synthetic {
        LaunchMode::Synthetic {
            page_count: synthetic_count.unwrap_or(10_000),
        }
    } else if let Some(path) = document {
        LaunchMode::Pdf(path)
    } else {
        return Err(CliError::MissingDocument);
    };
    Ok(LaunchConfig {
        mode,
        presenter: presenter.unwrap_or_default(),
    })
}
