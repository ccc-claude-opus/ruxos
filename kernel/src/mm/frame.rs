//! Physical memory: a buddy allocator over the RAM ranges of the device tree, and one `PageMeta`
//! per 4 KB frame (reference count for shared and copy-on-write pages).
use crate::arch::{phys_to_virt, virt_to_phys};
use crate::sync::SpinLock;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

pub const PAGE_SIZE: usize = 4096;
pub const MAX_ORDER: usize = 10; // 4 MB blocks

#[repr(C)]
pub struct PageMeta {
    pub refs: AtomicU32,
    /// Buddy bookkeeping: the order of a free block's head page, or NOT_FREE.
    free_order: core::sync::atomic::AtomicU8,
    _pad: [u8; 3],
    pub data: AtomicUsize,
}
const NOT_FREE: u8 = 0xff;

struct FreeNode {
    next: usize, // physical address, 0 = end
    prev: usize,
}

struct Buddy {
    heads: [usize; MAX_ORDER + 1],
    free_pages: usize,
}

static BUDDY: SpinLock<Buddy> = SpinLock::new(Buddy { heads: [0; MAX_ORDER + 1], free_pages: 0 });
static mut META: *mut PageMeta = core::ptr::null_mut();
static mut RAM_START: usize = 0;
static mut RAM_PAGES: usize = 0;
pub static TOTAL_PAGES: AtomicUsize = AtomicUsize::new(0);

fn node(pa: usize) -> &'static mut FreeNode {
    unsafe { &mut *(phys_to_virt(pa) as *mut FreeNode) }
}

pub fn meta(pa: usize) -> &'static PageMeta {
    unsafe {
        let i = (pa - RAM_START) / PAGE_SIZE;
        debug_assert!(i < RAM_PAGES);
        &*META.add(i)
    }
}

pub fn is_ram(pa: usize) -> bool {
    unsafe { pa >= RAM_START && pa < RAM_START + RAM_PAGES * PAGE_SIZE }
}

impl Buddy {
    fn push(&mut self, pa: usize, order: usize) {
        let n = node(pa);
        n.next = self.heads[order];
        n.prev = 0;
        if n.next != 0 {
            node(n.next).prev = pa;
        }
        self.heads[order] = pa;
        meta(pa).free_order.store(order as u8, Ordering::Relaxed);
    }
    fn remove(&mut self, pa: usize, order: usize) {
        let n = node(pa);
        let (next, prev) = (n.next, n.prev);
        if prev != 0 { node(prev).next = next } else { self.heads[order] = next }
        if next != 0 {
            node(next).prev = prev;
        }
        meta(pa).free_order.store(NOT_FREE, Ordering::Relaxed);
    }
    fn alloc(&mut self, order: usize) -> Option<usize> {
        let mut o = order;
        while o <= MAX_ORDER && self.heads[o] == 0 {
            o += 1;
        }
        if o > MAX_ORDER {
            return None;
        }
        let pa = self.heads[o];
        self.remove(pa, o);
        while o > order {
            o -= 1;
            self.push(pa + (PAGE_SIZE << o), o);
        }
        self.free_pages -= 1 << order;
        Some(pa)
    }
    fn free(&mut self, mut pa: usize, mut order: usize) {
        self.free_pages += 1 << order;
        let base = unsafe { RAM_START };
        while order < MAX_ORDER {
            let buddy = base + ((pa - base) ^ (PAGE_SIZE << order));
            if !is_ram(buddy) || meta(buddy).free_order.load(Ordering::Relaxed) != order as u8 {
                break;
            }
            self.remove(buddy, order);
            pa = pa.min(buddy);
            order += 1;
        }
        self.push(pa, order);
    }
}

/// Sets up the allocator. `ram` is the (single, contiguous) RAM range; `reserved` are physical ranges
/// that must never be handed out (kernel image, device tree, initrd).
pub fn init(ram: (usize, usize), reserved: &[(usize, usize)]) {
    let (start, size) = ram;
    let pages = size / PAGE_SIZE;
    // the PageMeta array goes in the first free spot after the kernel image
    let meta_bytes = (pages * core::mem::size_of::<PageMeta>() + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let kernel_end = virt_to_phys(unsafe { &crate::arch::_end as *const u8 as usize });
    let meta_pa = (kernel_end + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    unsafe {
        RAM_START = start;
        RAM_PAGES = pages;
        META = phys_to_virt(meta_pa) as *mut PageMeta;
        core::ptr::write_bytes(META as *mut u8, 0, meta_bytes);
        for i in 0..pages {
            (*META.add(i)).free_order.store(NOT_FREE, Ordering::Relaxed);
        }
    }
    let busy = |pa: usize| -> bool {
        pa < meta_pa + meta_bytes // RAM below the kernel, the kernel, the PageMeta array
            || reserved.iter().any(|&(s, e)| pa + PAGE_SIZE > s && pa < e)
    };
    let mut b = BUDDY.lock();
    let mut pa = start;
    let end = start + pages * PAGE_SIZE;
    while pa < end {
        if busy(pa) {
            pa += PAGE_SIZE;
            continue;
        }
        // largest aligned block that is entirely free
        let mut order = 0;
        while order < MAX_ORDER {
            let sz = PAGE_SIZE << (order + 1);
            if (pa - start) % sz != 0 || pa + sz > end {
                break;
            }
            if (0..(1 << (order + 1))).any(|k| busy(pa + k * PAGE_SIZE)) {
                break;
            }
            order += 1;
        }
        b.free(pa, order);
        pa += PAGE_SIZE << order;
    }
    TOTAL_PAGES.store(b.free_pages, Ordering::Relaxed);
}

/// Allocates 2^order contiguous frames (not zeroed). Returns the physical address.
pub fn alloc_pages(order: usize) -> Option<usize> {
    let pa = BUDDY.lock().alloc(order)?;
    meta(pa).refs.store(1, Ordering::Relaxed);
    Some(pa)
}

pub fn free_pages(pa: usize, order: usize) {
    BUDDY.lock().free(pa, order);
}

/// One zeroed page; panics when memory is exhausted (callers that can fail use `try_zeroed_page`).
pub fn zeroed_page() -> usize {
    try_zeroed_page().expect("out of memory")
}

pub fn try_zeroed_page() -> Option<usize> {
    let pa = alloc_pages(0)?;
    unsafe { core::ptr::write_bytes(phys_to_virt(pa) as *mut u8, 0, PAGE_SIZE) };
    Some(pa)
}

/// Takes another reference to a page (shared mapping, copy-on-write, page cache).
pub fn get_page(pa: usize) {
    meta(pa).refs.fetch_add(1, Ordering::Relaxed);
}

/// Drops a reference; frees the page when it was the last one.
pub fn put_page(pa: usize) {
    if meta(pa).refs.fetch_sub(1, Ordering::AcqRel) == 1 {
        free_pages(pa, 0);
    }
}

pub fn page_refs(pa: usize) -> u32 {
    meta(pa).refs.load(Ordering::Acquire)
}

pub fn free_count() -> usize {
    BUDDY.lock().free_pages
}
