//! Loop and abuse guard rules for work requests and wakes. Pure functions and constants;
//! the callers read the counts and pass them in.

use std::fmt;

use sha2::{Digest, Sha256};
use time::Duration;

use crate::domain::{AgentId, ProjectId, Task, TaskId};

/// An identical request (same sender, project and normalised title) inside this window is
/// folded into the existing one.
pub const DEDUPE_WINDOW: Duration = Duration::minutes(10);
/// Work requests one agent may raise per hour.
pub const REQUESTS_PER_HOUR: u32 = 10;
/// Requests waiting for the human, per target project.
pub const PENDING_PER_PROJECT: u32 = 20;
/// Wakes delivered to one session per minute.
pub const SESSION_WAKES_PER_MINUTE: u32 = 1;
/// Wakes delivered to one session per hour.
pub const SESSION_WAKES_PER_HOUR: u32 = 30;
/// Minimum gap between wakes about the same task.
pub const TASK_WAKE_GAP: Duration = Duration::minutes(5);
/// Peer-triggered wakes per session per hour.
pub const PEER_WAKES_PER_SESSION_HOUR: u32 = 6;
/// Peer-triggered wakes per hour for one (poster, owner) pair.
pub const PEER_WAKES_PER_PAIR_HOUR: u32 = 3;

/// Lowercase, collapse whitespace, trim, and drop trailing `.`, `!` and `?`.
pub fn normalise_title(t: &str) -> String {
    let collapsed = t
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    collapsed
        .trim_end_matches(['.', '!', '?'])
        .trim_end()
        .to_string()
}

/// SHA-256 hex of `"{from}\n{project}\n{normalised title}"`.
pub fn dedupe_key(from: AgentId, project: ProjectId, title: &str) -> String {
    let input = format!("{from}\n{project}\n{}", normalise_title(title));
    Sha256::digest(input.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Where a work request sits in a chain of requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lineage {
    pub depth: u32,
    pub chain: Vec<ProjectId>,
    pub origin: Option<TaskId>,
}

/// The lineage of a request raised while working on `origin` (if any).
pub fn lineage_of(origin: Option<&Task>) -> Lineage {
    match origin {
        Some(t) => {
            let mut chain = t.request_chain.clone();
            chain.push(t.project_id);
            Lineage {
                depth: t.request_depth + 1,
                chain,
                origin: Some(t.id),
            }
        }
        None => Lineage {
            depth: 1,
            chain: Vec::new(),
            origin: None,
        },
    }
}

/// Why a work request was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardError {
    /// The configured depth limit.
    DepthLimit(u32),
    /// The project key the request would loop back to.
    Loop(String),
    TooMany {
        minutes: u32,
    },
    /// The target project's key.
    ProjectFull(String),
}

impl fmt::Display for GuardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DepthLimit(n) => write!(f, "request depth limit reached ({n})"),
            Self::Loop(key) => write!(f, "request would loop back to {key}"),
            Self::TooMany { minutes } => {
                write!(f, "too many requests; try again in {minutes} minutes")
            }
            Self::ProjectFull(key) => write!(
                f,
                "{key} already has {PENDING_PER_PROJECT} requests waiting for the human"
            ),
        }
    }
}

impl std::error::Error for GuardError {}

/// Refuse a request that is too deep or would return to a project already in its chain.
pub fn check_lineage(
    l: &Lineage,
    target: ProjectId,
    target_key: &str,
    max_depth: u32,
) -> Result<(), GuardError> {
    if l.depth > max_depth {
        return Err(GuardError::DepthLimit(max_depth));
    }
    if l.chain.contains(&target) {
        return Err(GuardError::Loop(target_key.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Actor, Eligibility, PlanState, TaskStatus};
    use time::macros::datetime;

    fn lineage(depth: u32, chain: Vec<ProjectId>) -> Lineage {
        Lineage {
            depth,
            chain,
            origin: None,
        }
    }

    fn task(depth: u32, chain: Vec<ProjectId>) -> Task {
        let now = datetime!(2026-11-27 9:00 UTC);
        Task {
            id: TaskId::new(),
            project_id: ProjectId::new(),
            parent_id: None,
            title: "t".into(),
            body: String::new(),
            status: TaskStatus::Open,
            owner_session_id: None,
            owner_agent_id: None,
            lease_expires_at: None,
            lease_epoch: 0,
            lease_ttl: None,
            eligibility: Eligibility::default(),
            created_by: Actor::from_ids(None, None),
            created_at: now,
            updated_at: now,
            done_at: None,
            acked_at: None,
            version: 1,
            requires_plan: false,
            plan_state: PlanState::None,
            sanctioned: false,
            request_depth: depth,
            origin_task_id: None,
            request_chain: chain,
            request_dedupe_key: None,
            block: None,
        }
    }

    #[test]
    fn normalises_titles() {
        assert_eq!(
            normalise_title("  Roll the  CNPG users. "),
            "roll the cnpg users"
        );
        assert_eq!(normalise_title("Ship it?!"), "ship it");
    }

    #[test]
    fn dedupe_keys_ignore_case_and_spacing_but_not_project() {
        let (a, p, q) = (AgentId::new(), ProjectId::new(), ProjectId::new());
        let k = dedupe_key(a, p, "Roll the CNPG users.");
        assert_eq!(k, dedupe_key(a, p, "  roll  the cnpg   users "));
        assert_ne!(k, dedupe_key(a, q, "Roll the CNPG users."));
        assert_ne!(k, dedupe_key(AgentId::new(), p, "Roll the CNPG users."));
        assert_eq!(k.len(), 64);
    }

    #[test]
    fn lineage_without_origin_is_depth_one() {
        let l = lineage_of(None);
        assert_eq!((l.depth, l.chain.len(), l.origin), (1, 0, None));
    }

    #[test]
    fn lineage_extends_the_origin() {
        let t = task(1, vec![ProjectId::new()]);
        let l = lineage_of(Some(&t));
        assert_eq!(l.depth, 2);
        assert_eq!(l.chain.len(), 2);
        assert_eq!(l.chain[1], t.project_id);
        assert_eq!(l.origin, Some(t.id));
    }

    #[test]
    fn depth_limit() {
        let target = ProjectId::new();
        assert!(check_lineage(&lineage(2, vec![]), target, "k", 2).is_ok());
        let e = check_lineage(&lineage(3, vec![]), target, "k", 2).unwrap_err();
        assert_eq!(e.to_string(), "request depth limit reached (2)");
        let e = check_lineage(&lineage(4, vec![]), target, "k", 3).unwrap_err();
        assert_eq!(e.to_string(), "request depth limit reached (3)");
    }

    #[test]
    fn loop_back_is_refused() {
        let (web, infra) = (ProjectId::new(), ProjectId::new());
        let l = lineage(2, vec![web, infra]);
        let e = check_lineage(&l, web, "github.com/acme/web-app", 2).unwrap_err();
        assert_eq!(
            e.to_string(),
            "request would loop back to github.com/acme/web-app"
        );
        assert!(check_lineage(&l, ProjectId::new(), "x", 2).is_ok());
    }

    #[test]
    fn error_strings() {
        assert_eq!(
            GuardError::TooMany { minutes: 7 }.to_string(),
            "too many requests; try again in 7 minutes"
        );
        assert_eq!(
            GuardError::ProjectFull("github.com/a/b".into()).to_string(),
            "github.com/a/b already has 20 requests waiting for the human"
        );
    }
}
