//! The jbd2 journal (Documentation/filesystems/ext4/journal.rst): recovery at mount, and
//! committing metadata transactions (write-ahead: journal, flush, mark, checkpoint in place,
//! flush, mark clean). Big-endian on disk; checksum v3 when the journal has it.
use super::Ext4;
use crate::errno::*;
use crate::fs::crc32c::crc32c;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

const MAGIC: u32 = 0xc03b_3998;
const DESCRIPTOR: u32 = 1;
const COMMIT: u32 = 2;
const SB_V2: u32 = 4;
const REVOKE: u32 = 5;
const INCOMPAT_REVOKE: u32 = 0x1;
const INCOMPAT_64BIT: u32 = 0x2;
const INCOMPAT_CSUM_V2: u32 = 0x8;
const INCOMPAT_CSUM_V3: u32 = 0x10;
const TAG_ESCAPE: u32 = 1;
const TAG_SAME_UUID: u32 = 2;
const TAG_LAST: u32 = 8;

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}
fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_be_bytes());
}

pub struct Journal {
    /// physical block of each journal block
    map: Vec<u64>,
    pub jsb: Vec<u8>,
    first: u32,
    maxlen: u32,
    incompat: u32,
    seed: u32,
    pub seq: u32,
}

impl Journal {
    pub fn load(fs: &Ext4) -> Result<Option<Journal>> {
        let (compat, jino) = {
            let sb = fs.sb.lock();
            (sb.compat, super::le32(&sb.raw, 224))
        };
        if compat & 0x4 == 0 || jino == 0 {
            return Ok(None);
        }
        let ji = fs.iget(jino)?;
        let nblocks = (ji.size() / fs.bs as u64) as u32;
        let mut map = Vec::with_capacity(nblocks as usize);
        let mut l = 0;
        while l < nblocks {
            let r = ji.map(l)?.ok_or(EIO)?;
            for k in 0..r.len.min(nblocks - l) {
                map.push(r.pblk + k as u64);
            }
            l += r.len.min(nblocks - l).max(1);
        }
        let b = fs.bufs.get(map[0])?;
        let jsb = b.read(|d| d.to_vec());
        if be32(&jsb, 0) != MAGIC || be32(&jsb, 4) != SB_V2 {
            return Err(EINVAL);
        }
        let incompat = be32(&jsb, 0x28);
        let seed = crc32c(!0, &jsb[0x30..0x40]);
        Ok(Some(Journal { map, first: be32(&jsb, 0x14), maxlen: be32(&jsb, 0x10).min(nblocks), incompat, seed, seq: be32(&jsb, 0x18), jsb }))
    }

    fn csum_v3(&self) -> bool {
        self.incompat & (INCOMPAT_CSUM_V3 | INCOMPAT_CSUM_V2) != 0
    }
    fn tag_size(&self) -> usize {
        if self.incompat & INCOMPAT_CSUM_V3 != 0 {
            16
        } else {
            let base = 8 + if self.incompat & INCOMPAT_CSUM_V2 != 0 { 2 } else { 0 };
            base + if self.incompat & INCOMPAT_64BIT != 0 { 4 } else { 0 }
        }
    }

    fn read(&self, fs: &Ext4, j: u32) -> Result<Vec<u8>> {
        let mut v = alloc::vec![0u8; fs.bs];
        fs.dev.read(fs.block_sectors(self.map[j as usize]), &mut v)?;
        Ok(v)
    }
    fn write(&self, fs: &Ext4, j: u32, data: &[u8]) -> Result<()> {
        fs.dev.write(fs.block_sectors(self.map[j as usize]), data)
    }

