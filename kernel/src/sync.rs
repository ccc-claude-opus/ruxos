//! Locks. `SpinLock` disables interrupts on this CPU while held (so the same lock can be taken in an
//! interrupt handler) and is a test-and-test-and-set lock. `Mutex` sleeps (for long sections that
//! may block, such as file system operations).
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

pub struct SpinLock<T: ?Sized> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

unsafe impl<T: ?Sized + Send> Sync for SpinLock<T> {}
unsafe impl<T: ?Sized + Send> Send for SpinLock<T> {}

pub struct SpinGuard<'a, T: ?Sized> {
    lock: &'a SpinLock<T>,
    irq: bool,
}

impl<T> SpinLock<T> {
    pub const fn new(v: T) -> Self {
        SpinLock { locked: AtomicBool::new(false), data: UnsafeCell::new(v) }
    }
}

impl<T: ?Sized> SpinLock<T> {
    pub fn lock(&self) -> SpinGuard<'_, T> {
        let irq = crate::arch::irq_save();
        while self.locked.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        SpinGuard { lock: self, irq }
    }

    pub fn try_lock(&self) -> Option<SpinGuard<'_, T>> {
        let irq = crate::arch::irq_save();
        if self.locked.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            Some(SpinGuard { lock: self, irq })
        } else {
            crate::arch::irq_restore(irq);
            None
        }
    }

    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }

    /// # Safety
    /// The caller must otherwise guarantee exclusive access.
    pub unsafe fn get_unchecked(&self) -> &mut T {
        unsafe { &mut *self.data.get() }
    }
}

impl<T: ?Sized> Deref for SpinGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T: ?Sized> DerefMut for SpinGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T: ?Sized> Drop for SpinGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
        crate::arch::irq_restore(self.irq);
    }
}

impl<'a, T: ?Sized> SpinGuard<'a, T> {
    /// Releases the lock but keeps interrupts disabled (returns the saved state).
    pub fn unlock_keep_irq(self) -> bool {
        let irq = self.irq;
        self.lock.locked.store(false, Ordering::Release);
        core::mem::forget(self);
        irq
    }
}

/// A value set once at boot and read-only afterwards.
pub struct Once<T> {
    set: AtomicBool,
    v: UnsafeCell<Option<T>>,
}
unsafe impl<T: Send + Sync> Sync for Once<T> {}

impl<T> Once<T> {
    pub const fn new() -> Self {
        Once { set: AtomicBool::new(false), v: UnsafeCell::new(None) }
    }
    pub fn set(&self, v: T) {
        unsafe { *self.v.get() = Some(v) };
        self.set.store(true, Ordering::Release);
    }
    pub fn get(&self) -> Option<&T> {
        if self.set.load(Ordering::Acquire) { unsafe { (*self.v.get()).as_ref() } } else { None }
    }
}

impl<T> Deref for Once<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.get().expect("Once not set")
    }
}
