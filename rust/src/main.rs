use a000104::checkpoint::Checkpoint;
use a000104::ring::{MODULI_16, Ring, WithRing16, Wrap16, Wrap64, with_ring_16};
use a000104::spill::{self, SpillDir};
use a000104::tm::{Force, Limit};
use a000104::{
    Counts, Terms, bfile, brute, fixed_counts, fixed_from_bfile, resume_summary, search, terms, tm,
};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

#[global_allocator]
static ALLOC: spill::Allocator = spill::Allocator;

/// Free polyominoes without holes (OEIS A000104) and the counts behind them.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print a(0..=N) in b-file format.
    Terms(Run),
    /// Print n, F, R2, R4, M, D and a(n) for n = 0..=N.
    Components(Run),
    /// Print F(1..=N), the fixed hole-free polyominoes (A006724), in b-file format.
    Fixed {
        n: usize,
        /// Skip merging states with their mirror images.
        #[arg(long)]
        no_merge: bool,
        #[command(flatten)]
        counts: CountsArg,
        #[command(flatten)]
        spill: SpillArgs,
        #[command(flatten)]
        checkpoint: CheckpointArgs,
    },
    /// Print F(n) and R2(n) modulo 2^16 from the transfer matrix, for n = 1..=N.
    ///
    /// Uses a quarter of the memory of `fixed`, for checking known values at sizes too
    /// large to compute exactly.
    Residues {
        n: usize,
        /// Skip merging states with their mirror images.
        #[arg(long)]
        no_merge: bool,
        /// 65536, or one of the primes 65521, 65519, 65497, 65479, 65449, 65447, 65437.
        #[arg(long, default_value_t = 65536)]
        modulus: u32,
        #[command(flatten)]
        spill: SpillArgs,
        #[command(flatten)]
        checkpoint: CheckpointArgs,
    },
    /// Print the brute-force counts n, F, a(n), R2, R4, M, D for n = 1..=N (small N only).
    Brute { n: usize },
}

#[derive(Args)]
struct Run {
    n: usize,
    /// Read F from this A006724 b-file instead of running the transfer matrix.
    #[arg(long, value_name = "FILE")]
    fixed_from: Option<PathBuf>,
    /// Where R2 comes from; `both` computes it both ways and requires them to agree.
    #[arg(long, value_enum)]
    r2: Option<R2Source>,
    /// Skip merging states with their mirror images.
    #[arg(long)]
    no_merge: bool,
    /// Compare with the OEIS b-files b000104.original.txt and b006724.original.ERROR.txt in DIR.
    #[arg(long, value_name = "DIR")]
    check: Option<PathBuf>,
    #[command(flatten)]
    counts: CountsArg,
    #[command(flatten)]
    spill: SpillArgs,
    #[command(flatten)]
    checkpoint: CheckpointArgs,
}

#[derive(Args)]
struct CountsArg {
    /// The sweep's counts: 64 (runs modulo 2^64 and 2^61 - 1) or 16 (one run per 16-bit
    /// modulus, as many as F(n) < 4^n needs, each in about a third of the memory).
    #[arg(long, value_name = "BITS", default_value = "64", value_parser = parse_counts)]
    counts: Counts,
}

fn parse_counts(s: &str) -> Result<Counts, String> {
    match s {
        "64" => Ok(Counts::Bits64),
        "16" => Ok(Counts::Bits16),
        _ => Err(format!("{s}: expected 16 or 64")),
    }
}

fn counts_flag(counts: Counts) -> &'static str {
    match counts {
        Counts::Bits64 => "",
        Counts::Bits16 => " --counts 16",
    }
}

/// `prepare` and the checkpoints' disk estimate for the counts' record size.
fn prepare_for(
    spill: &SpillArgs,
    n: usize,
    merge: bool,
    counts: Counts,
) -> Result<Option<(SpillDir, usize, Force)>, String> {
    match counts {
        Counts::Bits64 => prepare::<Wrap64>(spill, n, merge),
        Counts::Bits16 => prepare::<Wrap16>(spill, n, merge),
    }
}

