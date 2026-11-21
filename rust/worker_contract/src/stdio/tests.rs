use super::*;
use crate::WorkerResponse;
use serde::Deserialize;

#[derive(Default)]
struct Output {
    bytes: Vec<u8>,
    writes: usize,
    flushes: usize,
}

impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.writes += 1;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flushes += 1;
        Ok(())
    }
}

fn request(operation: &str, payload: &str) -> String {
    format!(
        r#"{{"contract":"{CONTRACT}","protocol_version":{PROTOCOL_VERSION},"request_id":"de305d54-75b4-431b-adb2-eb6b9e546014","operation":"{operation}","payload":{payload}}}"#
    )
}

fn responses(output: &Output) -> Vec<WorkerResponse> {
    std::str::from_utf8(&output.bytes)
        .unwrap()
        .lines()
        .map(|line| {
            let response: WorkerResponse = serde_json::from_str(line).unwrap();
            response.validate().unwrap();
            response
        })
        .collect()
}

#[test]
fn frame_is_bounded_and_reuses_storage_for_one_write_and_flush() {
    let mut frame = FrameBuffer::new(8);
    let mut output = Output::default();
    frame.encode(&1_234_567_u64).unwrap();
    let allocation = frame.bytes.as_ptr();
    frame.write_to(&mut output).unwrap();
    assert_eq!(output.bytes, b"1234567\n");
    assert_eq!((output.writes, output.flushes), (1, 1));

    let error = frame.encode(&12_345_678_u64).unwrap_err();
    assert_eq!(error.io_error_kind(), Some(io::ErrorKind::FileTooLarge));
    assert!(frame.bytes.is_empty());
    frame.encode(&42).unwrap();
    assert_eq!(frame.bytes.as_ptr(), allocation);
    assert_eq!(frame.bytes.capacity(), 8);
    frame.write_to(&mut output).unwrap();
    assert_eq!(output.bytes, b"1234567\n42\n");
    assert_eq!((output.writes, output.flushes), (2, 2));
}

#[test]
fn v4_reads_last_payload_key_with_exact_numbers_and_rejects_duplicate_envelope_keys() {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Payload {
        exact: Value,
    }

    let number = "123456789012345678901234567890.1234567890123456789";
    let line = request(
        "archive_office_parse",
        &format!(r#"{{"exact":0,"exact":{number}}}"#),
    );
    let input = format!("{line}\r\n{line}");
    let mut output = Output::default();
    let mut frame = FrameBuffer::new(4096);
    assert_eq!(
        serve_session(
            WorkerKind::Office,
            &mut input.as_bytes(),
            &mut output,
            &mut frame,
            |operation, payload| {
                assert_eq!(operation, WorkerOperation::ArchiveOfficeParse);
                handle_payload(
                    payload,
                    |_: &Payload| Ok(()),
                    |input| {
                        Ok(ArchiveArtifact {
                            version: 1,
                            coverage: input.exact.clone(),
                            body_sha256: String::new(),
                            units_sha256: String::new(),
                            index_sha256: String::new(),
                            body_bytes: u64::MAX,
                            units_bytes: 0,
                            units: 0,
                            chars: 0,
                            usable_chars: 0,
                            truncated: false,
                            gaps: vec![],
                            error: None,
                            parser_backend: String::new(),
                            worker_lane: String::new(),
                            duration_ms: 0,
                            raw_sha256: String::new(),
                        })
                    },
                )
            },
        ),
        0
    );
    let out = responses(&output);
    assert_eq!(out.len(), 2);
    assert_eq!((output.writes, output.flushes), (2, 2));
    for response in out {
        let artifact: ArchiveArtifact = serde_json::from_value(response.result.unwrap()).unwrap();
        assert_eq!(artifact.coverage.to_string(), number);
        assert_eq!(artifact.body_bytes, u64::MAX);
    }

    let duplicate = format!("{{\"operation\":\"archive_office_parse\",{}", &line[1..]);
    let mut output = Output::default();
    assert_eq!(
        serve_session(
            WorkerKind::Office,
            &mut duplicate.as_bytes(),
            &mut output,
            &mut frame,
            |_, _| panic!("duplicate envelope field reached the parser"),
        ),
        2
    );
    assert_eq!((output.writes, output.flushes), (0, 0));
}

#[test]
fn invalid_payload_writes_one_error_before_applying_the_existing_close_policy() {
    for (kind, operation, expected_exit, expected_frames) in [
        (WorkerKind::Office, "archive_office_parse", 0, 2),
        (WorkerKind::Office, "office_parse", 2, 1),
        (WorkerKind::Pdf, "pdf_parse", 2, 1),
    ] {
        let line = request(operation, "null");
        let input = format!("{line}\n{line}\n");
        let mut output = Output::default();
        let mut frame = FrameBuffer::new(4096);
        assert_eq!(
            serve_session(
                kind,
                &mut input.as_bytes(),
                &mut output,
                &mut frame,
                |_, _| None
            ),
            expected_exit
        );
        assert_eq!(
            (output.writes, output.flushes),
            (expected_frames, expected_frames)
        );
        let out = responses(&output);
        assert_eq!(out.len(), expected_frames);
        for response in out {
            assert_eq!(response.error.unwrap().error_code, "INVALID_REQUEST");
        }
    }
}
