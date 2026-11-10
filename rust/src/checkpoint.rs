//! Checkpoints, so a stopped or crashed run resumes: the settings file that ties a checkpoint
//! directory to one command and build, a file per finished strip, column checkpoints of the
//! strip in progress, and a file per finished unit of the search. Every file is written whole
//! under a temporary name, then renamed, and is covered by a checksum.

use crate::ring::{P61, Ring};
use crate::search::Unit;
use crate::tm::{DiskStats, Strip};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The build a checkpoint is tied to (see `build.rs`).
pub const COMMIT: &str = env!("A000104_COMMIT");

/// The version of the file layouts.
pub const FORMAT: u64 = 2;

const SETTINGS: &str = "settings";
const STRIP_MAGIC: &[u8; 8] = b"A000104S";
const COLUMN_MAGIC: &[u8; 8] = b"A000104C";
const UNIT_MAGIC: &[u8; 8] = b"A000104U";
/// The file in a column checkpoint that describes it; the layer is in `layer-<k>` files.
const MANIFEST: &str = "manifest";

/// A run's checkpoint directory.
pub struct Checkpoint {
    dir: PathBuf,
    resumed: bool,
    /// The most time between column checkpoints.
    every: Duration,
    /// When progress was last saved.
    last: Mutex<Instant>,
}

/// A strip's sweep at the start of column c, besides its layer: the tables and counters so far,
/// and each layer file's records and checksum.
pub(crate) struct Column<R> {
    pub c: usize,
    pub steps: u64,
    pub peak: usize,
    pub prev: Vec<usize>,
    pub closures: Vec<Vec<R>>,
    pub r2: [Vec<Vec<R>>; 2],
    pub files: Vec<(u64, u64)>,
}