fn checkpoint_need(n: usize, merge: bool, counts: Counts) -> u64 {
    match counts {
        Counts::Bits64 => tm::checkpoint_estimate::<Wrap64>(n, merge),
        Counts::Bits16 => tm::checkpoint_estimate::<Wrap16>(n, merge),
    }
}

#[derive(Args)]
struct SpillArgs {
    /// Keep memory under SIZE (K, M or G, powers of 1024), moving the sweep's states to disk
    /// past it.
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    mem_limit: Option<usize>,
    /// Put the spill's bucket files in a new directory under DIR [default: the system
    /// temporary directory].
    #[arg(long, value_name = "DIR", requires = "mem_limit")]
    spill_dir: Option<PathBuf>,
    /// For tests: every column on disk, in N buckets.
    #[arg(long, value_name = "N", requires = "mem_limit", hide = true)]
    force_buckets: Option<usize>,
}

#[derive(Args)]
struct CheckpointArgs {
    /// Save finished work in DIR, and resume from it when the same command runs again.
    #[arg(long, value_name = "DIR")]
    checkpoint_dir: Option<PathBuf>,
    /// The most time between checkpoints inside a strip (s, m or h).
    #[arg(long, value_name = "TIME", default_value = "30m", value_parser = parse_duration,
          requires = "checkpoint_dir")]
    checkpoint_every: Duration,
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    let (digits, unit) = match s.char_indices().last() {
        Some((i, 's')) => (&s[..i], 1),
        Some((i, 'm')) => (&s[..i], 60),
        Some((i, 'h')) => (&s[..i], 3600),
        _ => (s, 1),
    };
    digits
        .parse::<u64>()
        .ok()
        .and_then(|v| v.checked_mul(unit))
        .map(Duration::from_secs)
        .ok_or(format!("{s}: expected a time such as 90s, 20m or 2h"))
}

/// The checkpoint directory for `command`, or the refusal with its fix: another run's directory,
/// or too little free disk for `need` bytes of column checkpoints. On resume, prints `summary`
/// of what it holds.
fn open_checkpoint(
    args: &CheckpointArgs,
    command: &str,
    need: u64,
    summary: impl FnOnce(&Checkpoint) -> String,
) -> Result<Option<Checkpoint>, String> {
    let Some(dir) = &args.checkpoint_dir else {
        return Ok(None);
    };
    let ck = Checkpoint::open(dir, command, args.checkpoint_every)?;
    let need = need.saturating_sub(ck.column_bytes());
    let free = spill::free_bytes(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if need > free {
        if !ck.resumed() {
            ck.remove_new();
        }
        return Err(format!(
            "checkpoints need about {} in {}, which has {} free; pass --checkpoint-dir on a \
             larger volume",
            disk_text(need),
            dir.display(),
            disk_text(free)
        ));
    }
    eprintln!("# checkpoint directory {}", ck.path().display());
    if ck.resumed() {
        let summary = summary(&ck);
        if !summary.is_empty() {
            eprintln!("# resuming: {summary}");
        }
    }
    Ok(Some(ck))
}

fn no_merge_flag(merge: bool) -> &'static str {
    if merge { "" } else { " --no-merge" }
}

/// The part of `--mem-limit` kept for code, stacks and allocator slack.
const RESERVE: usize = 64 << 20;

fn parse_size(s: &str) -> Result<usize, String> {
    let (digits, shift) = match s.char_indices().last() {
        Some((i, 'K' | 'k')) => (&s[..i], 10),
        Some((i, 'M' | 'm')) => (&s[..i], 20),
        Some((i, 'G' | 'g')) => (&s[..i], 30),
        _ => (s, 0),
    };
    digits
        .parse::<usize>()
        .ok()
        .and_then(|v| v.checked_mul(1 << shift))
        .ok_or(format!("{s}: expected a size such as 512M or 24G"))
}

