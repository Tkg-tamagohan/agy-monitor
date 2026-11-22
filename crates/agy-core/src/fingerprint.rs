use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FileStamp {
    pub exists: bool,
    pub mtime_ns: u128,
    pub size: u64,
}

impl FileStamp {
    pub fn missing() -> Self {
        Self::default()
    }

    pub fn from_path(path: &Path) -> Self {
        let Ok(meta) = std::fs::metadata(path) else {
            return Self::missing();
        };
        if !meta.is_file() {
            return Self::missing();
        }
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Self {
            exists: true,
            mtime_ns,
            size: meta.len(),
        }
    }

    pub fn mtime_ms(&self) -> i64 {
        if !self.exists {
            return 0;
        }
        (self.mtime_ns / 1_000_000) as i64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DbFingerprint {
    pub db: FileStamp,
    pub wal: FileStamp,
}

impl DbFingerprint {
    pub fn from_db_path(db_path: &Path) -> Self {
        let db = FileStamp::from_path(db_path);
        let wal = FileStamp::from_path(&wal_path_for(db_path));
        Self { db, wal }
    }

    pub fn exists(&self) -> bool {
        self.db.exists
    }

    pub fn latest_mtime_ms(&self) -> i64 {
        self.db.mtime_ms().max(self.wal.mtime_ms())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TranscriptFingerprint {
    pub full: FileStamp,
    pub compact: FileStamp,
}

impl TranscriptFingerprint {
    pub fn from_logs_dir(logs_dir: &Path) -> Self {
        Self {
            full: FileStamp::from_path(&logs_dir.join("transcript_full.jsonl")),
            compact: FileStamp::from_path(&logs_dir.join("transcript.jsonl")),
        }
    }

    pub fn exists(&self) -> bool {
        self.full.exists || self.compact.exists
    }

    pub fn latest_mtime_ms(&self) -> i64 {
        self.full.mtime_ms().max(self.compact.mtime_ms())
    }
}

pub fn wal_path_for(db_path: &Path) -> PathBuf {
    let mut os: OsString = db_path.as_os_str().to_os_string();
    os.push("-wal");
    PathBuf::from(os)
}
