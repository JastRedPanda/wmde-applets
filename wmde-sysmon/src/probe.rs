// SPDX-License-Identifier: GPL-3.0-only

//! Sampling layer: `/proc` counters turned into one [`Sample`] per tick.
//!
//! Contract, the same one the settings collector follows: no libcosmic types
//! here, no `fl!`, no localization, no network. Everything is `std`.
//!
//! The layer is written to be called once a second for the whole session
//! inside a panel process, so a tick costs one `pread` per enabled source and
//! allocates nothing:
//!
//! - descriptors are opened on first use and stay open - no `open`/`close`;
//! - `pread` at offset 0 restarts a seq_file, so there is no `lseek` either;
//! - the read buffer is reused and only ever grows;
//! - a graph that is switched off is never read;
//! - `/proc/stat` is read [`CPU_LINE_LIMIT`] bytes deep - the aggregate `cpu`
//!   line - instead of the whole file with a line per core;
//! - parsing walks the bytes: no `String`, no `Vec`, no `format!`.

use std::fs::File;
use std::io::ErrorKind;
use std::os::unix::fs::FileExt;
use std::time::{Duration, Instant};

/// The aggregate `cpu` line is ten numbers; it never approaches this. Asking
/// for less than the whole file means the kernel formats less, and the
/// per-core lines below it are pure cost here.
const CPU_LINE_LIMIT: usize = 256;
/// `MemTotal` … `SReclaimable` all live in the first kilobyte and a half.
const MEMINFO_LIMIT: usize = 4096;
/// `/proc/loadavg` is one short line.
const LOADAVG_LIMIT: usize = 128;
/// Growth ceiling for the two files whose length depends on the machine.
const GROW_LIMIT: usize = 256 * 1024;
/// The whole-device list is re-read this often. Disks do not come and go on a
/// desktop, and scanning `/sys/block` every tick would dwarf the sample.
const DISK_RESCAN: Duration = Duration::from_secs(30);
/// Sector size assumed by `/proc/diskstats`, fixed by the kernel ABI
/// regardless of the drive's real sector size.
const SECTOR_BYTES: u64 = 512;
/// Block devices whose traffic is either a duplicate of a real device
/// (mappers, raid, partitions are not listed here) or noise.
const SKIP_DISK_PREFIXES: [&str; 6] = ["loop", "ram", "zram", "dm-", "md", "nbd"];

/// Which sources this tick needs. A `false` here is a syscall not made.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sources {
    pub cpu: bool,
    pub memory: bool,
    pub network: bool,
    pub swap: bool,
    pub load: bool,
    pub disk: bool,
}

/// One tick. Rates are per second and already divided by the time that
/// actually passed, not by the nominal interval.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sample {
    /// Fractions of the interval: user, nice, system, iowait.
    pub cpu: [f32; 4],
    /// Bytes: used, buffers, cached.
    pub mem: [u64; 3],
    pub mem_total: u64,
    /// Bytes: used.
    pub swap_used: u64,
    pub swap_total: u64,
    /// Bytes per second: in, out.
    pub net: [f64; 2],
    /// Bytes per second: read, written.
    pub disk: [f64; 2],
    /// One-minute load average.
    pub load: f32,
}

/// Jiffy counters of the aggregate `cpu` line.
#[derive(Debug, Clone, Copy, Default)]
struct Cpu {
    user: u64,
    nice: u64,
    system: u64,
    iowait: u64,
    total: u64,
}

pub struct Probe {
    stat: Option<File>,
    meminfo: Option<File>,
    netdev: Option<File>,
    diskstats: Option<File>,
    loadavg: Option<File>,
    /// Shared read buffer. Grows to the largest source and stays there.
    buf: Vec<u8>,
    cpu_prev: Option<Cpu>,
    net_prev: Option<[u64; 2]>,
    disk_prev: Option<[u64; 2]>,
    stamp: Option<Instant>,
    /// Whole block devices, as named in `/sys/block`, minus the synthetic
    /// ones. Partitions are absent from `/sys/block` by construction, which
    /// is exactly the filter `/proc/diskstats` needs.
    disks: Vec<Box<str>>,
    disks_stamp: Option<Instant>,
}

impl Default for Probe {
    fn default() -> Self {
        Self::new()
    }
}

impl Probe {
    pub fn new() -> Self {
        Self {
            stat: None,
            meminfo: None,
            netdev: None,
            diskstats: None,
            loadavg: None,
            buf: Vec::new(),
            cpu_prev: None,
            net_prev: None,
            disk_prev: None,
            stamp: None,
            disks: Vec::new(),
            disks_stamp: None,
        }
    }

