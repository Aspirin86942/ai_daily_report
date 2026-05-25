use ai_daily_discovery::{discover_files, DiscoveryRequest};
use std::io::{self, Read};

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let request: DiscoveryRequest = serde_json::from_str(&input)?;
    let files = discover_files(&request)?;
    println!("{}", serde_json::to_string(&files)?);
    Ok(())
}
