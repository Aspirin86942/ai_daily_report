use super::{index_error, terms, ReportError, SearchArgs, SearchKind};
use rusqlite::{params, params_from_iter, types::Value as SqlValue, Connection};
use serde_json::{json, Value};

struct Query<'a> {
    text: &'a str,
    short: bool,
    original: bool,
    phrase: bool,
    snippet_db: Option<Connection>,
}

impl Query<'_> {
    fn snippet(
        &mut self,
        index: &Connection,
        id: i64,
        title: &str,
        encoded_title: &str,
        encoded_body: &str,
    ) -> rusqlite::Result<String> {
        let body = (self.short || self.original).then(|| terms::decode(encoded_body));
        if self.short {
            if let Some(hit) = body
                .as_deref()
                .and_then(|b| like_snippet(b, self.text))
                .or_else(|| like_snippet(title, self.text))
            {
                return Ok(hit);
            }
        }
        // Render only the winning rows. Native trigram snippet windows and the
        // original highlight offsets remain byte-compatible with v5.
        if self.snippet_db.is_none() {
            let db = Connection::open_in_memory()?;
            db.execute_batch(
                "CREATE VIRTUAL TABLE excerpt USING fts5(a,b,tokenize='trigram');
                              SAVEPOINT snippet",
            )?;
            self.snippet_db = Some(db);
        }
        let db = self.snippet_db.as_ref().unwrap();
        let literals;
        let (a, b) = if self.phrase {
            let pieces: String = index.query_one(
                "SELECT literals FROM phrase_content WHERE id=?",
                [id],
                |r| r.get(0),
            )?;
            literals = format!("\x1f{}\x1f", pieces.replace("\x1f\x1e|\x1f", "\x1f\x1f"));
            (literals.as_str(), "")
        } else if self.original {
            (title, body.as_deref().unwrap_or_default())
        } else {
            (encoded_title, encoded_body)
        };
        db.prepare_cached("INSERT INTO excerpt(rowid,a,b) VALUES(1,?,?)")?
            .execute(params![a, b])?;
        let tokens = if self.original { 40 } else { 64 };
        let expression = terms::expression(self.text, self.original, self.phrase);
        let snippet: String = db
            .prepare_cached(
                "SELECT snippet(excerpt,-1,'[',']','…',?) FROM excerpt WHERE excerpt MATCH ?",
            )?
            .query_row(params![tokens, expression], |r| r.get(0))?;
        db.execute_batch("ROLLBACK TO snippet")?;
        Ok(if self.original {
            snippet
        } else {
            terms::decode(&snippet)
        })
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
    let mut query = Query {
        text,
        short,
        original,
        phrase: args.phrase,
        snippet_db: None,
    };
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
    let ordering = if args.source.is_some() {
        ""
    } else {
        "INDEXED BY fragment_order"
    };
    let joined = format!("{table} JOIN fragments f {ordering} ON f.id={table}.rowid");
    let counted = if parameters.len() == 1 {
        table
    } else {
        &joined
    };
    let total: i64 = index
        .query_one(
            &format!("SELECT count(*) FROM {counted} WHERE {filter}"),
            params_from_iter(&parameters),
            |r| r.get(0),
        )
        .map_err(index_error)?;
    parameters.push(args.limit.clamp(1, 500).into());
    let mut stmt = index
        .prepare(&format!(
            "SELECT f.id FROM {joined} WHERE {filter}
         ORDER BY {ranking},f.kind!='daily',f.report_key DESC,f.id LIMIT ?{}",
            parameters.len()
        ))
        .map_err(index_error)?;
    let ids = stmt
        .query_map(params_from_iter(&parameters), |r| r.get::<_, i64>(0))
        .map_err(index_error)?;
    let mut get = index
        .prepare("SELECT kind,report_key,title,body,sources,location FROM fragments WHERE id=?")
        .map_err(index_error)?;
    let mut results = Vec::new();
    for id in ids {
        let id = id.map_err(index_error)?;
        results.push(get.query_row([id], |r| {
            let kind = r.get_ref(0)?.as_str()?;
            let key = r.get_ref(1)?.as_str()?;
            let encoded_title = r.get_ref(2)?.as_str()?;
            let encoded_body = r.get_ref(3)?.as_str()?;
            let title = terms::decode(encoded_title);
            let snippet = query.snippet(index, id, &title, encoded_title, encoded_body)?;
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
    Ok(
        json!({"query":text,"total":total,"index_rebuilt":index_rebuilt,"source_rows_read":source_rows_read,"results":results}),
    )
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