    /// Reads every enabled source once. The first call has no previous
    /// counters to subtract from, so its rates come out zero by design.
    pub fn sample(&mut self, want: Sources) -> Sample {
        let now = Instant::now();
        // Rates are divided by the time that actually passed: after the VM
        // was suspended the nominal interval would multiply the counters by
        // however long the machine slept.
        let dt = self
            .stamp
            .replace(now)
            .map_or(0.0, |prev| now.duration_since(prev).as_secs_f64());
        let mut out = Sample::default();

        if want.cpu {
            self.sample_cpu(&mut out);
        }
        if want.memory || want.swap {
            self.sample_meminfo(&mut out, want);
        }
        if want.network {
            self.sample_net(&mut out, dt);
        }
        if want.disk {
            self.sample_disk(&mut out, dt, now);
        }
        if want.load {
            self.sample_load(&mut out);
        }

        out
    }

    fn sample_cpu(&mut self, out: &mut Sample) {
        let Some(file) = open(&mut self.stat, "/proc/stat") else {
            return;
        };
        let Ok(n) = read_head(file, &mut self.buf, CPU_LINE_LIMIT) else {
            return;
        };
        let Some(line) = first_line(&self.buf[..n]) else {
            return;
        };
        let mut f = fields(line);
        if f.next() != Some(&b"cpu"[..]) {
            return;
        }
        // user nice system idle iowait irq softirq steal guest guest_nice
        let mut v = [0u64; 10];
        let mut count = 0;
        for (slot, field) in v.iter_mut().zip(f) {
            *slot = atou64(field);
            count += 1;
        }
        if count < 5 {
            return;
        }
        let cur = Cpu {
            user: v[0],
            nice: v[1],
            // Interrupt and stolen time are system time as far as a strip
            // 20 pixels tall is concerned.
            system: v[2] + v[5] + v[6] + v[7],
            iowait: v[4],
            // guest and guest_nice are already counted inside user and nice.
            total: v[0] + v[1] + v[2] + v[3] + v[4] + v[5] + v[6] + v[7],
        };
        if let Some(prev) = self.cpu_prev.replace(cur) {
            let total = cur.total.saturating_sub(prev.total);
            if total > 0 {
                let scale = 1.0 / total as f32;
                out.cpu = [
                    cur.user.saturating_sub(prev.user) as f32 * scale,
                    cur.nice.saturating_sub(prev.nice) as f32 * scale,
                    cur.system.saturating_sub(prev.system) as f32 * scale,
                    cur.iowait.saturating_sub(prev.iowait) as f32 * scale,
                ];
            }
        }
    }

    fn sample_meminfo(&mut self, out: &mut Sample, want: Sources) {
        let Some(file) = open(&mut self.meminfo, "/proc/meminfo") else {
            return;
        };
        let Ok(n) = read_head(file, &mut self.buf, MEMINFO_LIMIT) else {
            return;
        };

        let (mut total, mut free, mut buffers, mut cached) = (0u64, 0u64, 0u64, 0u64);
        let (mut sreclaim, mut shmem) = (0u64, 0u64);
        let (mut swap_total, mut swap_free) = (0u64, 0u64);
        let mut found = 0u8;

        for line in self.buf[..n].split(|&b| b == b'\n') {
            let Some((key, rest)) = split_key(line) else {
                continue;
            };
            let slot = match key {
                b"MemTotal" => &mut total,
                b"MemFree" => &mut free,
                b"Buffers" => &mut buffers,
                b"Cached" => &mut cached,
                b"SReclaimable" => &mut sreclaim,
                b"Shmem" => &mut shmem,
                b"SwapTotal" => &mut swap_total,
                b"SwapFree" => &mut swap_free,
                _ => continue,
            };
            *slot = fields(rest).next().map_or(0, atou64) * 1024;
            found += 1;
            // Everything wanted lives above SReclaimable; the rest of the
            // file is not worth walking.
            if found == 8 {
                break;
            }
        }

        if want.memory && total > 0 {
            // The split `free(1)` reports: page cache minus the part that is
            // really anonymous shared memory, plus reclaimable slab.
            let cache = cached.saturating_add(sreclaim).saturating_sub(shmem);
            let used = total
                .saturating_sub(free)
                .saturating_sub(buffers)
                .saturating_sub(cache);
            out.mem = [used, buffers, cache];
            out.mem_total = total;
        }
        if want.swap && swap_total > 0 {
            out.swap_used = swap_total.saturating_sub(swap_free);
            out.swap_total = swap_total;
        }
    }

