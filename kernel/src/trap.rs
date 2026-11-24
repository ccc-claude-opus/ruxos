//! Traps from user mode (filled in by the process milestone).
use crate::arch::TrapFrame;

pub fn user_exception(_f: &mut TrapFrame, cause: usize, addr: usize) {
    panic!("user exception {cause:#x} at {addr:#x}");
}

pub fn return_to_user(_f: &mut TrapFrame) {
    crate::sched::resched_if_needed();
}
