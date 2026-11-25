//! ext4 (the on-disk format of Documentation/filesystems/ext4 in Linux): superblock, block groups,
//! inodes with extent trees, directories, and (later milestones) allocation and the jbd2 journal.
mod dir;
mod extent;
mod inode;

use super::bufcache::BufCache;
use crate::block::BlockDevice;
use crate::errno::*;
use crate::sync::SpinLock;
use crate::vfs::{FileSystem, Inode, StatFs};
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
pub use inode::Ext4Inode;

pub const MAGIC: u16 = 0xef53;
pub const ROOT_INO: u32 = 2;

// incompat
const INCOMPAT_FILETYPE: u32 = 0x2;
const INCOMPAT_RECOVER: u32 = 0x4;
const INCOMPAT_META_BG: u32 = 0x10;
const INCOMPAT_EXTENTS: u32 = 0x40;
const INCOMPAT_64BIT: u32 = 0x80;
const INCOMPAT_FLEX_BG: u32 = 0x200;
const INCOMPAT_CSUM_SEED: u32 = 0x2000;
const INCOMPAT_LARGEDIR: u32 = 0x4000;
const INCOMPAT_SUPPORTED: u32 =
    INCOMPAT_FILETYPE | INCOMPAT_RECOVER | INCOMPAT_META_BG | INCOMPAT_EXTENTS | INCOMPAT_64BIT | INCOMPAT_FLEX_BG | INCOMPAT_CSUM_SEED | INCOMPAT_LARGEDIR;
pub const RO_COMPAT_HUGE_FILE: u32 = 0x8;
pub const RO_COMPAT_METADATA_CSUM: u32 = 0x400;

pub fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
pub fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

#[derive(Clone, Copy, Default)]
pub struct Group {
    pub block_bitmap: u64,
    pub inode_bitmap: u64,
    pub inode_table: u64,
    pub free_blocks: u32,
    pub free_inodes: u32,
    pub used_dirs: u32,
    pub flags: u16,
}

pub struct Super {
    pub raw: Vec<u8>,
    pub inodes_count: u32,
    pub blocks_count: u64,
    pub free_blocks: u64,
    pub free_inodes: u32,
    pub first_data_block: u32,
    pub blocks_per_group: u32,
    pub inodes_per_group: u32,
    pub inode_size: usize,
    pub desc_size: usize,
    pub incompat: u32,
    pub ro_compat: u32,
    pub compat: u32,
    pub hash_seed: [u32; 4],
    pub def_hash_version: u8,
}

pub struct Ext4 {
    pub dev: Arc<dyn BlockDevice>,
    pub devno: u64,
    pub bs: usize,
    pub sb: SpinLock<Super>,
    pub groups: SpinLock<Vec<Group>>,
    pub bufs: BufCache,
    inodes: SpinLock<BTreeMap<u32, Weak<Ext4Inode>>>,
    pub ro: bool,
    pub me: Weak<Ext4>,
    pub csum_seed: u32,
}

impl Ext4 {
    pub fn mount(dev: Arc<dyn BlockDevice>, devno: u64, ro: bool) -> Result<Arc<Ext4>> {
        let mut raw = alloc::vec![0u8; 1024];
        dev.read(2, &mut raw)?;
        if le16(&raw, 56) != MAGIC {
            return Err(EINVAL);
        }
        let log_bs = le32(&raw, 24);
        if log_bs > 2 {
            return Err(EINVAL); // block sizes above 4 KB (the page size) are not supported
        }
        let bs = 1024usize << log_bs;
        let incompat = le32(&raw, 96);
        let ro_compat = le32(&raw, 100);
        if incompat & !INCOMPAT_SUPPORTED != 0 {
            println!("ruxos: ext4: unsupported incompatible features {:#x}", incompat & !INCOMPAT_SUPPORTED);
            return Err(EINVAL);
        }
        let is64 = incompat & INCOMPAT_64BIT != 0;
        let hi = |o: usize| if is64 { le32(&raw, o) as u64 } else { 0 };
        let blocks_count = le32(&raw, 4) as u64 | (hi(336) << 32);
        let free_blocks = le32(&raw, 12) as u64 | (hi(344) << 32);
        let rev = le32(&raw, 76);
        let inode_size = if rev == 0 { 128 } else { le16(&raw, 88) as usize };
        let desc_size = if is64 { le16(&raw, 254).max(32) as usize } else { 32 };
        let csum_seed = if incompat & INCOMPAT_CSUM_SEED != 0 { le32(&raw, 624) } else { crate::fs::crc32c::crc32c(!0, &raw[104..120]) };
        let sb = Super {
            inodes_count: le32(&raw, 0),
            blocks_count,
            free_blocks,
            free_inodes: le32(&raw, 16),
            first_data_block: le32(&raw, 20),
            blocks_per_group: le32(&raw, 32),
            inodes_per_group: le32(&raw, 40),
            inode_size,
            desc_size,
            incompat,
            ro_compat,
            compat: le32(&raw, 92),
            hash_seed: [le32(&raw, 236), le32(&raw, 240), le32(&raw, 244), le32(&raw, 248)],
            def_hash_version: raw[252],
            raw,
        };
        if sb.blocks_per_group == 0 || sb.inodes_per_group == 0 {
            return Err(EINVAL);
        }
        let ngroups = (sb.blocks_count - sb.first_data_block as u64).div_ceil(sb.blocks_per_group as u64) as usize;
        let mut ro = ro || dev.readonly();
        if incompat & INCOMPAT_RECOVER != 0 {
            println!("ruxos: ext4: the journal needs recovery; mounting read-only");
            ro = true;
        }
        let fs = Arc::new_cyclic(|me| Ext4 {
            dev: dev.clone(),
            devno,
            bs,
            sb: SpinLock::new(sb),
            groups: SpinLock::new(Vec::new()),
            bufs: BufCache::new(dev.clone(), bs),
            inodes: SpinLock::new(BTreeMap::new()),
            ro,
            me: me.clone(),
            csum_seed,
        });
        fs.read_groups(ngroups)?;
        Ok(fs)
    }

