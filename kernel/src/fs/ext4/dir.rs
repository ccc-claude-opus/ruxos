//! Directory blocks: linear entries (ext4_dir_entry_2). Hashed (htree) directories keep their
//! entries in ordinary leaf blocks, so a linear walk sees every entry.
use super::{le16, le32};

pub struct Entry<'a> {
    pub ino: u32,
    pub name: &'a [u8],
    pub ftype: u8,
    /// offset of the entry in the block, and its record length
    pub off: usize,
    pub rec_len: usize,
}

/// Calls `f` for every live entry of a directory block; stops when it returns false.
pub fn walk_block(b: &[u8], mut f: impl FnMut(&Entry) -> bool) -> bool {
    let mut off = 0;
    while off + 8 <= b.len() {
        let ino = le32(b, off);
        let rec_len = le16(b, off + 4) as usize;
        let rec_len = if rec_len == 0 || rec_len == 65535 { b.len() - off } else { rec_len };
        if rec_len < 8 || off + rec_len > b.len() {
            return true;
        }
        let name_len = b[off + 6] as usize;
        if ino != 0 && name_len > 0 && off + 8 + name_len <= b.len() {
            let e = Entry { ino, name: &b[off + 8..off + 8 + name_len], ftype: b[off + 7], off, rec_len };
            if !f(&e) {
                return false;
            }
        }
        off += rec_len;
    }
    true
}

pub fn ftype_to_dt(t: u8) -> u8 {
    match t {
        1 => 8,  // regular
        2 => 4,  // directory
        3 => 2,  // char
        4 => 6,  // block
        5 => 1,  // fifo
        6 => 12, // socket
        7 => 10, // symlink
        _ => 0,
    }
}
