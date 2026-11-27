//! Embeds build information for `moi --version` without requiring git at build time.

fn main() {
    let sha = std::env::var("MOI_GIT_SHA")
        .or_else(|_| std::env::var("GITHUB_SHA"))
        .map(|s| s.chars().take(12).collect::<String>())
        .unwrap_or_else(|_| "unknown".into());
    let target = std::env::var("TARGET").unwrap_or_default();
    // the day of the build: `SOURCE_DATE_EPOCH` when set (reproducible builds), else now
    let seconds = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .and_then(|d| i64::try_from(d.as_secs()).ok())
        })
        .unwrap_or(0);
    // days since 1970-01-01 to a civil date (Howard Hinnant's algorithm)
    let z = seconds.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let (day, month) = (doy - (153 * mp + 2) / 5 + 1, if mp < 10 { mp + 3 } else { mp - 9 });
    let year = yoe + era * 400 + i64::from(month <= 2);
    println!("cargo:rustc-env=MOI_BUILD_DATE={year:04}-{month:02}-{day:02}");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    println!("cargo:rustc-env=MOI_BUILD_SHA={sha}");
    println!("cargo:rustc-env=MOI_BUILD_TARGET={target}");
    println!("cargo:rerun-if-env-changed=MOI_GIT_SHA");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
}
