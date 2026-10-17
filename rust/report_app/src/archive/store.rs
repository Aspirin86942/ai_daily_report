use super::{error, file_hash};
use crate::{config::AppConfig, ReportError};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

pub struct ArchiveStore {
    pub root: PathBuf,
    pub conn: Connection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Version {
    pub id: String,
    pub document_id: String,
    pub parent_id: Option<String>,
    pub sha256: String,
    pub path: String,
    pub extension: String,
    pub size_bytes: u64,
    pub modified_at: String,
    pub observed_at: String,
    pub baseline: bool,
}

impl ArchiveStore {
    pub fn open(config: &AppConfig, writable: bool) -> Result<Self, ReportError> {
        Self::at(&config.paths.data_dir.join("archive"), writable)
    }
    pub fn at(root: &Path, writable: bool) -> Result<Self, ReportError> {
        crate::snapshots::reject_redirects(root, "ARCHIVE_PATH_REDIRECT")?;
        if writable {
            for name in [
                "objects",
                "extractions",
                "report_evidence",
                "staging",
                "prune_plans",
            ] {
                fs::create_dir_all(root.join(name))?;
            }
        }
        let path = root.join("index.sqlite3");
        if !writable && !path.is_file() {
            return Err(error("ARCHIVE_NOT_INITIALIZED"));
        }
        crate::snapshots::reject_redirects(root, "ARCHIVE_PATH_REDIRECT")?;
        let flags = if writable {
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
        } else {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        };
        let conn = Connection::open_with_flags(path, flags)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let version: u32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version == 0 && writable {
            let count: i64 = conn.query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table'",
                [],
                |r| r.get(0),
            )?;
            if count != 0 {
                return Err(error("ARCHIVE_SCHEMA_INVALID"));
            }
            conn.execute_batch("BEGIN IMMEDIATE;")?;
            conn.execute_batch(include_str!("schema.sql"))?;
            conn.execute_batch("COMMIT;")?;
        } else if version != 1 {
            return Err(error("ARCHIVE_SCHEMA_INVALID"));
        }
        if writable {
            conn.pragma_update(None, "journal_mode", "WAL")?;
            conn.pragma_update(None, "synchronous", "FULL")?;
        }
        Ok(Self {
            root: root.to_owned(),
            conn,
        })
    }
    pub fn object(&self, hash: &str) -> Result<PathBuf, ReportError> {
        if !crate::snapshots::valid_hash(hash) {
            return Err(error("ARCHIVE_ID_INVALID"));
        }
        Ok(self.root.join("objects").join(&hash[..2]).join(hash))
    }
    pub fn extraction_dir(&self, id: &str) -> Result<PathBuf, ReportError> {
        if !crate::snapshots::valid_hash(id) {
            return Err(error("ARCHIVE_ID_INVALID"));
        }
        Ok(self.root.join("extractions").join(id))
    }
    pub fn version(&self, id: &str) -> Result<Version, ReportError> {
        Ok(self.conn.query_row("SELECT id,document_id,parent_id,sha256,path,extension,size_bytes,modified_at,observed_at,baseline FROM versions WHERE id=? AND pruned_at IS NULL", [id], version_row)?)
    }
    pub fn versions(&self, path: Option<&str>) -> Result<Vec<Version>, ReportError> {
        let path = path.map(|p| {
            p.replace('\\', "/")
                .trim_start_matches("//?/")
                .to_lowercase()
        });
        let mut stmt = self.conn.prepare("SELECT v.id,v.document_id,v.parent_id,v.sha256,v.path,v.extension,v.size_bytes,v.modified_at,v.observed_at,v.baseline FROM versions v WHERE v.pruned_at IS NULL AND (?1 IS NULL OR v.document_id IN(SELECT document_id FROM paths WHERE replace(replace(lower(path),char(92),'/'),'//?/','')=?1)) ORDER BY v.observed_at,v.rowid")?;
        let rows = stmt
            .query_map([path], version_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
    pub fn gap(
        &self,
        run: &str,
        path: Option<&str>,
        code: &str,
        detail: &str,
    ) -> Result<(), ReportError> {
        self.conn.execute(
            "INSERT INTO gaps(collection_id,path,code,detail) VALUES(?,?,?,?)",
            rusqlite::params![run, path, code, detail],
        )?;
        Ok(())
    }
    pub fn put_json(&self, path: &Path, value: &impl Serialize) -> Result<String, ReportError> {
        let bytes = crate::snapshots::canonical_json(&serde_json::to_value(value)?)?;
        if path.exists() {
            if fs::read(path)? != bytes {
                return Err(error("ARCHIVE_IMMUTABLE_CONFLICT"));
            }
        } else {
            atomic_write(path, &bytes)?;
        }
        Ok(crate::snapshots::digest(&bytes))
    }
    pub fn read_manifest(&self, id: &str) -> Result<Value, ReportError> {
        let text: String = self.conn.query_row(
            "SELECT manifest_json FROM extractions WHERE id=? AND status IN('complete','partial')",
            [id],
            |r| r.get(0),
        )?;
        Ok(serde_json::from_str(&text)?)
    }
    pub fn verify_object(&self, hash: &str) -> Result<(), ReportError> {
        if file_hash(&self.object(hash)?)? != hash {
            return Err(error("ARCHIVE_OBJECT_CORRUPT"));
        }
        Ok(())
    }
}

fn version_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Version> {
    Ok(Version {
        id: r.get(0)?,
        document_id: r.get(1)?,
        parent_id: r.get(2)?,
        sha256: r.get(3)?,
        path: r.get(4)?,
        extension: r.get(5)?,
        size_bytes: r.get::<_, i64>(6)? as u64,
        modified_at: r.get(7)?,
        observed_at: r.get(8)?,
        baseline: r.get(9)?,
    })
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ReportError> {
    let parent = path.parent().ok_or_else(|| error("ARCHIVE_PATH_INVALID"))?;
    fs::create_dir_all(parent)?;
    crate::snapshots::reject_redirects(parent, "ARCHIVE_PATH_REDIRECT")?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path)
        .map_err(|e| ReportError::from(e.error))?;
    Ok(())
}
