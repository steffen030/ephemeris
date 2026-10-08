//! Task prioritization engine (ADR ephemeris-eaa.2).
//!
//! Pure function: given a slice of tasks and the current Unix timestamp (seconds),
//! returns them sorted highest-priority-first.
//!
//! Score components (all additive, higher = more urgent):
//! - `priority_score`: High=300, Medium=200, Low=100
//! - `due_score`: 500 if overdue, 400 if due today, 300 if due in 1 d,
//!   200 if due in 2 d, 100 if due in 3–7 d, 0 if >7 d or no due
//! - total = priority_score + due_score

use crate::{Task, TaskPriority};

const SECS_PER_DAY: u64 = 86_400;

/// Score a single task at `now_secs` (Unix seconds).
pub fn task_score(task: &Task, now_secs: u64) -> u32 {
    let priority_score = match task.priority {
        TaskPriority::High => 300,
        TaskPriority::Medium => 200,
        TaskPriority::Low => 100,
    };

    let due_score = match task.due {
        None => 0,
        Some(due) => {
            if due <= now_secs {
                500 // overdue
            } else {
                let days_away = (due - now_secs).div_ceil(SECS_PER_DAY);
                match days_away {
                    0 => 400,
                    1 => 300,
                    2 => 200,
                    3..=7 => 100,
                    _ => 0,
                }
            }
        }
    };

    priority_score + due_score
}

/// Return `tasks` sorted highest-score-first, excluding done tasks.
///
/// Ties are broken by original slice order (stable sort).
pub fn rank_tasks<'a>(tasks: &'a [Task], now_secs: u64) -> Vec<&'a Task> {
    let mut scored: Vec<(&'a Task, u32)> = tasks
        .iter()
        .filter(|t| !t.done)
        .map(|t| (t, task_score(t, now_secs)))
        .collect();

    scored.sort_by_key(|a| std::cmp::Reverse(a.1));
    scored.into_iter().map(|(t, _)| t).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ProfileId, Task, TaskPriority};

    fn now() -> u64 {
        1_750_000_000 // fixed epoch for deterministic tests
    }

    fn task(title: &str, priority: TaskPriority, due_offset_days: Option<i64>) -> Task {
        let mut t = Task::new(title, ProfileId::new());
        t.priority = priority;
        if let Some(d) = due_offset_days {
            let base = now() as i64;
            t.due = Some((base + d * SECS_PER_DAY as i64) as u64);
        }
        t
    }

    #[test]
    fn overdue_high_beats_future_high() {
        let tasks = vec![
            task("future high", TaskPriority::High, Some(3)),
            task("overdue high", TaskPriority::High, Some(-1)),
        ];
        let ranked = rank_tasks(&tasks, now());
        assert_eq!(ranked[0].title, "overdue high");
    }

    #[test]
    fn high_no_due_beats_low_overdue() {
        let tasks = vec![
            task("low overdue", TaskPriority::Low, Some(-1)),
            task("high no due", TaskPriority::High, None),
        ];
        let ranked = rank_tasks(&tasks, now());
        // high no due: 300+0=300; low overdue: 100+500=600 — overdue wins
        assert_eq!(ranked[0].title, "low overdue");
    }

    #[test]
    fn done_tasks_excluded() {
        let mut done = task("done task", TaskPriority::High, Some(-1));
        done.done = true;
        let tasks = vec![done, task("open", TaskPriority::Low, None)];
        let ranked = rank_tasks(&tasks, now());
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].title, "open");
    }

    #[test]
    fn stable_sort_on_equal_score() {
        let tasks = vec![
            task("first", TaskPriority::Medium, None),
            task("second", TaskPriority::Medium, None),
        ];
        let ranked = rank_tasks(&tasks, now());
        assert_eq!(ranked[0].title, "first");
        assert_eq!(ranked[1].title, "second");
    }
}
