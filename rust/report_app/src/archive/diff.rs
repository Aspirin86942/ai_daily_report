use super::{error, identity, store::ArchiveStore};
use crate::{config::AppConfig, ReportError};
use ai_daily_worker_contract::archive::{ArchiveArtifact, ArchiveUnit};
use ai_daily_worker_contract::ContentUnitKind;
use chrono::Utc;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::Path,
    time::Instant,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Span {
    pub start: u64,
    pub end: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hunk {
    pub before: Span,
    pub after: Span,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Changes {
    pub version: u32,
    pub kind: String,
    pub granularity: String,
    pub hunks: Vec<Hunk>,
}
struct Unit {
    hash: String,
    key: String,
}

pub fn unit(dir: &Path, id: u64) -> Result<ArchiveUnit, ReportError> {
    if id == 0 {
        return Err(error("ARCHIVE_UNIT_INVALID"));
    }
    let mut index = File::open(dir.join("units.idx"))?;
    index.seek(SeekFrom::Start((id - 1) * 8))?;
    let mut bytes = [0; 8];
    index.read_exact(&mut bytes)?;
    let mut units = BufReader::new(File::open(dir.join("units.jsonl"))?);
    units.seek(SeekFrom::Start(u64::from_le_bytes(bytes)))?;
    let mut line = String::new();
    units.take(1024 * 1024).read_line(&mut line)?;
    let parsed: ArchiveUnit = serde_json::from_str(&line)?;
    if parsed.unit_id != format!("u{id}") {
        return Err(error("ARCHIVE_UNIT_INVALID"));
    }
    Ok(parsed)
}
pub fn text(dir: &Path, unit: &ArchiveUnit) -> Result<String, ReportError> {
    let size = unit
        .byte_end
        .checked_sub(unit.byte_start)
        .filter(|size| *size <= 1024 * 1024)
        .ok_or_else(|| error("ARCHIVE_UNIT_INVALID"))?;
    let mut file = File::open(dir.join("body.md"))?;
    file.seek(SeekFrom::Start(unit.byte_start))?;
    let mut bytes = vec![0; size as usize];
    file.read_exact(&mut bytes)?;
    if crate::snapshots::digest(&bytes) != unit.sha256 {
        return Err(error("ARCHIVE_QUOTE_CORRUPT"));
    }
    String::from_utf8(bytes).map_err(|_| error("ARCHIVE_TEXT_INVALID"))
}
fn indexes(dir: &Path, keys: &BTreeMap<String, Vec<String>>) -> Result<Vec<Unit>, ReportError> {
    let mut output = Vec::new();
    let reader = BufReader::new(File::open(dir.join("units.jsonl"))?);
    let mut body = BufReader::new(File::open(dir.join("body.md"))?);
    let mut body_pos = 0;
    let mut headers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in reader.lines() {
        let unit: ArchiveUnit = serde_json::from_str(&line?)?;
        let mut key = unit.sha256.clone();
        if matches!(
            unit.kind,
            ContentUnitKind::TableHeader | ContentUnitKind::TableRow | ContentUnitKind::Total
        ) {
            let label = unit
                .ranges
                .first()
                .and_then(|r| r.label.as_deref())
                .unwrap_or("table")
                .to_owned();
            body.seek_relative(unit.byte_start as i64 - body_pos as i64)?;
            let mut bytes = vec![0; (unit.byte_end - unit.byte_start) as usize];
            body.read_exact(&mut bytes)?;
            body_pos = unit.byte_end;
            let text = String::from_utf8(bytes).map_err(|_| error("ARCHIVE_TEXT_INVALID"))?;
            let cells = cells(&text);
            if unit.kind == ContentUnitKind::TableHeader
                && !cells
                    .iter()
                    .all(|c| c.chars().all(|c| matches!(c, '-' | ':' | ' ')))
            {
                headers.entry(label.clone()).or_insert(cells.clone());
            }
            if let Some(header) = headers.get(&label) {
                let configured = keys.get(&label).cloned().unwrap_or_else(|| {
                    header
                        .iter()
                        .filter(|v| {
                            let v = v.to_lowercase();
                            v == "id"
                                || v.ends_with("编号")
                                || v.ends_with("编码")
                                || v.ends_with("单号")
                        })
                        .take(1)
                        .cloned()
                        .collect()
                });
                let values = configured
                    .iter()
                    .map(|h| {
                        header
                            .iter()
                            .position(|c| c == h)
                            .and_then(|i| cells.get(i))
                            .filter(|s| !s.is_empty())
                    })
                    .collect::<Option<Vec<_>>>();
                if let Some(values) = values.filter(|v| !v.is_empty()) {
                    key = identity(&["row-key", &label, &serde_json::to_string(&values)?]);
                }
            }
        }
        output.push(Unit {
            hash: unit.sha256,
            key,
        });
    }
    Ok(output)
}
fn cells(text: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for c in text.trim().trim_matches('|').chars() {
        if escaped {
            current.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '|' {
            values.push(current.trim().to_owned());
            current.clear();
        } else {
            current.push(c);
        }
    }
    values.push(current.trim().to_owned());
    values
}
pub fn compare(
    store: &ArchiveStore,
    before: Option<&str>,
    after: &str,
    keys: &BTreeMap<String, Vec<String>>,
) -> Result<Changes, ReportError> {
    let new: ArchiveArtifact = serde_json::from_value(store.read_manifest(after)?)?;
    let Some(before) = before else {
        return Ok(Changes {
            version: 1,
            kind: "added".into(),
            granularity: "unit".into(),
            hunks: vec![Hunk {
                before: Span { start: 1, end: 1 },
                after: Span {
                    start: 1,
                    end: new.units + 1,
                },
            }],
        });
    };
    let old: ArchiveArtifact = serde_json::from_value(store.read_manifest(before)?)?;
    if old.body_sha256 == new.body_sha256 {
        return Ok(Changes {
            version: 1,
            kind: "continued".into(),
            granularity: "unit".into(),
            hunks: vec![],
        });
    }
    if old.units > 100000 || new.units > 100000 {
        return Ok(Changes {
            version: 1,
            kind: "modified".into(),
            granularity: "document".into(),
            hunks: vec![Hunk {
                before: Span {
                    start: 1,
                    end: old.units + 1,
                },
                after: Span {
                    start: 1,
                    end: new.units + 1,
                },
            }],
        });
    }
    let a = indexes(&store.extraction_dir(before)?, keys)?;
    let b = indexes(&store.extraction_dir(after)?, keys)?;
    let mut left: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut right: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, u) in a.iter().enumerate() {
        left.entry(&u.key).or_default().push(i);
    }
    for (i, u) in b.iter().enumerate() {
        right.entry(&u.key).or_default().push(i);
    }
    let pairs: Vec<_> = a
        .iter()
        .enumerate()
        .filter_map(|(i, u)| {
            let r = right.get(u.key.as_str())?;
            if left[u.key.as_str()].len() == 1 && r.len() == 1 {
                Some((i, r[0]))
            } else {
                None
            }
        })
        .collect();
    // Patience anchors: O(n log n), then changed gaps retain all before/after units.
    let mut tails: Vec<usize> = vec![];
    let mut previous = vec![None; pairs.len()];
    for (i, &(_, j)) in pairs.iter().enumerate() {
        let pos = tails.partition_point(|p| pairs[*p].1 < j);
        if pos > 0 {
            previous[i] = Some(tails[pos - 1]);
        }
        if pos == tails.len() {
            tails.push(i);
        } else {
            tails[pos] = i;
        }
    }
    let mut anchors = vec![];
    let mut next = tails.last().copied();
    while let Some(i) = next {
        anchors.push(pairs[i]);
        next = previous[i];
    }
    anchors.reverse();
    let mut hunks = vec![];
    let (mut x, mut y) = (0, 0);
    for (i, j) in anchors.into_iter().chain(Some((a.len(), b.len()))) {
        let (mut begin_a, mut begin_b, mut end_a, mut end_b) = (x, y, i, j);
        while begin_a < end_a && begin_b < end_b && a[begin_a].hash == b[begin_b].hash {
            begin_a += 1;
            begin_b += 1;
        }
        while begin_a < end_a && begin_b < end_b && a[end_a - 1].hash == b[end_b - 1].hash {
            end_a -= 1;
            end_b -= 1;
        }
        if begin_a != end_a || begin_b != end_b {
            hunks.push(Hunk {
                before: Span {
                    start: begin_a as u64 + 1,
                    end: end_a as u64 + 1,
                },
                after: Span {
                    start: begin_b as u64 + 1,
                    end: end_b as u64 + 1,
                },
            });
        }
        if i < a.len() && j < b.len() && a[i].hash != b[j].hash {
            hunks.push(Hunk {
                before: Span {
                    start: i as u64 + 1,
                    end: i as u64 + 2,
                },
                after: Span {
                    start: j as u64 + 1,
                    end: j as u64 + 2,
                },
            });
        }
        x = i + 1;
        y = j + 1;
    }
    Ok(Changes {
        version: 1,
        kind: "modified".into(),
        granularity: "unit".into(),
        hunks,
    })
}
pub fn prepare_changes(
    store: &ArchiveStore,
    config: &AppConfig,
    profile: &str,
    deadline: Instant,
) -> Result<(), ReportError> {
    let mut statement=store.conn.prepare("SELECT v.id,v.parent_id,v.observed_at,e.id FROM versions v JOIN version_extractions ve ON ve.version_id=v.id JOIN extractions e ON e.id=ve.extraction_id WHERE v.baseline=0 AND v.pruned_at IS NULL AND e.profile=? AND e.status IN('complete','partial') ORDER BY v.rowid")?;
    let rows = statement
        .query_map([profile], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (version, parent, observed, extraction) in rows {
        if Instant::now() >= deadline {
            break;
        }
        let previous: Option<String> = if let Some(parent) = &parent {
            store.conn.query_row("SELECT e.id FROM version_extractions v JOIN extractions e ON e.id=v.extraction_id WHERE v.version_id=? AND e.profile=? AND e.status IN('complete','partial')",params![parent,profile],|r|r.get(0)).optional()?
        } else {
            None
        };
        if let Some(parent) = parent.as_ref().filter(|_| previous.is_none()) {
            let failed: bool = store.conn.query_row("SELECT EXISTS(SELECT 1 FROM version_extractions ve JOIN extractions e ON e.id=ve.extraction_id WHERE ve.version_id=? AND e.profile=? AND e.status='failed')", params![parent,profile], |r|r.get(0))?;
            if !failed {
                continue;
            }
        }
        let id = identity(&[
            "diff-v1",
            &version,
            previous.as_deref().unwrap_or(""),
            &extraction,
            &serde_json::to_string(&config.archive.table_keys)?,
        ]);
        if store.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM changes WHERE id=?)",
            [&id],
            |r| r.get::<_, bool>(0),
        )? {
            continue;
        }
        let start = Instant::now();
        let mut changes = compare(
            store,
            previous.as_deref(),
            &extraction,
            &config.archive.table_keys,
        )?;
        if parent.is_some() && previous.is_none() {
            changes.kind = "modified".into();
            changes.granularity = "before_unavailable".into();
        }
        store.conn.execute(
            "INSERT INTO changes VALUES(?,?,?,?,?,?,?,?,?)",
            params![
                id,
                version,
                parent,
                extraction,
                previous,
                changes.kind,
                observed,
                Utc::now().to_rfc3339(),
                serde_json::to_string(&changes)?
            ],
        )?;
        store.conn.execute(
            "UPDATE file_metrics SET diff_ms=? WHERE version_id=?",
            params![start.elapsed().as_millis() as i64, version],
        )?;
    }
    Ok(())
}
