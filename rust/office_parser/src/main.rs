use std::io::{self, Read};

use ai_daily_office_parser::{parse_office_file, worker_version_response, OfficeParseRequest};

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["version"] {
        println!("{}", serde_json::to_string(&worker_version_response())?);
        return Ok(());
    }
    if !args.is_empty() {
        return Err("usage: ai-daily-office-parser [version]".into());
    }

    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let request: OfficeParseRequest = serde_json::from_str(&input)?;
    let context = parse_office_file(&request);
    println!("{}", serde_json::to_string(&context)?);
    Ok(())
}
