//! SQLite-backed resumable job state for batch document processing.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

#[derive(Debug)]
pub struct JobStore {
    connection: Connection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Pending,
    Running,
    Complete,
    Partial,
    Failed,
    Cancelled,
}

impl JobStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
    fn parse(value: &str) -> Result<Self, BatchError> {
        match value {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "complete" => Ok(Self::Complete),
            "partial" => Ok(Self::Partial),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(BatchError::InvalidState(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub id: i64,
    pub source_path: PathBuf,
    pub source_hash: String,
    pub configuration_hash: String,
    pub page_count: u32,
    pub status: JobStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFingerprint {
    pub content_hash: String,
    pub byte_len: u64,
}

impl JobStore {
    pub fn open(path: &Path) -> Result<Self, BatchError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.execute_batch("CREATE TABLE IF NOT EXISTS jobs (id INTEGER PRIMARY KEY, source_path TEXT NOT NULL, source_hash TEXT NOT NULL, configuration_hash TEXT NOT NULL, page_count INTEGER NOT NULL, status TEXT NOT NULL, error TEXT, created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, UNIQUE(source_hash, configuration_hash)); CREATE TABLE IF NOT EXISTS pages (job_id INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE, page_index INTEGER NOT NULL, status TEXT NOT NULL, shard_path TEXT, error TEXT, updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, PRIMARY KEY(job_id, page_index));")?;
        Ok(Self { connection })
    }

    pub fn create_or_resume(
        &mut self,
        source_path: &Path,
        source_hash: &str,
        configuration_hash: &str,
        page_count: u32,
    ) -> Result<Job, BatchError> {
        let transaction = self.connection.transaction()?;
        transaction.execute("INSERT INTO jobs(source_path, source_hash, configuration_hash, page_count, status) VALUES (?1, ?2, ?3, ?4, 'pending') ON CONFLICT(source_hash, configuration_hash) DO UPDATE SET source_path=excluded.source_path, page_count=excluded.page_count, updated_at=CURRENT_TIMESTAMP", params![source_path.to_string_lossy(), source_hash, configuration_hash, page_count])?;
        let id: i64 = transaction.query_row(
            "SELECT id FROM jobs WHERE source_hash=?1 AND configuration_hash=?2",
            params![source_hash, configuration_hash],
            |row| row.get(0),
        )?;
        for page in 0..page_count {
            transaction.execute("INSERT OR IGNORE INTO pages(job_id, page_index, status) VALUES (?1, ?2, 'pending')", params![id, page])?;
        }
        transaction.commit()?;
        self.job(id)?.ok_or(BatchError::MissingJob(id))
    }

    pub fn job(&self, id: i64) -> Result<Option<Job>, BatchError> {
        self.connection.query_row("SELECT id, source_path, source_hash, configuration_hash, page_count, status FROM jobs WHERE id=?1", [id], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?, row.get::<_, u32>(4)?, row.get::<_, String>(5)?))).optional()?.map(|(id,path,source_hash,configuration_hash,page_count,status)| Ok(Job { id, source_path: PathBuf::from(path), source_hash, configuration_hash, page_count, status: JobStatus::parse(&status)? })).transpose()
    }

    pub fn set_job_status(
        &self,
        id: i64,
        status: JobStatus,
        error: Option<&str>,
    ) -> Result<(), BatchError> {
        let changed = self.connection.execute(
            "UPDATE jobs SET status=?2, error=?3, updated_at=CURRENT_TIMESTAMP WHERE id=?1",
            params![id, status.as_str(), error],
        )?;
        if changed == 0 {
            return Err(BatchError::MissingJob(id));
        }
        Ok(())
    }

    pub fn complete_page(
        &self,
        job_id: i64,
        page_index: u32,
        shard_path: &Path,
    ) -> Result<(), BatchError> {
        let changed = self.connection.execute("UPDATE pages SET status='complete', shard_path=?3, error=NULL, updated_at=CURRENT_TIMESTAMP WHERE job_id=?1 AND page_index=?2", params![job_id, page_index, shard_path.to_string_lossy()])?;
        if changed == 0 {
            return Err(BatchError::MissingPage { job_id, page_index });
        }
        Ok(())
    }

    pub fn pending_pages(&self, job_id: i64) -> Result<Vec<u32>, BatchError> {
        let mut statement = self.connection.prepare("SELECT page_index FROM pages WHERE job_id=?1 AND status!='complete' ORDER BY page_index")?;
        Ok(statement
            .query_map([job_id], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub fn completed_page_shards(&self, job_id: i64) -> Result<Vec<(u32, PathBuf)>, BatchError> {
        let mut statement = self.connection.prepare(
            "SELECT page_index, shard_path FROM pages WHERE job_id=?1 AND status='complete' AND shard_path IS NOT NULL ORDER BY page_index",
        )?;
        Ok(statement
            .query_map([job_id], |row| {
                Ok((
                    row.get::<_, u32>(0)?,
                    PathBuf::from(row.get::<_, String>(1)?),
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub fn reset_page(&self, job_id: i64, page_index: u32) -> Result<(), BatchError> {
        let changed = self.connection.execute(
            "UPDATE pages SET status='pending', shard_path=NULL, error=NULL, updated_at=CURRENT_TIMESTAMP WHERE job_id=?1 AND page_index=?2",
            params![job_id, page_index],
        )?;
        if changed == 0 {
            return Err(BatchError::MissingPage { job_id, page_index });
        }
        Ok(())
    }
}

/// Durably replace a checkpoint without exposing a partially written JSON file.
pub fn atomic_checkpoint(path: &Path, bytes: &[u8]) -> Result<(), BatchError> {
    let parent = path
        .parent()
        .ok_or_else(|| BatchError::InvalidCheckpointPath(path.to_path_buf()))?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| BatchError::Io(error.error))?;
    Ok(())
}

pub fn fingerprint(path: &Path) -> Result<SourceFingerprint, BatchError> {
    let mut file = File::open(path)?;
    let byte_len = file.metadata()?.len();
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(SourceFingerprint {
        content_hash: format!("blake3:{}", hasher.finalize().to_hex()),
        byte_len,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum BatchError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("job {0} does not exist")]
    MissingJob(i64),
    #[error("page {page_index} of job {job_id} does not exist")]
    MissingPage { job_id: i64, page_index: u32 },
    #[error("invalid persisted state `{0}`")]
    InvalidState(String),
    #[error("invalid checkpoint path: {0}")]
    InvalidCheckpointPath(PathBuf),
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resumes_same_source_and_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = JobStore::open(&directory.path().join("jobs.sqlite")).unwrap();
        let first = store
            .create_or_resume(Path::new("book.pdf"), "source", "config", 2)
            .unwrap();
        store
            .complete_page(first.id, 0, Path::new("page-0.json"))
            .unwrap();
        let resumed = store
            .create_or_resume(Path::new("book.pdf"), "source", "config", 2)
            .unwrap();
        assert_eq!(first.id, resumed.id);
        assert_eq!(store.pending_pages(first.id).unwrap(), vec![1]);
        assert_eq!(
            store.completed_page_shards(first.id).unwrap(),
            vec![(0, PathBuf::from("page-0.json"))]
        );
        store.reset_page(first.id, 0).unwrap();
        assert_eq!(store.pending_pages(first.id).unwrap(), vec![0, 1]);
    }

    #[test]
    fn checkpoints_are_atomically_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("page.json");
        atomic_checkpoint(&path, b"one").unwrap();
        atomic_checkpoint(&path, b"two").unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"two");
    }
}
