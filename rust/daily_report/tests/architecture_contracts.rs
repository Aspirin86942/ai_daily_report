use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

fn rust_workspace_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("daily_report lives under rust/")
        .to_path_buf()
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", dir.display()))
        .map(|entry| entry.expect("dir entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

fn rel_posix(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .expect("path under root")
        .to_string_lossy()
        .replace('\\', "/")
}

fn is_test_only_file(rel: &str) -> bool {
    rel == "tests.rs" || rel.ends_with("/tests.rs") || rel.contains("/tests/")
}

/// Extract production lines while preserving code below `#[cfg(test)]\nmod tests;`.
fn non_test_source(source: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();
    for (idx, line) in lines.iter().enumerate() {
        if line.trim() == "#[cfg(test)]" {
            let next = lines[idx + 1..]
                .iter()
                .map(|l| l.trim())
                .find(|l| !l.is_empty());
            if let Some(next) = next {
                if next.starts_with("mod ") && next.ends_with(';') {
                    continue;
                }
            }
            return lines[..idx].join("\n");
        }
    }
    source.to_owned()
}

fn non_test_line_count(source: &str) -> usize {
    let lines: Vec<&str> = source.lines().collect();
    for (idx, line) in lines.iter().enumerate() {
        if line.trim() == "#[cfg(test)]" {
            let next = lines[idx + 1..]
                .iter()
                .map(|l| l.trim())
                .find(|l| !l.is_empty());
            if let Some(next) = next {
                if next.starts_with("mod ") && next.ends_with(';') {
                    continue;
                }
            }
            return idx;
        }
    }
    lines.len()
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn strip_comments_and_strings(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"//") {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            out.push(b' ');
            continue;
        }
        if bytes[i..].starts_with(b"/*") {
            let mut depth = 1usize;
            i += 2;
            while i < bytes.len() && depth > 0 {
                if bytes[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if bytes[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            out.push(b' ');
            continue;
        }
        // Raw string literals: `r"..."`, `r#"..."#`, `br#"..."#`
        let prev_is_ident = i > 0 && is_ident_byte(bytes[i - 1]);
        if !prev_is_ident && (bytes[i] == b'r' || bytes[i..].starts_with(b"br")) {
            let start_hashes = i + if bytes[i] == b'b' { 2 } else { 1 };
            let mut hashes = 0usize;
            while start_hashes + hashes < bytes.len() && bytes[start_hashes + hashes] == b'#' {
                hashes += 1;
            }
            if start_hashes + hashes < bytes.len() && bytes[start_hashes + hashes] == b'"' {
                i = start_hashes + hashes + 1;
                while i < bytes.len() {
                    if bytes[i] == b'"'
                        && i + 1 + hashes <= bytes.len()
                        && bytes[i + 1..i + 1 + hashes].iter().all(|&b| b == b'#')
                    {
                        i += 1 + hashes;
                        break;
                    }
                    i += 1;
                }
                out.extend_from_slice(b"\"\"");
                continue;
            }
        }
        if bytes[i] == b'"' {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(bytes.len());
                } else if bytes[i] == b'"' {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            out.extend_from_slice(b"\"\"");
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    // Replacing only ASCII comment/string spans preserves valid UTF-8 boundaries.
    String::from_utf8(out).expect("valid utf-8 after stripping")
}

fn leading_ident(s: &str) -> Option<&str> {
    let trimmed = s.trim_start();
    let end = trimmed
        .as_bytes()
        .iter()
        .position(|&b| !is_ident_byte(b))
        .unwrap_or(trimmed.len());
    (end > 0).then(|| &trimmed[..end])
}

fn parse_top_level_braced_arms(group: &str, known: &BTreeSet<String>, out: &mut BTreeSet<String>) {
    let mut depth = 0usize;
    let mut start = 0usize;
    for (idx, ch) in group.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                if let Some(ident) = leading_ident(&group[start..idx]) {
                    if known.contains(ident) {
                        out.insert(ident.to_owned());
                    }
                }
                start = idx + 1;
            }
            _ => {}
        }
    }
    if let Some(ident) = leading_ident(&group[start..]) {
        if known.contains(ident) {
            out.insert(ident.to_owned());
        }
    }
}

fn parse_crate_refs(source: &str, known: &BTreeSet<String>) -> BTreeSet<String> {
    let cleaned = strip_comments_and_strings(source);
    let bytes = cleaned.as_bytes();
    let mut deps = BTreeSet::new();
    let mut search_from = 0usize;
    while let Some(rel_pos) = cleaned[search_from..].find("crate::") {
        let pos = search_from + rel_pos;
        if pos > 0 && is_ident_byte(bytes[pos - 1]) {
            search_from = pos + 7;
            continue;
        }
        let after = &cleaned[pos + 7..];
        let trimmed = after.trim_start();
        if let Some(rest) = trimmed.strip_prefix('{') {
            let mut depth = 1usize;
            let mut end = rest.len();
            for (idx, ch) in rest.char_indices() {
                match ch {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            end = idx;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            parse_top_level_braced_arms(&rest[..end], known, &mut deps);
        } else if let Some(ident) = leading_ident(trimmed) {
            if known.contains(ident) {
                deps.insert(ident.to_owned());
            }
        }
        search_from = pos + 7;
    }
    deps
}

struct CrateGraph {
    nodes: BTreeSet<String>,
    edges: BTreeMap<String, BTreeSet<String>>,
    non_test_files: Vec<(String, String)>,
    non_test_lines: usize,
}

fn build_crate_graph(crate_src_dir: &Path) -> CrateGraph {
    let mut files = Vec::new();
    collect_rs_files(crate_src_dir, &mut files);

    let mut nodes = BTreeSet::from(["_root_".to_owned()]);
    for entry in fs::read_dir(crate_src_dir).expect("read crate src") {
        let path = entry.expect("entry").path();
        if path.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
            let stem = path.file_stem().unwrap().to_string_lossy();
            if stem != "lib" && stem != "main" {
                nodes.insert(stem.into_owned());
            }
        } else if path.is_dir() && path.join("mod.rs").is_file() {
            nodes.insert(path.file_name().unwrap().to_string_lossy().into_owned());
        }
    }

    let mut edges: BTreeMap<String, BTreeSet<String>> = nodes
        .iter()
        .map(|node| (node.clone(), BTreeSet::new()))
        .collect();
    let mut non_test_files = Vec::new();
    let mut non_test_lines = 0usize;

    for path in files {
        let rel = rel_posix(crate_src_dir, &path);
        if is_test_only_file(&rel) {
            continue;
        }
        let raw = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        non_test_lines += non_test_line_count(&raw);
        let prod = non_test_source(&raw);
        let owner = match rel.split('/').next() {
            Some("lib.rs" | "main.rs") => "_root_".to_owned(),
            Some(first) => first.strip_suffix(".rs").unwrap_or(first).to_owned(),
            None => "_root_".to_owned(),
        };
        if nodes.contains(&owner) {
            for target in parse_crate_refs(&prod, &nodes) {
                if target != owner {
                    edges.get_mut(&owner).unwrap().insert(target);
                }
            }
        }
        non_test_files.push((rel, prod));
    }

    CrateGraph {
        nodes,
        edges,
        non_test_files,
        non_test_lines,
    }
}

struct Tarjan<'a> {
    edges: &'a BTreeMap<String, BTreeSet<String>>,
    index: usize,
    indices: BTreeMap<String, usize>,
    lowlink: BTreeMap<String, usize>,
    stack: Vec<String>,
    on_stack: BTreeSet<String>,
    sccs: Vec<Vec<String>>,
}

impl<'a> Tarjan<'a> {
    fn find_cycles(graph: &'a CrateGraph) -> Vec<Vec<String>> {
        let mut solver = Self {
            edges: &graph.edges,
            index: 0,
            indices: BTreeMap::new(),
            lowlink: BTreeMap::new(),
            stack: Vec::new(),
            on_stack: BTreeSet::new(),
            sccs: Vec::new(),
        };
        for node in &graph.nodes {
            if !solver.indices.contains_key(node) {
                solver.strong_connect(node);
            }
        }
        solver.sccs.into_iter().filter(|c| c.len() > 1).collect()
    }

    fn strong_connect(&mut self, v: &str) {
        let v_idx = self.index;
        self.index += 1;
        self.indices.insert(v.to_owned(), v_idx);
        self.lowlink.insert(v.to_owned(), v_idx);
        self.stack.push(v.to_owned());
        self.on_stack.insert(v.to_owned());

        if let Some(neighbors) = self.edges.get(v) {
            for w in neighbors {
                if !self.indices.contains_key(w) {
                    self.strong_connect(w);
                    let low_w = self.lowlink[w];
                    let low_v = self.lowlink.get_mut(v).unwrap();
                    *low_v = (*low_v).min(low_w);
                } else if self.on_stack.contains(w) {
                    let idx_w = self.indices[w];
                    let low_v = self.lowlink.get_mut(v).unwrap();
                    *low_v = (*low_v).min(idx_w);
                }
            }
        }

        if self.lowlink[v] == v_idx {
            let mut component = Vec::new();
            while let Some(w) = self.stack.pop() {
                self.on_stack.remove(&w);
                let done = w == v;
                component.push(w);
                if done {
                    break;
                }
            }
            component.sort();
            self.sccs.push(component);
        }
    }
}

#[test]
fn workspace_has_zero_module_cycles() {
    let rust_dir = rust_workspace_dir();
    for crate_name in ["daily_report", "daily_parsers", "pdf_worker"] {
        let graph = build_crate_graph(&rust_dir.join(crate_name).join("src"));
        let cycles = Tarjan::find_cycles(&graph);
        assert!(
            cycles.is_empty(),
            "crate {crate_name} introduced module SCC cycle(s): {cycles:?}"
        );
    }
}

#[test]
fn daily_report_and_parsers_layer_boundaries() {
    let rust_dir = rust_workspace_dir();
    let dr = build_crate_graph(&rust_dir.join("daily_report/src"));

    let assert_allowed = |module: &str, allowed: &[&str]| {
        let allowed_set: BTreeSet<String> = allowed.iter().map(|s| (*s).to_owned()).collect();
        let actual = &dr.edges[module];
        let forbidden: Vec<_> = actual.difference(&allowed_set).collect();
        assert!(
            forbidden.is_empty(),
            "daily_report module `{module}` violated layer boundary: unexpected deps {forbidden:?} (actual: {actual:?})"
        );
    };

    assert_allowed("infra", &[]);
    assert_allowed("types", &["infra"]);
    assert_allowed("processing_cache", &["infra", "types"]);
    assert_allowed("scan", &["infra", "types"]);
    assert_allowed("reports", &["infra", "scan", "types"]);

    let dp = build_crate_graph(&rust_dir.join("daily_parsers/src"));
    assert!(
        !dp.edges["settings"].contains("contract"),
        "daily_parsers::settings must never depend back on contract: {:?}",
        dp.edges["settings"]
    );
}

#[test]
fn single_source_of_truth_invariants() {
    let rust_dir = rust_workspace_dir();
    let dr = build_crate_graph(&rust_dir.join("daily_report/src"));

    let assert_only_in = |needle: &str, allowed_files: &[&str]| {
        let mut hits = Vec::new();
        for (rel, prod) in &dr.non_test_files {
            if prod.contains(needle) {
                hits.push(rel.as_str());
            }
        }
        assert!(
            !hits.is_empty(),
            "expected SSOT needle `{needle}` to appear in at least one file"
        );
        let unexpected: Vec<_> = hits
            .iter()
            .copied()
            .filter(|rel| {
                !allowed_files.iter().any(|allowed| {
                    if let Some(prefix) = allowed.strip_suffix('*') {
                        rel.starts_with(prefix)
                    } else {
                        *rel == *allowed
                    }
                })
            })
            .collect();
        assert!(
            unexpected.is_empty(),
            "SSOT needle `{needle}` leaked outside {allowed_files:?}: found in {unexpected:?}"
        );
    };

    assert_only_in("FixedOffset::east_opt", &["types.rs"]);
    assert_only_in(r#"\\?\UNC\"#, &["infra/fs.rs"]);
    assert_only_in("\"reports.sqlite3\"", &["types.rs"]);
    assert_only_in("\"report_search.sqlite3\"", &["types.rs"]);
    assert_only_in("\"scan_cache_v4.sqlite3\"", &["types.rs"]);
    assert_only_in("\"personal_run:\"", &["store/*"]);
    assert_only_in("\"personal_calls:\"", &["store/*"]);
    assert_only_in("\"personal_summary:\"", &["store/*"]);
    assert_only_in("\"personal_compacted:\"", &["store/*"]);
    assert_only_in(
        "Connection::open_with_flags",
        &["infra/sqlite.rs", "scan/cache/storage.rs"],
    );
}

#[test]
fn workspace_non_test_line_ceiling() {
    let rust_dir = rust_workspace_dir();
    let total: usize = ["daily_report", "daily_parsers", "pdf_worker"]
        .into_iter()
        .map(|name| build_crate_graph(&rust_dir.join(name).join("src")).non_test_lines)
        .sum();
    assert!(
        total <= 31_000,
        "workspace non-test source lines exceeded ceiling: {total} > 31000"
    );
}
