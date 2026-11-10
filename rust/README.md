# a000104 (Rust)

Rust port of `../python/a000104.py`. It counts free polyominoes without holes (OEIS A000104) by
Burnside's lemma:

```
8 a(n) = F(n) + R2(n) + 2 R4(n) + 2 M(n) + 2 D(n)
```

The terms come from two places:

- **F and R2:** a transfer matrix over self-avoiding polygons by area. F is A006724. R2 is read
  off the same sweep, by a half-turn gluing test.
- **R4, M and D:** a Redelmeier search over symmetry orbits, which can also compute R2 as a
  check.

The method and the plan are in `../a000104.md`.

## Usage

```sh
cargo build --release
B=target/release/a000104

$B terms 28 --check ../data                         # a(0..=28), checked against the OEIS b-files in ../data
$B components 32                                   # n, F, R2, R4, M, D, a(n)
$B components 41 --fixed-from ../data/b006724.txt  # F from the A006724 b-file; R2, R4, M, D by search
$B fixed 32                                        # F(1..=32), A006724, in b-file format
$B fixed 42 --counts 16                            # the same from six 16-bit runs, in a third of the memory
$B terms 24 --r2 both                              # R2 from the sweep and from the search, which must agree
$B brute 10                                        # brute-force counts for small n
```

Timings and work counts go to stderr. `--no-merge` turns off the mirror merge; the
transfer-matrix state-steps and peak states then equal the Python program's exactly.

## 16-bit counts

`--counts 16` on `fixed`, `terms` and `components` runs the sweep with 16-bit counts: one run
per 16-bit modulus, one after another, combined by the CRT. The output is the same as with
`--counts 64`, the default, which runs modulo 2^64 and 2^61 - 1.

- **Memory:** each run needs about a third of the memory of a 64-bit one (220 bytes a peak
  state, against about 630).
- **Runs:** the moduli are 65536, then the primes 65521, 65519, 65497, 65479, 65449, 65447 and
  65437, taken in order until their product passes 4^n, which bounds F(n) and R2(n): 4 runs
  for n = 24 to 31, 5 for 32 to 39, 6 for 40 to 47, 7 for 48 to 55 and 8 for 56 to 60.
- **With the memory limit and checkpoints,** each run spills and saves as a 64-bit one does,
  with a strip file per modulus. `--fixed-from` runs no sweep, so it refuses `--counts 16`.

## Memory limit

`--mem-limit SIZE` (K, M or G, powers of 1024) works on `fixed`, `residues`, `terms` and
`components`. The sweep stays in RAM while it fits. Past that, its states move between steps
through bucket files on disk, and the output is the same as without the flag.

```sh
$B residues 43 --modulus 65521 --mem-limit 24G
$B residues 43 --modulus 65521 --mem-limit 24G --spill-dir /Volumes/Scratch
$B fixed 40 --mem-limit 4G
```

- **Memory:** the heap stays under SIZE minus 64 MiB, which is kept for code, stacks and
  allocator slack. Blocks of 64 KiB or more are mapped straight from the system, so freed ones
  leave the resident size at once.
- **Bucket files** go in a new directory under `--spill-dir` (default: the system temporary
  directory). The run prints its path, and removes it at the end, also after an error or
  Ctrl-C.
- **Refusals,** before the run starts:
  - a limit below the least the run needs. With 16-bit counts (`residues`) that is 256M at
    n = 36 and 1536M at n = 43 to 46; with 64-bit counts (`fixed`, `terms`, `components`),
    512M and 3072M to 3328M;
  - a spill predicted to need more disk than the directory's volume has free.
- **Progress lines** add, for each strip that spilled, its columns on disk, bytes written, peak
  bytes on disk and window halvings.

## Checkpoints

`--checkpoint-dir DIR` works on `fixed`, `residues`, `terms` and `components`. The sweep and
the search save their progress in DIR, and the same command run again carries on from it,
printing the same output as a run that was never stopped.

