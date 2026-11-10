//! Bucket files for a sweep that runs past its memory limit: the spill directory, sets of
//! bucket files and their records, and the checks on free disk space and open files.

use crate::checkpoint::Sum;
use crate::ring::Ring;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};

/// Bytes of a record: the key, then `stride` counts.
pub fn record_len<R: Ring>(stride: usize) -> usize {
    8 + stride * R::BYTES
}

/// Writes state `key` and its counts into the record `out`, zero-padding the counts.
pub fn put<R: Ring>(out: &mut [u8], key: u64, counts: &[R]) {
    out[..8].copy_from_slice(&key.to_le_bytes());
    for (j, x) in counts.iter().enumerate() {
        x.write(&mut out[8 + j * R::BYTES..]);
    }
    out[8 + counts.len() * R::BYTES..].fill(0);
}

/// The key of a record.
pub fn key_of(rec: &[u8]) -> u64 {
    u64::from_le_bytes(rec[..8].try_into().unwrap())
}

/// Count `j` of a record.
pub fn count_of<R: Ring>(rec: &[u8], j: usize) -> R {
    R::read(&rec[8 + j * R::BYTES..])
}

/// A run's directory of bucket files. Dropping it removes the directory, also while a panic
/// unwinds.
pub struct SpillDir {
    path: PathBuf,
    sets: AtomicUsize,
    /// Open files left for bucket writers.
    files: AtomicUsize,
    written: AtomicU64,
    on_disk: AtomicU64,
    peak: AtomicU64,
    /// Heap bytes of the live bucket sets' tables.
    heap: AtomicUsize,
    /// Whether new bucket sets keep a checksum of each file, for column checkpoints.
    sums: AtomicBool,
}

