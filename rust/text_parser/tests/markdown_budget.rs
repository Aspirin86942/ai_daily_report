use ai_daily_scanner_contract::{default_csv_encodings, TextParseProfile};
use ai_daily_text_parser::parse_light_text;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct CountedSystem;
static ALLOCATED: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountedSystem {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountedSystem = CountedSystem;

#[test]
fn wide_markdown_header_is_rendered_only_for_selected_groups() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wide.md");
    let header = format!("| {} | 金额 |", "h".repeat(32768));
    let row_count = 2048;
    let mut input = format!("{header}\n| --- | --- |\n");
    for i in 0..row_count {
        input += &format!("| 项目{i} | 20.25 |\n");
    }
    input += "下一步核对总账。";
    std::fs::write(&path, &input).unwrap();
    for (budget, kept_headers) in [(128, 0), (header.chars().count() as u64 + 200, 1)] {
        let profile = TextParseProfile {
            backend: "light_text_v2".into(),
            csv_encodings: default_csv_encodings(),
            read_head_bytes: input.len() as u64,
            read_tail_bytes: 1024,
            max_chars: budget,
            excerpt_max_chars: budget,
        };
        let before = ALLOCATED.load(Ordering::Relaxed);
        let parsed = parse_light_text(&path, ".md", &profile, input.len() as u64).unwrap();
        let bytes = ALLOCATED.load(Ordering::Relaxed) - before;
        assert!(
            bytes < header.len() * row_count / 4,
            "allocated {bytes} bytes"
        );
        eprintln!("budget={budget} allocated_bytes={bytes}");
        assert_eq!(
            parsed.content.matches(header.as_str()).count(),
            kept_headers
        );
        assert!(parsed.truncated && parsed.content.ends_with("下一步核对总账。"));
        parsed.evidence_audit.validate(&parsed.content).unwrap();
    }
}
