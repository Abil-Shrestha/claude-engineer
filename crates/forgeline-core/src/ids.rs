//! Strongly typed, time-ordered identifiers.
//!
//! Every id wraps a UUIDv7 (so ids sort by creation time) and renders with a
//! short type prefix, e.g. `run_01926f4c…`, which keeps logs, URLs and the UI
//! unambiguous about what kind of thing an id points at.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

/// Error returned when parsing an id from a string fails.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {kind} id `{input}`")]
pub struct ParseIdError {
    kind: &'static str,
    input: String,
}

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(Uuid);

        impl $name {
            /// The prefix used when rendering this id as a string.
            pub const PREFIX: &'static str = $prefix;

            /// Creates a new, time-ordered id.
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            /// Wraps an existing UUID.
            pub const fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            /// Returns the underlying UUID.
            pub const fn as_uuid(&self) -> &Uuid {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}_{}", $prefix, self.0.simple())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(self, f)
            }
        }

        impl FromStr for $name {
            type Err = ParseIdError;

            /// Accepts both the prefixed form (`run_…`) and a bare UUID.
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                let raw = s
                    .strip_prefix(concat!($prefix, "_"))
                    .unwrap_or(s);
                Uuid::parse_str(raw).map(Self).map_err(|_| ParseIdError {
                    kind: $prefix,
                    input: s.to_owned(),
                })
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let s = String::deserialize(deserializer)?;
                s.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

define_id!(
    /// A run: one requested unit of work (an issue, a feature request) flowing
    /// through the factory pipeline.
    RunId,
    "run"
);
define_id!(
    /// A task: one node in a run's task graph.
    TaskId,
    "task"
);
define_id!(
    /// One attempt by an agent at a task. Retries create new attempts.
    AttemptId,
    "att"
);
define_id!(
    /// An isolated workspace (worktree, container, VM…) an attempt runs in.
    WorkspaceId,
    "ws"
);
define_id!(
    /// A pending human decision (plan approval, merge approval, agent question).
    ApprovalId,
    "apr"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_uses_prefix_and_round_trips() {
        let id = RunId::new();
        let rendered = id.to_string();
        assert!(rendered.starts_with("run_"));
        assert_eq!(rendered.parse::<RunId>().unwrap(), id);
    }

    #[test]
    fn parses_bare_uuid() {
        let id = TaskId::new();
        let bare = id.as_uuid().to_string();
        assert_eq!(bare.parse::<TaskId>().unwrap(), id);
    }

    #[test]
    fn rejects_garbage() {
        let err = "run_nope".parse::<RunId>().unwrap_err();
        assert_eq!(err.to_string(), "invalid run id `run_nope`");
    }

    #[test]
    fn ids_sort_by_creation_time() {
        let a = AttemptId::new();
        let b = AttemptId::new();
        assert!(a < b);
    }

    #[test]
    fn serde_uses_prefixed_string() {
        let id = WorkspaceId::new();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
        let back: WorkspaceId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }
}