    fn gdt_block(&self, g: usize) -> u64 {
        let sb = self.sb.lock();
        let per_block = self.bs / sb.desc_size;
        let first = sb.first_data_block as u64 + 1;
        if sb.incompat & INCOMPAT_META_BG != 0 {
            let first_meta_bg = le32(&sb.raw, 260) as usize;
            let metagroup = g / per_block;
            if metagroup >= first_meta_bg {
                // the descriptor block is in the first group of its meta group (plus a backup)
                let grp = (metagroup * per_block) as u64;
                let has_sb = self.group_has_super(grp as usize, &sb);
                return sb.first_data_block as u64 + grp * sb.blocks_per_group as u64 + has_sb as u64;
            }
        }
        first + (g / per_block) as u64
    }

    fn group_has_super(&self, g: usize, sb: &Super) -> bool {
        if sb.ro_compat & 1 == 0 || g <= 1 {
            return true; // without sparse_super every group has a copy
        }
        let pow = |mut n: usize, b: usize| {
            while n % b == 0 && n > 1 {
                n /= b;
            }
            n == 1
        };
        pow(g, 3) || pow(g, 5) || pow(g, 7)
    }

    fn read_groups(&self, n: usize) -> Result<()> {
        let ds = self.sb.lock().desc_size;
        let per_block = self.bs / ds;
        let mut v = Vec::with_capacity(n);
        for g in 0..n {
            let b = self.bufs.get(self.gdt_block(g))?;
            let off = (g % per_block) * ds;
            let d = b.read(|d| d[off..off + ds].to_vec());
            let hi = |o: usize| if ds >= 64 { le32(&d, o) as u64 } else { 0 };
            let hi16 = |o: usize| if ds >= 64 { le16(&d, o) as u32 } else { 0 };
            v.push(Group {
                block_bitmap: le32(&d, 0) as u64 | (hi(32) << 32),
                inode_bitmap: le32(&d, 4) as u64 | (hi(36) << 32),
                inode_table: le32(&d, 8) as u64 | (hi(40) << 32),
                free_blocks: le16(&d, 12) as u32 | (hi16(44) << 16),
                free_inodes: le16(&d, 14) as u32 | (hi16(46) << 16),
                used_dirs: le16(&d, 16) as u32 | (hi16(48) << 16),
                flags: le16(&d, 18),
            });
        }
        *self.groups.lock() = v;
        Ok(())
    }

    /// (block, byte offset in it) of inode `ino` in the inode table.
    pub fn inode_location(&self, ino: u32) -> Result<(u64, usize)> {
        let (ipg, isz) = {
            let sb = self.sb.lock();
            if ino == 0 || ino > sb.inodes_count {
                return Err(EIO);
            }
            (sb.inodes_per_group, sb.inode_size)
        };
        let g = ((ino - 1) / ipg) as usize;
        let idx = ((ino - 1) % ipg) as usize;
        let table = self.groups.lock().get(g).ok_or(EIO)?.inode_table;
        let off = idx * isz;
        Ok((table + (off / self.bs) as u64, off % self.bs))
    }

    pub fn iget(&self, ino: u32) -> Result<Arc<Ext4Inode>> {
        if let Some(i) = self.inodes.lock().get(&ino).and_then(|w| w.upgrade()) {
            return Ok(i);
        }
        let i = Ext4Inode::load(self.me.upgrade().unwrap(), ino)?;
        let mut m = self.inodes.lock();
        if let Some(e) = m.get(&ino).and_then(|w| w.upgrade()) {
            return Ok(e);
        }
        m.insert(ino, Arc::downgrade(&i));
        if m.len() % 1024 == 0 {
            m.retain(|_, w| w.strong_count() > 0);
        }
        Ok(i)
    }

    pub fn block_sectors(&self, block: u64) -> u64 {
        block * (self.bs as u64 / 512)
    }
}

impl FileSystem for Ext4 {
    fn name(&self) -> &'static str {
        "ext4"
    }
    fn root(&self) -> Arc<dyn Inode> {
        self.iget(ROOT_INO).expect("ext4: cannot read the root inode")
    }
    fn dev(&self) -> u64 {
        self.devno
    }
    fn statfs(&self) -> StatFs {
        let sb = self.sb.lock();
        let reserved = le32(&sb.raw, 8) as u64;
        StatFs {
            fs_type: MAGIC as u64,
            bsize: self.bs as u64,
            blocks: sb.blocks_count,
            bfree: sb.free_blocks,
            bavail: sb.free_blocks.saturating_sub(reserved),
            files: sb.inodes_count as u64,
            ffree: sb.free_inodes as u64,
            namelen: 255,
            flags: 0,
        }
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}
