//! Allocating and freeing blocks and inodes: bitmaps, group descriptors (with the BLOCK_UNINIT /
//! INODE_UNINIT lazy initialization of flex_bg file systems), free counts and checksums.
use super::{le16, le32, Ext4, Group};
use crate::errno::*;
use alloc::vec::Vec;

const BG_INODE_UNINIT: u16 = 1;
const BG_BLOCK_UNINIT: u16 = 2;
const BG_INODE_ZEROED: u16 = 4;

impl Ext4 {
    pub fn ngroups(&self) -> usize {
        self.groups.lock().len()
    }

    pub fn group_first_block(&self, g: usize) -> u64 {
        let sb = self.sb.lock();
        sb.first_data_block as u64 + g as u64 * sb.blocks_per_group as u64
    }

    fn blocks_in_group(&self, g: usize) -> u32 {
        let sb = self.sb.lock();
        let first = sb.first_data_block as u64 + g as u64 * sb.blocks_per_group as u64;
        (sb.blocks_count - first).min(sb.blocks_per_group as u64) as u32
    }

    /// Writes group descriptor `g` back into its GDT block (with its checksum).
    pub fn write_group(&self, g: usize) -> Result<()> {
        let gd = self.groups.lock()[g];
        let ds = self.sb.lock().desc_size;
        let per_block = self.bs / ds;
        let b = self.bufs.get(self.gdt_block_pub(g))?;
        let off = (g % per_block) * ds;
        b.write(|d| {
            let e = &mut d[off..off + ds];
            let p32 = |e: &mut [u8], o: usize, v: u64| e[o..o + 4].copy_from_slice(&(v as u32).to_le_bytes());
            let p16 = |e: &mut [u8], o: usize, v: u32| e[o..o + 2].copy_from_slice(&(v as u16).to_le_bytes());
            p32(e, 0, gd.block_bitmap);
            p32(e, 4, gd.inode_bitmap);
            p32(e, 8, gd.inode_table);
            p16(e, 12, gd.free_blocks);
            p16(e, 14, gd.free_inodes);
            p16(e, 16, gd.used_dirs);
            p16(e, 18, gd.flags as u32);
            p16(e, 28, gd.itable_unused);
            if ds >= 64 {
                p32(e, 32, gd.block_bitmap >> 32);
                p32(e, 36, gd.inode_bitmap >> 32);
                p32(e, 40, gd.inode_table >> 32);
                p16(e, 44, gd.free_blocks >> 16);
                p16(e, 46, gd.free_inodes >> 16);
                p16(e, 48, gd.used_dirs >> 16);
                p16(e, 50, gd.itable_unused >> 16);
            }
            let c = self.group_desc_csum(g as u32, e);
            p16(e, 30, c as u32);
        });
        Ok(())
    }

    pub fn gdt_block_pub(&self, g: usize) -> u64 {
        self.gdt_block(g)
    }

    /// The metadata blocks (superblock/GDT backups, bitmaps, inode table) inside group `g`, for
    /// initializing a BLOCK_UNINIT bitmap.
    fn init_block_bitmap(&self, g: usize, bm: &mut [u8]) {
        bm.fill(0);
        let first = self.group_first_block(g);
        let n = self.blocks_in_group(g);
        let (has_sb, gdt_blocks, reserved_gdt) = {
            let sb = self.sb.lock();
            let has = self.group_has_super(g, &sb);
            let ngroups = (sb.blocks_count - sb.first_data_block as u64).div_ceil(sb.blocks_per_group as u64);
            let gdt = (ngroups as usize * sb.desc_size).div_ceil(self.bs) as u64;
            (has, gdt, le16(&sb.raw, 206) as u64)
        };
        let mut mark = |b: u64| {
            if b >= first && b < first + n as u64 {
                let i = (b - first) as usize;
                bm[i / 8] |= 1 << (i % 8);
            }
        };
        if has_sb {
            for b in 0..1 + gdt_blocks + reserved_gdt {
                mark(first + b);
            }
        }
        // bitmaps and inode tables of every group that live in this one (flex_bg)
        let ipg = self.sb.lock().inodes_per_group as u64;
        let isz = self.sb.lock().inode_size as u64;
        let itable_blocks = (ipg * isz).div_ceil(self.bs as u64);
        for gd in self.groups.lock().iter() {
            mark(gd.block_bitmap);
            mark(gd.inode_bitmap);
            for k in 0..itable_blocks {
                mark(gd.inode_table + k);
            }
        }
        // past the end of the file system
        for i in n as usize..bm.len() * 8 {
            bm[i / 8] |= 1 << (i % 8);
        }
    }