impl Checkpoint {
    /// Opens `dir` for `command` (such as "components 45 --r2 both"), creating it and its
    /// settings file on the first run. Refuses a directory of another command, build or format.
    pub fn open(dir: &Path, command: &str, every: Duration) -> Result<Checkpoint, String> {
        let shown = dir.display();
        let path = dir.join(SETTINGS);
        let want = settings(command, COMMIT, FORMAT);
        let resumed = match fs::read_to_string(&path) {
            Ok(have) if have == want => true,
            Ok(have) => {
                let field = |k: &str| {
                    have.lines()
                        .find_map(|l| l.strip_prefix(k)?.strip_prefix(' '))
                        .unwrap_or("?")
                        .to_string()
                };
                let (format, ours) = (field("format"), FORMAT.to_string());
                let named = |f: &str| {
                    if format == ours {
                        String::new()
                    } else {
                        format!(" in format {f}")
                    }
                };
                return Err(format!(
                    "{shown} holds a checkpoint of {} at {}{}, not of {command} at {COMMIT}{}; \
                     pass a new --checkpoint-dir, or start over with: rm -r {shown}",
                    field("command"),
                    field("commit"),
                    named(&format),
                    named(&ours),
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::create_dir_all(dir).map_err(|e| format!("{shown}: {e}"))?;
                let others = fs::read_dir(dir)
                    .map_err(|e| format!("{shown}: {e}"))?
                    .filter_map(Result::ok)
                    .any(|e| !e.file_name().to_string_lossy().ends_with(".tmp"));
                if others {
                    return Err(format!(
                        "{shown} holds files but no checkpoint settings; pass a new \
                         --checkpoint-dir"
                    ));
                }
                write_whole(&path, want.as_bytes(), true).map_err(|e| format!("{shown}: {e}"))?;
                false
            }
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        // Files and column checkpoints a killed run was writing.
        for entry in fs::read_dir(dir)
            .map_err(|e| format!("{shown}: {e}"))?
            .flatten()
        {
            if entry.file_name().to_string_lossy().ends_with(".tmp") {
                let p = entry.path();
                let _ = fs::remove_dir_all(&p).or_else(|_| fs::remove_file(&p));
            }
        }
        Ok(Checkpoint {
            dir: dir.to_path_buf(),
            resumed,
            every,
            last: Mutex::new(Instant::now()),
        })
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Whether the directory held this command's checkpoint before this run.
    pub fn resumed(&self) -> bool {
        self.resumed
    }

    /// Removes the settings file `open` just wrote, and the directory if that leaves it empty.
    pub fn remove_new(self) {
        let _ = fs::remove_file(self.dir.join(SETTINGS));
        let _ = fs::remove_dir(&self.dir);
    }

    fn saved(&self) {
        *self.last.lock().unwrap() = Instant::now();
    }

    fn strip_path<R: Ring>(&self, w: usize) -> PathBuf {
        self.dir.join(format!("strip-{w}.{}", modulus_name::<R>()))
    }

    /// Strip w's tables mod R, if a sound strip file holds them. A damaged one is reported and
    /// left to be overwritten.
    pub fn read_strip<R: Ring>(&self, w: usize, n: usize, merge: bool) -> Option<Strip<R>> {
        let path = self.strip_path::<R>(w);
        let file = match fs::read(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
            Err(e) => {
                eprintln!(
                    "# checkpoint: {} not read ({e}); strip {w} runs again",
                    path.display()
                );
                return None;
            }
        };
        let strip = decode_strip(&file, w, n, merge);
        if strip.is_none() {
            eprintln!(
                "# checkpoint: {} is damaged; strip {w} runs again",
                path.display()
            );
        }
        strip
    }

    /// Saves a finished strip and removes its column checkpoints. A failed write is reported,
    /// and the run goes on without it.
    pub fn write_strip<R: Ring>(&self, w: usize, n: usize, merge: bool, strip: &Strip<R>) {
        let path = self.strip_path::<R>(w);
        match write_whole(&path, &encode_strip(strip, w, n, merge), true) {
            Ok(()) => {
                for (_, p) in self.columns::<R>(w) {
                    let _ = fs::remove_dir_all(p);
                }
                self.saved();
            }
            Err(e) => eprintln!(
                "# checkpoint: {} not written ({e}); the run goes on without it",
                path.display()
            ),
        }
    }

    fn column_path<R: Ring>(&self, w: usize, c: usize) -> PathBuf {
        self.dir
            .join(format!("column-{w}-{c}.{}", modulus_name::<R>()))
    }

    /// Strip w's complete column checkpoints mod R, by column, newest first.
    fn columns<R: Ring>(&self, w: usize) -> Vec<(usize, PathBuf)> {
        let (head, tail) = (format!("column-{w}-"), format!(".{}", modulus_name::<R>()));
        let mut found: Vec<(usize, PathBuf)> = fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let c = name
                    .strip_prefix(&head)?
                    .strip_suffix(&tail)?
                    .parse()
                    .ok()?;
                Some((c, e.path()))
            })
            .collect();
        found.sort_by_key(|f| std::cmp::Reverse(f.0));
        found
    }

    /// Bytes of all column checkpoints in the directory.
    pub fn column_bytes(&self) -> u64 {
        let dirs = fs::read_dir(&self.dir).into_iter().flatten().flatten();
        dirs.filter(|e| e.file_name().to_string_lossy().starts_with("column-"))
            .flat_map(|e| fs::read_dir(e.path()).into_iter().flatten().flatten())
            .filter_map(|f| f.metadata().ok())
            .map(|m| m.len())
            .sum()
    }

    /// Whether a column checkpoint is due: `every` has passed since progress was last saved.
    pub(crate) fn due(&self) -> bool {
        self.last.lock().unwrap().elapsed() >= self.every
    }

    /// A new temporary directory for column c of strip w, to write the layer files into.
    pub(crate) fn begin_column<R: Ring>(&self, w: usize, c: usize) -> io::Result<PathBuf> {
        let tmp = tmp_path(&self.column_path::<R>(w, c));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir(&tmp)?;
        Ok(tmp)
    }

    /// Completes the column checkpoint in `tmp` with its manifest, then removes the strip's
    /// older ones.
    pub(crate) fn finish_column<R: Ring>(
        &self,
        w: usize,
        n: usize,
        merge: bool,
        tmp: &Path,
        column: &Column<R>,
    ) -> io::Result<()> {
        write_whole(
            &tmp.join(MANIFEST),
            &encode_column(column, w, n, merge),
            true,
        )?;
        let path = self.column_path::<R>(w, column.c);
        let _ = fs::remove_dir_all(&path);
        fs::rename(tmp, &path)?;
        sync_dir(&self.dir);
        for (c, p) in self.columns::<R>(w) {
            if c != column.c {
                let _ = fs::remove_dir_all(p);
            }
        }
        self.saved();
        Ok(())
    }

    /// Strip w's column checkpoints mod R with a sound manifest, newest first: the directory and
    /// what the manifest holds. A damaged manifest is reported and skipped.
    pub(crate) fn read_columns<R: Ring>(
        &self,
        w: usize,
        n: usize,
        merge: bool,
    ) -> Vec<(PathBuf, Column<R>)> {
        self.columns::<R>(w)
            .into_iter()
            .filter_map(|(c, path)| {
                let column = fs::read(path.join(MANIFEST))
                    .ok()
                    .and_then(|f| decode_column(&f, w, n, merge))
                    .filter(|col| col.c == c);
                if column.is_none() {
                    eprintln!("# checkpoint: {} is damaged; not used", path.display());
                }
                Some((path, column?))
            })
            .collect()
    }

    fn unit_path(&self, u: &Unit) -> PathBuf {
        self.dir.join(format!("unit-{}-{}-{}", u.job, u.run, u.id))
    }

    /// A finished search unit's counts by size, if a sound unit file holds them. A damaged one
    /// is reported and left to be overwritten.
    pub fn read_unit(&self, n: usize, u: &Unit) -> Option<Vec<u64>> {
        let path = self.unit_path(u);
        let counts = decode_unit(&fs::read(&path).ok()?, n, u);
        if counts.is_none() {
            eprintln!(
                "# checkpoint: {} is damaged; the unit runs again",
                path.display()
            );
        }
        counts
    }

    /// Saves a finished search unit, unflushed: a unit that a crash loses or tears runs again.
    /// A failed write is reported, and the run goes on without it.
    pub fn write_unit(&self, n: usize, u: &Unit, counts: &[u64]) {
        let path = self.unit_path(u);
        if let Err(e) = write_whole(&path, &encode_unit(n, u, counts), false) {
            eprintln!(
                "# checkpoint: {} not written ({e}); the run goes on without it",
                path.display()
            );
        }
    }

    /// The search's progress over `units`, for the "# resuming" line: "search done", "search 120
    /// of 4096 units done" or "search not started".
    pub fn describe_search(&self, units: &[Unit]) -> String {
        match units.iter().filter(|u| self.unit_path(u).exists()).count() {
            0 => "search not started".into(),
            k if k == units.len() => "search done".into(),
            k => format!("search {k} of {} units done", units.len()),
        }
    }

    /// The sweep mod R for n, for the "# resuming" line: "<label> done", or "<label> strips 1
    /// to 21 done, strip 22 from column 14", or "<label> not started".
    pub fn describe<R: Ring>(&self, label: &str, n: usize) -> String {
        let all = n.div_ceil(2);
        let done: Vec<usize> = (1..=all)
            .filter(|&w| self.strip_path::<R>(w).exists())
            .collect();
        let mut parts = vec![match done.len() {
            0 => String::new(),
            k if k == all => return format!("{label} done"),
            1 => format!("strip {} done", done[0]),
            _ => format!("strips {} done", ranges(&done)),
        }];
        for w in (1..=all).filter(|w| !done.contains(w)) {
            if let Some((c, _)) = self.columns::<R>(w).first() {
                parts.push(format!("strip {w} from column {c}"));
            }
        }
        parts.retain(|p| !p.is_empty());
        if parts.is_empty() {
            format!("{label} not started")
        } else {
            format!("{label} {}", parts.join(", "))
        }
    }
}

/// R's modulus as file names give it: 2^64, 2^61-1, 65536 or a 16-bit prime.
pub fn modulus_name<R: Ring>() -> String {
    match R::MODULUS {
        m if m == 1 << 64 => "2^64".into(),
        m if m == P61 as u128 => "2^61-1".into(),
        m => m.to_string(),
    }
}

fn settings(command: &str, commit: &str, format: u64) -> String {
    format!("command {command}\ncommit {commit}\nformat {format}\n")
}

/// "1 to 3, 7" for 1, 2, 3, 7.
fn ranges(ws: &[usize]) -> String {
    let mut parts = Vec::new();
    let mut i = 0;
    while i < ws.len() {
        let mut j = i;
        while j + 1 < ws.len() && ws[j + 1] == ws[j] + 1 {
            j += 1;
        }
        parts.push(if j > i {
            format!("{} to {}", ws[i], ws[j])
        } else {
            ws[i].to_string()
        });
        i = j + 1;
    }
    parts.join(", ")
}

/// Little-endian fields of a checkpoint file.
struct Fields(Vec<u8>);

impl Fields {
    /// A file of kind `magic` for strip w of a sweep to n, mod R.
    fn new<R: Ring>(magic: &[u8; 8], w: usize, n: usize, merge: bool) -> Fields {
        let mut f = Fields(magic.to_vec());
        let m = R::MODULUS;
        for x in [
            FORMAT,
            w as u64,
            n as u64,
            merge as u64,
            m as u64,
            (m >> 64) as u64,
        ] {
            f.u64(x);
        }
        f.u64(R::BYTES as u64);
        f
    }

    fn u64(&mut self, x: u64) {
        self.0.extend(x.to_le_bytes());
    }

    /// The closure and two R2 tables, row by row.
    fn tables<R: Ring>(&mut self, closures: &[Vec<R>], r2: &[Vec<Vec<R>>; 2]) {
        let mut buf = [0u8; 8];
        for table in [closures, &r2[0], &r2[1]] {
            for &x in table.iter().flatten() {
                x.write(&mut buf);
                self.0.extend(&buf[..R::BYTES]);
            }
        }
    }
}

/// The closure and two R2 tables of a strip.
type Tables<R> = (Vec<Vec<R>>, [Vec<Vec<R>>; 2]);

/// Reads what `Fields` wrote.
struct Parse<'a>(&'a [u8]);

impl<'a> Parse<'a> {
    /// The fields after the header `Fields::new` writes, if `file` is sealed and starts with it.
    fn new<R: Ring>(
        file: &'a [u8],
        magic: &[u8; 8],
        w: usize,
        n: usize,
        merge: bool,
    ) -> Option<Self> {
        let head = Fields::new::<R>(magic, w, n, merge).0;
        Some(Parse(unseal(file)?.strip_prefix(&head[..])?))
    }

