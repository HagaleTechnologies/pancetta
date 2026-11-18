//! Build identity, compiled by both build.rs and the binary's tests (PAN-377).
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) const GIT_LOCATION_VARS: [&str; 5] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_PREFIX",
];
pub(crate) const WATCHED_GIT_PATHS: [&str; 4] = ["HEAD", "refs/heads", "packed-refs", "reftable"];

pub(crate) fn git_command(manifest_dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command.args(args).current_dir(manifest_dir);
    // Discover the repository holding these sources, not an outer hook's repository.
    for name in GIT_LOCATION_VARS {
        command.env_remove(name);
    }
    command
}

pub(crate) fn git(manifest_dir: &Path, args: &[&str]) -> Option<String> {
    let output = git_command(manifest_dir, args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!value.is_empty()).then_some(value)
}

/// Missing rerun paths cause Cargo to rebuild the package on every build.
pub(crate) fn rerun_paths(
    manifest_dir: &Path,
    git_path: impl Fn(&str) -> Option<String>,
) -> Vec<PathBuf> {
    WATCHED_GIT_PATHS
        .iter()
        .filter_map(|name| git_path(name).map(|path| manifest_dir.join(path)))
        .filter(|path| path.exists())
        .collect()
}

pub(crate) fn short_sha(value: Option<&str>) -> String {
    let value = value.unwrap_or("").trim();
    if (7..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        value.to_owned()
    } else {
        "unknown".to_owned()
    }
}

pub(crate) fn build_date(
    source_date_epoch: Option<&str>,
    now_unix_secs: i64,
) -> Result<String, String> {
    let raw = source_date_epoch.unwrap_or("").trim();
    let seconds = if raw.is_empty() {
        now_unix_secs
    } else {
        raw.parse::<u64>()
            .ok()
            .and_then(|n| i64::try_from(n).ok())
            .ok_or_else(|| {
                format!("SOURCE_DATE_EPOCH must be whole seconds since 1970-01-01 UTC, got {raw:?}")
            })?
    };
    chrono::DateTime::from_timestamp(seconds, 0)
        .map(|date| date.format("%Y-%m-%d").to_string())
        .ok_or_else(|| format!("SOURCE_DATE_EPOCH is out of range: {seconds}"))
}

#[cfg(test)]
#[path = "build_identity/tests/build_identity_tests.rs"]
mod build_identity_tests;
