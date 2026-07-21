//! Shared test scaffolding: a throwaway repo you can build stacks in.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use git2::{Oid, Repository};

static COUNTER: AtomicU32 = AtomicU32::new(0);

pub struct TestRepo {
    pub dir: PathBuf,
    pub repo: Repository,
}

impl TestRepo {
    pub fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("gt-test-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let repo = Repository::init(&dir).unwrap();
        {
            let mut cfg = repo.config().unwrap();
            cfg.set_str("user.name", "test").unwrap();
            cfg.set_str("user.email", "test@test").unwrap();
        }
        TestRepo { dir, repo }
    }

    /// Write `files`, stage them, and commit (moving HEAD). Returns the commit oid.
    pub fn commit(&self, msg: &str, files: &[(&str, &str)]) -> Oid {
        self.write_and_stage(files);
        let mut index = self.repo.index().unwrap();
        let tree = self.repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = self.repo.signature().unwrap();
        let parents: Vec<git2::Commit> = match self.repo.head() {
            Ok(h) => vec![h.peel_to_commit().unwrap()],
            Err(_) => vec![],
        };
        let prefs: Vec<&git2::Commit> = parents.iter().collect();
        self.repo
            .commit(Some("HEAD"), &sig, &sig, msg, &tree, &prefs)
            .unwrap()
    }

    /// Stage files without committing (op C's input).
    pub fn stage(&self, files: &[(&str, &str)]) {
        self.write_and_stage(files);
    }

    /// Commit a single executable (mode 0o100755) file, moving HEAD.
    pub fn commit_exec(&self, msg: &str, path: &str, content: &str) -> Oid {
        use std::os::unix::fs::PermissionsExt;
        let full = self.dir.join(path);
        std::fs::write(&full, content).unwrap();
        std::fs::set_permissions(&full, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut index = self.repo.index().unwrap();
        index.add_path(Path::new(path)).unwrap();
        index.write().unwrap();
        let tree = self.repo.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = self.repo.signature().unwrap();
        let parents: Vec<git2::Commit> = match self.repo.head() {
            Ok(h) => vec![h.peel_to_commit().unwrap()],
            Err(_) => vec![],
        };
        let prefs: Vec<&git2::Commit> = parents.iter().collect();
        self.repo
            .commit(Some("HEAD"), &sig, &sig, msg, &tree, &prefs)
            .unwrap()
    }

    /// The filemode git2 records for `path` at commit `oid` (e.g. 0o100755).
    pub fn mode_at(&self, oid: Oid, path: &str) -> Option<i32> {
        let tree = self.repo.find_commit(oid).unwrap().tree().unwrap();
        tree.get_path(Path::new(path)).ok().map(|e| e.filemode())
    }

    /// Write a tracked file in the worktree WITHOUT staging (a dirty tree).
    pub fn dirty(&self, path: &str, content: &str) {
        std::fs::write(self.dir.join(path), content).unwrap();
    }

    fn write_and_stage(&self, files: &[(&str, &str)]) {
        let mut index = self.repo.index().unwrap();
        for (path, content) in files {
            let full = self.dir.join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&full, content).unwrap();
            index.add_path(Path::new(path)).unwrap();
        }
        index.write().unwrap();
    }

    pub fn head(&self) -> Oid {
        self.repo.head().unwrap().peel_to_commit().unwrap().id()
    }

    pub fn branch_oid(&self) -> Oid {
        self.repo.head().unwrap().target().unwrap()
    }

    pub fn reflog_len(&self) -> usize {
        let name = self.repo.head().unwrap().name().unwrap().to_string();
        self.repo.reflog(&name).map(|r| r.len()).unwrap_or(0)
    }

    /// Content of `path` in the tree of commit `oid`, or None if absent.
    pub fn read_at(&self, oid: Oid, path: &str) -> Option<String> {
        let tree = self.repo.find_commit(oid).unwrap().tree().unwrap();
        let entry = tree.get_path(Path::new(path)).ok()?;
        let obj = entry.to_object(&self.repo).unwrap();
        Some(String::from_utf8(obj.as_blob().unwrap().content().to_vec()).unwrap())
    }

    /// nth ancestor of `oid` via first parent (0 = itself).
    pub fn nth_parent(&self, oid: Oid, n: usize) -> Oid {
        let mut c = self.repo.find_commit(oid).unwrap();
        for _ in 0..n {
            c = c.parent(0).unwrap();
        }
        c.id()
    }

    /// Is `path` staged (differs in the INDEX vs HEAD)? "Left staged" means this,
    /// not merely "the file on disk still has my text".
    pub fn is_staged(&self, path: &str) -> bool {
        let mut opts = git2::StatusOptions::new();
        opts.include_untracked(true);
        self.repo
            .statuses(Some(&mut opts))
            .unwrap()
            .iter()
            .any(|e| {
                e.path() == Some(path)
                    && (e.status().is_index_modified()
                        || e.status().is_index_new()
                        || e.status().is_index_deleted())
            })
    }

    /// Number of commits reachable from HEAD via first parents.
    pub fn commit_count(&self) -> usize {
        let mut n = 0;
        let mut c = self.repo.head().unwrap().peel_to_commit().unwrap();
        loop {
            n += 1;
            match c.parent(0) {
                Ok(p) => c = p,
                Err(_) => break,
            }
        }
        n
    }

    /// Is the worktree + index clean vs HEAD?
    pub fn is_clean(&self) -> bool {
        let mut opts = git2::StatusOptions::new();
        opts.include_untracked(false);
        self.repo.statuses(Some(&mut opts)).unwrap().is_empty()
    }
}

impl Drop for TestRepo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
