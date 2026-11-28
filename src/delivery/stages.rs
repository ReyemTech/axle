//! Stage rules: where a completed task goes, a parent's stage, and the board column.

use crate::domain::{DeliveryStage, TaskStatus};

/// SQL for a stage's rank (`0` for no stage), shared by both stores' forward-only guards.
pub const STAGE_RANK_SQL: &str = "CASE delivery_stage WHEN 'in_pr' THEN 1 WHEN 'merged' THEN 2 \
     WHEN 'released' THEN 3 WHEN 'deployed' THEN 4 ELSE 0 END";

/// The final enabled stage, where a delivering task becomes `done`.
pub fn last_enabled(enabled: &[DeliveryStage]) -> Option<DeliveryStage> {
    enabled.iter().copied().max()
}

/// Where `task_complete` sends a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionTarget {
    Done,
    Delivering(DeliveryStage),
}

/// What the parent rule needs to know about one child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildState {
    pub status: TaskStatus,
    pub stage: Option<DeliveryStage>,
}

/// The `task_complete` decision, with the parent rule: a task with a PR delivers at its
/// furthest stage (at least `in_pr`), a parent at the minimum over itself and its delivering
/// children; either is `done` once that minimum reaches the last enabled stage.
pub fn completion_target(
    enabled: &[DeliveryStage],
    own: Option<DeliveryStage>,
    has_pr: bool,
    children: &[ChildState],
) -> CompletionTarget {
    let Some(last) = last_enabled(enabled) else {
        return CompletionTarget::Done;
    };
    let own = has_pr.then(|| own.map_or(DeliveryStage::InPr, |s| s.max(DeliveryStage::InPr)));
    match parent_stage(own, children) {
        Some(m) if m.rank() < last.rank() => CompletionTarget::Delivering(m),
        _ => CompletionTarget::Done,
    }
}

/// A parent's stage: the minimum over its own and its delivering children's stages; `None`
/// when nothing delivers. A delivering child without a stage counts as `in_pr`, so a bad
/// row holds the parent back rather than finishing it.
pub fn parent_stage(own: Option<DeliveryStage>, children: &[ChildState]) -> Option<DeliveryStage> {
    let delivering = children
        .iter()
        .filter(|c| c.status == TaskStatus::Delivering)
        .map(|c| c.stage.unwrap_or(DeliveryStage::InPr));
    own.into_iter().chain(delivering).min()
}

/// The board column for a delivering task: the greatest enabled stage it has reached, or
/// the first enabled stage and `true` ("waiting for") when it has reached none. `None` when
/// no stages are enabled.
pub fn column(
    enabled: &[DeliveryStage],
    furthest: Option<DeliveryStage>,
) -> Option<(DeliveryStage, bool)> {
    let reached = enabled
        .iter()
        .copied()
        .filter(|s| Some(*s) <= furthest)
        .max();
    match reached {
        Some(stage) => Some((stage, false)),
        None => enabled.iter().copied().min().map(|first| (first, true)),
    }
}

/// The stage **Advance stage** moves to: the first enabled stage past `furthest`.
pub fn next_enabled_after(
    enabled: &[DeliveryStage],
    furthest: Option<DeliveryStage>,
) -> Option<DeliveryStage> {
    enabled
        .iter()
        .copied()
        .filter(|s| Some(*s) > furthest)
        .min()
}

/// The stages skipped by a move from `from` to `to` (strictly between, in order), recorded
/// as `implied` on the timeline.
pub fn implied_between(from: Option<DeliveryStage>, to: DeliveryStage) -> Vec<DeliveryStage> {
    DeliveryStage::ALL
        .into_iter()
        .filter(|s| Some(*s) > from && *s < to)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::DeliveryStage::*;

    fn delivering(stage: DeliveryStage) -> ChildState {
        ChildState {
            status: TaskStatus::Delivering,
            stage: Some(stage),
        }
    }

    fn closed(status: TaskStatus) -> ChildState {
        ChildState {
            status,
            stage: None,
        }
    }

    #[test]
    fn completion_table() {
        assert_eq!(
            completion_target(&[], None, true, &[]),
            CompletionTarget::Done
        );
        assert_eq!(
            completion_target(&[Merged], None, false, &[]),
            CompletionTarget::Done
        );
        assert_eq!(
            completion_target(&[InPr, Merged], None, true, &[]),
            CompletionTarget::Delivering(InPr)
        );
        assert_eq!(
            completion_target(&[InPr, Merged], Some(Merged), true, &[]),
            CompletionTarget::Done
        );
        assert_eq!(
            completion_target(&[Released], Some(InPr), true, &[]),
            CompletionTarget::Delivering(InPr)
        );
    }

    #[test]
    fn parent_minimum() {
        let children = [
            delivering(Released),
            delivering(Merged),
            closed(TaskStatus::Done),
            closed(TaskStatus::Cancelled),
        ];
        let all = DeliveryStage::ALL;
        assert_eq!(
            completion_target(&all, None, false, &children),
            CompletionTarget::Delivering(Merged)
        );
        assert_eq!(parent_stage(None, &children), Some(Merged));
        assert_eq!(parent_stage(Some(InPr), &children), Some(InPr));

        let finished = [closed(TaskStatus::Done), closed(TaskStatus::Cancelled)];
        assert_eq!(
            completion_target(&all, None, false, &finished),
            CompletionTarget::Done
        );
        assert_eq!(parent_stage(None, &finished), None);
    }

    #[test]
    fn column_cases() {
        assert_eq!(
            column(&[Released, Deployed], Some(Merged)),
            Some((Released, true))
        );
        assert_eq!(
            column(&[InPr, Deployed], Some(Released)),
            Some((InPr, false))
        );
        assert_eq!(
            column(&[InPr, Deployed], Some(Deployed)),
            Some((Deployed, false))
        );
        assert_eq!(column(&[Merged], None), Some((Merged, true)));
        assert_eq!(column(&[], Some(Merged)), None);
    }

    #[test]
    fn next_and_implied() {
        assert_eq!(
            next_enabled_after(&[InPr, Deployed], Some(InPr)),
            Some(Deployed)
        );
        assert_eq!(
            implied_between(Some(InPr), Deployed),
            vec![Merged, Released]
        );
        assert_eq!(next_enabled_after(&[InPr, Deployed], None), Some(InPr));
        assert_eq!(next_enabled_after(&[InPr, Deployed], Some(Deployed)), None);
        assert_eq!(implied_between(None, Merged), vec![InPr]);
        assert!(implied_between(Some(Merged), Released).is_empty());
        assert_eq!(last_enabled(&[Released, InPr]), Some(Released));
        assert_eq!(last_enabled(&[]), None);
    }

    #[test]
    fn rank_sql_matches_rank() {
        for stage in DeliveryStage::ALL {
            let arm = format!("WHEN '{stage}' THEN {}", stage.rank());
            assert!(STAGE_RANK_SQL.contains(&arm), "{arm}");
        }
    }
}
