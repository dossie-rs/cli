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

/// The git identity that first added a file: the *author* of the addition
/// commit (who wrote it), not the committer (who applied it). Used to credit a
/// spec that declares no `authors:` frontmatter. `email` feeds a GitHub / Gravatar
/// avatar lookup; `commit_sha` lets the producer resolve the linked GitHub
/// account via the commits API.
#[derive(Debug, Clone)]
pub struct GitAuthor {
    pub name: String,
    pub email: String,
    pub commit_sha: String,
}

#[derive(Default, Clone)]
struct PathTimes {
    addition: Option<i64>,
    last_change: Option<i64>,
    /// Author of the commit that added this path (only set on an
    /// Added/Renamed/Copied delta, never a plain modification).
    addition_author: Option<GitAuthor>,
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

    /// The author of the commit that added the spec, tied to the same addition
    /// as [`latest_addition`] (the most recent addition among `paths`) so the
    /// credited author and the displayed "Created" date come from one commit.
    /// `None` when no addition was recorded — e.g. an existing file merely
    /// modified within an incremental range, so the caller keeps the stored
    /// author instead of overwriting it with the editor.
    pub fn addition_author(&self, paths: &[PathBuf]) -> Option<&GitAuthor> {
        paths
            .iter()
            .filter_map(|path| self.times.get(path))
            .filter(|times| times.addition.is_some())
            .max_by_key(|times| times.addition)
            .and_then(|times| times.addition_author.as_ref())
    }
}

/// Extract the git *author* identity from a commit (name, email, and the
/// commit's own SHA). `None` when the signature carries neither name nor email.
fn commit_author(commit: &git2::Commit) -> Option<GitAuthor> {
    let sig = commit.author();
    let name = sig.name().unwrap_or("").trim().to_string();
    let email = sig.email().unwrap_or("").trim().to_string();
    if name.is_empty() && email.is_empty() {
        return None;
    }
    Some(GitAuthor {
        name,
        email,
        commit_sha: commit.id().to_string(),
    })
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
                            entry.addition_author = commit_author(&commit);
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
                let status = delta.status();
                if let Some(entry) = times.get_mut(path) {
                    entry.addition = Some(time);
                    entry.last_change = Some(time);
                    // Only credit an author when the file is genuinely (re)added
                    // in this range. A plain modification of an existing spec
                    // leaves the author unset so the producer sends none and the
                    // server keeps the originally-detected initial committer.
                    if matches!(status, Delta::Added | Delta::Renamed | Delta::Copied) {
                        entry.addition_author = commit_author(&commit);
                    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{IndexAddOption, Repository, Signature, Time};
    use std::fs;

    fn temp_repo(tag: &str) -> (PathBuf, Repository) {
        let dir = std::env::temp_dir().join(format!(
            "dossiers-git-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock after epoch")
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let repo = Repository::init(&dir).unwrap();
        (dir, repo)
    }

    fn commit_all(repo: &Repository, sig: &Signature, message: &str) {
        let mut index = repo.index().unwrap();
        index
            .add_all(["*"].iter(), IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let parents: Vec<&git2::Commit> = parent.iter().collect();
        repo.commit(Some("HEAD"), sig, sig, message, &tree, &parents)
            .unwrap();
    }

    #[test]
    fn addition_author_credits_the_original_committer_not_the_editor() {
        let (dir, repo) = temp_repo("addition-author");
        let alice = Signature::new("Alice", "alice@example.com", &Time::new(1_000, 0)).unwrap();
        fs::write(dir.join("spec.md"), b"# One\n").unwrap();
        commit_all(&repo, &alice, "add spec");

        // A later edit by Bob must NOT change the credited author.
        let bob = Signature::new("Bob", "bob@example.com", &Time::new(2_000, 0)).unwrap();
        fs::write(dir.join("spec.md"), b"# One\n\nmore\n").unwrap();
        commit_all(&repo, &bob, "edit spec");
        drop(repo);

        let git = open_git_repository(&dir).unwrap();
        let paths = vec![PathBuf::from("spec.md")];
        let cache = GitTimestampCache::from_paths(&git, &paths);
        let author = cache.addition_author(&paths).expect("addition author");
        assert_eq!(author.name, "Alice");
        assert_eq!(author.email, "alice@example.com");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn addition_author_absent_when_only_modified_in_incremental_range() {
        let (dir, repo) = temp_repo("addition-since");
        let alice = Signature::new("Alice", "alice@example.com", &Time::new(1_000, 0)).unwrap();
        fs::write(dir.join("spec.md"), b"# One\n").unwrap();
        commit_all(&repo, &alice, "add spec");
        let base = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .id()
            .to_string();

        let bob = Signature::new("Bob", "bob@example.com", &Time::new(2_000, 0)).unwrap();
        fs::write(dir.join("spec.md"), b"# One\n\nmore\n").unwrap();
        commit_all(&repo, &bob, "edit spec");
        drop(repo);

        let git = open_git_repository(&dir).unwrap();
        let paths = vec![PathBuf::from("spec.md")];
        // Only Bob's modification falls in (base, HEAD]; it's not an addition,
        // so no author is credited and the server keeps the stored one.
        let cache = GitTimestampCache::since(&git, &base, &paths);
        assert!(cache.addition_author(&paths).is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
