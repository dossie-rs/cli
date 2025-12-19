use git2::{Delta, Repository, Sort};
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub struct GitRepository {
    repo: Repository,
    workdir: PathBuf,
}

impl GitRepository {
    pub fn workdir(&self) -> &Path {
        &self.workdir
    }
}

pub fn open_git_repository(path: &Path) -> Option<GitRepository> {
    let repo = Repository::discover(path).ok()?;
    let workdir = repo
        .workdir()
        .or_else(|| repo.path().parent())
        .map(|p| p.to_path_buf())?;
    let workdir = workdir
        .canonicalize()
        .unwrap_or_else(|_| workdir.clone());

    Some(GitRepository { repo, workdir })
}

pub fn first_commit_timestamp(repo: &GitRepository, paths: &[PathBuf]) -> Option<i64> {
    GitTimestampCache::from_paths(repo, paths).latest_addition(paths)
}

pub fn last_commit_timestamp(repo: &GitRepository, paths: &[PathBuf]) -> Option<i64> {
    GitTimestampCache::from_paths(repo, paths).latest_change(paths)
}

#[derive(Default, Clone, Copy)]
struct PathTimes {
    addition: Option<i64>,
    last_change: Option<i64>,
}

pub struct GitTimestampCache {
    times: HashMap<PathBuf, PathTimes>,
}

impl GitTimestampCache {
    pub fn from_paths(repo: &GitRepository, paths: &[PathBuf]) -> Self {
        build_cache(repo, paths)
    }

    pub fn latest_addition(&self, paths: &[PathBuf]) -> Option<i64> {
        paths
            .iter()
            .filter_map(|path| self.times.get(path))
            .filter_map(|times| times.addition)
            .max()
    }

    pub fn latest_change(&self, paths: &[PathBuf]) -> Option<i64> {
        paths
            .iter()
            .filter_map(|path| self.times.get(path))
            .filter_map(|times| times.last_change)
            .max()
    }
}

struct UpdateFlags {
    addition: bool,
    last_change: bool,
}

fn build_cache(repo: &GitRepository, paths: &[PathBuf]) -> GitTimestampCache {
    let rel_paths = normalize_paths(&repo.workdir, paths);
    let mut times: HashMap<PathBuf, PathTimes> =
        rel_paths.iter().map(|p| (p.clone(), PathTimes::default())).collect();

    if times.is_empty() {
        return GitTimestampCache { times };
    }

    let mut pending_additions: HashSet<PathBuf> = rel_paths.iter().cloned().collect();
    let mut pending_changes: HashSet<PathBuf> = rel_paths.iter().cloned().collect();

    let mut revwalk = match repo.repo.revwalk() {
        Ok(walk) => walk,
        Err(_) => return GitTimestampCache { times },
    };
    let _ = revwalk.set_sorting(Sort::TIME);
    let _ = revwalk.push_head();

    for oid in revwalk {
        let oid = match oid {
            Ok(oid) => oid,
            Err(_) => continue,
        };
        let commit = match repo.repo.find_commit(oid) {
            Ok(commit) => commit,
            Err(_) => continue,
        };
        let time = commit_time_to_millis(&commit);
        let tree = match commit.tree() {
            Ok(tree) => tree,
            Err(_) => continue,
        };
        let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());

        let diff = repo
            .repo
            .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None);

        if let Ok(diff) = diff {
            for delta in diff.deltas() {
                let path = delta.new_file().path().or_else(|| delta.old_file().path());
                let Some(path) = path else { continue };
                if !times.contains_key(path) {
                    continue;
                }

                let status = delta.status();
                let mut updated = UpdateFlags {
                    addition: false,
                    last_change: false,
                };

                if pending_changes.contains(path) {
                    if let Some(entry) = times.get_mut(path) {
                        if entry.last_change.is_none() {
                            entry.last_change = Some(time);
                            updated.last_change = true;
                        }
                    }
                }

                if pending_additions.contains(path)
                    && matches!(status, Delta::Added | Delta::Renamed | Delta::Copied)
                {
                    if let Some(entry) = times.get_mut(path) {
                        if entry.addition.is_none() {
                            entry.addition = Some(time);
                            updated.addition = true;
                        }
                    }
                }

                if updated.last_change {
                    pending_changes.remove(path);
                }
                if updated.addition {
                    pending_additions.remove(path);
                }
            }
        }

        if pending_additions.is_empty() && pending_changes.is_empty() {
            break;
        }
    }

    GitTimestampCache { times }
}

fn normalize_paths(root: &Path, paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut normalized: HashSet<PathBuf> = HashSet::new();

    for path in paths {
        let absolute = if path.is_absolute() {
            path.canonicalize()
                .ok()
                .unwrap_or_else(|| path.to_path_buf())
        } else {
            root.join(path)
                .canonicalize()
                .ok()
                .unwrap_or_else(|| root.join(path))
        };

        if let Ok(relative) = absolute.strip_prefix(root) {
            normalized.insert(relative.to_path_buf());
        }
    }

    normalized.into_iter().collect()
}

fn commit_time_to_millis(commit: &git2::Commit) -> i64 {
    commit.time().seconds() * 1000
}
