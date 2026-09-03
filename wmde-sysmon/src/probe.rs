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
use std::path::{Path, PathBuf};
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
/// The device lists are re-read this often. Disks and network cards do not come
/// and go on a desktop, and scanning `/sys` every tick would dwarf the sample.
const DEVICE_RESCAN: Duration = Duration::from_secs(30);
/// Sector size assumed by `/proc/diskstats`, fixed by the kernel ABI
/// regardless of the drive's real sector size.
const SECTOR_BYTES: u64 = 512;
/// SCSI device type of an optical drive, as `/sys/block/sr0/device/type` spells
/// it. A drive is real hardware, so the `device` link alone does not drop it.
const SCSI_TYPE_ROM: &str = "5";

/// Which sources this tick needs. A `false` here is a syscall not made.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sources {
    pub cpu: bool,
    /// Also break the load down per logical processor, into [`Probe::cpu_cores`].
    ///
    /// Off by default and worth keeping off: `/proc/stat` is read [`CPU_LINE_LIMIT`] bytes
    /// deep without it, and the kernel does not format the per-core lines that would follow.
    pub cpu_cores: bool,
    pub memory: bool,
    /// Also fill [`Sample::mem_detail`].
    ///
    /// Off by default and worth keeping off: the keys it needs sit past `SReclaimable`, and
    /// without it the parse of `/proc/meminfo` stops there instead of walking the tail of the
    /// file every tick.
    pub memory_detail: bool,
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
    /// Bytes per second: in, out. The sum over [`Probe::nets`].
    pub net: [f64; 2],
    /// Bytes per second: read, written. The sum over [`Probe::disks`].
    pub disk: [f64; 2],
    /// One-minute load average.
    pub load: f32,
    /// What [`Sources::memory_detail`] adds, and zero without it. Bytes throughout.
    ///
    /// These live below `SReclaimable` in `/proc/meminfo`, which is where the parse stops
    /// when nobody asks for them.
    pub mem_detail: MemDetail,
}

/// The second half of `/proc/meminfo`: what a window has room to print and a twenty-pixel
/// strip does not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemDetail {
    /// `MemAvailable` - what a new allocation can have without pushing anything to swap.
    pub available: u64,
    /// `Committed_AS` against `CommitLimit`: what has been promised, and the ceiling.
    pub committed: u64,
    pub commit_limit: u64,
    /// `SReclaimable` and `SUnreclaim`, the two halves of the slab.
    pub slab_reclaimable: u64,
    pub slab_unreclaimable: u64,
}

/// One device's share of a counter file: what the kernel calls it, and what it did over the
/// last tick.
///
/// Handed out as a slice for the same reason [`Probe::cpu_cores`] is: [`Sample`] is `Copy`,
/// and a `Vec` inside it would cost every consumer an allocation a tick. The counters behind
/// the rate are private - a device is identified by its name and nothing else.
#[derive(Debug, Clone, Default)]
pub struct DeviceRate {
    /// `nvme0n1`, `sda`, `enp5s0` - the name in `/sys`, which is also the key in
    /// `/proc/diskstats` and `/proc/net/dev`.
    pub name: Box<str>,
    /// Bytes per second: read and written for a disk, in and out for an interface.
    pub rate: [f64; 2],
    prev: Option<[u64; 2]>,
    /// Cleared before every walk. A device the counter file stopped listing would otherwise
    /// hold its last rate until the next rescan.
    seen: bool,
}

