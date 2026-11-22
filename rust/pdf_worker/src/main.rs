use daily_parsers::{
    pdf_wire::{
        pinned_pdfium_sha256, read_frame, write_frame, PdfCommand, PdfReply, PdfRequestFrame,
        PdfResponseFrame, PdfWorkerHello, MAX_REQUEST_FRAME, PDF_PROTOCOL_VERSION,
    },
    PARSER_CONTENT_IDENTITY,
};
use std::{env, io, process};

mod excerpt;
#[cfg(feature = "ocr")]
mod ocr;
mod reader;
mod source;

use reader::{check_deadline, is_valid_text_char, PdfError, PdfReader};

fn main() -> process::ExitCode {
    match run() {
        Ok(()) => process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            process::ExitCode::FAILURE
        }
    }
}

fn run() -> io::Result<()> {
    let mut args = env::args_os().skip(1);
    match (
        args.next().as_deref().and_then(|arg| arg.to_str()),
        args.next(),
    ) {
        (Some("--build-info"), None) => {
            println!(
                "{}",
                serde_json::json!({
                    "parser_content_identity": PARSER_CONTENT_IDENTITY,
                    "protocol_version": PDF_PROTOCOL_VERSION,
                    "ocr_enabled": cfg!(feature = "ocr"),
                })
            );
            Ok(())
        }
        (Some("--stdio"), None) => serve(&mut io::stdin().lock(), &mut io::stdout().lock()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected --stdio or --build-info",
        )),
    }
}

fn serve(input: &mut impl io::Read, output: &mut impl io::Write) -> io::Result<()> {
    // The parent sends Hello after assigning this process to its kill-on-close job.
    let Some(hello) = read_frame::<PdfRequestFrame>(input, MAX_REQUEST_FRAME)? else {
        return Ok(());
    };
    if hello.version != PDF_PROTOCOL_VERSION
        || hello.id != 0
        || !matches!(hello.command, PdfCommand::Hello)
    {
        return Err(protocol_error());
    }
    let hello_limit = hello.command.response_frame_limit();
    let program = env::current_exe()?;
    let directory = program
        .parent()
        .ok_or_else(|| io::Error::other("worker executable directory missing"))?;
    let reader = match PdfReader::open(directory) {
        Ok(reader) => reader,
        Err(error) => {
            respond(
                output,
                0,
                PdfReply::PdfiumUnavailable(error.to_string()),
                hello_limit,
            )?;
            return Ok(());
        }
    };
    respond(
        output,
        0,
        PdfReply::Hello(PdfWorkerHello {
            pid: process::id(),
            parser_identity: PARSER_CONTENT_IDENTITY.to_owned(),
            pdfium_sha256: pinned_pdfium_sha256().to_owned(),
            ocr: cfg!(feature = "ocr"),
        }),
        hello_limit,
    )?;
    let mut last_id = 0;
    while let Some(frame) = read_frame::<PdfRequestFrame>(input, MAX_REQUEST_FRAME)? {
        if frame.version != PDF_PROTOCOL_VERSION || frame.id <= last_id {
            return Err(protocol_error());
        }
        let response_limit = frame.command.response_frame_limit();
        let PdfCommand::InspectAndExtract(request) = frame.command else {
            return Err(protocol_error());
        };
        let response = reader.inspect_and_extract(&request);
        respond(
            output,
            frame.id,
            PdfReply::InspectAndExtract(Box::new(response)),
            response_limit,
        )?;
        last_id = frame.id;
    }
    Ok(())
}

fn respond(output: &mut impl io::Write, id: u64, reply: PdfReply, limit: usize) -> io::Result<()> {
    write_frame(
        output,
        &PdfResponseFrame {
            version: PDF_PROTOCOL_VERSION,
            id,
            reply,
        },
        limit,
    )
}

fn protocol_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid PDF frame version, request ID or command",
    )
}