    fn block_bitmap(&self, g: usize) -> Result<alloc::sync::Arc<crate::fs::bufcache::Buf>> {
        let gd = self.groups.lock()[g];
        if gd.flags & BG_BLOCK_UNINIT != 0 {
            let b = self.bufs.get_new(gd.block_bitmap);
            b.write(|d| self.init_block_bitmap(g, d));
            self.groups.lock()[g].flags &= !BG_BLOCK_UNINIT;
            return Ok(b);
        }
        self.bufs.get(gd.block_bitmap)
    }

    fn set_block_bitmap_csum(&self, g: usize, bm: &[u8]) {
        if !self.has_csum() {
            return;
        }
        let bpg = self.sb.lock().blocks_per_group as usize;
        let c = self.bitmap_csum(&bm[..bpg / 8]);
        self.bitmap_csums.lock().insert((g, true), c);
    }

    /// Allocates up to `want` contiguous blocks, preferably at `goal`. Returns (first, count).
    pub fn alloc_blocks(&self, goal: u64, want: u32) -> Result<(u64, u32)> {
        let ng = self.ngroups();
        let bpg = self.sb.lock().blocks_per_group as u64;
        let fdb = self.sb.lock().first_data_block as u64;
        let goal = goal.max(fdb);
        let g0 = ((goal - fdb) / bpg) as usize % ng.max(1);
        for k in 0..ng {
            let g = (g0 + k) % ng;
            if self.groups.lock()[g].free_blocks == 0 {
                continue;
            }
            let n = self.blocks_in_group(g) as usize;
            let first = self.group_first_block(g);
            let bm = self.block_bitmap(g)?;
            let start = if k == 0 { ((goal - first) as usize).min(n.saturating_sub(1)) } else { 0 };
            let found = bm.read(|d| {
                let free = |i: usize| d[i / 8] & (1 << (i % 8)) == 0;
                let mut i = start;
                let mut wrapped = false;
                loop {
                    if i >= n {
                        if wrapped || start == 0 {
                            return None;
                        }
                        wrapped = true;
                        i = 0;
                    }
                    if wrapped && i >= start {
                        return None;
                    }
                    if free(i) {
                        let mut len = 1;
                        while (len as u32) < want && i + len < n && free(i + len) {
                            len += 1;
                        }
                        return Some((i, len));
                    }
                    i += 1;
                }
            });
            if let Some((i, len)) = found {
                bm.write(|d| {
                    for x in i..i + len {
                        d[x / 8] |= 1 << (x % 8);
                    }
                    self.set_block_bitmap_csum(g, d);
                });
                self.groups.lock()[g].free_blocks -= len as u32;
                self.sb.lock().free_blocks -= len as u64;
                self.write_group(g)?;
                return Ok((first + i as u64, len as u32));
            }
        }
        Err(ENOSPC)
    }

    pub fn free_blocks(&self, start: u64, count: u32) -> Result<()> {
        let bpg = self.sb.lock().blocks_per_group as u64;
        let fdb = self.sb.lock().first_data_block as u64;
        let mut b = start;
        let end = start + count as u64;
        while b < end {
            let g = ((b - fdb) / bpg) as usize;
            let first = self.group_first_block(g);
            let stop = end.min(first + bpg);
            let bm = self.block_bitmap(g)?;
            let mut freed = 0;
            bm.write(|d| {
                for x in b..stop {
                    let i = (x - first) as usize;
                    if d[i / 8] & (1 << (i % 8)) != 0 {
                        d[i / 8] &= !(1 << (i % 8));
                        freed += 1;
                    }
                }
                self.set_block_bitmap_csum(g, d);
            });
            self.groups.lock()[g].free_blocks += freed;
            self.sb.lock().free_blocks += freed as u64;
            self.write_group(g)?;
            for x in b..stop {
                self.bufs.forget(x);
            }
            b = stop;
        }
        Ok(())
    }

