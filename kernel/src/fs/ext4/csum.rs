//! metadata_csum checksums (crc32c), as fs/ext4 computes them.
use super::Ext4;
use crate::fs::crc32c::crc32c;

impl Ext4 {
    pub fn has_csum(&self) -> bool {
        self.sb.lock().ro_compat & super::RO_COMPAT_METADATA_CSUM != 0
    }

    /// The per-inode seed: crc32c(fs seed, inode number, generation).
    pub fn inode_seed(&self, ino: u32, generation: u32) -> u32 {
        let c = crc32c(self.csum_seed, &ino.to_le_bytes());
        crc32c(c, &generation.to_le_bytes())
    }

    pub fn set_sb_csum(&self, raw: &mut [u8]) {
        if self.has_csum() {
            let c = crc32c(!0, &raw[..1020]);
            raw[1020..1024].copy_from_slice(&c.to_le_bytes());
        }
    }

    pub fn group_desc_csum(&self, g: u32, desc: &[u8]) -> u16 {
        if !self.has_csum() {
            return 0;
        }
        let mut c = crc32c(self.csum_seed, &g.to_le_bytes());
        c = crc32c(c, &desc[..0x1e]);
        c = crc32c(c, &[0, 0]);
        if desc.len() > 0x20 {
            c = crc32c(c, &desc[0x20..]);
        }
        (c & 0xffff) as u16
    }

    pub fn bitmap_csum(&self, bitmap: &[u8]) -> u32 {
        crc32c(self.csum_seed, bitmap)
    }

    /// Sets i_checksum_lo/hi of a raw inode.
    pub fn set_inode_csum(&self, ino: u32, raw: &mut [u8]) {
        if !self.has_csum() {
            return;
        }
        let generation = u32::from_le_bytes(raw[100..104].try_into().unwrap());
        let seed = self.inode_seed(ino, generation);
        let isz = raw.len();
        let extra = if isz > 128 { u16::from_le_bytes([raw[128], raw[129]]) as usize } else { 0 };
        let has_hi = isz > 128 && 128 + extra >= 0x84;
        let mut c = crc32c(seed, &raw[..0x7c]);
        c = crc32c(c, &[0, 0]);
        c = crc32c(c, &raw[0x7e..128]);
        if isz > 128 {
            if has_hi {
                c = crc32c(c, &raw[128..0x82]);
                c = crc32c(c, &[0, 0]);
                c = crc32c(c, &raw[0x84..isz]);
            } else {
                c = crc32c(c, &raw[128..isz]);
            }
        }
        raw[0x7c..0x7e].copy_from_slice(&((c & 0xffff) as u16).to_le_bytes());
        if has_hi {
            raw[0x82..0x84].copy_from_slice(&((c >> 16) as u16).to_le_bytes());
        }
    }

    /// The tail checksum of an extent tree block (after eh_max entries).
    pub fn set_extent_csum(&self, seed: u32, block: &mut [u8]) {
        if !self.has_csum() {
            return;
        }
        let max = u16::from_le_bytes([block[4], block[5]]) as usize;
        let off = 12 + 12 * max;
        if off + 4 <= block.len() {
            let c = crc32c(seed, &block[..off]);
            block[off..off + 4].copy_from_slice(&c.to_le_bytes());
        }
    }

    /// The fake dirent at the end of a linear directory block.
    pub fn set_dirent_tail(&self, seed: u32, block: &mut [u8]) {
        if !self.has_csum() {
            return;
        }
        let n = block.len();
        block[n - 12..n - 8].fill(0);
        block[n - 8..n - 6].copy_from_slice(&12u16.to_le_bytes());
        block[n - 6] = 0;
        block[n - 5] = 0xde;
        let c = crc32c(seed, &block[..n - 12]);
        block[n - 4..].copy_from_slice(&c.to_le_bytes());
    }

    /// Bytes at the end of a directory block reserved for the checksum tail.
    pub fn dir_tail_len(&self) -> usize {
        if self.has_csum() { 12 } else { 0 }
    }

    /// The dx_tail of an htree node: `count_off` is where the (limit, count) header is.
    pub fn set_dx_csum(&self, seed: u32, block: &mut [u8], count_off: usize) {
        if !self.has_csum() {
            return;
        }
        let limit = u16::from_le_bytes([block[count_off], block[count_off + 1]]) as usize;
        let count = u16::from_le_bytes([block[count_off + 2], block[count_off + 3]]) as usize;
        let tail = count_off + 8 * limit;
        if tail + 8 > block.len() {
            return;
        }
        let mut c = crc32c(seed, &block[..count_off + 8 * count]);
        c = crc32c(c, &block[tail..tail + 4]);
        c = crc32c(c, &[0, 0, 0, 0]);
        block[tail + 4..tail + 8].copy_from_slice(&c.to_le_bytes());
    }
}
