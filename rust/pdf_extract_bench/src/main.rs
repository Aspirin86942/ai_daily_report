// 评估用：pdf-extract 文本提取。质量或速度不过门禁时不进入生产迁移。
use std::env;
use std::process;

fn main() {
    let path = env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: pdf-extract-bench <file.pdf>");
        process::exit(2);
    });

    match pdf_extract::extract_text(&path) {
        Ok(text) => print!("{text}"),
        Err(error) => {
            eprintln!("extract failed: {error}");
            process::exit(1);
        }
    }
}
