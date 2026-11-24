//! User threads (filled in by the process milestone).
use crate::sched::Task;

pub struct UserThread {}

/// Architecture state that belongs to a user thread (FP registers, TLS register, address space),
/// switched with the thread.
pub fn switch_user_state(_prev: &Task, _next: &Task) {}

pub fn signal_pending() -> bool {
    false
}
