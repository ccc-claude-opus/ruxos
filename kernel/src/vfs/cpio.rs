//! Unpacks an initramfs (cpio "newc" archive, optionally gzip-compressed) into a directory.
use super::*;
use crate::task::Cred;

fn hex(b: &[u8]) -> Result<u64> {
    let s = core::str::from_utf8(b).map_err(|_| EINVAL)?;
    u64::from_str_radix(s, 16).map_err(|_| EINVAL)
}

pub fn unpack(data: &[u8], root: &PathRef) -> Result<usize> {
    let owned;
    let data = if data.starts_with(&[0x1f, 0x8b]) {
        owned = gunzip(data)?;
        &owned[..]
    } else {
        data
    };
    let cred = Cred::root();
    let lk = path::Lookup { root, cred: &cred, follow_last: false, beneath: false, no_symlinks: false };
    let mut off = 0;
    let mut count = 0;
    while off + 110 <= data.len() {
        let h = &data[off..off + 110];
        if &h[..6] != b"070701" && &h[..6] != b"070702" {
            return Err(EINVAL);
        }
        let f = |i: usize| hex(&h[6 + 8 * i..14 + 8 * i]);
        let mode = f(1)? as u32;
        let (uid, gid) = (f(2)? as u32, f(3)? as u32);
        let size = f(6)? as usize;
        let rdev = makedev(f(9)? as u32, f(10)? as u32);
        let namesize = f(11)? as usize;
        let name_start = off + 110;
        let name = &data[name_start..name_start + namesize.saturating_sub(1)];
        let data_start = (name_start + namesize + 3) & !3;
        let body = &data[data_start..(data_start + size).min(data.len())];
        off = (data_start + size + 3) & !3;
        if name == b"TRAILER!!!" {
            break;
        }
        let name = name.strip_prefix(b"./").unwrap_or(name);
        if name.is_empty() || name == b"." {
            continue;
        }
        let Ok((dir, last, _)) = lk.resolve_parent(root, name) else { continue };
        let dinode = dir.inode()?;
        let made = match mode & S_IFMT {
            S_IFLNK => dinode.symlink(&last, body, uid, gid),
            S_IFDIR => match dinode.lookup(&last) {
                Ok(i) => Ok(i),
                Err(_) => dinode.create(&last, mode, 0, uid, gid),
            },
            S_IFREG => {
                let _ = dinode.unlink(&last);
                dinode.create(&last, mode, 0, uid, gid).and_then(|i| i.write_at(0, body).map(|_| i))
            }
            _ => dinode.create(&last, mode, rdev, uid, gid),
        };
        if let Ok(i) = made {
            dir.d.set_child(&last, Some(i));
            count += 1;
        }
    }
    Ok(count)
}

fn gunzip(d: &[u8]) -> Result<Vec<u8>> {
    // RFC 1952 header
    if d.len() < 18 || d[2] != 8 {
        return Err(EINVAL);
    }
    let flg = d[3];
    let mut p = 10;
    if flg & 4 != 0 {
        let xlen = u16::from_le_bytes([d[p], d[p + 1]]) as usize;
        p += 2 + xlen;
    }
    for bit in [8u8, 16] {
        if flg & bit != 0 {
            while p < d.len() && d[p] != 0 {
                p += 1;
            }
            p += 1;
        }
    }
    if flg & 2 != 0 {
        p += 2;
    }
    let isize = u32::from_le_bytes(d[d.len() - 4..].try_into().unwrap()) as usize;
    miniz_oxide::inflate::decompress_to_vec_with_limit(&d[p..d.len() - 8], isize.max(1 << 20) * 2).map_err(|e| {
        println!("ruxos: gunzip: {:?} (header {} bytes, isize {})", e.status, p, isize);
        EINVAL
    })
}
