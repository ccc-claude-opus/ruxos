//! Locks. `SpinLock` disables interrupts on this CPU while held (so the same lock can be taken in an
//! interrupt handler) and is a test-and-test-and-set lock. `Mutex` sleeps (for long sections that
//! may block, such as file system operations).
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

pub struct SpinLock<T: ?Sized> {
    locked: AtomicBool,
    /// Who holds it (debugging stuck locks): the caller's source location and CPU.
    owner: core::sync::atomic::AtomicPtr<core::panic::Location<'static>>,
    owner_cpu: core::sync::atomic::AtomicUsize,
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
        SpinLock {
            locked: AtomicBool::new(false),
            owner: core::sync::atomic::AtomicPtr::new(core::ptr::null_mut()),
            owner_cpu: core::sync::atomic::AtomicUsize::new(0),
            data: UnsafeCell::new(v),
        }
    }
}

impl<T: ?Sized> SpinLock<T> {
    #[track_caller]
    pub fn lock(&self) -> SpinGuard<'_, T> {
        let caller = core::panic::Location::caller();
        let irq = crate::arch::irq_save();
        let mut spins = 0u64;
        while self.locked.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
                spins += 1;
                if spins == 500_000_000 {
                    let o = self.owner.load(Ordering::Relaxed);
                    if !o.is_null() {
                        let o = unsafe { &*o };
                        crate::console::print(format_args!(
                            "ruxos: a spin lock seems stuck: wanted at {}:{}, held by cpu {} since {}:{}\n",
                            caller.file(), caller.line(), self.owner_cpu.load(Ordering::Relaxed), o.file(), o.line()
                        ));
                    }
                }
            }
        }
        self.owner.store(caller as *const _ as *mut _, Ordering::Relaxed);
        if crate::arch::percpu_ptr() != 0 {
            self.owner_cpu.store(crate::percpu::this().id, Ordering::Relaxed);
        }
        SpinGuard { lock: self, irq }
    }

    #[track_caller]
    pub fn try_lock(&self) -> Option<SpinGuard<'_, T>> {
        let irq = crate::arch::irq_save();
        if self.locked.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            self.owner.store(core::panic::Location::caller() as *const _ as *mut _, Ordering::Relaxed);
            Some(SpinGuard { lock: self, irq })
        } else {
            crate::arch::irq_restore(irq);
            None
        }
    }

    pub fn get_mut_inner(&mut self) -> &mut T {
        self.data.get_mut()
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
    busy: AtomicBool,
    v: UnsafeCell<Option<T>>,
}
unsafe impl<T: Send + Sync> Sync for Once<T> {}

impl<T> Once<T> {
    pub const fn new() -> Self {
        Once { set: AtomicBool::new(false), busy: AtomicBool::new(false), v: UnsafeCell::new(None) }
    }
    /// The value, made by `f` by the first caller (the others wait for it).
    pub fn call_once(&self, f: impl FnOnce() -> T) -> &T {
        if let Some(v) = self.get() {
            return v;
        }
        if self.busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            self.set(f());
        } else {
            while !self.set.load(Ordering::Acquire) {
                core::hint::spin_loop();
            }
        }
        self.get().unwrap()
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
