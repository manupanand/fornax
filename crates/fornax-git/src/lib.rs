// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Repository storage for Fornax.
//!
//! Bare repositories live at `<root>/<owner>/<name>.git`. All access goes through the system
//! `git` binary (D-019, D-026): no user input is ever passed to `git` as an option — revisions
//! are resolved with `rev-parse --verify --end-of-options` first and only object IDs are used
//! afterwards.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::Serialize;
use tokio::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("git {command} failed ({status}): {stderr}")]
    Command {
        command: String,
        status: String,
        stderr: String,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, GitError>;

/// The two Git smart-protocol services.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitService {
    UploadPack,
    ReceivePack,
}

impl GitService {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "git-upload-pack" => Some(Self::UploadPack),
            "git-receive-pack" => Some(Self::ReceivePack),
            _ => None,
        }
    }

    /// Service name as used on the wire, e.g. `git-upload-pack`.
    pub fn name(self) -> &'static str {
        match self {
            Self::UploadPack => "git-upload-pack",
            Self::ReceivePack => "git-receive-pack",
        }
    }

    fn subcommand(self) -> &'static str {
        match self {
            Self::UploadPack => "upload-pack",
            Self::ReceivePack => "receive-pack",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Branch {
    pub name: String,
    pub commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Commit {
    pub id: String,
    pub author_name: String,
    pub author_email: String,
    pub authored_at: i64,
    pub summary: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    Blob,
    Tree,
    Commit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TreeEntry {
    pub name: String,
    pub kind: EntryKind,
    pub mode: String,
    pub id: String,
    /// Size in bytes for blobs; `None` for trees and submodules.
    pub size: Option<u64>,
}

/// Directory holding pipeline and deployment specs (D-024, D-027).
pub const FORNAX_DIR: &str = ".fornax";
/// [`RepoStore::fornax_files`]: maximum size of one file (1 MiB).
pub const MAX_FORNAX_FILE_SIZE: u64 = 1024 * 1024;
/// [`RepoStore::fornax_files`]: maximum size of all files together (8 MiB).
pub const MAX_FORNAX_TOTAL_SIZE: u64 = 8 * 1024 * 1024;
/// [`RepoStore::fornax_files`]: maximum number of files read.
pub const MAX_FORNAX_FILES: usize = 200;
/// [`RepoStore::fornax_files`]: at most this much `ls-tree` output (4 MiB) is read; a
/// larger `.fornax/` tree is reported as "too many files".
pub const MAX_FORNAX_LISTING_BYTES: usize = 4 * 1024 * 1024;
/// [`RepoStore::changed_paths`]: more changed paths than this means "unknown".
pub const MAX_CHANGED_PATHS: usize = 10_000;

/// A `.fornax/` file that was skipped, with the reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileProblem {
    /// Repository-relative path, e.g. `.fornax/ci.yaml`.
    pub path: String,
    /// Human-readable reason.
    pub message: String,
}

impl FileProblem {
    fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

/// Result of [`RepoStore::fornax_files`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FornaxFiles {
    /// `(path, content)` of every readable spec file, sorted by path.
    pub files: Vec<(String, String)>,
    /// Files that were skipped (too large, too many, not UTF-8).
    pub problems: Vec<FileProblem>,
}

/// Storage root for bare repositories.
#[derive(Debug, Clone)]
pub struct RepoStore {
    root: PathBuf,
    git: PathBuf,
}

impl RepoStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            git: PathBuf::from("git"),
        }
    }

    /// Use a specific `git` executable.
    pub fn with_git(mut self, git: impl Into<PathBuf>) -> Self {
        self.git = git.into();
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// On-disk path of a repository. Callers pass names validated by `fornax-core`.
    pub fn repo_path(&self, owner: &str, name: &str) -> PathBuf {
        self.root
            .join(owner.to_ascii_lowercase())
            .join(format!("{}.git", name.to_ascii_lowercase()))
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.git);
        cmd.env("GIT_TERMINAL_PROMPT", "0")
            .kill_on_drop(true)
            .stdin(Stdio::null());
        cmd
    }

    async fn run(&self, repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
        let out = self
            .command()
            .arg("--git-dir")
            .arg(repo)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await?;
        if out.status.success() {
            Ok(out.stdout)
        } else {
            Err(GitError::Command {
                command: args.first().copied().unwrap_or_default().to_owned(),
                status: out.status.to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
            })
        }
    }

    /// Like [`Self::run`], but reads at most `limit` bytes of stdout. Returns the output and
    /// whether it was cut off (the process is killed then, and its exit status ignored).
    async fn run_limited(
        &self,
        repo: &Path,
        args: &[&str],
        limit: usize,
    ) -> Result<(Vec<u8>, bool)> {
        use tokio::io::AsyncReadExt;

        let mut child = self
            .command()
            .arg("--git-dir")
            .arg(repo)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut out = Vec::new();
        if let Some(stdout) = child.stdout.take() {
            let cap = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
            stdout.take(cap).read_to_end(&mut out).await?;
        }
        if out.len() > limit {
            out.truncate(limit);
            // Best effort: `kill_on_drop` also reaps it if this fails.
            let _ = child.kill().await;
            return Ok((out, true));
        }
        let status = child.wait().await?;
        if status.success() {
            Ok((out, false))
        } else {
            Err(GitError::Command {
                command: args.first().copied().unwrap_or_default().to_owned(),
                status: status.to_string(),
                stderr: String::new(),
            })
        }
    }

    /// Create a new bare repository with `default_branch` as HEAD.
    pub async fn create(&self, owner: &str, name: &str, default_branch: &str) -> Result<PathBuf> {
        fornax_core::refname::validate_branch_name(default_branch)
            .map_err(|e| GitError::InvalidInput(e.to_string()))?;
        let path = self.repo_path(owner, name);
        if tokio::fs::try_exists(&path).await? {
            return Err(GitError::InvalidInput(format!(
                "{} already exists",
                path.display()
            )));
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let out = self
            .command()
            .arg("init")
            .arg("--bare")
            .arg("--quiet")
            .arg(format!("--initial-branch={default_branch}"))
            .arg(&path)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .await?;
        if !out.status.success() {
            return Err(GitError::Command {
                command: "init".into(),
                status: out.status.to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
            });
        }
        Ok(path)
    }

    /// Remove a repository from disk.
    pub async fn remove(&self, owner: &str, name: &str) -> Result<()> {
        let path = self.repo_path(owner, name);
        match tokio::fs::remove_dir_all(&path).await {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    /// Move a repository to a new name (same owner).
    pub async fn rename(&self, owner: &str, from: &str, to: &str) -> Result<PathBuf> {
        let (src, dst) = (self.repo_path(owner, from), self.repo_path(owner, to));
        if src == dst {
            return Ok(dst);
        }
        if tokio::fs::try_exists(&dst).await? {
            return Err(GitError::InvalidInput(format!(
                "{} already exists",
                dst.display()
            )));
        }
        tokio::fs::rename(&src, &dst).await?;
        Ok(dst)
    }

    /// Point `HEAD` (the default branch for clones) at `branch`.
    pub async fn set_head(&self, repo: &Path, branch: &str) -> Result<()> {
        fornax_core::refname::validate_branch_name(branch)
            .map_err(|e| GitError::InvalidInput(e.to_string()))?;
        let target = format!("refs/heads/{branch}");
        self.run(repo, &["symbolic-ref", "HEAD", &target]).await?;
        Ok(())
    }

    /// Current `HEAD` branch name, if `HEAD` is symbolic.
    pub async fn head(&self, repo: &Path) -> Result<Option<String>> {
        match self
            .run(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])
            .await
        {
            Ok(out) => Ok(Some(String::from_utf8_lossy(&out).trim().to_owned())),
            Err(GitError::Command { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Resolve a user-supplied revision (branch, tag, commit ID) to a commit ID.
    pub async fn resolve_commit(&self, repo: &Path, rev: &str) -> Result<String> {
        if rev.is_empty()
            || rev.len() > 255
            || rev.bytes().any(|b| b < 0x20 || b == 0x7f || b == b' ')
        {
            return Err(GitError::InvalidInput("invalid revision".into()));
        }
        let spec = format!("{rev}^{{commit}}");
        match self
            .run(
                repo,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "--end-of-options",
                    &spec,
                ],
            )
            .await
        {
            Ok(out) => Ok(String::from_utf8_lossy(&out).trim().to_owned()),
            Err(GitError::Command { .. }) => Err(GitError::NotFound(format!("revision {rev}"))),
            Err(e) => Err(e),
        }
    }

    /// All branches, sorted by name.
    pub async fn branches(&self, repo: &Path) -> Result<Vec<Branch>> {
        let out = self
            .run(
                repo,
                &[
                    "for-each-ref",
                    "--sort=refname",
                    "--format=%(refname:strip=2)%09%(objectname)",
                    "refs/heads",
                ],
            )
            .await?;
        Ok(String::from_utf8_lossy(&out)
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .map(|(name, commit)| Branch {
                name: name.to_owned(),
                commit: commit.to_owned(),
            })
            .collect())
    }

    /// Up to `limit` commits reachable from `rev`, newest first.
    pub async fn commits(&self, repo: &Path, rev: &str, limit: usize) -> Result<Vec<Commit>> {
        let id = self.resolve_commit(repo, rev).await?;
        let limit = format!("--max-count={}", limit.clamp(1, 1000));
        let out = self
            .run(
                repo,
                &[
                    "log",
                    "-z",
                    "--format=%H%x1f%an%x1f%ae%x1f%at%x1f%s",
                    &limit,
                    &id,
                    "--",
                ],
            )
            .await?;
        Ok(String::from_utf8_lossy(&out)
            .split('\0')
            .filter(|rec| !rec.trim().is_empty())
            .filter_map(|rec| {
                let mut f = rec.trim_start_matches('\n').splitn(5, '\u{1f}');
                Some(Commit {
                    id: f.next()?.to_owned(),
                    author_name: f.next()?.to_owned(),
                    author_email: f.next()?.to_owned(),
                    authored_at: f.next()?.parse().unwrap_or(0),
                    summary: f.next().unwrap_or_default().to_owned(),
                })
            })
            .collect())
    }

    /// Object type at `<commit>:<path>` (`tree`, `blob`, `commit`).
    async fn object_type(&self, repo: &Path, object: &str) -> Result<String> {
        match self.run(repo, &["cat-file", "-t", object]).await {
            Ok(out) => Ok(String::from_utf8_lossy(&out).trim().to_owned()),
            Err(GitError::Command { .. }) => Err(GitError::NotFound("path".into())),
            Err(e) => Err(e),
        }
    }

    /// List the directory `path` ("" = root) at `rev`.
    pub async fn tree(&self, repo: &Path, rev: &str, path: &str) -> Result<Vec<TreeEntry>> {
        let id = self.resolve_commit(repo, rev).await?;
        let object = format!("{id}:{}", clean_path(path)?);
        if self.object_type(repo, &object).await? != "tree" {
            return Err(GitError::InvalidInput("path is not a directory".into()));
        }
        let out = self
            .run(repo, &["ls-tree", "-z", "--long", &object])
            .await?;
        let mut entries: Vec<TreeEntry> = out
            .split(|b| *b == 0)
            .filter(|rec| !rec.is_empty())
            .filter_map(|rec| parse_ls_tree(&String::from_utf8_lossy(rec)))
            .collect();
        entries.sort_by(|a, b| {
            (a.kind != EntryKind::Tree, &a.name).cmp(&(b.kind != EntryKind::Tree, &b.name))
        });
        Ok(entries)
    }

    /// Contents of the file `path` at `rev`, refusing files larger than `max_size` bytes.
    pub async fn blob(&self, repo: &Path, rev: &str, path: &str, max_size: u64) -> Result<Vec<u8>> {
        let id = self.resolve_commit(repo, rev).await?;
        let object = format!("{id}:{}", clean_path(path)?);
        if self.object_type(repo, &object).await? != "blob" {
            return Err(GitError::InvalidInput("path is not a file".into()));
        }
        let size: u64 =
            String::from_utf8_lossy(&self.run(repo, &["cat-file", "-s", &object]).await?)
                .trim()
                .parse()
                .unwrap_or(u64::MAX);
        if size > max_size {
            return Err(GitError::InvalidInput(format!(
                "file is larger than {max_size} bytes"
            )));
        }
        self.run(repo, &["cat-file", "blob", &object]).await
    }

    /// Every branch and tag with the object it points at: full ref name (`refs/heads/…`,
    /// `refs/tags/…`) → object ID. Annotated tags are peeled one level (to the object they
    /// tag, normally a commit). Two snapshots, taken before and after a push, give the ref
    /// updates of that push.
    pub async fn snapshot_refs(&self, repo: &Path) -> Result<BTreeMap<String, String>> {
        let out = self
            .run(
                repo,
                &[
                    "for-each-ref",
                    "--format=%(refname)%09%(objectname)%09%(*objectname)",
                    "refs/heads",
                    "refs/tags",
                ],
            )
            .await?;
        Ok(String::from_utf8_lossy(&out)
            .lines()
            .filter_map(|line| {
                let mut f = line.split('\t');
                let name = f.next()?;
                let object = f.next()?;
                let peeled = f.next().unwrap_or_default();
                let id = if peeled.is_empty() { object } else { peeled };
                Some((name.to_owned(), id.to_owned()))
            })
            .collect())
    }

    /// Paths changed between two commits (`diff-tree -r --name-only -z`, no rename
    /// detection, so a rename lists both the old and the new path). Both revisions are
    /// resolved to commits first.
    ///
    /// Returns `None` when more than [`MAX_CHANGED_PATHS`] paths changed: callers treat that
    /// like "no path information" (path filters then pass) instead of filtering on a
    /// truncated list.
    pub async fn changed_paths(
        &self,
        repo: &Path,
        old: &str,
        new: &str,
    ) -> Result<Option<Vec<String>>> {
        let old = self.resolve_commit(repo, old).await?;
        let new = self.resolve_commit(repo, new).await?;
        let out = self
            .run(
                repo,
                &[
                    "diff-tree",
                    "-r",
                    "--name-only",
                    "--no-commit-id",
                    "--no-renames",
                    "-z",
                    &old,
                    &new,
                    "--",
                ],
            )
            .await?;
        let mut paths = Vec::new();
        for rec in out.split(|b| *b == 0).filter(|rec| !rec.is_empty()) {
            if paths.len() == MAX_CHANGED_PATHS {
                return Ok(None);
            }
            paths.push(String::from_utf8_lossy(rec).into_owned());
        }
        Ok(Some(paths))
    }

    /// The `*.yaml` / `*.yml` files under `.fornax/` (recursively) at `rev`, sorted by path.
    ///
    /// Only regular files count (symlinks and submodules are ignored); a missing `.fornax/`
    /// gives no files. Files that cannot be used are not an error: they are listed in
    /// [`FornaxFiles::problems`] so the caller can report them next to spec diagnostics.
    /// Limits: [`MAX_FORNAX_FILE_SIZE`] per file, [`MAX_FORNAX_TOTAL_SIZE`] in total and
    /// [`MAX_FORNAX_FILES`] files; files beyond a limit are reported and not read (past the
    /// file count, one "too many files" problem stands for all of them). At most
    /// [`MAX_FORNAX_LISTING_BYTES`] of the `.fornax/` listing is read. Non-UTF-8 files are
    /// reported too.
    pub async fn fornax_files(&self, repo: &Path, rev: &str) -> Result<FornaxFiles> {
        let id = self.resolve_commit(repo, rev).await?;
        let mut result = FornaxFiles::default();
        let dir = format!("{id}:{FORNAX_DIR}");
        match self.object_type(repo, &dir).await {
            Ok(kind) if kind == "tree" => {}
            Ok(_) | Err(GitError::NotFound(_)) => return Ok(result),
            Err(e) => return Err(e),
        }
        let (mut out, truncated) = self
            .run_limited(
                repo,
                &["ls-tree", "-r", "-z", "--long", &dir],
                MAX_FORNAX_LISTING_BYTES,
            )
            .await?;
        if truncated {
            // Drop the partial last record.
            let end = out.iter().rposition(|b| *b == 0).map_or(0, |i| i + 1);
            out.truncate(end);
        }
        let too_many = || {
            FileProblem::new(
                format!("{FORNAX_DIR}/"),
                format!("too many files under {FORNAX_DIR}/ (limit {MAX_FORNAX_FILES})"),
            )
        };
        let mut total: u64 = 0;
        let mut limited = false;
        for rec in out.split(|b| *b == 0).filter(|rec| !rec.is_empty()) {
            let Some(entry) = parse_ls_tree(&String::from_utf8_lossy(rec)) else {
                continue;
            };
            let is_regular =
                entry.kind == EntryKind::Blob && (entry.mode == "100644" || entry.mode == "100755");
            let lower = entry.name.to_ascii_lowercase();
            if !is_regular || !(lower.ends_with(".yaml") || lower.ends_with(".yml")) {
                continue;
            }
            let path = format!("{FORNAX_DIR}/{}", entry.name);
            let size = entry.size.unwrap_or(u64::MAX);
            if result.files.len() + result.problems.len() >= MAX_FORNAX_FILES {
                result.problems.push(too_many());
                limited = true;
                break;
            }
            if size > MAX_FORNAX_FILE_SIZE {
                result.problems.push(FileProblem::new(
                    path,
                    format!("file is larger than {MAX_FORNAX_FILE_SIZE} bytes"),
                ));
                continue;
            }
            if total + size > MAX_FORNAX_TOTAL_SIZE {
                result.problems.push(FileProblem::new(
                    path,
                    format!(
                        "files under {FORNAX_DIR}/ exceed {MAX_FORNAX_TOTAL_SIZE} bytes in total"
                    ),
                ));
                continue;
            }
            total += size;
            // `entry.id` comes from ls-tree output (an object ID), never from user input.
            let data = self.run(repo, &["cat-file", "blob", &entry.id]).await?;
            match String::from_utf8(data) {
                Ok(text) => result.files.push((path, text)),
                Err(_) => result
                    .problems
                    .push(FileProblem::new(path, "file is not valid UTF-8")),
            }
        }
        if truncated && !limited {
            result.problems.push(too_many());
        }
        result.files.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(result)
    }

    /// A `git <service> <repo>` command for the SSH transport (full duplex, not stateless).
    pub fn ssh_service_command(&self, repo: &Path, service: GitService) -> Command {
        let mut cmd = self.command();
        cmd.arg(service.subcommand()).arg(repo);
        cmd
    }

    /// A `git <service> --stateless-rpc` command for the smart HTTP protocol. With
    /// `advertise` the command prints the ref advertisement (`info/refs`).
    pub fn service_command(&self, repo: &Path, service: GitService, advertise: bool) -> Command {
        let mut cmd = self.command();
        cmd.arg(service.subcommand()).arg("--stateless-rpc");
        if advertise {
            cmd.arg("--advertise-refs");
        }
        cmd.arg(repo);
        cmd
    }
}

/// Normalise a repository-relative path; reject `..` and absolute paths.
fn clean_path(path: &str) -> Result<String> {
    let parts: Vec<&str> = path
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    if parts
        .iter()
        .any(|p| *p == ".." || p.bytes().any(|b| b < 0x20))
    {
        return Err(GitError::InvalidInput("invalid path".into()));
    }
    Ok(parts.join("/"))
}

/// Parse one `ls-tree --long` record: `<mode> SP <type> SP <id> SP+ <size> TAB <name>`.
fn parse_ls_tree(rec: &str) -> Option<TreeEntry> {
    let (meta, name) = rec.split_once('\t')?;
    let mut f = meta.split_whitespace();
    let mode = f.next()?.to_owned();
    let kind = match f.next()? {
        "blob" => EntryKind::Blob,
        "tree" => EntryKind::Tree,
        "commit" => EntryKind::Commit,
        _ => return None,
    };
    let id = f.next()?.to_owned();
    let size = f.next().and_then(|s| s.parse().ok());
    Some(TreeEntry {
        name: name.to_owned(),
        kind,
        mode,
        id,
        size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ls_tree_records() {
        let e = parse_ls_tree(
            "100644 blob 3b18e512dba79e4c8300dd08aeb37f8e728b8dad      12\tREADME.md",
        )
        .expect("parse");
        assert_eq!(
            (e.kind, e.size, e.name.as_str()),
            (EntryKind::Blob, Some(12), "README.md")
        );
        let t =
            parse_ls_tree("040000 tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904       -\tsrc dir")
                .expect("parse");
        assert_eq!(
            (t.kind, t.size, t.name.as_str()),
            (EntryKind::Tree, None, "src dir")
        );
    }

    #[test]
    fn cleans_paths() {
        assert_eq!(clean_path("/a//b/./c/").expect("ok"), "a/b/c");
        assert_eq!(clean_path("").expect("ok"), "");
        assert!(clean_path("a/../b").is_err());
    }

    /// Run git in `dir` with an isolated config; returns trimmed stdout.
    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .output()
            .expect("run git");
        assert!(out.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn write(path: &Path, content: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(path, content).expect("write");
    }

    #[tokio::test]
    async fn refs_changed_paths_and_fornax_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = RepoStore::new(tmp.path().join("repos"));
        let bare = store.create("alice", "demo", "main").await.expect("create");
        let remote = bare.to_str().expect("utf8");
        assert!(store.snapshot_refs(&bare).await.expect("refs").is_empty());

        let work = tmp.path().join("work");
        write(&work.join("README.md"), b"hi\n");
        write(&work.join(".fornax/ci.yaml"), b"kind: Pipeline\n");
        write(
            &work.join(".fornax/nested/deploy.YML"),
            b"kind: Deployment\n",
        );
        write(&work.join(".fornax/notes.txt"), b"ignored\n");
        write(&work.join(".fornax/bad.yaml"), &[0xff, 0xfe, b'\n']);
        write(&work.join("other.yaml"), b"not under .fornax\n");
        std::fs::create_dir_all(&work).expect("mkdir");
        git(&work, &["init", "-q"]);
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "one"]);
        let first = git(&work, &["rev-parse", "HEAD"]);
        git(&work, &["tag", "-a", "v1", "-m", "release"]);
        write(&work.join("src/lib.rs"), b"// x\n");
        write(&work.join("README.md"), b"hello\n");
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "two"]);
        let second = git(&work, &["rev-parse", "HEAD"]);
        git(&work, &["push", "-q", remote, "main", "v1"]);

        let refs = store.snapshot_refs(&bare).await.expect("refs");
        assert_eq!(refs.get("refs/heads/main"), Some(&second));
        // Annotated tag peeled to its commit.
        assert_eq!(refs.get("refs/tags/v1"), Some(&first));
        assert_eq!(refs.len(), 2);

        let changed = store
            .changed_paths(&bare, &first, &second)
            .await
            .expect("diff");
        assert_eq!(
            changed,
            Some(vec!["README.md".to_owned(), "src/lib.rs".to_owned()])
        );
        assert!(matches!(
            store.changed_paths(&bare, "--output=x", &second).await,
            Err(GitError::NotFound(_))
        ));

        let files = store.fornax_files(&bare, &second).await.expect("files");
        let paths: Vec<_> = files.files.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, [".fornax/ci.yaml", ".fornax/nested/deploy.YML"]);
        assert_eq!(files.files[0].1, "kind: Pipeline\n");
        assert_eq!(
            files.problems,
            vec![FileProblem::new(
                ".fornax/bad.yaml",
                "file is not valid UTF-8"
            )]
        );

        // No `.fornax/` at all, and `.fornax` being a file.
        let work2 = tmp.path().join("work2");
        write(&work2.join(".fornax"), b"a file\n");
        git(&work2, &["init", "-q"]);
        git(&work2, &["add", "."]);
        git(&work2, &["commit", "-q", "-m", "file"]);
        git(&work2, &["push", "-q", remote, "main:other"]);
        let other = store.fornax_files(&bare, "other").await.expect("files");
        assert_eq!(other, FornaxFiles::default());
    }

    #[tokio::test]
    async fn fornax_file_limits() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = RepoStore::new(tmp.path().join("repos"));
        let bare = store.create("alice", "big", "main").await.expect("create");
        let work = tmp.path().join("work");
        let big = vec![b'#'; (MAX_FORNAX_FILE_SIZE + 1) as usize];
        write(&work.join(".fornax/a-big.yaml"), &big);
        for i in 0..MAX_FORNAX_FILES + 50 {
            write(&work.join(format!(".fornax/many/{i:03}.yaml")), b"x: 1\n");
        }
        git(&work, &["init", "-q"]);
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "many"]);
        git(&work, &["push", "-q", bare.to_str().expect("utf8"), "main"]);
        let files = store.fornax_files(&bare, "main").await.expect("files");
        assert_eq!(files.files.len(), MAX_FORNAX_FILES - 1);
        // One problem for the big file and a single one for everything past the limit.
        assert_eq!(files.problems.len(), 2, "{:?}", files.problems);
        assert_eq!(files.problems[0].path, ".fornax/a-big.yaml");
        assert!(files.problems[0].message.contains("larger than"));
        assert!(files.problems[1].message.contains("too many files"));

        // The listing read is bounded: output past the limit is cut off.
        let args = ["ls-tree", "-r", "-z", "--long", "main"];
        let (out, cut) = store.run_limited(&bare, &args, 100).await.expect("ls-tree");
        assert!(cut);
        assert_eq!(out.len(), 100);
        let (out, cut) = store
            .run_limited(&bare, &args, 1 << 20)
            .await
            .expect("ls-tree");
        assert!(!cut);
        assert!(out.len() > 100);
    }
}
