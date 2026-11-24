//! A small reader for the flattened device tree (Devicetree Specification v0.4, chapter 5).
//! No allocation: nodes are visited in order; each node knows its parent's #address-cells and
//! #size-cells so that `reg` can be decoded.

const MAGIC: u32 = 0xd00d_feed;
const BEGIN_NODE: u32 = 1;
const END_NODE: u32 = 2;
const PROP: u32 = 3;
const NOP: u32 = 4;
const END: u32 = 9;

#[derive(Clone, Copy)]
pub struct Fdt<'a> {
    data: &'a [u8],
    structs: usize,
    strings: usize,
}

fn be32(d: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([d[off], d[off + 1], d[off + 2], d[off + 3]])
}

fn align4(x: usize) -> usize {
    (x + 3) & !3
}

fn cstr(d: &[u8], off: usize) -> &str {
    let end = d[off..].iter().position(|&b| b == 0).map_or(d.len(), |p| off + p);
    core::str::from_utf8(&d[off..end]).unwrap_or("")
}

#[derive(Clone, Copy)]
pub struct Node<'a> {
    fdt: Fdt<'a>,
    pub name: &'a str,
    pub depth: usize,
    props: usize,
    /// The parent's #address-cells and #size-cells (they describe this node's `reg`).
    pub addr_cells: u32,
    pub size_cells: u32,
}

impl<'a> Fdt<'a> {
    /// # Safety
    /// `p` must point to a device tree blob that stays mapped and unchanged.
    pub unsafe fn from_ptr(p: *const u8) -> Option<Fdt<'static>> {
        let hdr = unsafe { core::slice::from_raw_parts(p, 40) };
        if be32(hdr, 0) != MAGIC {
            return None;
        }
        let total = be32(hdr, 4) as usize;
        let data = unsafe { core::slice::from_raw_parts(p, total) };
        Some(Fdt { data, structs: be32(hdr, 8) as usize, strings: be32(hdr, 12) as usize })
    }

    pub fn total_size(&self) -> usize {
        self.data.len()
    }

    /// Visits every node in document order.
    pub fn for_each_node(&self, mut f: impl FnMut(&Node<'a>)) {
        let d = self.data;
        let mut off = self.structs;
        // cells[depth] = (#address-cells, #size-cells) declared by the node at that depth
        let mut cells = [(2u32, 1u32); 32];
        let mut depth = 0usize;
        loop {
            let tok = be32(d, off);
            off += 4;
            match tok {
                BEGIN_NODE => {
                    let name = cstr(d, off);
                    off = align4(off + name.len() + 1);
                    let (pa, ps) = if depth == 0 { (2, 1) } else { cells[depth - 1] };
                    let node = Node { fdt: *self, name, depth, props: off, addr_cells: pa, size_cells: ps };
                    let ac = node.prop_u32("#address-cells").unwrap_or(2);
                    let sc = node.prop_u32("#size-cells").unwrap_or(1);
                    if depth < cells.len() {
                        cells[depth] = (ac, sc);
                    }
                    f(&node);
                    depth += 1;
                }
                END_NODE => depth = depth.saturating_sub(1),
                PROP => {
                    let len = be32(d, off) as usize;
                    off = align4(off + 8 + len);
                }
                NOP => {}
                END | _ => return,
            }
        }
    }

    pub fn find(&self, pred: impl Fn(&Node<'a>) -> bool) -> Option<Node<'a>> {
        let mut found = None;
        self.for_each_node(|n| {
            if found.is_none() && pred(n) {
                found = Some(*n);
            }
        });
        found
    }

    pub fn find_compatible(&self, c: &str) -> Option<Node<'a>> {
        self.find(|n| n.compatible(c))
    }

    pub fn chosen(&self) -> Option<Node<'a>> {
        self.find(|n| n.depth == 1 && n.name == "chosen")
    }

    pub fn chosen_bootargs(&self) -> Option<&'a str> {
        self.chosen()?.prop_str("bootargs")
    }

    /// The initrd's physical range, from /chosen.
    pub fn initrd(&self) -> Option<(usize, usize)> {
        let c = self.chosen()?;
        let num = |p: &[u8]| -> usize {
            if p.len() >= 8 { u64::from_be_bytes(p[..8].try_into().unwrap()) as usize } else { be32(p, 0) as usize }
        };
        Some((num(c.prop("linux,initrd-start")?), num(c.prop("linux,initrd-end")?)))
    }

    /// Calls `f(base, size)` for every range of every memory node.
    pub fn memory(&self, mut f: impl FnMut(usize, usize)) {
        self.for_each_node(|n| {
            if n.prop_str("device_type") == Some("memory") {
                let mut i = 0;
                while let Some((b, s)) = n.reg(i) {
                    f(b as usize, s as usize);
                    i += 1;
                }
            }
        });
    }

    /// The node with this phandle.
    pub fn by_phandle(&self, ph: u32) -> Option<Node<'a>> {
        self.find(|n| n.prop_u32("phandle") == Some(ph))
    }
}

impl<'a> Node<'a> {
    pub fn prop(&self, name: &str) -> Option<&'a [u8]> {
        let d = self.fdt.data;
        let mut off = self.props;
        loop {
            let tok = be32(d, off);
            match tok {
                PROP => {
                    let len = be32(d, off + 4) as usize;
                    let nameoff = be32(d, off + 8) as usize;
                    let val = &d[off + 12..off + 12 + len];
                    if cstr(d, self.fdt.strings + nameoff) == name {
                        return Some(val);
                    }
                    off = align4(off + 12 + len);
                }
                NOP => off += 4,
                _ => return None,
            }
        }
    }

    pub fn prop_str(&self, name: &str) -> Option<&'a str> {
        let p = self.prop(name)?;
        let end = p.iter().position(|&b| b == 0).unwrap_or(p.len());
        core::str::from_utf8(&p[..end]).ok()
    }

    pub fn prop_u32(&self, name: &str) -> Option<u32> {
        let p = self.prop(name)?;
        (p.len() >= 4).then(|| be32(p, 0))
    }

    /// The i-th 32-bit cell of a property.
    pub fn cell(&self, name: &str, i: usize) -> Option<u32> {
        let p = self.prop(name)?;
        (p.len() >= 4 * (i + 1)).then(|| be32(p, 4 * i))
    }

    /// Whether one of the strings in `compatible` is `c`.
    pub fn compatible(&self, c: &str) -> bool {
        match self.prop("compatible") {
            Some(p) => p.split(|&b| b == 0).any(|s| s == c.as_bytes()),
            None => false,
        }
    }

    /// The i-th (address, size) pair of `reg`.
    pub fn reg(&self, i: usize) -> Option<(u64, u64)> {
        let p = self.prop("reg")?;
        let (ac, sc) = (self.addr_cells as usize, self.size_cells as usize);
        let stride = 4 * (ac + sc);
        if stride == 0 || p.len() < stride * (i + 1) {
            return None;
        }
        let read = |off: usize, n: usize| (0..n).fold(0u64, |v, k| (v << 32) | be32(p, off + 4 * k) as u64);
        Some((read(stride * i, ac), read(stride * i + 4 * ac, sc)))
    }
}
