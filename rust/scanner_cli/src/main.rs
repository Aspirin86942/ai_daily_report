use std::io::{self, Read};

use ai_daily_scanner_core::{dispatch, invalid_request_output, CommandOutput, EngineShellError};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        exit_with_output(invalid_request_output());
    };
    if args.next().is_some() {
        exit_with_output(invalid_request_output());
    }

    let mut input = Vec::new();
    if command != "version" && io::stdin().read_to_end(&mut input).is_err() {
        exit_with_output(invalid_request_output());
    }

    exit_with_output(dispatch(&command, &input));
}

fn exit_with_output(result: Result<CommandOutput, EngineShellError>) -> ! {
    match result {
        Ok(output) => match serde_json::to_string(&output.payload) {
            Ok(json) => {
                println!("{json}");
                std::process::exit(output.exit_code);
            }
            Err(error) => {
                eprintln!("failed to serialize response: {error}");
                std::process::exit(1);
            }
        },
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
