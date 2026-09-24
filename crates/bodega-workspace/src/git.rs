//! A thin async wrapper over the `git` CLI.
//!
//! Bodega shells out to `git` instead of linking libgit2 so that behaviour
//! (hooks, config, credential helpers, LFS, sparse checkouts) matches exactly
//! what the developer gets in their own terminal.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// Identity used for commits Bodega makes on behalf of agents.
pub const COMMITTER_NAME: &str = "Bodega";
pub const COMMITTER_EMAIL: &str = "bodega@localhost";

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("failed to run git: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("`git {command}` exited with {code:?}: {stderr}")]
    Failed {
        command: String,
        code: Option<i32>,
        stderr: String,
    },
    #[error("`{0}` is not inside a git repository")]
    NotARepo(PathBuf),
    #[error("merging `{branch}` conflicted in: {}", .files.join(", "))]
    MergeConflict { branch: String, files: Vec<String> },
}

pub type Result<T, E = GitError> = std::result::Result<T, E>;

/// Runs `git` with `args` in `dir`, returning trimmed stdout.
async fn git<I, S>(dir: &Path, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<_> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
    let output = Command::new("git")
        .current_dir(dir)
        .args(&args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned())
    } else {
        Err(GitError::Failed {
            command: args
                .iter()
                .map(|a| a.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" "),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

/// Like [`git`] but reports only whether the command succeeded.
async fn git_ok<I, S>(dir: &Path, args: I) -> Result<bool>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    match git(dir, args).await {
        Ok(_) => Ok(true),
        Err(GitError::Failed { .. }) => Ok(false),
        Err(e) => Err(e),
    }
}

/// A repository Bodega operates on (the developer's checkout).
#[derive(Debug, Clone)]
pub struct GitRepo {
    root: PathBuf,
}

/// One entry from `git worktree list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub head: Option<String>,
    /// Short branch name, or `None` when detached.
    pub branch: Option<String>,
}

impl GitRepo {
    /// Opens the repository containing `path`.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        match git(path, ["rev-parse", "--show-toplevel"]).await {
            Ok(root) => Ok(Self {
                root: PathBuf::from(root),
            }),
            Err(GitError::Failed { .. }) => Err(GitError::NotARepo(path.to_owned())),
            Err(e) => Err(e),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves a revision (branch, tag, `HEAD`…) to a commit sha.
    pub async fn rev_parse(&self, rev: &str) -> Result<String> {
        git(
            &self.root,
            ["rev-parse", "--verify", &format!("{rev}^{{commit}}")],
        )
        .await
    }

    /// The currently checked-out branch, or `None` when detached.
    pub async fn current_branch(&self) -> Result<Option<String>> {
        let name = git(&self.root, ["rev-parse", "--abbrev-ref", "HEAD"]).await?;
        Ok((name != "HEAD").then_some(name))
    }

    pub async fn branch_exists(&self, branch: &str) -> Result<bool> {
        git_ok(
            &self.root,
            [
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
        )
        .await
    }

    /// Creates a worktree at `path` on a new branch `branch` starting at `base`.
    pub async fn add_worktree(&self, path: &Path, branch: &str, base: &str) -> Result<Worktree> {
        let path_arg = path.as_os_str().to_owned();
        git(
            &self.root,
            [
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("-b"),
                OsStr::new(branch),
                &path_arg,
                OsStr::new(base),
            ],
        )
        .await?;
        Ok(Worktree {
            path: path.to_owned(),
            branch: branch.to_owned(),
        })
    }

    /// Checks out an existing `branch` into a new worktree at `path`.
    pub async fn attach_worktree(&self, path: &Path, branch: &str) -> Result<Worktree> {
        let path_arg = path.as_os_str().to_owned();
        git(
            &self.root,
            [
                OsStr::new("worktree"),
                OsStr::new("add"),
                &path_arg,
                OsStr::new(branch),
            ],
        )
        .await?;
        Ok(Worktree {
            path: path.to_owned(),
            branch: branch.to_owned(),
        })
    }

    /// Removes a worktree directory (the branch is kept).
    pub async fn remove_worktree(&self, path: &Path, force: bool) -> Result<()> {
        let mut args = vec![OsStr::new("worktree"), OsStr::new("remove")];
        if force {
            args.push(OsStr::new("--force"));
        }
        args.push(path.as_os_str());
        git(&self.root, args).await.map(|_| ())
    }

    pub async fn list_worktrees(&self) -> Result<Vec<WorktreeInfo>> {
        let out = git(&self.root, ["worktree", "list", "--porcelain"]).await?;
        Ok(parse_worktree_list(&out))
    }

    /// Contents of `path` at revision `rev`, or `None` if it does not exist
    /// there. Used to read configuration from the trusted base branch.
    pub async fn show_file(&self, rev: &str, path: &str) -> Result<Option<String>> {
        match git(&self.root, ["show", &format!("{rev}:{path}")]).await {
            Ok(content) => Ok(Some(content)),
            Err(GitError::Failed { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn delete_branch(&self, branch: &str, force: bool) -> Result<()> {
        let flag = if force { "-D" } else { "-d" };
        git(&self.root, ["branch", flag, branch]).await.map(|_| ())
    }
}

/// A checked-out worktree on its own branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
}

/// How a path changed relative to the index/`HEAD`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStatus {
    /// Two-letter porcelain status code, e.g. `" M"`, `"??"`, `"R "`.
    pub code: String,
    pub path: String,
    /// Original path for renames and copies.
    pub from: Option<String>,
}

/// Per-file line counts from `git diff --numstat`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiffStat {
    pub path: String,
    /// `None` for binary files.
    pub added: Option<u64>,
    pub removed: Option<u64>,
}

impl Worktree {
    /// Uncommitted changes, including untracked files.
    pub async fn status(&self) -> Result<Vec<FileStatus>> {
        let out = git(
            &self.path,
            ["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        )
        .await?;
        Ok(parse_status_z(&out))
    }

    /// Stages everything and commits it. Returns the new commit sha, or
    /// `None` when there was nothing to commit.
    pub async fn commit_all(&self, message: &str) -> Result<Option<String>> {
        git(&self.path, ["add", "--all"]).await?;
        if git_ok(&self.path, ["diff", "--cached", "--quiet"]).await? {
            return Ok(None);
        }
        git(
            &self.path,
            [
                "-c",
                &format!("user.name={COMMITTER_NAME}"),
                "-c",
                &format!("user.email={COMMITTER_EMAIL}"),
                "commit",
                "--quiet",
                "-m",
                message,
            ],
        )
        .await?;
        git(&self.path, ["rev-parse", "HEAD"]).await.map(Some)
    }

    pub async fn head(&self) -> Result<String> {
        git(&self.path, ["rev-parse", "HEAD"]).await
    }

    /// Line counts for commits on this branch since it forked from `base`.
    pub async fn diff_numstat(&self, base: &str) -> Result<Vec<FileDiffStat>> {
        let out = git(&self.path, ["diff", "--numstat", &format!("{base}...HEAD")]).await?;
        Ok(parse_numstat(&out))
    }

    /// Unified diff of this branch since it forked from `base`.
    pub async fn diff(&self, base: &str) -> Result<String> {
        git(&self.path, ["diff", &format!("{base}...HEAD")]).await
    }

    /// Moves this worktree's branch (and files) to `rev`, discarding changes.
    pub async fn reset_hard(&self, rev: &str) -> Result<()> {
        git(&self.path, ["reset", "--hard", "--quiet", rev])
            .await
            .map(|_| ())
    }

    /// Merges `branch` into this worktree's branch with a merge commit. On
    /// conflict the merge is aborted, leaving the worktree clean, and the
    /// conflicting files are reported.
    pub async fn merge(&self, branch: &str, message: &str) -> Result<String> {
        let merged = git(
            &self.path,
            [
                "-c",
                &format!("user.name={COMMITTER_NAME}"),
                "-c",
                &format!("user.email={COMMITTER_EMAIL}"),
                "merge",
                "--no-ff",
                "-m",
                message,
                branch,
            ],
        )
        .await;
        match merged {
            Ok(_) => self.head().await,
            Err(GitError::Failed { .. }) => {
                let files = git(&self.path, ["diff", "--name-only", "--diff-filter=U"])
                    .await
                    .unwrap_or_default();
                // Best effort: leave the worktree clean for the next attempt.
                let _ = git(&self.path, ["merge", "--abort"]).await;
                Err(GitError::MergeConflict {
                    branch: branch.to_owned(),
                    files: files.lines().map(str::to_owned).collect(),
                })
            }
            Err(e) => Err(e),
        }
    }
}

fn parse_worktree_list(out: &str) -> Vec<WorktreeInfo> {
    out.split("\n\n")
        .filter_map(|block| {
            let mut info = WorktreeInfo {
                path: PathBuf::new(),
                head: None,
                branch: None,
            };
            for line in block.lines() {
                if let Some(p) = line.strip_prefix("worktree ") {
                    info.path = PathBuf::from(p);
                } else if let Some(h) = line.strip_prefix("HEAD ") {
                    info.head = Some(h.to_owned());
                } else if let Some(b) = line.strip_prefix("branch ") {
                    info.branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_owned());
                }
            }
            (!info.path.as_os_str().is_empty()).then_some(info)
        })
        .collect()
}

fn parse_status_z(out: &str) -> Vec<FileStatus> {
    let mut entries = out.split('\0').filter(|e| !e.is_empty());
    let mut result = Vec::new();
    while let Some(entry) = entries.next() {
        if entry.len() < 4 {
            continue;
        }
        let code = entry[..2].to_owned();
        let path = entry[3..].to_owned();
        // Renames and copies are followed by the original path.
        let from = if code.starts_with(['R', 'C']) {
            entries.next().map(str::to_owned)
        } else {
            None
        };
        result.push(FileStatus { code, path, from });
    }
    result
}

fn parse_numstat(out: &str) -> Vec<FileDiffStat> {
    out.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let added = parts.next()?;
            let removed = parts.next()?;
            let path = parts.next()?;
            Some(FileDiffStat {
                path: path.to_owned(),
                added: added.parse().ok(),
                removed: removed.parse().ok(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn init_repo(dir: &Path) -> GitRepo {
        git(dir, ["init", "--quiet", "--initial-branch=main"])
            .await
            .unwrap();
        std::fs::write(dir.join("README.md"), "hello\n").unwrap();
        let wt = Worktree {
            path: dir.to_owned(),
            branch: "main".into(),
        };
        wt.commit_all("initial commit").await.unwrap();
        GitRepo::open(dir).await.unwrap()
    }

    #[tokio::test]
    async fn worktree_lifecycle() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        std::fs::create_dir(&repo_dir).unwrap();
        let repo = init_repo(&repo_dir).await;
        assert_eq!(
            repo.current_branch().await.unwrap().as_deref(),
            Some("main")
        );

        let wt_path = tmp.path().join("wt-1");
        let wt = repo
            .add_worktree(&wt_path, "fl/task-1", "main")
            .await
            .unwrap();
        assert!(repo.branch_exists("fl/task-1").await.unwrap());
        let listed = repo.list_worktrees().await.unwrap();
        assert!(
            listed
                .iter()
                .any(|w| w.branch.as_deref() == Some("fl/task-1"))
        );

        // Nothing to commit yet.
        assert_eq!(wt.commit_all("noop").await.unwrap(), None);

        std::fs::write(wt_path.join("new.txt"), "a\nb\n").unwrap();
        std::fs::write(wt_path.join("README.md"), "hello\nworld\n").unwrap();
        let status = wt.status().await.unwrap();
        assert_eq!(status.len(), 2, "{status:?}");

        let sha = wt.commit_all("agent work").await.unwrap();
        assert!(sha.is_some());
        let mut stats = wt.diff_numstat("main").await.unwrap();
        stats.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(
            stats,
            vec![
                FileDiffStat {
                    path: "README.md".into(),
                    added: Some(1),
                    removed: Some(0)
                },
                FileDiffStat {
                    path: "new.txt".into(),
                    added: Some(2),
                    removed: Some(0)
                },
            ]
        );
        assert!(wt.diff("main").await.unwrap().contains("+world"));

        assert_eq!(
            repo.show_file("fl/task-1", "new.txt")
                .await
                .unwrap()
                .as_deref(),
            Some("a\nb")
        );
        assert_eq!(repo.show_file("main", "new.txt").await.unwrap(), None);
        let main_sha = repo.rev_parse("main").await.unwrap();
        wt.reset_hard(&main_sha).await.unwrap();
        assert_eq!(wt.head().await.unwrap(), main_sha);
        assert!(!wt_path.join("new.txt").exists());

        repo.remove_worktree(&wt_path, false).await.unwrap();
        assert!(!wt_path.exists());
        let again = repo.attach_worktree(&wt_path, "fl/task-1").await.unwrap();
        assert_eq!(again.head().await.unwrap(), main_sha);
        repo.remove_worktree(&wt_path, true).await.unwrap();
        repo.delete_branch("fl/task-1", true).await.unwrap();
        assert!(!repo.branch_exists("fl/task-1").await.unwrap());
    }

    #[tokio::test]
    async fn merge_reports_conflicts_and_leaves_tree_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repo");
        std::fs::create_dir(&repo_dir).unwrap();
        let repo = init_repo(&repo_dir).await;

        let a = repo
            .add_worktree(&tmp.path().join("a"), "a", "main")
            .await
            .unwrap();
        let b = repo
            .add_worktree(&tmp.path().join("b"), "b", "main")
            .await
            .unwrap();
        std::fs::write(a.path.join("README.md"), "from a\n").unwrap();
        a.commit_all("a").await.unwrap();
        std::fs::write(b.path.join("README.md"), "from b\n").unwrap();
        b.commit_all("b").await.unwrap();
        std::fs::write(b.path.join("other.txt"), "x\n").unwrap();
        b.commit_all("b2").await.unwrap();

        let integration = repo
            .add_worktree(&tmp.path().join("int"), "integration", "main")
            .await
            .unwrap();
        integration.merge("a", "merge a").await.unwrap();
        let err = integration.merge("b", "merge b").await.unwrap_err();
        match err {
            GitError::MergeConflict { branch, files } => {
                assert_eq!(branch, "b");
                assert_eq!(files, vec!["README.md".to_string()]);
            }
            other => panic!("expected a merge conflict, got {other}"),
        }
        assert!(integration.status().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn open_outside_a_repo_fails() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            GitRepo::open(tmp.path()).await,
            Err(GitError::NotARepo(_))
        ));
    }

    #[test]
    fn parses_porcelain_outputs() {
        let list =
            "worktree /r\nHEAD abc\nbranch refs/heads/main\n\nworktree /r/wt\nHEAD def\ndetached\n";
        let parsed = parse_worktree_list(list);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].branch.as_deref(), Some("main"));
        assert_eq!(parsed[1].branch, None);

        let status = parse_status_z("R  new.rs\0old.rs\0?? untracked.txt\0 M lib.rs\0");
        assert_eq!(status.len(), 3);
        assert_eq!(status[0].from.as_deref(), Some("old.rs"));
        assert_eq!(status[1].code, "??");

        let numstat = parse_numstat("3\t1\tsrc/lib.rs\n-\t-\tlogo.png\n");
        assert_eq!(numstat[1].added, None);
        assert_eq!(numstat[0].added, Some(3));
    }
}
