fn main() {
    // Cargo supplies the target triple. Do not invoke git, record host paths,
    // timestamps, or depend on a VCS checkout when building source packages.
    let target = std::env::var("TARGET").expect("Cargo supplies TARGET");
    assert!(target
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)));
    println!("cargo:rustc-env=ZTAIL_BUILD_TARGET={target}");
    println!("cargo:rerun-if-changed=build.rs");
}
