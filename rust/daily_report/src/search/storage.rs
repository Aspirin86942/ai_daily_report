use super::{index_error, terms, Fragment, ReportError};
use rusqlite::{params, Connection};

const ORDINARY: &str = "title,body,nul_title,nul_body,raw_title,raw_body,padding";
const LITERAL: &str = "literals,padding";
const INDEXES: &[(&str, &str, &str)] = &[
    ("report_search", "search_content", ORDINARY),
    ("report_phrases", "phrase_content", LITERAL),
];

pub(super) fn enable_compaction(index: &Connection) -> rusqlite::Result<()> {
    let layout = |db: &Connection| -> rusqlite::Result<(u32, u8)> {
        db.query_one(
            "SELECT page_size,auto_vacuum FROM pragma_page_size,pragma_auto_vacuum",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
    };
    // Larger pages keep the external-content rows and FTS segments together.
    // FULL gives old-index space back in the same commit as the new contents.
    if layout(index)? != (16_384, 1) {
        index.execute_batch("PRAGMA page_size=16384; PRAGMA auto_vacuum=FULL")?;
        if layout(index)? != (16_384, 1) {
            index.execute_batch("VACUUM")?;
        }
    }
    Ok(())
}

pub(super) fn initialize(index: &Connection) -> Result<(), ReportError> {
    let available = index
        .query_one("SELECT sqlite_compileoption_used('ENABLE_FTS5')", [], |r| {
            r.get::<_, bool>(0)
        })
        .map_err(index_error)?;
    if !available {
        return Err(ReportError::new(
            "SEARCH_FTS5_UNAVAILABLE",
            "当前 SQLite 未启用 FTS5，无法建立历史报告检索索引",
        ));
    }
    index.execute_batch(
        "DROP TABLE IF EXISTS report_phrases;
         DROP TABLE IF EXISTS report_search;
         DROP VIEW IF EXISTS phrase_content;
         DROP VIEW IF EXISTS search_content;
         DROP TABLE IF EXISTS literal_parts;
         DROP TABLE IF EXISTS fragments;
         CREATE TABLE IF NOT EXISTS indexed_reports(
             kind TEXT NOT NULL, report_key TEXT NOT NULL, raw_sha256 TEXT NOT NULL,
             fragments INTEGER NOT NULL, PRIMARY KEY(kind,report_key)
         ) WITHOUT ROWID;
         CREATE TABLE fragments(
             id INTEGER PRIMARY KEY, kind TEXT NOT NULL, report_key TEXT NOT NULL,
             title TEXT NOT NULL, body TEXT NOT NULL, sources TEXT NOT NULL, location TEXT,
             ordinary_pad INTEGER NOT NULL, literal_pad INTEGER NOT NULL
         );
         CREATE TABLE literal_parts(
             fragment_id INTEGER NOT NULL, ordinal INTEGER NOT NULL, start INTEGER, size INTEGER, literal TEXT,
             PRIMARY KEY(fragment_id,ordinal)
         ) WITHOUT ROWID;
         CREATE INDEX fragment_report ON fragments(kind,report_key);
         CREATE INDEX fragment_order ON fragments(id,kind,report_key);
         CREATE VIEW search_content AS SELECT id,
             CASE WHEN NOT instr(title,char(30)||'0') THEN title END AS title,
             CASE WHEN NOT instr(body,char(30)||'0') THEN body END AS body,
             CASE WHEN instr(title,char(30)||'0') THEN title END AS nul_title,
             CASE WHEN instr(body,char(30)||'0') THEN body END AS nul_body,
             CASE WHEN instr(title,char(30)||'0') THEN replace(title,char(30)||'0'||char(31),'') END AS raw_title,
             CASE WHEN instr(body,char(30)||'0') THEN replace(body,char(30)||'0'||char(31),'') END AS raw_body,
             replace(hex(zeroblob(ordinary_pad)), '00', 'x'||char(31)) AS padding FROM fragments;
         CREATE VIEW phrase_content AS SELECT f.id,
             (SELECT group_concat(coalesce(p.literal,
                 CAST(substr(CAST(f.body AS BLOB),p.start+1,p.size) AS TEXT)),
                 char(31)||char(30)||'|'||char(31) ORDER BY p.ordinal)
              FROM literal_parts p WHERE p.fragment_id=f.id) AS literals,
             replace(hex(zeroblob(literal_pad)), '00', 'x'||char(31)) AS padding FROM fragments f;"
    ).map_err(index_error)?;
    // Every scalar is a token, including spaces and punctuation. The separator
    // never occurs inside a token. unicode61 and trigram use SQLite's same fold.
    // Buffer more postings during a fresh build, reducing the number of segments to merge.
    for &(table, view, columns) in INDEXES {
        index.execute_batch(&format!(
            "CREATE VIRTUAL TABLE {table} USING fts5({columns},content='{view}',content_rowid='id',
             tokenize=\"unicode61 remove_diacritics 0 categories 'L* M* N* P* S* Z* C*' separators '\x1f'\");
             INSERT INTO {table}({table},rank) VALUES('hashsize',16777216)"
        )).map_err(index_error)?;
    }
    Ok(())
}

fn command_both(index: &Connection, command: &str) -> rusqlite::Result<()> {
    for &(table, _, _) in INDEXES {
        index.execute(
            &format!("INSERT INTO {table}({table}) VALUES(?)"),
            [command],
        )?;
    }
    Ok(())
}

pub(super) fn clear(index: &Connection) -> rusqlite::Result<()> {
    command_both(index, "delete-all")?;
    index.execute_batch(
        "DELETE FROM literal_parts; DELETE FROM fragments; DELETE FROM indexed_reports",
    )
}

pub(super) fn optimize(index: &Connection) -> rusqlite::Result<()> {
    command_both(index, "optimize")
}

pub(super) fn delete_report(index: &Connection, kind: &str, key: &str) -> rusqlite::Result<()> {
    for &(table, view, columns) in INDEXES {
        index
            .prepare_cached(&format!(
                "INSERT INTO {table}({table},rowid,{columns}) SELECT 'delete',id,{columns}
             FROM {view} WHERE id IN (SELECT id FROM fragments WHERE kind=?1 AND report_key=?2)"
            ))?
            .execute(params![kind, key])?;
    }
    index.execute("DELETE FROM literal_parts WHERE fragment_id IN (SELECT id FROM fragments WHERE kind=?1 AND report_key=?2)", params![kind, key])?;
    index.execute(
        "DELETE FROM fragments WHERE kind=?1 AND report_key=?2",
        params![kind, key],
    )?;
    Ok(())
}

// v5 BM25 used the combined trigram lengths of the raw and framed fields.
// Preserve that exact length with a non-queryable column of repeated tokens.
fn ordinary_padding(raw: &str, encoded: &str) -> usize {
    let chars = raw.chars().count();
    let nulls = raw.bytes().filter(|&b| b == 0).count();
    let raw_chars = chars - nulls;
    raw_chars.saturating_sub(2) + encoded.chars().count().saturating_sub(2)
        - chars
        - if nulls == 0 { 0 } else { raw_chars }
}

pub(super) fn insert(
    index: &Connection,
    kind: &str,
    key: &str,
    fragment: Fragment,
    utf8: bool,
) -> rusqlite::Result<()> {
    let title = terms::encode(&fragment.title);
    let body = terms::encode(&fragment.body);
    let range_body = if utf8 { body.as_str() } else { "" };
    let ordinary_pad =
        ordinary_padding(&fragment.title, &title) + ordinary_padding(&fragment.body, &body);
    // The v5 literal stream had a doubled separator between independent fields.
    // v6 represents each such boundary with one token that no query can produce.
    let mut parts = Vec::new();
    let mut tokens = 0;
    let mut cursor = 0;
    for part in fragment
        .literals
        .trim_matches('\x1f')
        .split("\x1f\x1f")
        .filter(|s| !s.is_empty())
    {
        tokens += part.bytes().filter(|&b| b == 31).count() + 1;
        let start = range_body[cursor..]
            .find(part)
            .map(|n| cursor + n)
            .or_else(|| range_body.find(part));
        if let Some(start) = start {
            cursor = start + part.len();
        }
        parts.push((start, part));
    }
    let literal_pad = fragment.literals.chars().count().saturating_sub(2)
        - tokens
        - parts.len().saturating_sub(1);
    let mut insert_fragment = index.prepare_cached(
        "INSERT INTO fragments(kind,report_key,title,body,sources,location,ordinary_pad,literal_pad)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8)"
    )?;
    insert_fragment.execute(params![
        kind,
        key,
        title,
        body,
        fragment.sources,
        fragment.location.map(|v| v.to_string()),
        ordinary_pad as i64,
        literal_pad as i64
    ])?;
    let id = index.last_insert_rowid();
    // Match the external-content views exactly. On insertion these fields are
    // already in memory, including the literals whose storage is just a BLOB offset.
    let title_nul = fragment.title.as_bytes().contains(&0);
    let body_nul = fragment.body.as_bytes().contains(&0);
    index.prepare_cached(
        "INSERT INTO report_search(rowid,title,body,nul_title,nul_body,raw_title,raw_body,padding)
         VALUES(?,?,?,?,?,?,?,?)"
    )?.execute(params![
        id, (!title_nul).then_some(&title), (!body_nul).then_some(&body),
        title_nul.then_some(&title), body_nul.then_some(&body),
        title_nul.then(|| title.replace("\x1e0\x1f", "")), body_nul.then(|| body.replace("\x1e0\x1f", "")),
        "x\x1f".repeat(ordinary_pad),
    ])?;
    let literal_text = (!parts.is_empty()).then(|| {
        parts
            .iter()
            .map(|(_, part)| *part)
            .collect::<Vec<_>>()
            .join("\x1f\x1e|\x1f")
    });
    index
        .prepare_cached("INSERT INTO report_phrases(rowid,literals,padding) VALUES(?,?,?)")?
        .execute(params![id, literal_text, "x\x1f".repeat(literal_pad)])?;
    let mut insert = index.prepare_cached(
        "INSERT INTO literal_parts(fragment_id,ordinal,start,size,literal) VALUES(?,?,?,?,?)",
    )?;
    for (i, (start, part)) in parts.into_iter().enumerate() {
        insert.execute(params![
            id,
            i as i64,
            start.map(|v| v as i64),
            start.map(|_| part.len() as i64),
            start.is_none().then_some(part)
        ])?;
    }
    Ok(())
}
