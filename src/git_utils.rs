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

    pub fn remote_url(&self) -> Option<String> {
        let remotes = self.repo.remotes().ok()?;
        for name in remotes.iter().flatten() {
            if let Ok(remote) = self.repo.find_remote(name) {
                if let Some(url) = remote.url() {
                    if !url.is_empty() {
                        return Some(url.to_string());
                    }
                }
            }
        }

        None
    }

    pub fn head_commit_sha(&self) -> Option<String> {
        let head = self.repo.head().ok()?;
        let commit = head.peel_to_commit().ok()?;
        Some(commit.id().to_string())
    }

    pub fn current_branch(&self) -> Option<String> {
        let head = self.repo.head().ok()?;
        head.shorthand().map(|s| s.to_string())
    }

    /// Whether `rev` (a commit SHA) resolves in this repository — false for a
    /// ref lost to a force-push or absent from a shallow clone, in which case an
    /// incremental diff against it isn't possible.
    pub fn has_commit(&self, rev: &str) -> bool {
        self.repo
            .revparse_single(rev)
            .and_then(|obj| obj.peel_to_commit())
            .is_ok()
    }

    /// Repository-relative paths that differ between `base` and the current
    /// HEAD (the union of each delta's old and new path). `None` if `base` can't
    /// be resolved locally — the caller should fall back to a full sync.
    pub fn changed_paths_since(&self, base: &str) -> Option<HashSet<PathBuf>> {
        let base_tree = self
            .repo
            .revparse_single(base)
            .ok()?
            .peel_to_commit()
            .ok()?
            .tree()
            .ok()?;
        let head_tree = self.repo.head().ok()?.peel_to_commit().ok()?.tree().ok()?;
        let diff = self
            .repo
            .diff_tree_to_tree(Some(&base_tree), Some(&head_tree), None)
            .ok()?;
        let mut paths = HashSet::new();
        for delta in diff.deltas() {
            if let Some(p) = delta.old_file().path() {
                paths.insert(p.to_path_buf());
            }
            if let Some(p) = delta.new_file().path() {
                paths.insert(p.to_path_buf());
            }
        }
        Some(paths)
    }
}

pub fn open_git_repository(path: &Path) -> Option<GitRepository> {
    let repo = Repository::discover(path).ok()?;
    let workdir = repo
        .workdir()
        .or_else(|| repo.path().parent())
        .map(|p| p.to_path_buf())?;
    let workdir = workdir.canonicalize().unwrap_or_else(|_| workdir.clone());

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

    /// Bounded timestamps for an incremental sync: walk only the commits in
    /// `(base, HEAD]` and record, for each path, the most recent commit that
    /// touched it in that range. Both `addition` and `last_change` are set to
    /// that time — a delta re-derives `updated` cheaply and lets the server keep
    /// the original `created` for specs that already exist. Paths not touched in
    /// the range are absent (untouched specs aren't in a delta anyway). Falls
    /// back to an empty cache if `base` can't be resolved.
    pub fn since(repo: &GitRepository, base: &str, paths: &[PathBuf]) -> Self {
        build_cache_since(repo, base, paths)
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
    let mut times: HashMap<PathBuf, PathTimes> = rel_paths
        .iter()
        .map(|p| (p.clone(), PathTimes::default()))
        .collect();

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

fn build_cache_since(repo: &GitRepository, base: &str, paths: &[PathBuf]) -> GitTimestampCache {
    let rel_paths = normalize_paths(&repo.workdir, paths);
    let mut times: HashMap<PathBuf, PathTimes> = rel_paths
        .iter()
        .map(|p| (p.clone(), PathTimes::default()))
        .collect();

    if times.is_empty() {
        return GitTimestampCache { times };
    }

    let base_oid = match repo
        .repo
        .revparse_single(base)
        .ok()
        .and_then(|obj| obj.peel_to_commit().ok())
    {
        Some(commit) => commit.id(),
        None => return GitTimestampCache { times },
    };

    let mut pending: HashSet<PathBuf> = rel_paths.iter().cloned().collect();

    let mut revwalk = match repo.repo.revwalk() {
        Ok(walk) => walk,
        Err(_) => return GitTimestampCache { times },
    };
    let _ = revwalk.set_sorting(Sort::TIME);
    let _ = revwalk.push_head();
    // Bound the walk to commits newer than `base`.
    let _ = revwalk.hide(base_oid);

    for oid in revwalk {
        let Ok(oid) = oid else { continue };
        let Ok(commit) = repo.repo.find_commit(oid) else {
            continue;
        };
        let time = commit_time_to_millis(&commit);
        let Ok(tree) = commit.tree() else { continue };
        let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());

        if let Ok(diff) = repo
            .repo
            .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None)
        {
            for delta in diff.deltas() {
                let Some(path) = delta.new_file().path().or_else(|| delta.old_file().path()) else {
                    continue;
                };
                if !pending.contains(path) {
                    continue;
                }
                // TIME sorting means the first sighting is the most recent
                // change. Record it as both the addition and last-change time.
                if let Some(entry) = times.get_mut(path) {
                    entry.addition = Some(time);
                    entry.last_change = Some(time);
                }
                pending.remove(path);
            }
        }

        if pending.is_empty() {
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
