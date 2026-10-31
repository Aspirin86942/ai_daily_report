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
#[derive(Debug, Subcommand)]
pub enum ArchiveCommand {
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
    Verify,
    #[command(name = "optimize", alias = "prune", hide = true)]
    Retired {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        arguments: Vec<String>,
    },
    Backup {
        #[arg(long)]
        output: PathBuf,
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
        ArchiveCommand::Retired { .. } => Err(error("FEATURE_RETIRED")),
        ArchiveCommand::Backup { output } => super::maintenance::backup(config, &output),
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
                    // Select a shared saved profile; historical reads need no live parser.
                    let (before_extraction, after_extraction):(String,String)=store.conn.query_row(
                        "SELECT a.id,b.id FROM version_extractions va JOIN extractions a ON a.id=va.extraction_id JOIN version_extractions vb ON vb.version_id=?2 JOIN extractions b ON b.id=vb.extraction_id WHERE va.version_id=?1 AND a.profile=b.profile AND a.status IN('complete','partial') AND b.status IN('complete','partial') ORDER BY a.rowid DESC,b.rowid DESC LIMIT 1",
                        rusqlite::params![from,to],|r|Ok((r.get(0)?,r.get(1)?)))?;
                    Ok(
                        json!({"before":before,"after":after,"changes":super::diff::compare(&store,Some(&before_extraction),&after_extraction,&config.archive.table_keys)?}),
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