```sh
$B components 46 --r2 both --checkpoint-dir /var/a000104-checkpoint
$B components 46 --r2 both --checkpoint-dir /var/a000104-checkpoint --checkpoint-every 20m
```

- **The first run** creates DIR and a settings file in it: the command (n, `--no-merge`, `--r2`,
  `--modulus`), the commit of the build and the file format. A build with uncommitted changes
  in `rust/` adds a hash of its sources to the commit. Outside git, as in the EC2 bundle, the
  build takes its commit from the `A000104_COMMIT` variable.
- **Strip files,** `strip-<w>.<modulus>`, hold each finished strip's tables. The modulus is
  `2^64`, `2^61-1`, `65536` or a 16-bit prime. A sweep whose strips are all saved is skipped.
- **Column checkpoints,** `column-<w>-<c>.<modulus>/`, hold the strip in progress at the start
  of column c: its layer, one `layer-<k>` file of records (key and counts) for each shard, and a
  manifest with the tables and counters so far. One is written at the first column boundary
  after `--checkpoint-every` (default 30m, in s, m or h) since progress was last saved, and
  replaces the one before. A finished strip's strip file replaces its column checkpoint. The
  layer files load back as shards on any thread count.
- **Search units,** `unit-<job>-<run>-<part>`, hold each finished unit's counts. A deep run of
  the search splits into 4^(depth - 11) parts, up to 1024, by its depth alone, so the units are
  the same on every machine and thread count. Unit files are not flushed to disk: a unit that a
  crash loses runs again.
- **On resume** the run prints what DIR holds, such as `# resuming: mod 2^64 done; mod 2^61 - 1
  strips 1 to 15 done, strip 16 from column 5; search not started`.
- **Every file** is written under a temporary name, flushed with fsync and renamed, and is
  covered by a checksum. A damaged strip file or column checkpoint is reported and not used.
  On macOS, fsync leaves the drive's cache, so a power cut can damage the newest files; the
  checksums catch it.
- **Progress lines** add each column checkpoint, with its size and write time.
- **Under `--mem-limit`,** a column on disk is saved as hard links to its bucket files, or as
  copies where the spill is on another volume. A resumed layer loads into RAM when a step on it
  fits in half the room, and otherwise goes back to disk as the column's first buckets.
- **Refusals,** with their fix: a DIR of another command, build or format; a DIR that holds
  other files; and too little free disk in DIR for two column checkpoints of the widest strip.

## Layout

- `src/ring.rs`: counts modulo 2^64, 2^61 - 1 or a 16-bit modulus, and the CRT that combines
  runs.
- `src/tm.rs`: the transfer matrix (F, R2), sharded across threads within each strip, and its
  columns on disk under a memory limit.
- `src/spill.rs`: bucket files and the spill directory, the free-space and open-file checks,
  and the allocator that maps large blocks.
- `src/checkpoint.rs`: the checkpoint directory's settings file, strip files, column
  checkpoints and search units.
- `build.rs`: the commit a checkpoint is tied to.
- `src/search.rs`: the orbit search (R2, R4, M, D), split by branch into units that the threads
  share.
- `src/brute.rs`: brute force over all fixed polyominoes, for the tests.
- `src/bfile.rs`, `src/lib.rs`, `src/main.rs`: b-files, the Burnside assembly, the CLI.
- `tests/known.rs`: checks against the OEIS data sections, the Python step counts and the
  brute force.
- `tests/checkpoint.rs`: sweeps killed between strips and resumed, against whole ones, and the
  checks on damaged files and on the settings.
- `tests/kill.rs`: runs killed with SIGKILL and run again: after a column checkpoint, with a
  damaged one, from RAM to disk and back, inside column writes, and in the search; and the
  unit files of 2 and 14 threads, which must be the same. Ignored, as they take minutes: kills at random
  times at n = 30 to 34, in RAM, on disk, and with the spill on another volume (a directory
  there in `A000104_OTHER_VOLUME`).
- `tests/spill.rs`: the bucket files, and sweeps on disk against the sweep in RAM, with a
  counting allocator for the heap limit.

Run the tests with `cargo test --release`.
