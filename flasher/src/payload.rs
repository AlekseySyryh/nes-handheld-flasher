/// RP2040 payload firmware in UF2 format, built and embedded by `build.rs`.
pub static PAYLOAD_UF2: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/rp2040-payload.uf2"));