    fn u64(&mut self) -> Option<u64> {
        let (x, rest) = self.0.split_first_chunk::<8>()?;
        self.0 = rest;
        Some(u64::from_le_bytes(*x))
    }

    fn table<R: Ring>(&mut self, n: usize) -> Option<Vec<Vec<R>>> {
        let (cells, rest) = self.0.split_at_checked((n + 1) * (n + 2) * R::BYTES)?;
        self.0 = rest;
        let mut it = cells.chunks_exact(R::BYTES).map(R::read);
        Some((0..=n).map(|_| it.by_ref().take(n + 2).collect()).collect())
    }

    fn tables<R: Ring>(&mut self, n: usize) -> Option<Tables<R>> {
        Some((self.table(n)?, [self.table(n)?, self.table(n)?]))
    }
}

/// What a unit file must start with, before its counts.
fn unit_header(n: usize, u: &Unit) -> Vec<u8> {
    let mut f = Fields(UNIT_MAGIC.to_vec());
    for x in [FORMAT, n as u64, u.job as u64, u.run as u64, u.parts, u.id] {
        f.u64(x);
    }
    f.0
}

fn encode_unit(n: usize, u: &Unit, counts: &[u64]) -> Vec<u8> {
    let mut f = Fields(unit_header(n, u));
    counts.iter().for_each(|&x| f.u64(x));
    seal(f.0)
}

fn decode_unit(file: &[u8], n: usize, u: &Unit) -> Option<Vec<u64>> {
    let mut p = Parse(unseal(file)?.strip_prefix(&unit_header(n, u)[..])?);
    let counts = (0..=n).map(|_| p.u64()).collect::<Option<Vec<_>>>()?;
    p.0.is_empty().then_some(counts)
}

fn encode_strip<R: Ring>(s: &Strip<R>, w: usize, n: usize, merge: bool) -> Vec<u8> {
    let mut f = Fields::new::<R>(STRIP_MAGIC, w, n, merge);
    f.u64(s.steps);
    f.u64(s.peak as u64);
    f.tables(&s.closures, &s.r2);
    seal(f.0)
}

fn decode_strip<R: Ring>(file: &[u8], w: usize, n: usize, merge: bool) -> Option<Strip<R>> {
    let mut p = Parse::new::<R>(file, STRIP_MAGIC, w, n, merge)?;
    let (steps, peak) = (p.u64()?, p.u64()? as usize);
    let (closures, r2) = p.tables(n)?;
    p.0.is_empty().then_some(Strip {
        closures,
        r2,
        steps,
        peak,
        disk: DiskStats::default(),
    })
}

fn encode_column<R: Ring>(col: &Column<R>, w: usize, n: usize, merge: bool) -> Vec<u8> {
    let mut f = Fields::new::<R>(COLUMN_MAGIC, w, n, merge);
    for x in [
        col.c as u64,
        col.steps,
        col.peak as u64,
        col.prev.len() as u64,
    ] {
        f.u64(x);
    }
    col.prev.iter().for_each(|&x| f.u64(x as u64));
    f.u64(col.files.len() as u64);
    for &(records, sum) in &col.files {
        f.u64(records);
        f.u64(sum);
    }
    f.tables(&col.closures, &col.r2);
    seal(f.0)
}

fn decode_column<R: Ring>(file: &[u8], w: usize, n: usize, merge: bool) -> Option<Column<R>> {
    let mut p = Parse::new::<R>(file, COLUMN_MAGIC, w, n, merge)?;
    let (c, steps, peak) = (p.u64()? as usize, p.u64()?, p.u64()? as usize);
    let prev = (0..p.u64()?)
        .map(|_| Some(p.u64()? as usize))
        .collect::<Option<_>>()?;
    let files = (0..p.u64()?)
        .map(|_| Some((p.u64()?, p.u64()?)))
        .collect::<Option<_>>()?;
    let (closures, r2) = p.tables(n)?;
    p.0.is_empty().then_some(Column {
        c,
        steps,
        peak,
        prev,
        closures,
        r2,
        files,
    })
}

/// Writes `count` records of `rec` bytes to a new file at `path`, `fill(i, out)` writing record
/// i, in pieces of about `chunk` bytes, and flushes it to disk. Returns the file's checksum.
pub(crate) fn write_records(
    path: &Path,
    rec: usize,
    count: usize,
    chunk: usize,
    fill: impl Fn(usize, &mut [u8]),
) -> io::Result<u64> {
    let mut f = File::create(path)?;
    let per = (chunk / rec).max(1);
    let mut buf = vec![0u8; per.min(count) * rec];
    let mut sum = Sum::default();
    for lo in (0..count).step_by(per) {
        let piece = &mut buf[..((lo + per).min(count) - lo) * rec];
        for (i, out) in piece.chunks_exact_mut(rec).enumerate() {
            fill(lo + i, out);
        }
        sum.add(piece);
        f.write_all(piece)?;
    }
    sync(&f)?;
    Ok(sum.value())
}

/// Reads the `count` records of `rec` bytes that `write_records` wrote to `path`, passing them
/// to `each` in pieces of about `chunk` bytes. Errs if the file is short, long or damaged.
pub(crate) fn read_records(
    path: &Path,
    rec: usize,
    count: u64,
    sum: u64,
    chunk: usize,
    mut each: impl FnMut(&[u8]),
) -> Result<(), String> {
    let name = path.display();
    let mut f = File::open(path).map_err(|e| format!("{name}: {e}"))?;
    let mut buf = vec![0u8; (chunk / rec).max(1).min(count as usize) * rec];
    let (mut left, mut got_sum) = (count as usize * rec, Sum::default());
    while left > 0 {
        let len = left.min(buf.len());
        let piece = &mut buf[..len];
        f.read_exact(piece).map_err(|e| format!("{name}: {e}"))?;
        got_sum.add(piece);
        each(piece);
        left -= piece.len();
    }
    let longer = f.read(&mut [0u8]).map_err(|e| format!("{name}: {e}"))? > 0;
    if longer || got_sum.value() != sum {
        return Err(format!("{name} is damaged"));
    }
    Ok(())
}

/// A running checksum over 8-byte words. Each step is a bijection of the running value, so a
/// change to any one word always shows.
#[derive(Clone, Default)]
pub(crate) struct Sum {
    h: u64,
    len: u64,
    tail: [u8; 8],
}

impl Sum {
    fn word(&mut self, x: u64) {
        self.h = (self.h.rotate_left(5) ^ x).wrapping_mul(0x517c_c1b7_2722_0a95);
    }