    /// Allocates an inode number (directories go to a group with more free space).
    pub fn alloc_inode(&self, dir: bool, near: u32) -> Result<u32> {
        let ng = self.ngroups();
        let ipg = self.sb.lock().inodes_per_group;
        let first_ino = { let sb = self.sb.lock(); if le32(&sb.raw, 76) == 0 { 11 } else { le32(&sb.raw, 84) } };
        let g0 = if dir {
            // the group with the most free inodes among those with free blocks
            let gs = self.groups.lock();
            (0..ng).max_by_key(|&g| (gs[g].free_blocks > 0, gs[g].free_inodes)).unwrap_or(0)
        } else {
            ((near.max(1) - 1) / ipg) as usize
        };
        for k in 0..ng {
            let g = (g0 + k) % ng;
            let gd = self.groups.lock()[g];
            if gd.free_inodes == 0 {
                continue;
            }
            let bm = if gd.flags & BG_INODE_UNINIT != 0 {
                let b = self.bufs.get_new(gd.inode_bitmap);
                b.write(|d| {
                    d.fill(0);
                    for i in ipg as usize..d.len() * 8 {
                        d[i / 8] |= 1 << (i % 8);
                    }
                });
                self.groups.lock()[g].flags &= !BG_INODE_UNINIT;
                b
            } else {
                self.bufs.get(gd.inode_bitmap)?
            };
            let found = bm.read(|d| (0..ipg as usize).find(|&i| d[i / 8] & (1 << (i % 8)) == 0 && g as u32 * ipg + i as u32 + 1 >= first_ino));
            if let Some(i) = found {
                bm.write(|d| {
                    d[i / 8] |= 1 << (i % 8);
                    if self.has_csum() {
                        let c = self.bitmap_csum(&d[..ipg as usize / 8]);
                        self.bitmap_csums.lock().insert((g, false), c);
                    }
                });
                {
                    let mut gs = self.groups.lock();
                    gs[g].free_inodes -= 1;
                    if dir {
                        gs[g].used_dirs += 1;
                    }
                    gs[g].flags |= BG_INODE_ZEROED;
                    // the inode is in use now: it is no longer in the unused tail of the table
                    let used_upto = i as u32 + 1;
                    if ipg - gs[g].itable_unused < used_upto {
                        gs[g].itable_unused = ipg - used_upto;
                    }
                }
                self.sb.lock().free_inodes -= 1;
                self.write_group(g)?;
                return Ok(g as u32 * ipg + i as u32 + 1);
            }
        }
        Err(ENOSPC)
    }

    pub fn free_inode(&self, ino: u32, dir: bool) -> Result<()> {
        let ipg = self.sb.lock().inodes_per_group;
        let g = ((ino - 1) / ipg) as usize;
        let i = ((ino - 1) % ipg) as usize;
        let bm = self.bufs.get(self.groups.lock()[g].inode_bitmap)?;
        bm.write(|d| {
            d[i / 8] &= !(1 << (i % 8));
            if self.has_csum() {
                let c = self.bitmap_csum(&d[..ipg as usize / 8]);
                self.bitmap_csums.lock().insert((g, false), c);
            }
        });
        {
            let mut gs = self.groups.lock();
            gs[g].free_inodes += 1;
            if dir {
                gs[g].used_dirs -= 1;
            }
        }
        self.sb.lock().free_inodes += 1;
        self.write_group(g)
    }

    /// Bitmap checksums changed since the last commit go into the group descriptors.
    pub fn apply_bitmap_csums(&self) -> Result<()> {
        let pending: Vec<((usize, bool), u32)> = core::mem::take(&mut *self.bitmap_csums.lock()).into_iter().collect();
        let ds = self.sb.lock().desc_size;
        let per_block = self.bs / ds;
        for ((g, block), c) in pending {
            let b = self.bufs.get(self.gdt_block_pub(g))?;
            let off = (g % per_block) * ds;
            b.write(|d| {
                let e = &mut d[off..off + ds];
                let (lo, hi) = if block { (24, 56) } else { (26, 58) };
                e[lo..lo + 2].copy_from_slice(&((c & 0xffff) as u16).to_le_bytes());
                if ds >= 64 {
                    e[hi..hi + 2].copy_from_slice(&((c >> 16) as u16).to_le_bytes());
                }
                let gc = self.group_desc_csum(g as u32, e);
                e[30..32].copy_from_slice(&gc.to_le_bytes());
            });
        }
        Ok(())
    }

    pub fn group(&self, g: usize) -> Group {
        self.groups.lock()[g]
    }
}