impl DeviceRate {
    /// Folds this tick's counters in. The first pair seen has nothing to subtract from and
    /// yields zero, which is the same rule the machine-wide counters follow.
    fn step(&mut self, cur: [u64; 2], dt: f64) {
        self.seen = true;
        self.rate = match self.prev.replace(cur) {
            Some(prev) => rate(&cur, &prev, dt),
            None => [0.0; 2],
        };
    }
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
    stamp: Option<Instant>,
    /// Whole block devices, as named in `/sys/block`, minus the synthetic
    /// ones. Partitions are absent from `/sys/block` by construction, which
    /// is exactly the filter `/proc/diskstats` needs.
    disks: Vec<DeviceRate>,
    /// Network cards, as named in `/sys/class/net`, minus everything virtual.
    nets: Vec<DeviceRate>,
    devices_stamp: Option<Instant>,
    /// Per-core shares of the last tick, in `/proc/stat` order. Grown once and reused, so
    /// the per-core walk allocates nothing after the first tick.
    cpu_cores: Vec<[f32; 4]>,
    cpu_cores_prev: Vec<Cpu>,
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
            stamp: None,
            disks: Vec::new(),
            nets: Vec::new(),
            devices_stamp: None,
            cpu_cores: Vec::new(),
            cpu_cores_prev: Vec::new(),
        }
    }

    /// Load per logical processor from the last tick, in the order `/proc/stat` lists them.
    ///
    /// Empty unless [`Sources::cpu_cores`] was asked for. Handed out as a slice rather than
    /// through [`Sample`], which is `Copy` and would have to allocate.
    pub fn cpu_cores(&self) -> &[[f32; 4]] {
        &self.cpu_cores
    }

    /// The disks behind [`Sample::disk`], in name order, as of the last tick.
    ///
    /// Empty until [`Sources::disk`] has been asked for once. The order is stable across
    /// ticks, so a caller may keep a history per index between rescans.
    pub fn disks(&self) -> &[DeviceRate] {
        &self.disks
    }

    /// The interfaces behind [`Sample::net`], in name order, as of the last tick.
    ///
    /// Empty until [`Sources::network`] has been asked for once.
    pub fn nets(&self) -> &[DeviceRate] {
        &self.nets
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
            self.sample_cpu(&mut out, want);
        }
        if want.memory || want.swap {
            self.sample_meminfo(&mut out, want);
        }
        if want.network {
            self.sample_net(&mut out, dt, now);
        }
        if want.disk {
            self.sample_disk(&mut out, dt, now);
        }
        if want.load {
            self.sample_load(&mut out);
        }

        out
    }

    fn sample_cpu(&mut self, out: &mut Sample, want: Sources) {
        let Some(file) = open(&mut self.stat, "/proc/stat") else {
            return;
        };
        let read = if want.cpu_cores {
            read_all(file, &mut self.buf)
        } else {
            read_head(file, &mut self.buf, CPU_LINE_LIMIT)
        };
        let Ok(n) = read else {
            return;
        };
        if want.cpu_cores {
            self.sample_cpu_cores(n);
        } else {
            self.cpu_cores.clear();
            self.cpu_cores_prev.clear();
        }
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

    /// The `cpuN` lines, in the order they appear. A core that disappears - hot unplug, or
    /// a machine that reports fewer than last time - drops out of both vectors together, so
    /// index N always means the same core in `cpu_cores` and in `cpu_cores_prev`.
    fn sample_cpu_cores(&mut self, n: usize) {
        let mut index = 0usize;
        for line in self.buf[..n].split(|b| *b == b'\n') {
            if !line.starts_with(b"cpu") || line.starts_with(b"cpu ") {
                continue;
            }
            let mut f = fields(line);
            // `cpu0`, `cpu1`, ... - the name itself carries no number we need, only its
            // position matters, and that is what `index` is.
            let _ = f.next();
            let mut v = [0u64; 10];
            let mut count = 0;
            for (slot, field) in v.iter_mut().zip(f) {
                *slot = atou64(field);
                count += 1;
            }
            if count < 5 {
                continue;
            }
            let cur = Cpu {
                user: v[0],
                nice: v[1],
                system: v[2] + v[5] + v[6] + v[7],
                iowait: v[4],
                total: v[0] + v[1] + v[2] + v[3] + v[4] + v[5] + v[6] + v[7],
            };
            if self.cpu_cores.len() <= index {
                self.cpu_cores.push([0.0; 4]);
                self.cpu_cores_prev.push(cur);
            } else {
                let prev = std::mem::replace(&mut self.cpu_cores_prev[index], cur);
                let total = cur.total.saturating_sub(prev.total);
                self.cpu_cores[index] = if total > 0 {
                    let scale = 1.0 / total as f32;
                    [
                        cur.user.saturating_sub(prev.user) as f32 * scale,
                        cur.nice.saturating_sub(prev.nice) as f32 * scale,
                        cur.system.saturating_sub(prev.system) as f32 * scale,
                        cur.iowait.saturating_sub(prev.iowait) as f32 * scale,
                    ]
                } else {
                    [0.0; 4]
                };
            }
            index += 1;
        }
        self.cpu_cores.truncate(index);
        self.cpu_cores_prev.truncate(index);
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
        let (mut available, mut committed, mut commit_limit, mut sunreclaim) =
            (0u64, 0u64, 0u64, 0u64);
        let mut found = 0u8;
        // The eight keys the strip needs all sit above `SReclaimable`; the four the window
        // adds are further down, and only then is the tail of the file worth walking.
        let wanted = if want.memory_detail { 12 } else { 8 };

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
                b"MemAvailable" if want.memory_detail => &mut available,
                b"SUnreclaim" if want.memory_detail => &mut sunreclaim,
                b"CommitLimit" if want.memory_detail => &mut commit_limit,
                b"Committed_AS" if want.memory_detail => &mut committed,
                _ => continue,
            };
            *slot = fields(rest).next().map_or(0, atou64) * 1024;
            found += 1;
            if found == wanted {
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
            if want.memory_detail {
                out.mem_detail = MemDetail {
                    available,
                    committed,
                    commit_limit,
                    slab_reclaimable: sreclaim,
                    slab_unreclaimable: sunreclaim,
                };
            }
        }
        if want.swap && swap_total > 0 {
            out.swap_used = swap_total.saturating_sub(swap_free);
            out.swap_total = swap_total;
        }
    }

    fn sample_net(&mut self, out: &mut Sample, dt: f64, now: Instant) {
        self.rescan(now);
        let Some(file) = open(&mut self.netdev, "/proc/net/dev") else {
            return;
        };
        let Ok(n) = read_all(file, &mut self.buf) else {
            return;
        };

        for meter in &mut self.nets {
            meter.seen = false;
        }
        // Two header lines, then "iface: rx_bytes ... tx_bytes ...".
        for line in self.buf[..n].split(|&b| b == b'\n').skip(2) {
            let Some((iface, rest)) = split_key(line) else {
                continue;
            };
            let Some(meter) = self.nets.iter_mut().find(|m| m.name.as_bytes() == iface) else {
                continue;
            };
            let mut f = fields(rest);
            let Some(rx_bytes) = f.next() else { continue };
            let mut cur = [atou64(rx_bytes), 0];
            // rx: bytes packets errs drop fifo frame compressed multicast
            if let Some(tx_bytes) = f.nth(7) {
                cur[1] = atou64(tx_bytes);
            }
            meter.step(cur, dt);
        }

        out.net = settle(&mut self.nets);
    }

    fn sample_disk(&mut self, out: &mut Sample, dt: f64, now: Instant) {
        self.rescan(now);
        let Some(file) = open(&mut self.diskstats, "/proc/diskstats") else {
            return;
        };
        let Ok(n) = read_all(file, &mut self.buf) else {
            return;
        };

        for meter in &mut self.disks {
            meter.seen = false;
        }
        for line in self.buf[..n].split(|&b| b == b'\n') {
            // major minor name reads merged sectors_read ms writes merged
            // sectors_written ...
            let mut f = fields(line).skip(2);
            let Some(name) = f.next() else { continue };
            let Some(meter) = self.disks.iter_mut().find(|m| m.name.as_bytes() == name) else {
                continue;
            };
            let mut f = f.skip(2);
            let mut cur = [0u64; 2];
            if let Some(sectors) = f.next() {
                cur[0] = atou64(sectors) * SECTOR_BYTES;
            }
            if let Some(sectors) = f.nth(3) {
                cur[1] = atou64(sectors) * SECTOR_BYTES;
            }
            meter.step(cur, dt);
        }

        out.disk = settle(&mut self.disks);
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

    /// Re-reads both device lists, at most once every [`DEVICE_RESCAN`].
    ///
    /// One stamp for the two: a tick that reads either source is a tick that can afford the
    /// other scan, and two stamps would drift apart for no gain.
    fn rescan(&mut self, now: Instant) {
        if self
            .devices_stamp
            .is_some_and(|stamp| now.duration_since(stamp) < DEVICE_RESCAN)
        {
            return;
        }
        self.devices_stamp = Some(now);
        if let Some(names) = scan("/sys/block", is_disk) {
            retain(&mut self.disks, &names);
        }
        if let Some(names) = scan("/sys/class/net", is_interface) {
            retain(&mut self.nets, &names);
        }
    }
}

