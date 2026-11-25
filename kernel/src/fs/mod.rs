//! File system types that can be mounted, and syncing them all.
pub mod bufcache;
pub mod cgroupfs;
pub mod crc32c;
pub mod ext4;
pub mod procfs;
pub mod sysfs;
use crate::errno::*;
use crate::vfs::FileSystem;
use alloc::sync::Arc;

/// "a=1,b,c=x" mount options.
fn options(data: &[u8]) -> alloc::vec::Vec<(&[u8], &[u8])> {
    data.split(|&c| c == b',')
        .filter(|o| !o.is_empty())
        .map(|o| match o.iter().position(|&c| c == b'=') {
            Some(i) => (&o[..i], &o[i + 1..]),
            None => (o, &b""[..]),
        })
        .collect()
}

fn num(v: &[u8], radix: u32) -> Result<u64> {
    u64::from_str_radix(core::str::from_utf8(v).map_err(|_| EINVAL)?, radix).map_err(|_| EINVAL)
}

/// A size option: bytes with k/m/g suffixes, or a percentage of memory.
fn size(v: &[u8]) -> Result<u64> {
    let total = crate::mm::frame::TOTAL_PAGES.load(core::sync::atomic::Ordering::Relaxed) as u64 * 4096;
    let (n, unit) = match v.last() {
        Some(b'k' | b'K') => (&v[..v.len() - 1], 1u64 << 10),
        Some(b'm' | b'M') => (&v[..v.len() - 1], 1 << 20),
        Some(b'g' | b'G') => (&v[..v.len() - 1], 1 << 30),
        Some(b'%') => return Ok(total / 100 * num(&v[..v.len() - 1], 10)?),
        _ => (v, 1),
    };
    num(n, 10)?.checked_mul(unit).ok_or(EINVAL)
}

/// tmpfs(5): size=, mode=, uid=, gid= (ids in the mounter's user namespace), nr_inodes= (accepted).
/// The root belongs to the mounter's fsuid/fsgid unless told otherwise.
fn tmpfs(data: &[u8]) -> Result<Arc<dyn FileSystem>> {
    let c = crate::task::current_proc().cred();
    let (mut mode, mut uid, mut gid, mut max) = (0o1777u32, c.fsuid, c.fsgid, u64::MAX);
    for (k, v) in options(data) {
        match k {
            b"size" => max = size(v)?,
            b"mode" => mode = num(v, 8)? as u32 & 0o7777,
            b"uid" => uid = c.user_ns.make_kuid(num(v, 10)? as u32).ok_or(EINVAL)?,
            b"gid" => gid = c.user_ns.make_kgid(num(v, 10)? as u32).ok_or(EINVAL)?,
            b"nr_inodes" | b"nr_blocks" | b"huge" | b"mpol" | b"inode64" | b"inode32" | b"noswap" => {}
            _ => return Err(EINVAL),
        }
    }
    if max == 0 {
        max = u64::MAX; // size=0: no limit
    }
    Ok(crate::vfs::tmpfs::TmpFs::with_options("tmpfs", mode, uid, gid, max))
}

/// devpts (newinstance): the ptmx node (ptmxmode=, default 0000) and the terminals' slaves
/// (mode=, default 0600; gid=, default the opener's), see pty.rs.
fn devpts(data: &[u8]) -> Result<Arc<dyn FileSystem>> {
    let c = crate::task::current_proc().cred();
    let (mut ptmxmode, mut mode, mut gid) = (0u32, 0o600u32, u32::MAX);
    for (k, v) in options(data) {
        match k {
            b"ptmxmode" => ptmxmode = num(v, 8)? as u32 & 0o777,
            b"mode" => mode = num(v, 8)? as u32 & 0o777,
            b"gid" => gid = c.user_ns.make_kgid(num(v, 10)? as u32).ok_or(EINVAL)?,
            b"newinstance" | b"uid" | b"max" => {}
            _ => return Err(EINVAL),
        }
    }
    // the root and ptmx belong to root of the mounter's user namespace
    let (ru, rg) = (c.user_ns.make_kuid(0).unwrap_or(0), c.user_ns.make_kgid(0).unwrap_or(0));
    let fs: Arc<dyn FileSystem> = crate::vfs::tmpfs::TmpFs::with_options("devpts", 0o755, ru, rg, u64::MAX);
    fs.root().create(b"ptmx", crate::vfs::S_IFCHR | ptmxmode, crate::vfs::makedev(5, 2), ru, rg)?;
    crate::pty::register(&fs, mode, gid);
    Ok(fs)
}

