#![cfg(windows)]

use ai_daily_report::{
    reports::personal::PersonalReport,
    types::{shanghai, utc_window},
};
use chrono::Utc;
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    fs::{self, File, FileTimes},
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc,
    thread,
    time::{Duration, UNIX_EPOCH},
};

const LIMIT: usize = 50 * 1024 * 1024;
const CASES: [(&str, usize); 3] = [
    ("below.xlsx", LIMIT - 1),
    ("equal.xlsx", LIMIT),
    ("above.xlsx", LIMIT + 1),
];

fn crc32(bytes: &[u8]) -> u32 {
    let table: [u32; 256] = std::array::from_fn(|index| {
        (0..8).fold(index as u32, |crc, _| {
            (crc >> 1) ^ if crc & 1 == 1 { 0xedb8_8320 } else { 0 }
        })
    });
    !bytes.iter().fold(!0u32, |crc, &byte| {
        (crc >> 8) ^ table[((crc as u8) ^ byte) as usize]
    })
}

fn write_workbook(file: &mut File, size: usize) {
    let original = include_bytes!("fixtures/personal-headings.xlsx");
    let eocd_start = original.len() - 22;
    let mut end = original[eocd_start..].to_vec();
    assert_eq!(&end[..4], b"PK\x05\x06");
    assert_eq!(&end[20..22], &[0, 0]);
    let directory = u32::from_le_bytes(end[16..20].try_into().unwrap()) as usize;
    let records = u16::from_le_bytes(end[10..12].try_into().unwrap()) + 1;
    let directory_len = eocd_start - directory;

    // The workbook declares application/xml for extra XML parts. Store a valid
    // XML part with trailing whitespace so every byte is inside the ZIP package.
    let name = b"customXml/size-fixture.xml";
    let mut local = [0; 30];
    let mut central = [0; 46];
    let mut payload =
        vec![b' '; size - original.len() - local.len() - central.len() - 2 * name.len()];
    payload[..10].copy_from_slice(b"<padding/>");

    local[..4].copy_from_slice(b"PK\x03\x04");
    local[4..6].copy_from_slice(&20u16.to_le_bytes());
    local[12..14].copy_from_slice(&0x21u16.to_le_bytes());
    local[14..18].copy_from_slice(&crc32(&payload).to_le_bytes());
    let stored_size = (payload.len() as u32).to_le_bytes();
    local[18..22].copy_from_slice(&stored_size);
    local[22..26].copy_from_slice(&stored_size);
    local[26..28].copy_from_slice(&(name.len() as u16).to_le_bytes());

    central[..4].copy_from_slice(b"PK\x01\x02");
    central[4..6].copy_from_slice(&20u16.to_le_bytes());
    central[6..32].copy_from_slice(&local[4..30]);
    central[42..46].copy_from_slice(&(directory as u32).to_le_bytes());
    let new_directory = directory + local.len() + name.len() + payload.len();
    let new_directory_len = directory_len + central.len() + name.len();
    end[8..10].copy_from_slice(&records.to_le_bytes());
    end[10..12].copy_from_slice(&records.to_le_bytes());
    end[12..16].copy_from_slice(&(new_directory_len as u32).to_le_bytes());
    end[16..20].copy_from_slice(&(new_directory as u32).to_le_bytes());
    for chunk in [
        &original[..directory],
        &local,
        name,
        &payload,
        &original[directory..eocd_start],
        &central,
        name,
        &end,
    ] {
        file.write_all(chunk).unwrap();
    }
    assert_eq!(file.metadata().unwrap().len(), size as u64);
}

fn answer(stream: &mut TcpStream) -> Vec<Value> {
    // The listener polls for cancellation. Windows can pass its nonblocking mode to the socket.
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut read = BufReader::new(&mut *stream);
    let mut headers = String::new();
    let mut length = None;
    loop {
        headers.clear();
        assert!(read.read_line(&mut headers).unwrap() > 0);
        if headers == "\r\n" {
            break;
        }
        if let Some(value) = headers.to_ascii_lowercase().strip_prefix("content-length:") {
            length = Some(value.trim().parse().unwrap());
        }
    }
    let mut body = vec![0; length.unwrap()];
    read.read_exact(&mut body).unwrap();
    let request: Value = serde_json::from_slice(&body).unwrap();
    let input: Value =
        serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    let sources = input["sources"].as_array().unwrap();
    let ids: Vec<_> = sources.iter().map(|s| &s["source_id"]).collect();
    let content = json!({"schema_version":5,"items":[{
        "title":"审批待定","text":"工作簿记录金额 120，状态待审批。",
        "kind":"attention","statement_kind":"observed","source_ids":ids
    }]});
    let body =
        json!({"choices":[{"finish_reason":"stop","message":{"content":content.to_string()}}]})
            .to_string();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: application/json\r\n\r\n{}",
        body.len(),
        body
    )
    .unwrap();
    sources.to_vec()
}