/// A size for `--mem-limit`, rounded up: whole G from 4G, else a multiple of 256M.
fn size_arg(bytes: usize) -> String {
    if bytes > 4 << 30 {
        format!("{}G", bytes.div_ceil(1 << 30))
    } else {
        format!("{}M", bytes.div_ceil(256 << 20) * 256)
    }
}

/// Bytes on disk, in decimal units as `df` gives them.
fn disk_text(bytes: u64) -> String {
    let gb = bytes as f64 / 1e9;
    if gb >= 10.0 {
        format!("{gb:.0} GB")
    } else {
        format!("{gb:.1} GB")
    }
}

/// The spill directory, heap budget and test settings for `--mem-limit`, or the refusal with
/// its fix.
fn prepare<R: Ring>(
    args: &SpillArgs,
    n: usize,
    merge: bool,
) -> Result<Option<(SpillDir, usize, Force)>, String> {
    let Some(limit) = args.mem_limit else {
        return Ok(None);
    };
    let floor = tm::floor_budget::<R>(n) + RESERVE;
    if limit < floor {
        return Err(format!(
            "n = {n} needs --mem-limit of at least {}",
            size_arg(floor)
        ));
    }
    let budget = limit - RESERVE;
    let base = args.spill_dir.clone().unwrap_or_else(std::env::temp_dir);
    if let Some(need) = tm::disk_estimate::<R>(n, merge, budget) {
        let free = spill::free_bytes(&base).map_err(|e| format!("{}: {e}", base.display()))?;
        if need > free {
            return Err(format!(
                "the spill needs about {} in {}, which has {} free; pass --spill-dir on a \
                 larger volume",
                disk_text(need),
                base.display(),
                disk_text(free)
            ));
        }
    }
    let dir = SpillDir::create(&base).map_err(|e| format!("{}: {e}", base.display()))?;
    eprintln!("# spill directory {}", dir.path().display());
    spill::catch_interrupts();
    let force = Force {
        buckets: args.force_buckets,
        ..Force::default()
    };
    Ok(Some((dir, budget, force)))
}

fn limit_of(prepared: &Option<(SpillDir, usize, Force)>) -> Option<Limit<'_>> {
    prepared.as_ref().map(|(dir, budget, force)| Limit {
        budget: *budget,
        dir,
        force: *force,
    })
}

