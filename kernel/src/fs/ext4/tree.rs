//! Changing extent trees: inserting a mapping (merging with the previous extent when contiguous),
//! and rebuilding the whole tree from a flat list when a node overflows or on truncate.
use super::extent::EXTENT_MAGIC;
use super::inode::{Ext4Inode, EXT4_EXTENTS_FL};
use super::{le16, le32};
use crate::errno::*;
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ext {
    pub lblk: u32,
    pub len: u32,
    pub pblk: u64,
    pub unwritten: bool,
}

const MAX_LEN: u32 = 32768;

fn hdr(b: &mut [u8], entries: usize, max: usize, depth: u16) {
    b[0..2].copy_from_slice(&EXTENT_MAGIC.to_le_bytes());
    b[2..4].copy_from_slice(&(entries as u16).to_le_bytes());
    b[4..6].copy_from_slice(&(max as u16).to_le_bytes());
    b[6..8].copy_from_slice(&depth.to_le_bytes());
    b[8..12].fill(0);
}

fn put_leaf(b: &mut [u8], i: usize, e: &Ext) {
    let o = 12 + 12 * i;
    b[o..o + 4].copy_from_slice(&e.lblk.to_le_bytes());
    let len = if e.unwritten { e.len + MAX_LEN } else { e.len } as u16;
    b[o + 4..o + 6].copy_from_slice(&len.to_le_bytes());
    b[o + 6..o + 8].copy_from_slice(&((e.pblk >> 32) as u16).to_le_bytes());
    b[o + 8..o + 12].copy_from_slice(&(e.pblk as u32).to_le_bytes());
}

fn put_index(b: &mut [u8], i: usize, lblk: u32, child: u64) {
    let o = 12 + 12 * i;
    b[o..o + 4].copy_from_slice(&lblk.to_le_bytes());
    b[o + 4..o + 8].copy_from_slice(&(child as u32).to_le_bytes());
    b[o + 8..o + 10].copy_from_slice(&((child >> 32) as u16).to_le_bytes());
    b[o + 10..o + 12].fill(0);
}

impl Ext4Inode {
    fn blocks_max(&self) -> usize {
        (self.fs.bs - 12 - 4) / 12
    }

    /// Every extent, in order, and the blocks the tree itself uses.
    pub fn extents_all(&self) -> Result<(Vec<Ext>, Vec<u64>)> {
        let mut out = Vec::new();
        let mut nodes = Vec::new();
        let root = self.iblock();
        if self.flags() & EXT4_EXTENTS_FL == 0 {
            return Ok((out, nodes));
        }
        fn walk(fs: &super::Ext4, node: &[u8], out: &mut Vec<Ext>, nodes: &mut Vec<u64>, depth_left: u32) -> Result<()> {
            if le16(node, 0) != EXTENT_MAGIC || depth_left == 0 {
                return Err(EIO);
            }
            let n = le16(node, 2) as usize;
            let depth = le16(node, 6);
            for i in 0..n {
                let o = 12 + 12 * i;
                if depth == 0 {
                    let raw = le16(node, o + 4) as u32;
                    let (len, unwritten) = if raw > MAX_LEN { (raw - MAX_LEN, true) } else { (raw, false) };
                    let pblk = ((le16(node, o + 6) as u64) << 32) | le32(node, o + 8) as u64;
                    out.push(Ext { lblk: le32(node, o), len, pblk, unwritten });
                } else {
                    let child = le32(node, o + 4) as u64 | ((le16(node, o + 8) as u64) << 32);
                    nodes.push(child);
                    let b = fs.bufs.get(child)?.read(|d| d.to_vec());
                    walk(fs, &b, out, nodes, depth_left - 1)?;
                }
            }
            Ok(())
        }
        walk(&self.fs, &root, &mut out, &mut nodes, 8)?;
        Ok((out, nodes))
    }

    /// Writes a new tree holding `ext` (sorted, non-overlapping); frees the old tree's blocks.
    pub fn rebuild_tree(&self, ext: &[Ext], old_nodes: &[u64]) -> Result<()> {
        for &b in old_nodes {
            self.fs.free_blocks(b, 1)?;
            self.add_blocks(-1);
        }
        let epb = self.blocks_max();
        let fill = (epb * 3 / 4).max(1);
        // level 0: leaves
        let mut level: Vec<(u32, Vec<u8>)> = Vec::new(); // (first lblk, node bytes) for blocks
        let mut depth = 0u16;
        if ext.len() <= 4 {
            let mut root = alloc::vec![0u8; 60];
            hdr(&mut root, ext.len(), 4, 0);
            for (i, e) in ext.iter().enumerate() {
                put_leaf(&mut root, i, e);
            }
            self.set_iblock(&root);
            return Ok(());
        }
        for chunk in ext.chunks(fill) {
            let mut b = alloc::vec![0u8; self.fs.bs];
            hdr(&mut b, chunk.len(), epb, 0);
            for (i, e) in chunk.iter().enumerate() {
                put_leaf(&mut b, i, e);
            }
            level.push((chunk[0].lblk, b));
        }
        loop {
            // write this level's blocks
            let goal = ext.first().map_or(0, |e| e.pblk.saturating_sub(16));
            let mut written: Vec<(u32, u64)> = Vec::new();
            for (first, mut b) in level.drain(..) {
                let (blk, _) = self.fs.alloc_blocks(goal, 1)?;
                self.add_blocks(1);
                let seed = self.seed();
                self.fs.set_extent_csum(seed, &mut b);
                let buf = self.fs.bufs.get_new(blk);
                buf.write(|d| d.copy_from_slice(&b));
                written.push((first, blk));
            }
            depth += 1;
            if written.len() <= 4 {
                let mut root = alloc::vec![0u8; 60];
                hdr(&mut root, written.len(), 4, depth);
                for (i, (l, c)) in written.iter().enumerate() {
                    put_index(&mut root, i, *l, *c);
                }
                self.set_iblock(&root);
                return Ok(());
            }
            for chunk in written.chunks(fill) {
                let mut b = alloc::vec![0u8; self.fs.bs];
                hdr(&mut b, chunk.len(), epb, depth);
                for (i, (l, c)) in chunk.iter().enumerate() {
                    put_index(&mut b, i, *l, *c);
                }
                level.push((chunk[0].0, b));
            }
        }
    }

