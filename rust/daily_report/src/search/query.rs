use super::{index_error, terms, ReportError, SearchArgs, SearchKind};
use rusqlite::{params, params_from_iter, types::Value as SqlValue, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::{cmp::Ordering, collections::BinaryHeap};

const SNIPPET_BATCH: usize = 8;

struct Query<'a> {
    text: &'a str,
    short: bool,
    original: bool,
    phrase: bool,
    single_batch: bool,
    snippet_db: Option<Connection>,
}

impl Query<'_> {
    fn prepare_snippet(
        &mut self,
        index: &Connection,
        fragment: i64,
        position: usize,
        title: &str,
        encoded_title: &str,
        encoded_body: &str,
    ) -> rusqlite::Result<Option<String>> {
        let body = (self.short || self.original).then(|| terms::decode(encoded_body));
        if self.short {
            if let Some(hit) = body
                .as_deref()
                .and_then(|b| like_snippet(b, self.text))
                .or_else(|| like_snippet(title, self.text))
            {
                return Ok(Some(hit));
            }
        }
        // Render only the winning rows. Native trigram snippet windows and the
        // original highlight offsets remain byte-compatible with v5.
        if self.snippet_db.is_none() {
            let db = Connection::open_in_memory()?;
            if self.single_batch {
                // This private in-memory table is discarded once the batch is rendered.
                db.execute_batch("PRAGMA journal_mode=OFF; BEGIN")?;
            }
            db.execute_batch("CREATE VIRTUAL TABLE excerpt USING fts5(a,b,tokenize='trigram')")?;
            if !self.single_batch {
                db.execute_batch("SAVEPOINT snippet")?;
            }
            self.snippet_db = Some(db);
        }
        let db = self.snippet_db.as_ref().unwrap();
        let literals;
        let (a, b) = if self.phrase {
            let pieces: String = index.query_one(
                "SELECT literals FROM phrase_content WHERE id=?",
                [fragment],
                |r| r.get(0),
            )?;
            literals = format!("\x1f{}\x1f", pieces.replace("\x1f\x1e|\x1f", "\x1f\x1f"));
            (literals.as_str(), "")
        } else if self.original {
            (title, body.as_deref().unwrap_or_default())
        } else {
            (encoded_title, encoded_body)
        };
        db.prepare_cached("INSERT INTO excerpt(rowid,a,b) VALUES(?,?,?)")?
            .execute(params![position as i64, a, b])?;
        Ok(None)
    }

    fn fill_snippets(&self, hits: &mut [Value]) -> rusqlite::Result<()> {
        let pending = hits.iter().filter(|hit| hit["snippet"].is_null()).count();
        if pending == 0 {
            return Ok(());
        }
        let db = self.snippet_db.as_ref().unwrap();
        let tokens = if self.original { 40 } else { 64 };
        let expression = terms::expression(self.text, self.original, self.phrase);
        let mut select = db.prepare_cached(
            "SELECT rowid,snippet(excerpt,-1,'[',']','…',?) FROM excerpt WHERE excerpt MATCH ?",
        )?;
        let mut rows = select.query(params![tokens, expression])?;
        let mut filled = 0;
        while let Some(row) = rows.next()? {
            let position = row.get::<_, u32>(0)? as usize;
            let snippet: String = row.get(1)?;
            hits[position]["snippet"] = Value::String(if self.original {
                snippet
            } else {
                terms::decode(&snippet)
            });
            filled += 1;
        }
        if filled != pending {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
        if !self.single_batch {
            db.execute_batch("ROLLBACK TO snippet")?;
        }
        Ok(())
    }
}

