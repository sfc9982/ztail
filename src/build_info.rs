//! Stable machine-readable information without host paths or timestamps.
pub fn json() -> String {
    let features = if cfg!(feature = "zstd") {
        r#"["gzip","plain","zstd"]"#
    } else {
        r#"["gzip","plain"]"#
    };
    // Both version (Cargo metadata) and target (validated by build.rs) are
    // build-time constants, never untrusted runtime strings.
    format!(
        "{{\"schema_version\":1,\"name\":\"ztail\",\"version\":\"{}\",\"target\":\"{}\",\"features\":{features}}}\n",
        env!("CARGO_PKG_VERSION"), env!("ZTAIL_BUILD_TARGET")
    )
}
