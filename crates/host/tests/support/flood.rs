// Shared by the finite test child and the subprocess assertions.
pub const LINES: usize = 4608;
pub const LINE_BYTES: usize = 2048;
pub const FINAL: &str = "HOST-SAFETY-FLOOD-FINAL-4608";

pub fn line(index: usize) -> Vec<u8> {
    let mut bytes = format!("FLOOD{index:05}\r\x1b[32m").into_bytes();
    bytes.resize(LINE_BYTES - 4, b'x');
    bytes.extend_from_slice(b"\x1b[0m");
    bytes
}
