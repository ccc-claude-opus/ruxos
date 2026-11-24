//! The kernel heap (`alloc`): size classes of 16..2048 bytes carved out of pages, larger requests
//! straight from the buddy allocator.
use super::frame::{self, PAGE_SIZE};
use crate::arch::{phys_to_virt, virt_to_phys};
use crate::sync::SpinLock;
use core::alloc::{GlobalAlloc, Layout};

const CLASSES: [usize; 8] = [16, 32, 64, 128, 256, 512, 1024, 2048];

struct Class {
    free: usize, // virtual address of the first free object (it holds the next one), 0 = none
}

pub struct Heap {
    classes: [SpinLock<Class>; 8],
}

#[global_allocator]
static HEAP: Heap = Heap {
    classes: [const { SpinLock::new(Class { free: 0 }) }; 8],
};

fn order_for(size: usize) -> usize {
    let pages = size.div_ceil(PAGE_SIZE);
    pages.next_power_of_two().trailing_zeros() as usize
}

unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let size = l.size().max(l.align());
        if let Some(ci) = CLASSES.iter().position(|&c| c >= size) {
            let mut c = self.classes[ci].lock();
            if c.free == 0 {
                let Some(pa) = frame::alloc_pages(0) else { return core::ptr::null_mut() };
                let va = phys_to_virt(pa);
                let sz = CLASSES[ci];
                for k in (0..PAGE_SIZE / sz).rev() {
                    let obj = va + k * sz;
                    unsafe { *(obj as *mut usize) = c.free };
                    c.free = obj;
                }
            }
            let obj = c.free;
            c.free = unsafe { *(obj as *const usize) };
            obj as *mut u8
        } else {
            match frame::alloc_pages(order_for(size)) {
                Some(pa) => phys_to_virt(pa) as *mut u8,
                None => core::ptr::null_mut(),
            }
        }
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        let size = l.size().max(l.align());
        if let Some(ci) = CLASSES.iter().position(|&c| c >= size) {
            let mut c = self.classes[ci].lock();
            unsafe { *(p as *mut usize) = c.free };
            c.free = p as usize;
        } else {
            frame::free_pages(virt_to_phys(p as usize), order_for(size));
        }
    }
}