pub(super) fn search(
    index: &Connection,
    args: &SearchArgs,
    text: &str,
    index_rebuilt: bool,
    source_rows_read: u64,
) -> Result<Value, ReportError> {
    let short = !args.phrase && text.chars().nth(2).is_none();
    let original =
        !args.phrase && !short && !text.contains(char::is_whitespace) && !text.contains('\0');
    let (table, ranking) = if args.phrase {
        ("report_phrases", "bm25(report_phrases,1,0)")
    } else {
        ("report_search", "bm25(report_search,4,1,4,1,4,1,0)")
    };
    let mut parameters: Vec<SqlValue> =
        vec![terms::compact_expression(text, original, args.phrase).into()];
    let mut filter = format!("{table} MATCH ?1");
    let mut add_filter = |expression: &str, value: String| {
        parameters.push(value.into());
        filter.push_str(&format!(" AND {expression} ?{}", parameters.len()));
    };
    match args.kind {
        SearchKind::Daily => add_filter("f.kind =", "daily".into()),
        SearchKind::Weekly => add_filter("f.kind =", "weekly".into()),
        SearchKind::Monthly => add_filter("f.kind =", "monthly".into()),
        SearchKind::All => {}
    }
    if args.from.is_some() || args.to.is_some() {
        add_filter("f.kind =", "daily".into());
    }
    if let Some(from) = args.from {
        add_filter("f.report_key >=", from.to_string());
    }
    if let Some(to) = args.to {
        add_filter("f.report_key <=", to.to_string());
    }
    if let Some(source) = &args.source {
        let escaped = source
            .trim()
            .to_ascii_uppercase()
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        add_filter("(' ' || f.sources || ' ') LIKE", format!("% {escaped} %"));
        filter.push_str(" ESCAPE '\\'");
    }
    let joined;
    let matching = if parameters.len() == 1 {
        table
    } else {
        let covering_index = if args.source.is_some() {
            ""
        } else {
            "INDEXED BY fragment_order"
        };
        joined = format!("{table} JOIN fragments f {covering_index} ON f.id={table}.rowid");
        &joined
    };
    let (total, ids) = ranked_ids(
        index,
        &format!("SELECT {table}.rowid,{ranking} FROM {matching} WHERE {filter}"),
        &parameters,
        args.limit.clamp(1, 500) as usize,
    )
    .map_err(index_error)?;
    let mut get = index
        .prepare("SELECT kind,report_key,title,body,sources,location FROM fragments WHERE id=?")
        .map_err(index_error)?;
    let mut query = Query {
        text,
        short,
        original,
        phrase: args.phrase,
        single_batch: ids.len() <= SNIPPET_BATCH,
        snippet_db: None,
    };
    let mut results = Vec::with_capacity(ids.len());
    for batch in ids.chunks(SNIPPET_BATCH) {
        let first = results.len();
        for (position, id) in batch.iter().copied().enumerate() {
            results.push(get.query_row([id], |r| {
            let kind = r.get_ref(0)?.as_str()?;
            let key = r.get_ref(1)?.as_str()?;
            let encoded_title = r.get_ref(2)?.as_str()?;
            let encoded_body = r.get_ref(3)?.as_str()?;
            let title = terms::decode(encoded_title);
            let snippet = query.prepare_snippet(index, id, position, &title, encoded_title, encoded_body)?;
            let sources: Vec<_> = r.get_ref(4)?.as_str()?.split_whitespace().collect();
            let mut hit = json!({
                "kind": kind, "report_label": key, "report_date": (kind == "daily").then_some(key),
                "title": title, "snippet": snippet, "sources": sources
            });
            if let Some(location) = r.get::<_, Option<String>>(5)? {
                hit["location"] = serde_json::from_str(&location).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(e))
                })?;
            }
            Ok(hit)
            }).map_err(index_error)?);
        }
        query
            .fill_snippets(&mut results[first..])
            .map_err(index_error)?;
    }
    Ok(
        json!({"query":text,"total":total,"index_rebuilt":index_rebuilt,"source_rows_read":source_rows_read,"results":results}),
    )
}

fn ranked_ids(
    index: &Connection,
    sql: &str,
    parameters: &[SqlValue],
    limit: usize,
) -> rusqlite::Result<(usize, Vec<i64>)> {
    let mut statement = index.prepare(sql)?;
    let mut rows = statement.query(params_from_iter(parameters))?;
    // SQLite's BINARY collation compares bytes in the database's encoding.
    let mut keys = index.prepare(
        "SELECT kind!='daily',CAST(report_key AS BLOB)
         FROM fragments INDEXED BY fragment_order WHERE id=?",
    )?;
    let mut total = 0;
    let mut top = BinaryHeap::<RankedFragment>::with_capacity(limit + 1);
    while let Some(row) = rows.next()? {
        total += 1;
        let score = row.get::<_, f64>(1)?;
        // Skip metadata only for strictly worse scores. Ties can still win on report order.
        if top.len() == limit && top.peek().is_some_and(|last| score > last.score) {
            continue;
        }
        let id = row.get::<_, i64>(0)?;
        let Some((non_daily, report_key)) = keys
            .query_row([id], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?
        else {
            continue;
        };
        top.push(RankedFragment {
            score,
            non_daily,
            report_key,
            id,
        });
        if top.len() > limit {
            top.pop();
        }
    }
    let ids = top
        .into_sorted_vec()
        .into_iter()
        .map(|hit| hit.id)
        .collect();
    Ok((total, ids))
}

#[derive(PartialEq)]
struct RankedFragment {
    score: f64,
    non_daily: bool,
    report_key: Vec<u8>,
    id: i64,
}

impl Eq for RankedFragment {}

impl PartialOrd for RankedFragment {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RankedFragment {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .partial_cmp(&other.score)
            .expect("SQLite returns NaN as NULL")
            .then(self.non_daily.cmp(&other.non_daily))
            .then(other.report_key.cmp(&self.report_key))
            .then(self.id.cmp(&other.id))
    }
}

fn like_snippet(text: &str, query: &str) -> Option<String> {
    let start = if query.bytes().any(|b| b.is_ascii_alphabetic()) {
        text.as_bytes()
            .windows(query.len())
            .position(|part| part.eq_ignore_ascii_case(query.as_bytes()))
    } else {
        text.find(query)
    }?;
    let end = start + query.len();
    let left = text[..start]
        .char_indices()
        .rev()
        .nth(39)
        .map_or(0, |(i, _)| i);
    let right = text[end..]
        .char_indices()
        .nth(40)
        .map_or(text.len(), |(i, _)| end + i);
    Some(format!(
        "{}{}[{}]{}{}",
        if left > 0 { "…" } else { "" },
        &text[left..start],
        &text[start..end],
        &text[end..right],
        if right < text.len() { "…" } else { "" },
    ))
}