#[test]
fn daily_xlsx_size_limit_preserves_admission_and_published_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let config = root.join("config");
    let work = root.join("work");
    fs::create_dir(&config).unwrap();
    fs::create_dir(&work).unwrap();
    let bundled = std::env::var_os("AI_DAILY_TEST_BUNDLE").map(PathBuf::from);
    let exe = bundled.as_ref().map_or_else(
        || PathBuf::from(env!("CARGO_BIN_EXE_ai-daily-report")),
        |path| path.join("ai-daily-report.exe"),
    );
    let day = Utc::now()
        .with_timezone(&shanghai())
        .date_naive()
        .pred_opt()
        .unwrap();
    let stamp = UNIX_EPOCH + Duration::from_secs(utc_window(day).unwrap().0.timestamp() as u64);
    for (name, size) in CASES {
        let mut file = File::create(work.join(name)).unwrap();
        write_workbook(&mut file, size);
        file.set_times(FileTimes::new().set_modified(stamp))
            .unwrap();
    }

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    fs::write(
        config.join("settings.yaml"),
        json!({
            "paths":{"work_dir":work,"data_dir":root.join("data"),"db_dir":root.join("db"),
                "reports_dir":root.join("reports"),"log_dir":root.join("logs")},
            "report":{"profile":"personal","notes_dir":root.join("notes")},
            "archive":{"allow_work_dir_for_model":true},
            "scanner":{"allowed_extensions":[".xlsx"],"max_workers":1},
            "llm":{"base_url":format!("http://{}",listener.local_addr().unwrap()),
                "model_id":"synthetic-local","request_timeout_seconds":5,
                "max_request_utf8_bytes":256000,"context_window_tokens":256000,
                "verified_max_output_tokens":8192,"max_tokens":4096}
        })
        .to_string(),
    )
    .unwrap();
    let (finished, stop) = mpsc::channel::<()>();
    let server = thread::spawn(move || {
        while stop.recv_timeout(Duration::from_millis(5)) == Err(mpsc::RecvTimeoutError::Timeout) {
            match listener.accept() {
                Ok((mut stream, _)) => return Some(answer(&mut stream)),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("local accept failed: {e}"),
            }
        }
        None
    });
    let exe = fs::canonicalize(exe).unwrap();
    let system = std::env::var_os("SystemRoot").unwrap();
    let output = Command::new(&exe)
        .env_clear()
        .env("SystemRoot", &system)
        .env("WINDIR", &system)
        .env("PATH", Path::new(&system).join("System32"))
        .env("TEMP", root)
        .env("TMP", root)
        .env("DEEPSEEK_API_KEY", "synthetic-local")
        .current_dir(root)
        .arg("--config-dir")
        .arg(&config)
        .args(["daily", "--target-date", &day.to_string()])
        .output()
        .unwrap();
    drop(finished);
    let received = server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let outcome: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(outcome["action"], "published");
    assert_eq!(outcome["model_calls"], 1);

    let received = received.expect("daily must send the admitted workbook data to the local model");
    let sent_paths: BTreeSet<_> = received
        .iter()
        .map(|source| source["path"].as_str().unwrap())
        .collect();
    assert_eq!(sent_paths, BTreeSet::from(["below.xlsx", "equal.xlsx"]));
    for name in sent_paths {
        assert!(received
            .iter()
            .any(|source| source["path"] == name
                && source["text"].as_str().unwrap().contains("Pending")));
    }

    let index = rusqlite::Connection::open(root.join("db/scan_cache_v4.sqlite3")).unwrap();
    let (cached, xlsx): (i64, i64) = index.query_one(
        "SELECT count(*),count(*) FILTER(WHERE parser_backend='rust_xlsx_bounded_v2') FROM parse_cache",
        [], |row| Ok((row.get(0)?, row.get(1)?)),
    ).unwrap();
    assert_eq!((cached, xlsx), (2, 2));
    let db = rusqlite::Connection::open(root.join("db/reports.sqlite3")).unwrap();
    let raw: String = db
        .query_row(
            "SELECT raw_json FROM daily_reports WHERE date=?",
            [day.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    let report: PersonalReport = serde_json::from_str(&raw).unwrap();
    report.validate().unwrap();
    assert_eq!(report.quality, "partial");
    assert_eq!(report.selection.selected_files, 2);
    assert_eq!(report.selection.omissions.get("file_size_policy"), Some(&1));
    assert_eq!(report.selection.omitted_paths, ["above.xlsx"]);
    let size_gaps: Vec<_> = report
        .gaps
        .iter()
        .filter(|gap| gap.code == "FILE_SIZE_LIMIT")
        .collect();
    let [gap] = size_gaps.as_slice() else {
        panic!("expected one file-size gap: {size_gaps:?}")
    };
    assert!(gap.source_id.is_some());
    assert_eq!(gap.stage, "parse");
    assert!(
        gap.message.contains(&LIMIT.to_string()) && gap.message.contains(&(LIMIT + 1).to_string())
    );
    let published = Path::new(outcome["output_path"].as_str().unwrap());
    let markdown = fs::read_to_string(published).unwrap();
    assert_eq!(markdown, outcome["markdown"].as_str().unwrap());
    let attachment = report.evidence_attachment.unwrap();
    let link = attachment.link(&day.to_string());
    assert!(markdown.contains(&format!("]({link})")));
    let file = published.parent().unwrap().join(link);
    let bytes = fs::read(file).unwrap();
    assert_eq!(bytes, attachment.markdown.as_bytes());
    assert!(
        attachment.markdown.contains("above.xlsx") && attachment.markdown.contains(&gap.message)
    );
    assert!(
        attachment.markdown.contains("below.xlsx") && attachment.markdown.contains("equal.xlsx")
    );
    println!(
        "candidate={} sizes={:?} published FILE_SIZE_LIMIT",
        exe.display(),
        CASES
    );
}
