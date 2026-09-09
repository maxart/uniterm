//! Durable tasks (docs/09 tiers 1-2): the human-facing unit of work that a
//! session's panes and agents serve. Pure model + list logic; persistence is a
//! projection of the event log, server-side.

use serde::{Deserialize, Serialize};

/// A task's lifecycle status.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum TaskStatus {
    Todo,
    Doing,
    Blocked,
    Done,
}

impl TaskStatus {
    /// Sort order for the list: active/attention-worthy first, Done last.
    pub fn order(self) -> u8 {
        match self {
            TaskStatus::Doing => 0,
            TaskStatus::Blocked => 1,
            TaskStatus::Todo => 2,
            TaskStatus::Done => 3,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TaskStatus::Todo => "todo",
            TaskStatus::Doing => "doing",
            TaskStatus::Blocked => "blocked",
            TaskStatus::Done => "done",
        }
    }

    /// The human-facing name shown in the task manager.
    pub fn display(self) -> &'static str {
        match self {
            TaskStatus::Todo => "planned",
            TaskStatus::Doing => "running",
            TaskStatus::Blocked => "blocked",
            TaskStatus::Done => "finished",
        }
    }

    /// The status's signature colour (used for badges/dots wherever tasks are
    /// drawn): planned blue, running amber, blocked red, finished green.
    pub fn color(self) -> crate::Color {
        match self {
            TaskStatus::Todo => crate::Color::Idx(75),
            TaskStatus::Doing => crate::Color::Idx(214),
            TaskStatus::Blocked => crate::Color::Idx(203),
            TaskStatus::Done => crate::Color::Idx(78),
        }
    }

    /// The next status in the manual cycle (planned -> running -> blocked ->
    /// finished -> planned), used by the task manager's status action.
    pub fn next(self) -> TaskStatus {
        match self {
            TaskStatus::Todo => TaskStatus::Doing,
            TaskStatus::Doing => TaskStatus::Blocked,
            TaskStatus::Blocked => TaskStatus::Done,
            TaskStatus::Done => TaskStatus::Todo,
        }
    }
}

/// One task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: u64,
    pub title: String,
    pub status: TaskStatus,
    pub notes: String,
}

/// An ordered collection of tasks with monotonically-assigned ids.
#[derive(Clone, Debug, Default)]
pub struct TaskList {
    tasks: Vec<Task>,
    next_id: u64,
}

impl TaskList {
    pub fn new() -> Self {
        TaskList {
            tasks: Vec::new(),
            next_id: 1,
        }
    }

    /// Add a task, returning its new id.
    pub fn add(&mut self, title: &str, status: TaskStatus) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.tasks.push(Task {
            id,
            title: title.to_string(),
            status,
            notes: String::new(),
        });
        id
    }

    /// Insert a task with an explicit id (used when projecting the event log).
    pub fn insert(&mut self, id: u64, title: &str, status: TaskStatus) {
        self.next_id = self.next_id.max(id + 1);
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
            t.title = title.to_string();
            t.status = status;
        } else {
            self.tasks.push(Task {
                id,
                title: title.to_string(),
                status,
                notes: String::new(),
            });
        }
    }

    /// Update a task's status; returns whether it existed.
    pub fn set_status(&mut self, id: u64, status: TaskStatus) -> bool {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
            t.status = status;
            true
        } else {
            false
        }
    }

    /// Rename a task; returns whether it existed.
    pub fn set_title(&mut self, id: u64, title: &str) -> bool {
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
            t.title = title.to_string();
            true
        } else {
            false
        }
    }

    /// Delete a task; returns whether it existed.
    pub fn remove(&mut self, id: u64) -> bool {
        let before = self.tasks.len();
        self.tasks.retain(|t| t.id != id);
        self.tasks.len() != before
    }

    pub fn get(&self, id: u64) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// Tasks ordered active-first (Doing, Blocked, Todo, Done), ties by id.
    pub fn ordered(&self) -> Vec<&Task> {
        let mut v: Vec<&Task> = self.tasks.iter().collect();
        v.sort_by_key(|t| (t.status.order(), t.id));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_get_and_status_transitions() {
        let mut l = TaskList::new();
        let a = l.add("fix the parser", TaskStatus::Doing);
        let b = l.add("write docs", TaskStatus::Todo);
        assert_eq!(l.len(), 2);
        assert_eq!(l.get(a).unwrap().title, "fix the parser");
        assert!(l.set_status(b, TaskStatus::Done));
        assert_eq!(l.get(b).unwrap().status, TaskStatus::Done);
        assert!(!l.set_status(999, TaskStatus::Done)); // missing id
    }

    #[test]
    fn ordered_puts_active_first_done_last() {
        let mut l = TaskList::new();
        l.add("done one", TaskStatus::Done);
        l.add("doing one", TaskStatus::Doing);
        l.add("todo one", TaskStatus::Todo);
        l.add("blocked one", TaskStatus::Blocked);
        let order: Vec<&str> = l.ordered().iter().map(|t| t.title.as_str()).collect();
        assert_eq!(order, ["doing one", "blocked one", "todo one", "done one"]);
    }

    #[test]
    fn insert_projects_without_duplicating() {
        // Simulating replay of the event log: create then status-change.
        let mut l = TaskList::new();
        l.insert(5, "ship it", TaskStatus::Doing);
        l.insert(5, "ship it", TaskStatus::Done); // same id -> update, not dup
        assert_eq!(l.len(), 1);
        assert_eq!(l.get(5).unwrap().status, TaskStatus::Done);
        // next add must not collide with the projected id.
        let n = l.add("next", TaskStatus::Todo);
        assert!(n > 5);
    }
}