#[derive(Clone, Copy, PartialEq, ValueEnum)]
enum R2Source {
    Tm,
    Search,
    Both,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if std::env::args().any(|a| a == "--mem-limit" || a.starts_with("--mem-limit=")) {
        spill::map_large_blocks();
        let default = std::panic::take_hook();
        let shown = std::sync::atomic::AtomicBool::new(false);
        std::panic::set_hook(Box::new(move |info| {
            // Threads that fail after the first, on its full disk or poisoned locks, add only noise.
            if shown.swap(true, std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            if info.payload().downcast_ref::<&str>() == Some(&"interrupted") {
                eprintln!("error: interrupted; removing the spill directory");
            } else {
                default(info);
            }
        }));
    }
    match cli.command {
        Command::Terms(run) => run_terms(run, false),
        Command::Components(run) => run_terms(run, true),
        Command::Fixed {
            n,
            no_merge,
            counts,
            spill,
            checkpoint,
        } => match fixed(n, !no_merge, counts.counts, &spill, &checkpoint) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        },
        Command::Residues {
            n,
            no_merge,
            modulus,
            spill,
            checkpoint,
        } => {
            let job = ResiduesRun {
                n,
                merge: !no_merge,
                spill: &spill,
                checkpoint: &checkpoint,
            };
            let done = with_ring_16(modulus, job).unwrap_or_else(|| {
                Err(format!(
                    "unsupported modulus {modulus}; the moduli are {MODULI_16:?}"
                ))
            });
            match done {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Brute { n } => {
            for (w, r) in brute::brute(n).iter().enumerate().skip(1) {
                println!(
                    "{w} {} {} {} {} {} {}",
                    r.fixed, r.free, r.r2, r.r4, r.m, r.d
                );
            }
            ExitCode::SUCCESS
        }
    }
}

/// Prints F(1..=n) in b-file format.
fn fixed(
    n: usize,
    merge: bool,
    counts: Counts,
    spill: &SpillArgs,
    checkpoint: &CheckpointArgs,
) -> Result<(), String> {
    let command = format!("fixed {n}{}{}", no_merge_flag(merge), counts_flag(counts));
    let need = checkpoint_need(n, merge, counts);
    let ck = open_checkpoint(checkpoint, &command, need, |ck| {
        resume_summary(ck, n, counts)
    })?;
    let prepared = prepare_for(spill, n, merge, counts)?;
    let t = Instant::now();
    let fc = fixed_counts(n, merge, counts, limit_of(&prepared).as_ref(), ck.as_ref());
    eprintln!(
        "# transfer matrix {:.1}s ({} state-steps, peak {} states)",
        t.elapsed().as_secs_f64(),
        fc.steps,
        fc.peak
    );
    for (w, v) in fc.f.iter().enumerate().skip(1) {
        println!("{w} {v}");
    }
    Ok(())
}

/// A `residues` run, for the ring of its modulus.
struct ResiduesRun<'a> {
    n: usize,
    merge: bool,
    spill: &'a SpillArgs,
    checkpoint: &'a CheckpointArgs,
}

impl WithRing16 for ResiduesRun<'_> {
    type Out = Result<(), String>;

    fn run<R: Ring>(self, modulus: u32) -> Self::Out {
        residues::<R>(self.n, self.merge, modulus, self.spill, self.checkpoint)
    }
}

/// Prints n, F(n) and R2(n) modulo `modulus` (the modulus of R), after a "# modulus" line.
fn residues<R: Ring>(
    n: usize,
    merge: bool,
    modulus: u32,
    spill: &SpillArgs,
    checkpoint: &CheckpointArgs,
) -> Result<(), String> {
    let command = format!("residues {n} --modulus {modulus}{}", no_merge_flag(merge));
    let need = tm::checkpoint_estimate::<R>(n, merge);
    let ck = open_checkpoint(checkpoint, &command, need, |ck| {
        ck.describe::<R>(&format!("mod {modulus}"), n)
    })?;
    let prepared = prepare::<R>(spill, n, merge)?;
    let t = Instant::now();
    let fx = tm::fixed_with::<R>(
        n,
        merge,
        limit_of(&prepared).as_ref(),
        ck.as_ref(),
        a000104::progress("residues"),
    );
    eprintln!(
        "# transfer matrix mod {modulus} {:.1}s ({} state-steps, peak {} states)",
        t.elapsed().as_secs_f64(),
        fx.steps,
        fx.peak
    );
    println!("# modulus {modulus}");
    for w in 1..=n {
        let r2 = fx.r2.iter().fold(R::ZERO, |acc, part| acc.add(part[w]));
        println!("{w} {} {}", fx.f[w].residue(), r2.residue());
    }
    Ok(())
}

