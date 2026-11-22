//! Shared discovery, bounded document parsing and typed evidence.
pub mod contract;
pub mod discovery;
pub mod office;
pub mod pdf_wire;
pub mod settings;
pub mod text;

pub const PARSER_CONTENT_IDENTITY: &str = env!("DAILY_PARSER_IDENTITY");
pub const OCR_AVAILABLE: bool = cfg!(feature = "ocr");

pub fn reader_sha256(mut reader: impl std::io::Read) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(hex(&hash.finalize())),
            Ok(n) => hash.update(&buffer[..n]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(DIGITS[(byte >> 4) as usize] as char);
        text.push(DIGITS[(byte & 15) as usize] as char);
    }
    text
}