    fn write_sb(&mut self, fs: &Ext4, start: u32, seq: u32) -> Result<()> {
        put32(&mut self.jsb, 0x18, seq);
        put32(&mut self.jsb, 0x1c, start);
        if self.csum_v3() {
            put32(&mut self.jsb, 0xfc, 0);
            let c = crc32c(!0, &self.jsb[..1024]);
            put32(&mut self.jsb, 0xfc, c);
        }
        let mut blk = alloc::vec![0u8; fs.bs];
        blk[..self.jsb.len().min(fs.bs)].copy_from_slice(&self.jsb[..self.jsb.len().min(fs.bs)]);
        self.write(fs, 0, &blk)?;
        // keep the buffer cache in step
        fs.bufs.forget(self.map[0]);
        Ok(())
    }

    fn next(&self, j: u32) -> u32 {
        if j + 1 >= self.maxlen { self.first } else { j + 1 }
    }

    /// Replays committed transactions left in the journal (after a crash).
    pub fn recover(&mut self, fs: &Ext4) -> Result<usize> {
        let start = be32(&self.jsb, 0x1c);
        if start == 0 {
            return Ok(0);
        }
        let ts = self.tag_size();
        let mut seq = be32(&self.jsb, 0x18);
        let mut j = start;
        // pass 1: find committed transactions and revokes
        let mut txns: Vec<(u32, Vec<(u64, u32, bool)>)> = Vec::new();
        let mut revoked: BTreeMap<u64, u32> = BTreeMap::new();
        let mut cur: Vec<(u64, u32, bool)> = Vec::new();
        for _ in 0..self.maxlen {
            let b = self.read(fs, j)?;
            if be32(&b, 0) != MAGIC || be32(&b, 8) != seq {
                break;
            }
            match be32(&b, 4) {
                DESCRIPTOR => {
                    let mut off = 12;
                    let end = fs.bs - if self.csum_v3() { 4 } else { 0 };
                    loop {
                        if off + ts > end {
                            break;
                        }
                        let lo = be32(&b, off) as u64;
                        let flags = if ts == 16 { be32(&b, off + 4) } else { u16::from_be_bytes([b[off + 6], b[off + 7]]) as u32 };
                        let hi = if self.incompat & INCOMPAT_64BIT != 0 { be32(&b, off + if ts == 16 { 8 } else { 8 }) as u64 } else { 0 };
                        j = self.next(j);
                        cur.push((lo | (hi << 32), j, flags & TAG_ESCAPE != 0));
                        off += ts;
                        if flags & TAG_SAME_UUID == 0 {
                            off += 16;
                        }
                        if flags & TAG_LAST != 0 {
                            break;
                        }
                    }
                }
                COMMIT => {
                    txns.push((seq, core::mem::take(&mut cur)));
                    seq = seq.wrapping_add(1);
                }
                REVOKE => {
                    let count = be32(&b, 12) as usize;
                    let w = if self.incompat & INCOMPAT_64BIT != 0 { 8 } else { 4 };
                    let mut off = 16;
                    while off + w <= count.min(fs.bs) {
                        let blk = if w == 8 { u64::from_be_bytes(b[off..off + 8].try_into().unwrap()) } else { be32(&b, off) as u64 };
                        let e = revoked.entry(blk).or_insert(seq);
                        *e = (*e).max(seq);
                        off += w;
                    }
                }
                _ => break,
            }
            j = self.next(j);
        }
        // pass 2: replay, oldest first
        let mut n = 0;
        for (tid, blocks) in &txns {
            for &(target, jb, escaped) in blocks {
                if revoked.get(&target).is_some_and(|&r| r >= *tid) {
                    continue;
                }
                let mut data = self.read(fs, jb)?;
                if escaped {
                    put32(&mut data, 0, MAGIC);
                }
                fs.dev.write(fs.block_sectors(target), &data)?;
                fs.bufs.forget(target);
                n += 1;
            }
        }
        fs.dev.flush()?;
        self.seq = seq;
        self.write_sb(fs, 0, seq)?;
        fs.dev.flush()?;
        Ok(n)
    }