fn run_terms(run: Run, components: bool) -> ExitCode {
    let n = run.n;
    let source = run.r2.unwrap_or(if run.fixed_from.is_some() {
        R2Source::Search
    } else {
        R2Source::Tm
    });
    if run.fixed_from.is_some() && source != R2Source::Search {
        eprintln!("error: with --fixed-from, R2 comes from the search (--r2 search)");
        return ExitCode::FAILURE;
    }
    let counts = run.counts.counts;
    if run.fixed_from.is_some() && counts == Counts::Bits16 {
        eprintln!("error: --counts 16 sets the sweep's counts, and --fixed-from runs no sweep");
        return ExitCode::FAILURE;
    }
    let command = format!(
        "{} {n}{}{}{}",
        if components { "components" } else { "terms" },
        no_merge_flag(!run.no_merge),
        match source {
            R2Source::Tm => "",
            R2Source::Search => " --r2 search",
            R2Source::Both => " --r2 both",
        },
        counts_flag(counts)
    );
    let sweeps = run.fixed_from.is_none();
    let need = if sweeps {
        checkpoint_need(n, !run.no_merge, counts)
    } else {
        0
    };
    let with_r2 = source != R2Source::Tm;
    let ck = match open_checkpoint(&run.checkpoint, &command, need, |ck| {
        let search = ck.describe_search(&search::units(n, with_r2));
        if sweeps {
            format!("{}; {search}", resume_summary(ck, n, counts))
        } else {
            search
        }
    }) {
        Ok(ck) => ck,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let t = Instant::now();
    let (f, r2_tm) = match &run.fixed_from {
        Some(path) => match bfile::read(path)
            .map_err(|e| e.to_string())
            .and_then(|b| fixed_from_bfile(&b, n))
        {
            Ok(f) => (f, None),
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::FAILURE;
            }
        },
        None => {
            let prepared = match prepare_for(&run.spill, n, !run.no_merge, counts) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let fc = fixed_counts(
                n,
                !run.no_merge,
                counts,
                limit_of(&prepared).as_ref(),
                ck.as_ref(),
            );
            eprintln!(
                "# transfer matrix {:.1}s ({} state-steps, peak {} states)",
                t.elapsed().as_secs_f64(),
                fc.steps,
                fc.peak
            );
            let r2 = fc.r2();
            (fc.f, Some(r2))
        }
    };
    let t = Instant::now();
    let sym = search::symmetric(n, with_r2, ck.as_ref());
    eprintln!("# symmetric search {:.1}s", t.elapsed().as_secs_f64());
    let mut ok = true;
    if source == R2Source::Both {
        let tm = r2_tm.as_ref().unwrap();
        let bad: Vec<usize> = (1..=n).filter(|&w| tm[w] != sym.r2[w] as u128).collect();
        eprintln!(
            "# R2, transfer matrix vs search: {}",
            if bad.is_empty() {
                "agree".into()
            } else {
                format!("DIFFER at {bad:?}")
            }
        );
        ok &= bad.is_empty();
    }
    let r2 = if source == R2Source::Search {
        None
    } else {
        r2_tm.as_deref()
    };
    let out = terms(n, &f, r2, &sym);
    if !out.bad.is_empty() {
        eprintln!(
            "# Burnside sum not a multiple of 8 at n = {:?}; a(n) printed as 0 there",
            out.bad
        );
        ok = false;
    }
    if let Some(dir) = &run.check {
        ok &= check(dir, &out);
    }
    for w in 0..=n {
        if components {
            println!(
                "{w} {} {} {} {} {} {}",
                out.f[w], out.r2[w], out.r4[w], out.m[w], out.d[w], out.a[w]
            );
        } else {
            println!("{w} {}", out.a[w]);
        }
    }
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn check(dir: &std::path::Path, out: &Terms) -> bool {
    let mut ok = true;
    for (name, values, first) in [
        ("b000104.original.txt", &out.a, 0),
        ("b006724.original.ERROR.txt", &out.f, 1),
    ] {
        match bfile::read(&dir.join(name)) {
            Ok(b) => {
                let bad: Vec<usize> = (first..values.len())
                    .filter(|w| b.get(w).is_some_and(|v| *v != values[*w]))
                    .collect();
                let shared = (first..values.len()).filter(|w| b.contains_key(w)).count();
                eprintln!(
                    "# {name}: {shared} shared terms, {}",
                    if bad.is_empty() {
                        "all agree".into()
                    } else {
                        format!("DIFFER at {bad:?}")
                    }
                );
                ok &= bad.is_empty();
            }
            Err(e) => {
                eprintln!("# {name}: not read ({e})");
                ok = false;
            }
        }
    }
    ok
}
