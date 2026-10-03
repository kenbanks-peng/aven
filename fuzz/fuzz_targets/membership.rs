#![no_main]
libfuzzer_sys::fuzz_target!(|data: &[u8]| aven_core::sync::fuzz::membership(data));
