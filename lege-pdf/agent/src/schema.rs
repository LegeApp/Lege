//! Versioned agent output envelope.

use serde::Serialize;
use serde_json::Value;

/// Schema identifier for every agent-facing record.
pub const SCHEMA_ID: &str = "lege-pdf.agent/v1";

/// Record status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Failed,
    Truncated,
}

/// One stdout JSON/JSONL record.
#[derive(Debug, Clone, Serialize)]
pub struct Envelope {
    pub schema: &'static str,
    pub document: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_index: Option<u32>,
    pub status: Status,
    pub warnings: Vec<String>,
    pub data: Value,
}

impl Envelope {
    pub fn ok(document: impl Into<String>, data: Value) -> Self {
        Self {
            schema: SCHEMA_ID,
            document: document.into(),
            page: None,
            page_index: None,
            status: Status::Ok,
            warnings: Vec::new(),
            data,
        }
    }

    pub fn page_ok(document: impl Into<String>, page_one_based: u32, data: Value) -> Self {
        Self {
            schema: SCHEMA_ID,
            document: document.into(),
            page: Some(page_one_based),
            page_index: Some(page_one_based.saturating_sub(1)),
            status: Status::Ok,
            warnings: Vec::new(),
            data,
        }
    }

    pub fn page_failed(
        document: impl Into<String>,
        page_one_based: u32,
        error: impl Into<String>,
    ) -> Self {
        Self {
            schema: SCHEMA_ID,
            document: document.into(),
            page: Some(page_one_based),
            page_index: Some(page_one_based.saturating_sub(1)),
            status: Status::Failed,
            warnings: Vec::new(),
            data: serde_json::json!({ "error": error.into() }),
        }
    }

    pub fn with_warnings(mut self, warnings: Vec<String>) -> Self {
        self.warnings = warnings;
        self
    }

    pub fn push_warning(&mut self, warning: impl Into<String>) {
        self.warnings.push(warning.into());
    }

    pub fn set_status(mut self, status: Status) -> Self {
        self.status = status;
        self
    }

    pub fn write_json(&self) -> anyhow::Result<()> {
        let mut out = serde_json::to_string_pretty(self)?;
        out.push('\n');
        print!("{out}");
        Ok(())
    }

    pub fn write_jsonl(&self) -> anyhow::Result<()> {
        let mut out = serde_json::to_string(self)?;
        out.push('\n');
        print!("{out}");
        Ok(())
    }
}

/// Output mode selected by the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputMode {
    #[default]
    Human,
    Json,
    Jsonl,
}
