//! Logical to physical block mapping: extent trees (ext4) and the old indirect block maps (ext2/3).
use super::{le16, le32, Ext4};
use crate::errno::*;

pub const EXTENT_MAGIC: u16 = 0xf30a;

/// A mapped run: physical start, number of blocks from the asked one to the end of the extent,
/// and whether it is unwritten (reads as zeros).
#[derive(Clone, Copy, Debug)]
pub struct Run {
    pub pblk: u64,
    pub len: u32,
    pub unwritten: bool,
}

/// Maps logical block `lblk` through the extent tree whose root is `root` (i_block, 60 bytes).
pub fn map_extent(fs: &Ext4, root: &[u8], lblk: u32) -> Result<Option<Run>> {
    let mut node: alloc::vec::Vec<u8> = root.to_vec();
    for _ in 0..8 {
        if le16(&node, 0) != EXTENT_MAGIC {
            return Err(EIO);
        }
        let entries = le16(&node, 2) as usize;
        let depth = le16(&node, 6);
        if 12 + entries * 12 > node.len() {
            return Err(EIO);
        }
        // the last entry starting at or before lblk
        let mut lo = 0usize;
        let mut hi = entries;
        while lo < hi {
            let mid = (lo + hi) / 2;
            if le32(&node, 12 + mid * 12) <= lblk {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == 0 {
            return Ok(None);
        }
        let e = 12 + (lo - 1) * 12;
        if depth == 0 {
            let start = le32(&node, e);
            let raw_len = le16(&node, e + 4) as u32;
            let (len, unwritten) = if raw_len > 32768 { (raw_len - 32768, true) } else { (raw_len, false) };
            if lblk >= start + len {
                return Ok(None);
            }
            let pstart = ((le16(&node, e + 6) as u64) << 32) | le32(&node, e + 8) as u64;
            let off = lblk - start;
            return Ok(Some(Run { pblk: pstart + off as u64, len: len - off, unwritten }));
        }
        let child = le32(&node, e + 4) as u64 | ((le16(&node, e + 8) as u64) << 32);
        let b = fs.bufs.get(child)?;
        node = b.read(|d| d.to_vec());
    }
    Err(EIO)
}

/// The old scheme: 12 direct blocks, then single, double and triple indirect blocks.
pub fn map_indirect(fs: &Ext4, root: &[u8], lblk: u32) -> Result<Option<Run>> {
    let per = (fs.bs / 4) as u32;
    let direct = |i: usize| le32(root, 4 * i) as u64;
    let lookup = |blk: u64, idx: u32| -> Result<u64> {
        if blk == 0 {
            return Ok(0);
        }
        let b = fs.bufs.get(blk)?;
        Ok(b.read(|d| le32(d, 4 * idx as usize)) as u64)
    };
    let mut l = lblk;
    let p = if l < 12 {
        direct(l as usize)
    } else {
        l -= 12;
        if l < per {
            lookup(direct(12), l)?
        } else {
            l -= per;
            if l < per * per {
                lookup(lookup(direct(13), l / per)?, l % per)?
            } else {
                l -= per * per;
                lookup(lookup(lookup(direct(14), l / (per * per))?, (l / per) % per)?, l % per)?
            }
        }
    };
    Ok(if p == 0 { None } else { Some(Run { pblk: p, len: 1, unwritten: false }) })
}
