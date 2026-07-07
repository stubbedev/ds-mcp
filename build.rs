fn main() {
    // Counterpart to the CXXSTDLIB overrides in .cargo/config.toml: link
    // libstdc++ statically on linux-gnu (duckdb's bundled engine is C++).
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os == "linux" && env == "gnu" {
        let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
        let out = std::process::Command::new(cc)
            .arg("-print-file-name=libstdc++.a")
            .output()
            .expect("run cc to locate libstdc++.a");
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let dir = std::path::Path::new(&path)
            .parent()
            .expect("libstdc++.a has a parent dir");
        println!("cargo:rustc-link-search=native={}", dir.display());
        println!("cargo:rustc-link-lib=static=stdc++");
        println!("cargo:rustc-link-arg=-static-libgcc");
    }
}