    pub(crate) fn add(&mut self, mut bytes: &[u8]) {
        let held = (self.len % 8) as usize;
        self.len += bytes.len() as u64;
        if held > 0 {
            let k = (8 - held).min(bytes.len());
            self.tail[held..held + k].copy_from_slice(&bytes[..k]);
            bytes = &bytes[k..];
            if held + k < 8 {
                return;
            }
            self.word(u64::from_le_bytes(self.tail));
        }
        let mut words = bytes.chunks_exact(8);
        for x in &mut words {
            self.word(u64::from_le_bytes(x.try_into().unwrap()));
        }
        let rest = words.remainder();
        self.tail = [0; 8];
        self.tail[..rest.len()].copy_from_slice(rest);
    }

    pub(crate) fn value(mut self) -> u64 {
        if !self.len.is_multiple_of(8) {
            self.word(u64::from_le_bytes(self.tail));
        }
        self.word(self.len);
        self.h
    }
}

fn checksum(bytes: &[u8]) -> u64 {
    let mut sum = Sum::default();
    sum.add(bytes);
    sum.value()
}

/// Appends the checksum of `bytes`.
fn seal(mut bytes: Vec<u8>) -> Vec<u8> {
    let sum = checksum(&bytes);
    bytes.extend(sum.to_le_bytes());
    bytes
}

/// The contents of a sealed file, if its checksum holds.
fn unseal(file: &[u8]) -> Option<&[u8]> {
    let (body, sum) = file.split_at_checked(file.len().checked_sub(8)?)?;
    (checksum(body) == u64::from_le_bytes(sum.try_into().unwrap())).then_some(body)
}

fn tmp_path(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap().to_string_lossy();
    path.with_file_name(format!("{name}.tmp"))
}

/// Puts a hard link to `from` at `to`, or a copy where `to` is on another volume, and flushes
/// it to disk.
pub(crate) fn link_or_copy(from: &Path, to: &Path) -> io::Result<()> {
    if fs::hard_link(from, to).is_err() {
        fs::copy(from, to)?;
    }
    sync(&File::open(to)?)
}

/// Flushes a file to disk with fsync. On macOS, `sync_all` also flushes the drive's cache
/// (F_FULLFSYNC), which costs tens of milliseconds a file.
fn sync(f: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: fsync only reads the open descriptor.
    match unsafe { libc::fsync(f.as_raw_fd()) } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

/// Flushes a directory's entries, where the system allows that, so that renames in it last.
fn sync_dir(dir: &Path) {
    let _ = File::open(dir).and_then(|d| sync(&d));
}

/// Writes `bytes` to `path` whole or not at all: under a temporary name, with `durable` flushed
/// to disk, then renamed.
fn write_whole(path: &Path, bytes: &[u8], durable: bool) -> io::Result<()> {
    let tmp = tmp_path(path);
    let written = File::create(&tmp).and_then(|mut f| {
        f.write_all(bytes)?;
        if durable { sync(&f) } else { Ok(()) }
    });
    if let Err(e) = written.and_then(|()| fs::rename(&tmp, path)) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if durable {
        sync_dir(path.parent().unwrap());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_join_runs() {
        assert_eq!(ranges(&[1, 2, 3, 7]), "1 to 3, 7");
        assert_eq!(ranges(&[4]), "4");
        assert_eq!(ranges(&[2, 3, 5, 6]), "2 to 3, 5 to 6");
    }

    #[test]
    fn a_changed_or_cut_file_fails_its_checksum() {
        let file = seal((0..100u8).collect());
        assert!(unseal(&file).is_some());
        for i in 0..file.len() {
            let mut bad = file.clone();
            bad[i] ^= 0x10;
            assert!(unseal(&bad).is_none(), "byte {i} changed");
        }
        for len in 0..file.len() {
            assert!(unseal(&file[..len]).is_none(), "cut to {len} bytes");
        }
    }

    #[test]
    fn the_checksum_does_not_depend_on_how_bytes_arrive() {
        let bytes: Vec<u8> = (0..203u32).map(|i| (i * 37 % 251) as u8).collect();
        let whole = checksum(&bytes);
        for piece in [1, 2, 3, 5, 8, 13, 64] {
            let mut sum = Sum::default();
            bytes.chunks(piece).for_each(|p| sum.add(p));
            assert_eq!(sum.value(), whole, "pieces of {piece}");
        }
    }
}