    /// Adds the mapping lblk -> pblk for `len` blocks (not overlapping existing ones).
    pub fn insert_extent(&self, lblk: u32, pblk: u64, len: u32) -> Result<()> {
        if self.flags() & EXT4_EXTENTS_FL == 0 {
            // an empty file without extents (new inodes always have the flag)
            let mut root = alloc::vec![0u8; 60];
            hdr(&mut root, 0, 4, 0);
            self.set_iblock(&root);
            self.set_flags(self.flags() | EXT4_EXTENTS_FL);
        }
        let root = self.iblock();
        let depth = le16(&root, 6);
        // fast path: the leaf that ends where the new range starts can be extended or appended to
        if depth == 0 {
            let n = le16(&root, 2) as usize;
            let mut r = root.clone();
            if n > 0 {
                let o = 12 + 12 * (n - 1);
                let raw = le16(&r, o + 4) as u32;
                let start = le32(&r, o);
                let p = ((le16(&r, o + 6) as u64) << 32) | le32(&r, o + 8) as u64;
                if raw <= MAX_LEN && start + raw == lblk && p + raw as u64 == pblk && raw + len <= MAX_LEN {
                    r[o + 4..o + 6].copy_from_slice(&((raw + len) as u16).to_le_bytes());
                    self.set_iblock(&r);
                    return Ok(());
                }
                if lblk > start && n < 4 {
                    put_leaf(&mut r, n, &Ext { lblk, len, pblk, unwritten: false });
                    r[2..4].copy_from_slice(&((n + 1) as u16).to_le_bytes());
                    self.set_iblock(&r);
                    return Ok(());
                }
            } else {
                put_leaf(&mut r, 0, &Ext { lblk, len, pblk, unwritten: false });
                r[2..4].copy_from_slice(&1u16.to_le_bytes());
                self.set_iblock(&r);
                return Ok(());
            }
        } else if depth == 1 {
            // the last leaf, when appending
            let n = le16(&root, 2) as usize;
            let o = 12 + 12 * (n - 1);
            let first = le32(&root, o);
            if lblk >= first {
                let child = le32(&root, o + 4) as u64 | ((le16(&root, o + 8) as u64) << 32);
                let buf = self.fs.bufs.get(child)?;
                let seed = self.seed();
                let done = buf.write(|b| {
                    let m = le16(b, 2) as usize;
                    let max = le16(b, 4) as usize;
                    if m > 0 {
                        let lo = 12 + 12 * (m - 1);
                        let raw = le16(b, lo + 4) as u32;
                        let start = le32(b, lo);
                        let p = ((le16(b, lo + 6) as u64) << 32) | le32(b, lo + 8) as u64;
                        if raw <= MAX_LEN && start + raw == lblk && p + raw as u64 == pblk && raw + len <= MAX_LEN {
                            b[lo + 4..lo + 6].copy_from_slice(&((raw + len) as u16).to_le_bytes());
                            self.fs.set_extent_csum(seed, b);
                            return true;
                        }
                        if lblk >= start + raw && m < max {
                            put_leaf(b, m, &Ext { lblk, len, pblk, unwritten: false });
                            b[2..4].copy_from_slice(&((m + 1) as u16).to_le_bytes());
                            self.fs.set_extent_csum(seed, b);
                            return true;
                        }
                    }
                    false
                });
                if done {
                    return Ok(());
                }
            }
        }
        // general case: rebuild with the new extent in place
        let (mut all, nodes) = self.extents_all()?;
        let pos = all.partition_point(|e| e.lblk < lblk);
        all.insert(pos, Ext { lblk, len, pblk, unwritten: false });
        // merge neighbours
        let mut merged: Vec<Ext> = Vec::with_capacity(all.len());
        for e in all {
            if let Some(l) = merged.last_mut() {
                if !l.unwritten && !e.unwritten && l.lblk + l.len == e.lblk && l.pblk + l.len as u64 == e.pblk && l.len + e.len <= MAX_LEN {
                    l.len += e.len;
                    continue;
                }
            }
            merged.push(e);
        }
        self.rebuild_tree(&merged, &nodes)
    }

    /// Removes every mapping at or after `from` (truncate); frees the data blocks.
    pub fn remove_extents_from(&self, from: u32) -> Result<()> {
        if self.flags() & EXT4_EXTENTS_FL == 0 {
            return Ok(());
        }
        let (all, nodes) = self.extents_all()?;
        if all.iter().all(|e| e.lblk + e.len <= from) {
            return Ok(());
        }
        let mut keep = Vec::new();
        for e in all {
            if e.lblk + e.len <= from {
                keep.push(e);
            } else if e.lblk >= from {
                self.fs.free_blocks(e.pblk, e.len)?;
                self.add_blocks(-(e.len as i64));
            } else {
                let k = from - e.lblk;
                self.fs.free_blocks(e.pblk + k as u64, e.len - k)?;
                self.add_blocks(-((e.len - k) as i64));
                keep.push(Ext { len: k, ..e });
            }
        }
        self.rebuild_tree(&keep, &nodes)
    }
}
