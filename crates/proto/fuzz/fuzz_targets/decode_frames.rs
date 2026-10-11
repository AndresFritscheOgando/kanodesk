//! Fuzzes the stream decoder: arbitrary bytes, arriving in arbitrary chunks,
//! decoded as frames and as every message type. See `kanodesk_proto::fuzz`.
//!
//! cargo +nightly fuzz run decode_frames
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    kanodesk_proto::fuzz::decode_all(data);
});
