//! Hashed directories (htree / dir_index): the name hashes (legacy, half_md4, tea), finding the
//! leaf block for a name, and splitting a full leaf.
use super::inode::Ext4Inode;
use super::{le16, le32};
use crate::errno::*;
use alloc::vec::Vec;

fn str2hashbuf(msg: &[u8], len: usize, num: usize, unsigned: bool) -> Vec<u32> {
    let mut out = Vec::with_capacity(num);
    let mut pad = len as u32 | ((len as u32) << 8);
    pad |= pad << 16;
    let mut val = pad;
    let n = len.min(num * 4);
    let mut left = num as isize;
    for (i, &c) in msg.iter().take(n).enumerate() {
        let x = if unsigned { c as u32 } else { c as i8 as i32 as u32 };
        val = x.wrapping_add(val << 8);
        if i % 4 == 3 {
            out.push(val);
            val = pad;
            left -= 1;
        }
    }
    left -= 1;
    if left >= 0 {
        out.push(val);
    }
    while left > 0 {
        out.push(pad);
        left -= 1;
    }
    out.resize(num, pad);
    out
}

fn half_md4(buf: &mut [u32; 4], inp: &[u32]) {
    let f = |x: u32, y: u32, z: u32| z ^ (x & (y ^ z));
    let g = |x: u32, y: u32, z: u32| (x & y).wrapping_add((x ^ y) & z);
    let h = |x: u32, y: u32, z: u32| x ^ y ^ z;
    let (mut a, mut b, mut c, mut d) = (buf[0], buf[1], buf[2], buf[3]);
    macro_rules! r {
        ($fn:expr, $a:ident, $b:ident, $c:ident, $d:ident, $x:expr, $s:expr) => {
            $a = $a.wrapping_add($fn($b, $c, $d)).wrapping_add($x);
            $a = $a.rotate_left($s);
        };
    }
    const K2: u32 = 0o13240474631;
    const K3: u32 = 0o15666365641;
    r!(f, a, b, c, d, inp[0], 3);
    r!(f, d, a, b, c, inp[1], 7);
    r!(f, c, d, a, b, inp[2], 11);
    r!(f, b, c, d, a, inp[3], 19);
    r!(f, a, b, c, d, inp[4], 3);
    r!(f, d, a, b, c, inp[5], 7);
    r!(f, c, d, a, b, inp[6], 11);
    r!(f, b, c, d, a, inp[7], 19);
    r!(g, a, b, c, d, inp[1].wrapping_add(K2), 3);
    r!(g, d, a, b, c, inp[3].wrapping_add(K2), 5);
    r!(g, c, d, a, b, inp[5].wrapping_add(K2), 9);
    r!(g, b, c, d, a, inp[7].wrapping_add(K2), 13);
    r!(g, a, b, c, d, inp[0].wrapping_add(K2), 3);
    r!(g, d, a, b, c, inp[2].wrapping_add(K2), 5);
    r!(g, c, d, a, b, inp[4].wrapping_add(K2), 9);
    r!(g, b, c, d, a, inp[6].wrapping_add(K2), 13);
    r!(h, a, b, c, d, inp[3].wrapping_add(K3), 3);
    r!(h, d, a, b, c, inp[7].wrapping_add(K3), 9);
    r!(h, c, d, a, b, inp[2].wrapping_add(K3), 11);
    r!(h, b, c, d, a, inp[6].wrapping_add(K3), 15);
    r!(h, a, b, c, d, inp[1].wrapping_add(K3), 3);
    r!(h, d, a, b, c, inp[5].wrapping_add(K3), 9);
    r!(h, c, d, a, b, inp[0].wrapping_add(K3), 11);
    r!(h, b, c, d, a, inp[4].wrapping_add(K3), 15);
    buf[0] = buf[0].wrapping_add(a);
    buf[1] = buf[1].wrapping_add(b);
    buf[2] = buf[2].wrapping_add(c);
    buf[3] = buf[3].wrapping_add(d);
}

