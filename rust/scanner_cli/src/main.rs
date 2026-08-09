use std::io::{self, Read};

use ai_daily_scanner_core::{
    dispatch_with_response_version, invalid_request_output, CommandOutput, EngineShellError,
};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        exit_with_output(invalid_request_output());
    };

    // `--response-version 2` selects the strict InspectRunResponseV2 /
    // VersionResponseV2 surface (spec Part 5.3). Any other trailing argument
    // is rejected exactly as before.
    let mut response_version = 1_u64;
    match args.next() {
        None => {}
        Some(flag) if flag == "--response-version" => {
            let Some(value) = args.next() else {
                exit_with_output(invalid_request_output());
            };
            match value.parse::<u64>() {
                Ok(2) => response_version = 2,
                _ => exit_with_output(invalid_request_output()),
            }
            if args.next().is_some() {
                exit_with_output(invalid_request_output());
            }
        }
        Some(_) => exit_with_output(invalid_request_output()),
    }

    let mut input = Vec::new();
    if command != "version" && io::stdin().read_to_end(&mut input).is_err() {
        exit_with_output(invalid_request_output());
    }

    exit_with_output(dispatch_with_response_version(
        &command,
        &input,
        response_version,
    ));
}

fn exit_with_output(result: Result<CommandOutput, EngineShellError>) -> ! {
    match result {
        Ok(output) => {
            println!("{}", output.json);
            std::process::exit(output.exit_code);
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
