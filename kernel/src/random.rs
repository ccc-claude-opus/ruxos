//! The kernel's random numbers: a ChaCha20 generator (RFC 8439 block function) keyed from the
//! entropy sources (virtio-rng, the device tree's rng-seed, timer jitter) and rekeyed after every
//! request (fast key erasure).
use crate::sync::SpinLock;

struct Rng {
    key: [u32; 8],
    counter: u64,
    seeded: bool,
}

static RNG: SpinLock<Rng> = SpinLock::new(Rng { key: [0; 8], counter: 0, seeded: false });

fn quarter(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(7);
}

fn block(key: &[u32; 8], counter: u64, out: &mut [u8; 64]) {
    let mut s = [0u32; 16];
    s[..4].copy_from_slice(&[0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574]);
    s[4..12].copy_from_slice(key);
    s[12] = counter as u32;
    s[13] = (counter >> 32) as u32;
    let init = s;
    for _ in 0..10 {
        quarter(&mut s, 0, 4, 8, 12);
        quarter(&mut s, 1, 5, 9, 13);
        quarter(&mut s, 2, 6, 10, 14);
        quarter(&mut s, 3, 7, 11, 15);
        quarter(&mut s, 0, 5, 10, 15);
        quarter(&mut s, 1, 6, 11, 12);
        quarter(&mut s, 2, 7, 8, 13);
        quarter(&mut s, 3, 4, 9, 14);
    }
    for i in 0..16 {
        out[4 * i..4 * i + 4].copy_from_slice(&s[i].wrapping_add(init[i]).to_le_bytes());
    }
}

/// Mixes bytes into the key (hash-like: the new key is a ChaCha20 block of the old key with the
/// input folded in).
pub fn mix(data: &[u8]) {
    let mut r = RNG.lock();
    for chunk in data.chunks(32) {
        for (i, b) in chunk.iter().enumerate() {
            r.key[i / 4] ^= (*b as u32) << (8 * (i % 4));
        }
        let mut out = [0u8; 64];
        let c = r.counter;
        block(&r.key, c ^ 0x5eed, &mut out);
        for i in 0..8 {
            r.key[i] = u32::from_le_bytes(out[4 * i..4 * i + 4].try_into().unwrap());
        }
    }
    if data.len() >= 16 {
        r.seeded = true;
    }
}

pub fn seeded() -> bool {
    RNG.lock().seeded
}

pub fn fill(buf: &mut [u8]) {
    let mut r = RNG.lock();
    // timer jitter as an extra input on every call
    let t = crate::arch::ticks();
    r.key[7] ^= t as u32;
    let mut out = [0u8; 64];
    for chunk in buf.chunks_mut(64) {
        r.counter += 1;
        let c = r.counter;
        block(&r.key, c, &mut out);
        chunk.copy_from_slice(&out[..chunk.len()]);
    }
    // fast key erasure
    r.counter += 1;
    let c = r.counter;
    block(&r.key, c, &mut out);
    for i in 0..8 {
        r.key[i] = u32::from_le_bytes(out[4 * i..4 * i + 4].try_into().unwrap());
    }
}

pub fn u64() -> u64 {
    let mut b = [0u8; 8];
    fill(&mut b);
    u64::from_le_bytes(b)
}

/// Seeds from the device tree (/chosen rng-seed, kaslr-seed) and the boot time.
pub fn init(fdt: &crate::fdt::Fdt) {
    if let Some(c) = fdt.chosen() {
        for p in ["rng-seed", "kaslr-seed"] {
            if let Some(s) = c.prop(p) {
                mix(s);
            }
        }
    }
    let t = crate::arch::ticks().to_le_bytes();
    let rt = crate::time::realtime_ns().to_le_bytes();
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&t);
    b[8..].copy_from_slice(&rt);
    mix(&b[..15]); // not counted as a full seed
}
