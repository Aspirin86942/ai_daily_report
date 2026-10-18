use super::{error, store::ArchiveStore};
use crate::{config::AppConfig, types, ReportError};
use chrono::NaiveDate;
use clap::{Args, Subcommand, ValueEnum};
use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Format {
    Text,
    Json,
}
#[derive(Debug, Args)]
pub struct CollectArgs {
    #[arg(long)]
    pub baseline: bool,
    #[arg(long)]
    pub scheduled: bool,
    #[arg(long, value_enum, default_value = "json")]
    pub format: Format,
}
#[derive(Debug, Subcommand)]
pub enum ArchiveCommand {
    /// Prepare a new recovery generation using frozen input and validated fact receipts; no model calls.
    Recover {
        #[arg(long)]
        run_id: String,
        #[arg(long)]
        reason: String,
    },
    History {
        #[arg(long)]
        path: String,
    },
    Show {
        #[arg(long)]
        version: String,
        #[arg(long)]
        unit: Option<u64>,
    },
    Diff {
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
    },
    Retry {
        #[arg(long)]
        version: String,
    },
    Verify,
    /// Compress extraction storage without changing evidence bytes.
    Optimize,
    Backup {
        #[arg(long)]
        output: PathBuf,
    },
    Prune {
        #[arg(long,value_parser=types::parse_date,conflicts_with="execute_plan")]
        before: Option<NaiveDate>,
        #[arg(long, required_unless_present = "before")]
        execute_plan: Option<PathBuf>,
    },
}
#[derive(Debug, Args)]
pub struct StatsArgs {
    #[arg(long)]
    pub files_csv: Option<PathBuf>,
    #[arg(long,value_parser=types::parse_date)]
    pub from: Option<NaiveDate>,
    #[arg(long,value_parser=types::parse_date)]
    pub to: Option<NaiveDate>,
    #[arg(long)]
    pub run_id: Option<String>,
    #[arg(long, value_enum, default_value = "json")]
    pub format: Format,
}
pub fn execute(config: &AppConfig, command: ArchiveCommand) -> Result<Value, ReportError> {
    match command {
        ArchiveCommand::Recover { run_id, reason } => {
            super::recovery::prepare(config, &run_id, &reason)
        }
        ArchiveCommand::Optimize => super::storage::optimize(config),
        ArchiveCommand::Backup { output } => super::maintenance::backup(config, &output),
        ArchiveCommand::Prune {
            before,
            execute_plan,
        } => match (before, execute_plan) {
            (Some(date), None) => super::maintenance::prune_plan(config, date),
            (None, Some(path)) => super::maintenance::prune_execute(config, &path),
            _ => Err(error("ARCHIVE_PRUNE_OPTIONS_INVALID")),
        },
        ArchiveCommand::Retry { version } => {
            let _lock = crate::store::locking::ReportLock::acquire(
                &config.paths.data_dir.join("archive/collector"),
                "archive",
                std::time::Duration::ZERO,
            )?;
            let mut store = ArchiveStore::open(config, true)?;
            let version = store.version(&version)?;
            let profile = super::parse::profile(config)?;
            let id =
                super::identity(&["extraction", &version.sha256, &version.extension, &profile]);
            let tx = store.conn.transaction()?;
            tx.execute("INSERT OR IGNORE INTO extractions(id,sha256,profile,extension,status) VALUES(?,?,?,?,'queued')",rusqlite::params![id,version.sha256,profile,version.extension])?;
            tx.execute(
                "INSERT OR IGNORE INTO version_extractions VALUES(?,?)",
                rusqlite::params![version.id, id],
            )?;
            // Readable evidence stays available during an explicit retry.
            tx.execute("UPDATE extractions SET status=CASE WHEN manifest_json IS NULL THEN 'queued' ELSE status END,attempts=0,error_code=NULL WHERE id=?",[&id])?;
            tx.execute(
                "INSERT OR REPLACE INTO metadata(key,value) VALUES(?,?)",
                rusqlite::params![format!("retry:{id}"), id],
            )?;
            tx.commit()?;
            Ok(json!({"queued":1,"extraction_id":id}))
        }
        command => {
            let store = ArchiveStore::open(config, false)?;
            match command {
                ArchiveCommand::History { path } => {
                    Ok(json!({"versions":store.versions(Some(&path))?}))
                }
                ArchiveCommand::Show { version, unit } => {
                    let version = store.version(&version)?;
                    let mut statement = store.conn.prepare("SELECT e.id,e.profile,e.status,e.error_code,e.manifest_json FROM version_extractions ve JOIN extractions e ON e.id=ve.extraction_id WHERE ve.version_id=? ORDER BY e.rowid DESC")?;
                    let mut parses = vec![];
                    for row in statement.query_map([&version.id], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, Option<String>>(3)?,
                            r.get::<_, Option<String>>(4)?,
                        ))
                    })? {
                        let (id, profile, status, error, manifest) = row?;
                        let dir = store.extraction_dir(&id)?;
                        let selected = unit.filter(|_| matches!(status.as_str(), "complete" | "partial")).map(|n| -> Result<Value,ReportError> { let selected = super::diff::unit(&dir,n)?; Ok(json!({"text":super::diff::text(&dir,&selected)?,"location":selected})) }).transpose()?;
                        parses.push(json!({"id":id,"profile":profile,"status":status,"error":error,"markdown":dir.join("body.md"),"locations":dir.join("units.jsonl"),"manifest":manifest.map(|s|serde_json::from_str::<Value>(&s)).transpose()?,"unit":selected}));
                    }
                    Ok(
                        json!({"original":store.object(&version.sha256)?,"version":version,"extractions":parses}),
                    )
                }
                ArchiveCommand::Diff { from, to } => {
                    let before = store.version(&from)?;
                    let after = store.version(&to)?;
                    if before.document_id != after.document_id {
                        return Err(error("ARCHIVE_DIFF_DOCUMENT_MISMATCH"));
                    }
                    let profile = super::parse::profile(config)?;
                    let extraction = |version: &str| -> Result<String, ReportError> {
                        Ok(store.conn.query_row("SELECT e.id FROM extractions e JOIN version_extractions v ON v.extraction_id=e.id WHERE v.version_id=? AND e.profile=? AND e.status IN('complete','partial')",rusqlite::params![version,profile],|r|r.get(0))?)
                    };
                    Ok(
                        json!({"before":before,"after":after,"changes":super::diff::compare(&store,Some(&extraction(&from)?),&extraction(&to)?,&config.archive.table_keys)?}),
                    )
                }
                ArchiveCommand::Verify => super::maintenance::verify(&store),
                _ => unreachable!(),
            }
        }
    }
}
pub fn print(value: &Value, format: Format) -> Result<(), ReportError> {
    match format {
        Format::Json => println!("{}", serde_json::to_string_pretty(value)?),
        Format::Text => {
            if let Some(object) = value.as_object() {
                for (key, value) in object {
                    println!(
                        "{key}: {}",
                        if let Some(s) = value.as_str() {
                            s.to_owned()
                        } else {
                            serde_json::to_string_pretty(value)?
                        }
                    );
                }
            } else {
                println!("{}", serde_json::to_string_pretty(value)?);
            }
        }
    }
    Ok(())
}