    fn sample_net(&mut self, out: &mut Sample, dt: f64) {
        let Some(file) = open(&mut self.netdev, "/proc/net/dev") else {
            return;
        };
        let Ok(n) = read_all(file, &mut self.buf) else {
            return;
        };

        let mut rx = 0u64;
        let mut tx = 0u64;
        // Two header lines, then "iface: rx_bytes ... tx_bytes ...".
        for line in self.buf[..n].split(|&b| b == b'\n').skip(2) {
            let Some((iface, rest)) = split_key(line) else {
                continue;
            };
            // Loopback traffic is the machine talking to itself; MATE keeps
            // it out of the in/out bands too.
            if iface == b"lo" {
                continue;
            }
            let mut f = fields(rest);
            let Some(rx_bytes) = f.next() else { continue };
            rx += atou64(rx_bytes);
            // rx: bytes packets errs drop fifo frame compressed multicast
            if let Some(tx_bytes) = f.nth(7) {
                tx += atou64(tx_bytes);
            }
        }

        if let Some(prev) = self.net_prev.replace([rx, tx]) {
            out.net = rate(&[rx, tx], &prev, dt);
        }
    }

    fn sample_disk(&mut self, out: &mut Sample, dt: f64, now: Instant) {
        if self
            .disks_stamp
            .is_none_or(|stamp| now.duration_since(stamp) >= DISK_RESCAN)
        {
            self.rescan_disks();
            self.disks_stamp = Some(now);
        }
        let Some(file) = open(&mut self.diskstats, "/proc/diskstats") else {
            return;
        };
        let Ok(n) = read_all(file, &mut self.buf) else {
            return;
        };

        let mut read = 0u64;
        let mut written = 0u64;
        for line in self.buf[..n].split(|&b| b == b'\n') {
            // major minor name reads merged sectors_read ms writes merged
            // sectors_written ...
            let mut f = fields(line).skip(2);
            let Some(name) = f.next() else { continue };
            if !self.disks.iter().any(|d| d.as_bytes() == name) {
                continue;
            }
            let mut f = f.skip(2);
            if let Some(sectors) = f.next() {
                read += atou64(sectors) * SECTOR_BYTES;
            }
            if let Some(sectors) = f.nth(3) {
                written += atou64(sectors) * SECTOR_BYTES;
            }
        }

        if let Some(prev) = self.disk_prev.replace([read, written]) {
            out.disk = rate(&[read, written], &prev, dt);
        }
    }

    fn sample_load(&mut self, out: &mut Sample) {
        let Some(file) = open(&mut self.loadavg, "/proc/loadavg") else {
            return;
        };
        let Ok(n) = read_head(file, &mut self.buf, LOADAVG_LIMIT) else {
            return;
        };
        if let Some(field) = fields(&self.buf[..n]).next() {
            out.load = std::str::from_utf8(field)
                .ok()
                .and_then(|s| s.parse::<f32>().ok())
                .unwrap_or(0.0);
        }
    }

    /// `/sys/block` lists whole devices only, so it is the partition filter
    /// `/proc/diskstats` lacks. Synthetic devices are dropped by name: their
    /// traffic is a second copy of a real device's.
    fn rescan_disks(&mut self) {
        let Ok(dir) = std::fs::read_dir("/sys/block") else {
            return;
        };
        self.disks.clear();
        for entry in dir.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if SKIP_DISK_PREFIXES.iter().any(|p| name.starts_with(p)) {
                continue;
            }
            self.disks.push(Box::from(name));
        }
    }
}

/// Opens the path once and hands out the descriptor from then on. A source
/// whose file is missing stays `None` and is simply never read.
fn open<'a>(slot: &'a mut Option<File>, path: &str) -> Option<&'a File> {
    if slot.is_none() {
        match File::open(path) {
            Ok(file) => *slot = Some(file),
            Err(err) => {
                tracing::warn!(path, ?err, "cannot read counter");
                return None;
            }
        }
    }
    slot.as_ref()
}

