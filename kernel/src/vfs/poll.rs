//! Readiness notification: a `PollQueue` wakes tasks blocked in read/write/poll and tells the
//! epoll instances watching it.
use crate::sched::WaitQueue;
use crate::sync::SpinLock;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

pub const POLLIN: u32 = 0x1;
pub const POLLPRI: u32 = 0x2;
pub const POLLOUT: u32 = 0x4;
pub const POLLERR: u32 = 0x8;
pub const POLLHUP: u32 = 0x10;
pub const POLLNVAL: u32 = 0x20;
pub const POLLRDNORM: u32 = 0x40;
pub const POLLRDBAND: u32 = 0x80;
pub const POLLWRNORM: u32 = 0x100;
pub const POLLWRBAND: u32 = 0x200;
pub const POLLRDHUP: u32 = 0x2000;

pub trait Watcher: Send + Sync {
    /// The watched object's readiness may have changed (`events` that just happened).
    fn notify(&self, events: u32);
}

pub struct PollQueue {
    pub wq: WaitQueue,
    watchers: SpinLock<Vec<Weak<dyn Watcher>>>,
}

impl Default for PollQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl PollQueue {
    pub const fn new() -> Self {
        PollQueue { wq: WaitQueue::new(), watchers: SpinLock::new(Vec::new()) }
    }

    pub fn notify(&self, events: u32) {
        self.wq.wake_all();
        let ws: Vec<Arc<dyn Watcher>> = {
            let mut w = self.watchers.lock();
            w.retain(|x| x.strong_count() > 0);
            w.iter().filter_map(|x| x.upgrade()).collect()
        };
        for w in ws {
            w.notify(events);
        }
    }

    pub fn watch(&self, w: Weak<dyn Watcher>) {
        self.watchers.lock().push(w);
    }

    pub fn unwatch(&self, w: &Weak<dyn Watcher>) {
        self.watchers.lock().retain(|x| !Weak::ptr_eq(x, w));
    }
}
