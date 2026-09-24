//! Isolated workspaces for Bodega agents.
//!
//! The first backend is local git worktrees: each attempt gets its own
//! checkout on its own branch, so many agents can work on one repository at
//! once without stepping on each other. Container and remote-sandbox backends
//! plug in behind the same interface later.

pub mod git;

pub use git::{FileDiffStat, FileStatus, GitError, GitRepo, Worktree, WorktreeInfo};