impl SpillDir {
    /// Creates a new directory under `base`, and raises the open-file limit for its buckets.
    pub fn create(base: &Path) -> io::Result<SpillDir> {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let files = raise_open_files().saturating_sub(64);
        loop {
            let i = NEXT.fetch_add(1, Relaxed);
            let path = base.join(format!("a000104-spill-{}-{i}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => {
                    LIVE.fetch_add(1, Relaxed);
                    return Ok(SpillDir {
                        path,
                        sets: AtomicUsize::new(0),
                        files: AtomicUsize::new(files),
                        written: AtomicU64::new(0),
                        on_disk: AtomicU64::new(0),
                        peak: AtomicU64::new(0),
                        heap: AtomicUsize::new(0),
                        sums: AtomicBool::new(false),
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bytes written to bucket files so far.
    pub fn written(&self) -> u64 {
        self.written.load(Relaxed)
    }

    /// The most bytes on disk at once since the last `reset_peak`.
    pub fn peak(&self) -> u64 {
        self.peak.load(Relaxed)
    }

    /// Heap bytes held by the live bucket sets' own tables.
    pub fn heap(&self) -> usize {
        self.heap.load(Relaxed)
    }

    /// Has the bucket sets made from now on keep a checksum of each file.
    pub fn keep_sums(&self) {
        self.sums.store(true, Relaxed);
    }

    pub fn reset_peak(&self) {
        self.peak.store(self.on_disk.load(Relaxed), Relaxed);
    }

    fn grow(&self, bytes: u64) {
        self.written.fetch_add(bytes, Relaxed);
        let now = self.on_disk.fetch_add(bytes, Relaxed) + bytes;
        self.peak.fetch_max(now, Relaxed);
    }
}

impl Drop for SpillDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
        LIVE.fetch_sub(1, Relaxed);
    }
}

/// A bucket file's writer, and the checksum of what it has written if its set keeps one.
#[derive(Default)]
struct Writer {
    file: Option<File>,
    sum: Sum,
}

/// A window's states on disk: records in `buckets` files, each file holding whole groups.
/// Each file's records are counted by load shard, so a load can reserve its tables exactly.
pub struct BucketSet<'a> {
    dir: &'a SpillDir,
    id: usize,
    record: usize,
    shards: usize,
    summed: bool,
    writers: Vec<Mutex<Writer>>,
    /// Records by bucket and load shard.
    counts: Vec<AtomicU64>,
    bytes: Vec<AtomicU64>,
    /// Open files held for the writers until `seal`.
    files: usize,
}

impl<'a> BucketSet<'a> {
    /// Up to `want` buckets of `record`-byte records, fewer if open files run short.
    pub fn new(dir: &'a SpillDir, want: usize, shards: usize, record: usize) -> Self {
        let mut buckets = 1;
        let _ = dir.files.fetch_update(Relaxed, Relaxed, |left| {
            buckets = want.clamp(1, (left / 2).max(1));
            Some(left.saturating_sub(buckets))
        });
        let set = BucketSet {
            dir,
            id: dir.sets.fetch_add(1, Relaxed),
            record,
            shards,
            summed: dir.sums.load(Relaxed),
            writers: (0..buckets).map(|_| Mutex::default()).collect(),
            counts: (0..buckets * shards).map(|_| AtomicU64::new(0)).collect(),
            bytes: (0..buckets).map(|_| AtomicU64::new(0)).collect(),
            files: buckets,
        };
        dir.heap.fetch_add(set.heap_bytes(), Relaxed);
        set
    }

    pub fn buckets(&self) -> usize {
        self.writers.len()
    }

    pub fn record(&self) -> usize {
        self.record
    }

    /// A seed for this set's bucket hash, so that re-bucketing splits groups differently.
    pub fn salt(&self) -> u64 {
        (self.id as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15)
    }

    /// Heap bytes of the set's own tables.
    pub fn heap_bytes(&self) -> usize {
        self.buckets() * (std::mem::size_of::<Mutex<Writer>>() + 8) + self.counts.len() * 8
    }

    fn path(&self, b: usize) -> PathBuf {
        self.dir.path.join(format!("{}-{b}", self.id))
    }

    /// Appends whole records to bucket `b`; `per_shard[s]` of them load into shard s.
    pub fn append(&self, b: usize, run: &[u8], per_shard: &[u64]) {
        debug_assert_eq!(run.len() % self.record, 0);
        {
            let mut w = self.writers[b].lock().unwrap();
            let path = self.path(b);
            let f = match &mut w.file {
                Some(f) => f,
                None => w.file.insert(
                    OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)
                        .unwrap_or_else(|e| fail(e, "creating", &path)),
                ),
            };
            f.write_all(run)
                .unwrap_or_else(|e| fail(e, "writing", &path));
            if self.summed {
                w.sum.add(run);
            }
        }
        self.bytes[b].fetch_add(run.len() as u64, Relaxed);
        self.dir.grow(run.len() as u64);
        for (s, &k) in per_shard.iter().enumerate() {
            if k > 0 {
                self.counts[b * self.shards + s].fetch_add(k, Relaxed);
            }
        }
    }

    /// Closes the writers: the set is complete.
    pub fn seal(&mut self) {
        for w in &self.writers {
            w.lock().unwrap_or_else(|e| e.into_inner()).file = None;
        }
        self.dir
            .files
            .fetch_add(std::mem::take(&mut self.files), Relaxed);
    }

    /// Bucket `b`'s file and its checksum, if the set keeps checksums.
    pub fn file(&self, b: usize) -> (PathBuf, Option<u64>) {
        let w = self.writers[b].lock().unwrap();
        (self.path(b), self.summed.then(|| w.sum.clone().value()))
    }

    /// Records in bucket `b`.
    pub fn records(&self, b: usize) -> u64 {
        self.bytes[b].load(Relaxed) / self.record as u64
    }

    pub fn total(&self) -> u64 {
        (0..self.buckets()).map(|b| self.records(b)).sum()
    }

    /// Bucket `b`'s records by load shard.
    pub fn shard_counts(&self, b: usize) -> Vec<u64> {
        let row = &self.counts[b * self.shards..(b + 1) * self.shards];
        row.iter().map(|k| k.load(Relaxed)).collect()
    }

    /// Reads bucket `b` into `buf` in pieces of at most `chunk` bytes of whole records, then
    /// deletes it.
    pub fn drain(&self, b: usize, buf: &mut Vec<u8>, chunk: usize, mut each: impl FnMut(&[u8])) {
        let bytes = self.bytes[b].load(Relaxed);
        if bytes == 0 {
            return;
        }
        let path = self.path(b);
        let mut f = File::open(&path).unwrap_or_else(|e| fail(e, "opening", &path));
        buf.resize((chunk / self.record).max(1) * self.record, 0);
        loop {
            let mut got = 0;
            while got < buf.len() {
                match f.read(&mut buf[got..]) {
                    Ok(0) => break,
                    Ok(k) => got += k,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => fail(e, "reading", &path),
                }
            }
            if got == 0 {
                break;
            }
            assert_eq!(
                got % self.record,
                0,
                "a partial record in {}",
                path.display()
            );
            each(&buf[..got]);
        }
        drop(f);
        self.remove(b);
    }

    fn remove(&self, b: usize) {
        let bytes = self.bytes[b].swap(0, Relaxed);
        if bytes > 0 {
            let path = self.path(b);
            fs::remove_file(&path).unwrap_or_else(|e| fail(e, "removing", &path));
            self.dir.on_disk.fetch_sub(bytes, Relaxed);
        }
        for k in &self.counts[b * self.shards..(b + 1) * self.shards] {
            k.store(0, Relaxed);
        }
    }
}

impl Drop for BucketSet<'_> {
    fn drop(&mut self) {
        self.dir.heap.fetch_sub(self.heap_bytes(), Relaxed);
        self.seal();
        for b in 0..self.buckets() {
            if self.bytes[b].load(Relaxed) > 0 && !std::thread::panicking() {
                self.remove(b);
            }
        }
    }
}

fn fail(e: io::Error, what: &str, path: &Path) -> ! {
    let hint = if e.raw_os_error() == Some(libc::ENOSPC) {
        "; pass --spill-dir on a larger volume"
    } else {
        ""
    };
    panic!("{what} {}: {e}{hint}", path.display())
}

/// Free bytes for this user in the file system that holds `path`.
pub fn free_bytes(path: &Path) -> io::Result<u64> {
    let c = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: statvfs fills the zeroed struct, and `c` outlives the call.
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut st) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(st.f_bavail as u64 * st.f_frsize as u64)
    }
}