/// mount(2): a new instance of the file system `fstype` on `source`.
pub fn make_fs(fstype: &[u8], _source: &[u8], _data: &[u8], _ro: bool) -> Result<Arc<dyn FileSystem>> {
    match fstype {
        b"tmpfs" | b"ramfs" => tmpfs(_data),
        b"devpts" => devpts(_data),
        b"cgroup2" => {
            let root = crate::task::current_proc().ns.lock().cgroup.root.clone();
            Ok(cgroupfs::new(cgroupfs::lookup_path(&root).unwrap_or_else(crate::cgroup::root)))
        }
        b"proc" => Ok(procfs::new(crate::task::current_proc().pid_ns.clone())),
        b"sysfs" => Ok(sysfs::new()),
        b"devtmpfs" => Ok(crate::vfs::dev::DEVTMPFS.get().ok_or(ENODEV)?.clone()),
        b"ext4" | b"ext3" | b"ext2" => {
            let (dev, devno) = block_dev_by_path(_source)?;
            Ok(ext4::Ext4::mount(dev, devno, _ro)?)
        }
        _ => Err(ENODEV),
    }
}

/// Every mount of every mount namespace.
fn all_mounts() -> alloc::vec::Vec<Arc<crate::vfs::Mount>> {
    crate::vfs::MntNs::all_mounts()
}

pub fn is_mounted(fs: &Arc<dyn FileSystem>) -> bool {
    all_mounts().iter().any(|m| core::ptr::addr_eq(Arc::as_ptr(&m.root.fs), Arc::as_ptr(fs)))
}

pub fn sync_all() {
    for m in all_mounts() {
        let _ = m.root.fs.sync();
    }
}

/// Power off: every file system written out and marked clean.
pub fn unmount_all() {
    for m in all_mounts().iter().rev() {
        let _ = m.root.fs.unmount();
    }
}

/// The block device a path names (/dev/vda...).
fn block_dev_by_path(p: &[u8]) -> Result<(Arc<dyn crate::block::BlockDevice>, u64)> {
    let path = crate::syscall::fs::lookup_at(-100, p, true, false)?;
    let st = path.inode()?.stat()?;
    if st.mode & crate::vfs::S_IFMT != crate::vfs::S_IFBLK {
        return Err(ENOTBLK);
    }
    Ok((crate::vfs::dev::block_device(st.rdev).ok_or(ENXIO)?, st.rdev))
}

/// Mounts root= (a /dev/vdX name, ext4) as the root of the mount tree.
pub fn mount_root() -> Result<Arc<crate::vfs::Mount>> {
    let name = crate::kernel_param("root").ok_or(ENODEV)?;
    let dev = name.strip_prefix("/dev/").ok_or(ENODEV)?;
    let i = crate::vfs::FileSystem::root(&**crate::vfs::dev::DEVTMPFS).lookup(dev.as_bytes())?;
    let st = i.stat()?;
    let bdev = crate::vfs::dev::block_device(st.rdev).ok_or(ENXIO)?;
    let ro = crate::kernel_param("rw").is_none();
    let fs = ext4::Ext4::mount(bdev, st.rdev, ro)?;
    let flags = if ro { crate::vfs::path::MNT_RDONLY } else { 0 };
    if crate::kernel_param("quiet").is_none() {
        println!("ruxos: root {} (ext4{})", name, if fs.ro { ", read-only" } else { "" });
    }
    Ok(crate::vfs::Mount::new(crate::vfs::path::DEntry::new_root(fs), flags, name, "ext4", if ro { "ro" } else { "rw" }))
}