/// Reads at most `limit` bytes from the start. `buf` grows, never shrinks.
fn read_head(file: &File, buf: &mut Vec<u8>, limit: usize) -> std::io::Result<usize> {
    if buf.len() < limit {
        buf.resize(limit, 0);
    }
    let mut n = 0;
    while n < limit {
        match file.read_at(&mut buf[n..limit], n as u64) {
            Ok(0) => break,
            Ok(read) => n += read,
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(n)
}

/// Reads to EOF for the two sources whose length depends on the machine.
fn read_all(file: &File, buf: &mut Vec<u8>) -> std::io::Result<usize> {
    if buf.len() < MEMINFO_LIMIT {
        buf.resize(MEMINFO_LIMIT, 0);
    }
    let mut n = 0;
    loop {
        if n == buf.len() {
            if buf.len() >= GROW_LIMIT {
                break;
            }
            buf.resize(buf.len() * 2, 0);
        }
        match file.read_at(&mut buf[n..], n as u64) {
            Ok(0) => break,
            Ok(read) => n += read,
            Err(err) if err.kind() == ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(n)
}

/// Per-second rate of two counters. A counter that went backwards - module
/// reload, interface reset - yields zero instead of a spike.
fn rate(cur: &[u64; 2], prev: &[u64; 2], dt: f64) -> [f64; 2] {
    if dt <= 0.0 {
        return [0.0; 2];
    }
    [
        cur[0].saturating_sub(prev[0]) as f64 / dt,
        cur[1].saturating_sub(prev[1]) as f64 / dt,
    ]
}

fn first_line(buf: &[u8]) -> Option<&[u8]> {
    buf.split(|&b| b == b'\n').next()
}

/// Whitespace-separated fields of a line, without allocating.
fn fields(line: &[u8]) -> impl Iterator<Item = &[u8]> {
    line.split(u8::is_ascii_whitespace)
        .filter(|field| !field.is_empty())
}

/// Splits `Key: rest` (meminfo) or `iface: rest` (net/dev).
fn split_key(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let colon = line.iter().position(|&b| b == b':')?;
    let key = &line[..colon];
    let key_start = key
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(key.len());
    Some((&key[key_start..], &line[colon + 1..]))
}

/// Decimal digits into a number, stopping at the first byte that is not one.
fn atou64(field: &[u8]) -> u64 {
    let mut value = 0u64;
    for &byte in field {
        if !byte.is_ascii_digit() {
            break;
        }
        value = value.wrapping_mul(10).wrapping_add(u64::from(byte - b'0'));
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_numbers_and_keys() {
        assert_eq!(atou64(b"1405715934"), 1_405_715_934);
        assert_eq!(atou64(b"12abc"), 12);
        assert_eq!(atou64(b""), 0);
        assert_eq!(
            split_key(b"MemTotal:    16 kB"),
            Some((&b"MemTotal"[..], &b"    16 kB"[..]))
        );
        assert_eq!(
            split_key(b"    eno1: 42 0"),
            Some((&b"eno1"[..], &b" 42 0"[..]))
        );
        assert_eq!(split_key(b"no colon here"), None);
    }

    #[test]
    fn splits_fields_without_empties() {
        let got: Vec<&[u8]> = fields(b"  cpu   1 2\t3 ").collect();
        assert_eq!(got, vec![&b"cpu"[..], b"1", b"2", b"3"]);
    }

    #[test]
    fn rate_survives_a_counter_reset() {
        assert_eq!(rate(&[10, 10], &[100, 0], 1.0), [0.0, 10.0]);
        assert_eq!(rate(&[10, 10], &[0, 0], 0.0), [0.0, 0.0]);
    }

    /// The real files on the build host: the parser has to come back with
    /// something plausible, not just not crash.
    #[test]
    fn samples_the_running_system() {
        let mut probe = Probe::new();
        let want = Sources {
            cpu: true,
            memory: true,
            network: true,
            swap: true,
            load: true,
            disk: true,
        };
        let first = probe.sample(want);
        assert!(first.mem_total > 0, "MemTotal must parse");
        assert!(first.mem[0] > 0, "used memory must parse");
        assert!(
            first.cpu.iter().all(|v| *v == 0.0),
            "no delta on first tick"
        );

        std::thread::sleep(Duration::from_millis(120));
        let second = probe.sample(want);
        let busy: f32 = second.cpu.iter().sum();
        assert!(
            (0.0..=1.01).contains(&busy),
            "cpu fractions out of range: {busy}"
        );
        assert!(second.net.iter().all(|v| v.is_finite() && *v >= 0.0));
        assert!(second.disk.iter().all(|v| v.is_finite() && *v >= 0.0));
    }
}
