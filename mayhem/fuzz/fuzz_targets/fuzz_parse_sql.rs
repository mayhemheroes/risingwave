#![no_main]
use libfuzzer_sys::fuzz_target;
use risingwave_sqlparser::parser::Parser;

// Port of the upstream honggfuzz harness (src/sqlparser/fuzz/fuzz_targets/fuzz_parse_sql.rs)
// as a libFuzzer target for Mayhem. Ignore parse errors — only real panics/UB should crash.
fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = Parser::parse_sql(s);
    }
});
