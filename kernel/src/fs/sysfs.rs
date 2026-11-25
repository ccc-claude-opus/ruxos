//! /sys: what ruwebos's programs read — CPUs, the console tty, network interfaces (registered by
//! the network driver), block devices, transparent huge pages, and the cgroup mount point.
use crate::vfs::pseudo::{self, dir, file, ino_of, link, text, Lister};
use crate::vfs::*;
use crate::sync::SpinLock;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

/// A network interface for /sys/class/net/<name>: (name, MAC, MTU, driver, ifindex).
pub type NetIf = (String, [u8; 6], u32, &'static str, u32);
pub static NET_IFS: SpinLock<Vec<NetIf>> = SpinLock::new(Vec::new());

pub fn new() -> Arc<dyn FileSystem> {
    let fs = Arc::new(pseudo::PseudoFs { name: "sysfs", dev: tmpfs::anon_dev(), magic: 0x6265_6572, root: SpinLock::new(None) });
    let dev = fs.dev;
    *fs.root.lock() = Some(dir(dev, 1, 0o555, root(dev)));
    fs
}

fn e(n: &str, i: Arc<dyn Inode>) -> (Vec<u8>, Arc<dyn Inode>) {
    (n.as_bytes().to_vec(), i)
}

fn val(dev: u64, path: &str, v: String) -> Arc<dyn Inode> {
    file(dev, ino_of(&[path.as_bytes()]), 0o444, alloc::boxed::Box::new(move || text(v.clone())), None)
}

fn d(dev: u64, path: &'static str, l: Lister) -> Arc<dyn Inode> {
    dir(dev, ino_of(&[path.as_bytes()]), 0o755, l)
}

fn root(dev: u64) -> Lister {
    alloc::boxed::Box::new(move || {
        alloc::vec![
            e("devices", d(dev, "devices", alloc::boxed::Box::new(move || alloc::vec![
                e("system", d(dev, "devices/system", alloc::boxed::Box::new(move || alloc::vec![
                    e("cpu", d(dev, "devices/system/cpu", alloc::boxed::Box::new(move || {
                        let n = crate::percpu::ncpus();
                        let range = if n > 1 { format!("0-{}\n", n - 1) } else { String::from("0\n") };
                        let mut v = alloc::vec![
                            e("online", val(dev, "cpu/online", range.clone())),
                            e("possible", val(dev, "cpu/possible", range.clone())),
                            e("present", val(dev, "cpu/present", range)),
                        ];
                        for c in 0..n {
                            v.push((format!("cpu{}", c).into_bytes(), dir(dev, ino_of(&[b"cpu", format!("{}", c).as_bytes()]), 0o755, alloc::boxed::Box::new(Vec::new))));
                        }
                        v
                    }))),
                ]))),
                e("virtual", d(dev, "devices/virtual", alloc::boxed::Box::new(Vec::new))),
            ]))),
            e("class", d(dev, "class", alloc::boxed::Box::new(move || alloc::vec![
                e("tty", d(dev, "class/tty", alloc::boxed::Box::new(move || alloc::vec![
                    e("console", d(dev, "class/tty/console", alloc::boxed::Box::new(move || {
                        let (_, pl011) = crate::console::uart();
                        alloc::vec![e("active", val(dev, "tty/console/active", String::from(if pl011 { "ttyAMA0\n" } else { "ttyS0\n" })))]
                    }))),
                ]))),
                e("net", d(dev, "class/net", alloc::boxed::Box::new(move || {
                    NET_IFS.lock().iter().map(|i| (i.0.as_bytes().to_vec(), net_if(dev, i.clone()))).collect()
                }))),
            ]))),
            e("fs", d(dev, "fs", alloc::boxed::Box::new(move || alloc::vec![
                e("cgroup", d(dev, "fs/cgroup", alloc::boxed::Box::new(Vec::new))),
            ]))),
            e("kernel", d(dev, "kernel", alloc::boxed::Box::new(move || alloc::vec![
                e("mm", d(dev, "kernel/mm", alloc::boxed::Box::new(move || alloc::vec![
                    e("transparent_hugepage", d(dev, "kernel/mm/thp", alloc::boxed::Box::new(move || alloc::vec![
                        e("enabled", val(dev, "thp/enabled", String::from("always madvise [never]\n"))),
                    ]))),
                ]))),
            ]))),
            e("block", d(dev, "block", alloc::boxed::Box::new(Vec::new))),
            e("bus", d(dev, "bus", alloc::boxed::Box::new(Vec::new))),
        ]
    })
}

fn net_if(dev: u64, i: NetIf) -> Arc<dyn Inode> {
    let (name, mac, mtu, driver, idx) = i;
    let path = format!("net/{}", name);
    dir(dev, ino_of(&[path.as_bytes()]), 0o755, alloc::boxed::Box::new(move || {
        let m = format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}\n", mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);
        let drv = driver;
        let base = path.clone();
        let mut v = alloc::vec![
            e("address", val(dev, &format!("{}/address", base), m)),
            e("mtu", val(dev, &format!("{}/mtu", base), format!("{}\n", mtu))),
            e("ifindex", val(dev, &format!("{}/ifindex", base), format!("{}\n", idx))),
            e("type", val(dev, &format!("{}/type", base), String::from(if mac == [0; 6] { "772\n" } else { "1\n" }))),
            e("operstate", val(dev, &format!("{}/operstate", base), String::from(if mac == [0; 6] { "unknown\n" } else { "up\n" }))),
        ];
        if !drv.is_empty() {
            let dpath = format!("{}/device", base);
            v.push(e("device", dir(dev, ino_of(&[dpath.as_bytes()]), 0o755, alloc::boxed::Box::new(move || {
                alloc::vec![e("driver", link(dev, ino_of(&[b"driver", drv.as_bytes()]), alloc::boxed::Box::new(move || text(format!("../../../bus/virtio/drivers/{}", drv)))))]
            }))));
        }
        v
    }))
}