/// Raises the soft limit on open files as far as the system allows, and returns it.
fn raise_open_files() -> usize {
    // SAFETY: getrlimit and setrlimit only read and write the struct passed.
    unsafe {
        let mut lim: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return 256;
        }
        // macOS refuses values past kern.maxfilesperproc, so try smaller ones in turn.
        for want in [lim.rlim_max, 1 << 20, 1 << 16, 10240] {
            if want <= lim.rlim_cur {
                break;
            }
            let try_lim = libc::rlimit {
                rlim_cur: want,
                rlim_max: lim.rlim_max,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &try_lim) == 0 {
                lim.rlim_cur = want;
                break;
            }
        }
        lim.rlim_cur.min(1 << 20) as usize
    }
}

/// A global allocator that, once `map_large_blocks` is called, maps blocks of 64 KiB or more
/// straight from the system, so that freeing one returns its pages at once. The system
/// allocator keeps freed pages resident, which a memory limit would have to count.
pub struct Allocator;

static DIRECT: AtomicBool = AtomicBool::new(false);
static PAGE: AtomicUsize = AtomicUsize::new(0);

/// Turns on direct mapping. Call it before any block of 64 KiB or more is allocated.
pub fn map_large_blocks() {
    // SAFETY: sysconf only reads a system setting.
    PAGE.store(
        unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize,
        Relaxed,
    );
    DIRECT.store(true, Relaxed);
}

/// The mapped length of a direct block, or None for one from the system allocator.
fn mapped(layout: std::alloc::Layout) -> Option<usize> {
    let page = PAGE.load(Relaxed);
    (DIRECT.load(Relaxed) && layout.size() >= 64 << 10 && layout.align() <= page)
        .then(|| layout.size().next_multiple_of(page))
}

// SAFETY: direct blocks are fresh anonymous mappings, unmapped with the length they were
// mapped with; the rest go to the system allocator.
unsafe impl std::alloc::GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        match mapped(layout) {
            Some(len) => unsafe {
                let p = libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANON,
                    -1,
                    0,
                );
                if p == libc::MAP_FAILED {
                    std::ptr::null_mut()
                } else {
                    p.cast()
                }
            },
            None => unsafe { std::alloc::System.alloc(layout) },
        }
    }

    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        match mapped(layout) {
            // Fresh mappings are zero.
            Some(_) => unsafe { self.alloc(layout) },
            None => unsafe { std::alloc::System.alloc_zeroed(layout) },
        }
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: std::alloc::Layout) {
        match mapped(layout) {
            Some(len) => unsafe {
                libc::munmap(p.cast(), len);
            },
            None => unsafe { std::alloc::System.dealloc(p, layout) },
        }
    }

    unsafe fn realloc(&self, p: *mut u8, layout: std::alloc::Layout, size: usize) -> *mut u8 {
        // SAFETY: the caller guarantees `size`, rounded to the alignment, fits in isize.
        let new = unsafe { std::alloc::Layout::from_size_align_unchecked(size, layout.align()) };
        if mapped(layout).is_none() && mapped(new).is_none() {
            return unsafe { std::alloc::System.realloc(p, layout, size) };
        }
        unsafe {
            let q = self.alloc(new);
            if !q.is_null() {
                std::ptr::copy_nonoverlapping(p, q, layout.size().min(size));
                self.dealloc(p, layout);
            }
            q
        }
    }
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
/// Spill directories that exist, which an interrupt must remove.
static LIVE: AtomicUsize = AtomicUsize::new(0);

extern "C" fn on_signal(sig: libc::c_int) {
    if LIVE.load(Relaxed) == 0 {
        // SAFETY: raise is async-signal-safe, and SA_RESETHAND restored the default action.
        unsafe { libc::raise(sig) };
    }
    INTERRUPTED.store(true, Relaxed);
}

/// Turns SIGINT and SIGTERM into a panic at the next `check_interrupt` while a spill directory
/// exists, so that it is removed. Otherwise, and at a second signal, the program ends at once.
pub fn catch_interrupts() {
    // SAFETY: the handler only uses atomics and raise, which are async-signal-safe.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_signal as *const () as usize;
        sa.sa_flags = libc::SA_RESETHAND;
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
    }
}

pub fn check_interrupt() {
    if INTERRUPTED.load(Relaxed) {
        panic!("interrupted");
    }
}
