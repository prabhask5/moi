//! Embeds build information for `moi --version` without requiring git at build time.

fn main() {
    let sha = std::env::var("MOI_GIT_SHA")
        .or_else(|_| std::env::var("GITHUB_SHA"))
        .map(|s| s.chars().take(12).collect::<String>())
        .unwrap_or_else(|_| "unknown".into());
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo:rustc-env=MOI_BUILD_SHA={sha}");
    println!("cargo:rustc-env=MOI_BUILD_TARGET={target}");
    println!("cargo:rerun-if-env-changed=MOI_GIT_SHA");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
}