fn tea(buf: &mut [u32; 4], inp: &[u32]) {
    let (mut b0, mut b1) = (buf[0], buf[1]);
    let (a, b, c, d) = (inp[0], inp[1], inp[2], inp[3]);
    let mut sum: u32 = 0;
    for _ in 0..16 {
        sum = sum.wrapping_add(0x9e37_79b9);
        b0 = b0.wrapping_add(((b1 << 4).wrapping_add(a)) ^ (b1.wrapping_add(sum)) ^ ((b1 >> 5).wrapping_add(b)));
        b1 = b1.wrapping_add(((b0 << 4).wrapping_add(c)) ^ (b0.wrapping_add(sum)) ^ ((b0 >> 5).wrapping_add(d)));
    }
    buf[0] = buf[0].wrapping_add(b0);
    buf[1] = buf[1].wrapping_add(b1);
}

/// The major hash of a name (ext4fs_dirhash) for hash version `v` (0-2 signed, 3-5 unsigned).
pub fn hash(name: &[u8], v: u8, seed: [u32; 4]) -> u32 {
    let unsigned = v >= 3;
    let mut buf = if seed == [0; 4] { [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476] } else { seed };
    let h = match v % 3 {
        0 => {
            let (mut h0, mut h1): (u32, u32) = (0x12a3_fe2d, 0x37ab_e8f9);
            for &c in name {
                let x = if unsigned { c as u32 } else { c as i8 as i32 as u32 };
                let mut hh = h1.wrapping_add(h0 ^ x.wrapping_mul(7_152_373));
                if hh & 0x8000_0000 != 0 {
                    hh = hh.wrapping_sub(0x7fff_ffff);
                }
                h1 = h0;
                h0 = hh;
            }
            h0 << 1
        }
        1 => {
            let mut p = 0;
            let mut len = name.len() as isize;
            loop {
                let inp = str2hashbuf(&name[p..], len.max(0) as usize, 8, unsigned);
                half_md4(&mut buf, &inp);
                len -= 32;
                p += 32;
                if len <= 0 {
                    break;
                }
            }
            buf[1]
        }
        _ => {
            let mut p = 0;
            let mut len = name.len() as isize;
            loop {
                let inp = str2hashbuf(&name[p..], len.max(0) as usize, 4, unsigned);
                tea(&mut buf, &inp);
                len -= 16;
                p += 16;
                if len <= 0 {
                    break;
                }
            }
            buf[0]
        }
    };
    let h = h & !1;
    if h == 0x7fff_ffff << 1 { (0x7fff_ffff - 1) << 1 } else { h }
}

fn hash_params(dir: &Ext4Inode, root: &[u8]) -> (u8, [u32; 4]) {
    let sb = dir.fs.sb.lock();
    let mut v = root[0x1c];
    let flags = le32(&sb.raw, 0x160);
    if v <= 2 && flags & 0x2 != 0 {
        v += 3; // EXT2_FLAGS_UNSIGNED_HASH
    }
    (v, sb.hash_seed)
}

/// (lblk of the node, offset of its count/limit) along the path to the leaf for `h`, and the leaf.
fn path_for(dir: &Ext4Inode, h: u32) -> Result<(Vec<(u32, usize, usize)>, u32)> {
    let root_buf = dir.fs.bufs.get(dir.map(0)?.ok_or(EIO)?.pblk)?;
    let root = root_buf.read(|d| d.to_vec());
    let levels = root[0x1e] as usize;
    let mut path = Vec::new();
    let mut node = root;
    let mut lblk = 0u32;
    let mut off = 0x20usize;
    for level in 0..=levels {
        let count = le16(&node, off + 2) as usize;
        let mut pick = 0;
        for i in 1..count {
            if le32(&node, off + 8 * i) <= h {
                pick = i;
            } else {
                break;
            }
        }
        path.push((lblk, off, pick));
        let child = le32(&node, off + 8 * pick + 4) & 0x0fff_ffff;
        if level == levels {
            return Ok((path, child));
        }
        lblk = child;
        node = dir.fs.bufs.get(dir.map(child)?.ok_or(EIO)?.pblk)?.read(|d| d.to_vec());
        off = 8;
    }
    Err(EIO)
}