/// Names in a `/sys` directory that pass a test, sorted so that the order a caller sees does
/// not depend on the order the filesystem happened to hand them over.
///
/// `None` when the directory cannot be read at all, which leaves the previous list standing
/// rather than emptying it.
fn scan(dir: &str, keep: fn(&Path) -> bool) -> Option<Vec<Box<str>>> {
    let mut names: Vec<Box<str>> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|entry| keep(&entry.path()))
        .filter_map(|entry| entry.file_name().to_str().map(Box::from))
        .collect();
    names.sort_unstable();
    Some(names)
}

/// A whole block device backed by real hardware, and not an optical drive.
///
/// The `device` link is the filter `/sys/block` needs: loop, ram, zram, dm-\* and md\* have
/// none, and partitions are not listed here at all. A drive does have one, and is dropped
/// separately - an empty tray is not a disk of the machine.
fn is_disk(path: &Path) -> bool {
    path.join("device").exists() && !holds(path.join("device/type"), SCSI_TYPE_ROM)
}

/// A network card of this machine, with its link up.
///
/// `/proc/net/dev` counts everything the kernel routes over, and on a machine with
/// containers that is sixty veth pairs, ten bridges and the card - where one packet is
/// counted three times. What is left here is what the reference lists.
fn is_interface(path: &Path) -> bool {
    // A wireless card in managed mode reports ARPHRD_ETHER like any other, so the wireless
    // directory is the test and `type` is not.
    let hardware = path.join("device").exists()
        || path.join("wireless").is_dir()
        || path.join("phy80211").exists();
    hardware && !holds(path.join("operstate"), "down")
}

