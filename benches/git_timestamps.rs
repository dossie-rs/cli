use chrono::DateTime;
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use dossiers::git_utils::{open_git_repository, GitTimestampCache};
use std::path::{Path, PathBuf};
use std::process::Command;

fn bench_git_timestamps(c: &mut Criterion) {
    let repo = open_git_repository(Path::new(".")).expect("repository available");
    let git_root = repo.workdir().to_path_buf();
    let paths = sample_paths(&git_root);
    let cache = GitTimestampCache::from_paths(&repo, &paths);

    // Ensure both implementations produce values before benchmarking.
    let _ = cache.latest_addition(&paths);
    let _ = cache.latest_change(&paths);

    c.bench_function("cli_last_commit", |b| {
        b.iter(|| cli_last_commit(black_box(&git_root), black_box(&paths)))
    });

    c.bench_function("git2_last_commit_cached", |b| {
        b.iter(|| cache.latest_change(black_box(&paths)))
    });

    c.bench_function("git2_first_commit", |b| {
        b.iter(|| cache.latest_addition(black_box(&paths)))
    });

    c.bench_function("cli_first_commit", |b| {
        b.iter(|| cli_first_commit(black_box(&git_root), black_box(&paths)))
    });

    c.bench_function("git2_cache_build", |b| {
        b.iter(|| GitTimestampCache::from_paths(black_box(&repo), black_box(&paths)))
    });
}

fn sample_paths(root: &Path) -> Vec<PathBuf> {
    vec![root.join("src/main.rs"), root.join("Cargo.toml")]
}

fn cli_last_commit(git_root: &Path, paths: &[PathBuf]) -> Option<i64> {
    cli_git_timestamp(git_root, paths, false)
}

fn cli_first_commit(git_root: &Path, paths: &[PathBuf]) -> Option<i64> {
    cli_git_timestamp(git_root, paths, true)
}

fn cli_git_timestamp(git_root: &Path, paths: &[PathBuf], first: bool) -> Option<i64> {
    if paths.is_empty() {
        return None;
    }

    let mut command = Command::new("git");
    command.arg("-C").arg(git_root).arg("log");

    if first {
        command.arg("--diff-filter=A");
    }

    command.arg("--format=%cI").arg("-1").arg("--");

    for path in paths {
        let relative = path.strip_prefix(git_root).unwrap_or(path);
        command.arg(relative);
    }

    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }

    let timestamp = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()?
        .trim()
        .to_string();

    if timestamp.is_empty() {
        return None;
    }

    DateTime::parse_from_rfc3339(&timestamp)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

criterion_group!(benches, bench_git_timestamps);
criterion_main!(benches);