pub fn leaf_for(dir: &Ext4Inode, name: &[u8]) -> Result<u32> {
    let root = dir.fs.bufs.get(dir.map(0)?.ok_or(EIO)?.pblk)?.read(|d| d.to_vec());
    let (v, seed) = hash_params(dir, &root);
    Ok(path_for(dir, hash(name, v, seed))?.1)
}

/// The leaf for `name` is full: move the upper half (by hash) to a new block, index it in the
/// parent node, and add the entry.
pub fn split_and_add(dir: &Ext4Inode, name: &[u8], ino: u32, ftype: u8) -> Result<()> {
    let fs = &dir.fs;
    let bs = fs.bs;
    let tail = fs.dir_tail_len();
    let root = fs.bufs.get(dir.map(0)?.ok_or(EIO)?.pblk)?.read(|d| d.to_vec());
    let (v, seed) = hash_params(dir, &root);
    let h = hash(name, v, seed);
    let (path, leaf) = path_for(dir, h)?;
    let leaf_buf = fs.bufs.get(dir.map(leaf)?.ok_or(EIO)?.pblk)?;
    let mut entries: Vec<(u32, Vec<u8>, u32, u8)> = leaf_buf.read(|d| {
        let mut v2 = Vec::new();
        super::dir::walk_block(&d[..bs - tail], |e| {
            v2.push((hash(e.name, v, seed), e.name.to_vec(), e.ino, e.ftype));
            true
        });
        v2
    });
    entries.push((h, name.to_vec(), ino, ftype));
    entries.sort_by_key(|e| e.0);
    let mid = entries.len() / 2;
    let split_hash = entries[mid].0 | if entries[mid].0 == entries[mid - 1].0 { 1 } else { 0 };
    // the parent must have room for one more index entry
    let (plblk, poff, pick) = *path.last().unwrap();
    let pbuf = fs.bufs.get(dir.map(plblk)?.ok_or(EIO)?.pblk)?;
    let (limit, count) = pbuf.read(|d| (le16(d, poff) as usize, le16(d, poff + 2) as usize));
    if count >= limit {
        println!("ruxos: ext4: htree index node full in directory inode {} (splitting index nodes is not implemented)", dir.ino);
        return Err(ENOSPC);
    }
    let new_lblk = (dir.size() / bs as u64) as u32;
    let seed_i = dir.seed();
    let write_leaf = |d: &mut [u8], es: &[(u32, Vec<u8>, u32, u8)]| {
        d.fill(0);
        let mut off = 0;
        for (k, (_, n, i, t)) in es.iter().enumerate() {
            let rec = if k + 1 == es.len() { bs - tail - off } else { (8 + n.len() + 3) & !3 };
            d[off..off + 4].copy_from_slice(&i.to_le_bytes());
            d[off + 4..off + 6].copy_from_slice(&(rec as u16).to_le_bytes());
            d[off + 6] = n.len() as u8;
            d[off + 7] = *t;
            d[off + 8..off + 8 + n.len()].copy_from_slice(n);
            off += rec;
        }
        if tail > 0 {
            fs.set_dirent_tail(seed_i, d);
        }
    };
    leaf_buf.write(|d| write_leaf(d, &entries[..mid]));
    let pblk = fs.alloc_blocks(dir.map(leaf)?.map_or(0, |r| r.pblk), 1)?.0;
    dir.insert_extent(new_lblk, pblk, 1)?;
    dir.add_blocks(1);
    fs.bufs.get_new(pblk).write(|d| write_leaf(d, &entries[mid..]));
    dir.set_size((new_lblk as u64 + 1) * bs as u64);
    pbuf.write(|d| {
        let at = poff + 8 * (pick + 1);
        d.copy_within(at..poff + 8 * count, at + 8);
        d[at..at + 4].copy_from_slice(&split_hash.to_le_bytes());
        d[at + 4..at + 8].copy_from_slice(&new_lblk.to_le_bytes());
        d[poff + 2..poff + 4].copy_from_slice(&((count + 1) as u16).to_le_bytes());
        fs.set_dx_csum(seed_i, d, poff);
    });
    dir.touch(true, true);
    Ok(())
}