    /// Writes `blocks` (block number, contents) as one or more transactions and checkpoints them.
    pub fn commit(&mut self, fs: &Ext4, blocks: &[(u64, Vec<u8>)]) -> Result<()> {
        if blocks.is_empty() {
            return Ok(());
        }
        let ts = self.tag_size();
        let tail = if self.csum_v3() { 4 } else { 0 };
        let per_desc = (fs.bs - 12 - tail - 16) / ts;
        // leave room: descriptors + data + commit
        let room = (self.maxlen - self.first) as usize;
        let max_per_txn = (room - 2) * per_desc / (per_desc + 1);
        for chunk in blocks.chunks(max_per_txn.max(1)) {
            self.commit_one(fs, chunk, per_desc, ts, tail)?;
        }
        Ok(())
    }

    fn commit_one(&mut self, fs: &Ext4, blocks: &[(u64, Vec<u8>)], per_desc: usize, ts: usize, tail: usize) -> Result<()> {
        let seq = self.seq;
        let mut j = self.first;
        for group in blocks.chunks(per_desc) {
            let mut d = alloc::vec![0u8; fs.bs];
            put32(&mut d, 0, MAGIC);
            put32(&mut d, 4, DESCRIPTOR);
            put32(&mut d, 8, seq);
            let mut off = 12;
            let desc_j = j;
            j += 1;
            for (k, (blk, data)) in group.iter().enumerate() {
                let mut flags = if k > 0 { TAG_SAME_UUID } else { 0 };
                if k + 1 == group.len() {
                    flags |= TAG_LAST;
                }
                let mut out = data.clone();
                if be32(&out, 0) == MAGIC {
                    flags |= TAG_ESCAPE;
                    out[..4].fill(0);
                }
                put32(&mut d, off, *blk as u32);
                if ts == 16 {
                    put32(&mut d, off + 4, flags);
                    put32(&mut d, off + 8, (*blk >> 32) as u32);
                    let c = crc32c(crc32c(self.seed, &seq.to_be_bytes()), data);
                    put32(&mut d, off + 12, c);
                } else {
                    d[off + 6..off + 8].copy_from_slice(&(flags as u16).to_be_bytes());
                    if self.incompat & INCOMPAT_64BIT != 0 {
                        put32(&mut d, off + 8, (*blk >> 32) as u32);
                    }
                }
                off += ts;
                if k == 0 {
                    d[off..off + 16].copy_from_slice(&self.jsb[0x30..0x40]);
                    off += 16;
                }
                self.write(fs, j, &out)?;
                j += 1;
            }
            if tail > 0 {
                let n = fs.bs;
                put32(&mut d, n - 4, 0);
                let c = crc32c(self.seed, &d);
                put32(&mut d, n - 4, c);
            }
            self.write(fs, desc_j, &d)?;
        }
        // commit block
        let mut c = alloc::vec![0u8; fs.bs];
        put32(&mut c, 0, MAGIC);
        put32(&mut c, 4, COMMIT);
        put32(&mut c, 8, seq);
        let now = crate::time::realtime_ns();
        c[0x30..0x38].copy_from_slice(&((now / 1_000_000_000) as u64).to_be_bytes());
        put32(&mut c, 0x38, (now % 1_000_000_000) as u32);
        if self.csum_v3() {
            c[12] = 4; // JBD2_CRC32C_CHKSUM
            c[13] = 4;
            let s = crc32c(self.seed, &c);
            put32(&mut c, 16, s);
        }
        fs.dev.flush()?;
        self.write(fs, j, &c)?;
        // mark the journal as holding this transaction, then checkpoint in place
        self.write_sb(fs, self.first, seq)?;
        fs.dev.flush()?;
        for (blk, data) in blocks {
            fs.dev.write(fs.block_sectors(*blk), data)?;
        }
        fs.dev.flush()?;
        self.seq = seq.wrapping_add(1);
        self.write_sb(fs, 0, self.seq)?;
        let _ = INCOMPAT_REVOKE;
        Ok(())
    }
}