/// Whether a one-line `/sys` attribute reads exactly this. A file that is missing, or that
/// answers with an error - `speed` and `duplex` do while the link is down - reads as no.
fn holds(path: PathBuf, value: &str) -> bool {
    std::fs::read_to_string(path).is_ok_and(|text| text.trim() == value)
}

/// Rebuilds a meter list to match the names just scanned, in that order.
///
/// A device that stayed keeps its counters, so a rescan does not cost it a tick of history.
/// One that came back after an absence starts over: its counters restarted with it.
fn retain(meters: &mut Vec<DeviceRate>, names: &[Box<str>]) {
    if meters.len() == names.len() && meters.iter().zip(names).all(|(m, n)| m.name == *n) {
        return;
    }
    let mut next = Vec::with_capacity(names.len());
    for name in names {
        match meters.iter().position(|m| m.name == *name) {
            Some(i) => next.push(meters.swap_remove(i)),
            None => next.push(DeviceRate {
                name: name.clone(),
                ..DeviceRate::default()
            }),
        }
    }
    *meters = next;
}

/// Zeroes the devices the counter file did not mention this tick and returns the total.
///
/// A device that vanished between rescans - a stick pulled out, a cable unplugged - would
/// otherwise hold its last rate for half a minute and be added into the sum all the while.
fn settle(meters: &mut [DeviceRate]) -> [f64; 2] {
    let mut total = [0.0; 2];
    for meter in meters {
        if !meter.seen {
            meter.rate = [0.0; 2];
            meter.prev = None;
        }
        total[0] += meter.rate[0];
        total[1] += meter.rate[1];
    }
    total
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
            cpu_cores: true,
            memory: true,
            memory_detail: true,
            network: true,
            swap: true,
            load: true,
            disk: true,
        };
        let first = probe.sample(want);
        assert!(first.mem_total > 0, "MemTotal must parse");
        assert!(first.mem[0] > 0, "used memory must parse");
        // The four keys past `SReclaimable`, which the strip never asks for.
        let detail = first.mem_detail;
        assert!(detail.available > 0, "MemAvailable must parse");
        assert!(detail.commit_limit > 0, "CommitLimit must parse");
        assert!(detail.committed > 0, "Committed_AS must parse");
        assert!(
            detail.available <= first.mem_total,
            "available beyond the total: {} of {}",
            detail.available,
            first.mem_total
        );

        // Without the flag the tail is not walked at all, and the block stays zero.
        let plain = probe.sample(Sources {
            memory_detail: false,
            ..want
        });
        assert_eq!(plain.mem_detail, MemDetail::default());
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

        // One entry per logical processor, and each within the same bounds as the aggregate.
        let cores = probe.cpu_cores();
        assert!(!cores.is_empty(), "a running machine has at least one core");
        for core in cores {
            let busy: f32 = core.iter().sum();
            assert!(
                (0.0..=1.01).contains(&busy),
                "per-core fractions out of range: {busy}"
            );
        }
    }

    #[test]
    fn per_core_costs_nothing_when_it_is_not_asked_for() {
        let mut probe = Probe::new();
        let _ = probe.sample(Sources {
            cpu: true,
            ..Sources::default()
        });
        assert!(probe.cpu_cores().is_empty());
    }

    /// The machine-wide figure is the devices added up, and stays that way. A window draws
    /// the parts and a panel draws the total; the two have to agree.
    #[test]
    fn the_total_is_the_devices_added_up() {
        let want = Sources {
            network: true,
            disk: true,
            ..Sources::default()
        };
        let mut probe = Probe::new();
        let _ = probe.sample(want);
        std::thread::sleep(Duration::from_millis(120));
        let second = probe.sample(want);

        assert!(
            !probe.disks().is_empty(),
            "a running machine has at least one disk"
        );
        for (figure, meters) in [(second.disk, probe.disks()), (second.net, probe.nets())] {
            let mut sum = [0.0; 2];
            for meter in meters {
                sum[0] += meter.rate[0];
                sum[1] += meter.rate[1];
            }
            for (part, whole) in sum.iter().zip(figure) {
                assert!((part - whole).abs() < 1.0, "{part} against {whole}");
            }
        }
    }

    /// A rescan is not allowed to cost a device its counters: a disk that was there before
    /// and is there now keeps the same meter, and only the newcomer starts from nothing.
    #[test]
    fn a_rescan_keeps_the_counters_of_what_stayed() {
        let mut meters = Vec::new();
        retain(&mut meters, &[Box::from("sda"), Box::from("sdb")]);
        meters[0].step([100, 100], 1.0);
        meters[0].step([200, 200], 1.0);
        assert_eq!(meters[0].rate, [100.0, 100.0]);

        // sdb pulled out, nvme0n1 plugged in. Order follows the names given.
        retain(&mut meters, &[Box::from("nvme0n1"), Box::from("sda")]);
        assert_eq!(meters.len(), 2);
        assert_eq!(&*meters[0].name, "nvme0n1");
        assert!(
            meters[0].prev.is_none(),
            "a new device has nothing to subtract from"
        );
        assert_eq!(meters[1].rate, [100.0, 100.0], "sda kept its rate");
        assert!(meters[1].prev.is_some(), "sda kept its counters");
    }

    /// A device the counter file stopped listing reads as idle, not as busy forever.
    #[test]
    fn a_device_that_went_quiet_stops_counting() {
        let mut meters = Vec::new();
        retain(&mut meters, &[Box::from("sda")]);
        meters[0].step([100, 0], 1.0);
        meters[0].step([200, 0], 1.0);
        assert_eq!(settle(&mut meters), [100.0, 0.0]);

        meters[0].seen = false;
        assert_eq!(settle(&mut meters), [0.0, 0.0]);
        assert_eq!(meters[0].rate, [0.0, 0.0]);
    }
}
