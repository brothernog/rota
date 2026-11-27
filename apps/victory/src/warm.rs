//! `victory warm`: host pipelines made ahead of a run (`known_pipelines`), in
//! processes of their own: a compiler that crashes or hangs costs one pipeline.
//!
//! Two passes. The first makes the pipelines in several processes at once,
//! which fills the driver's own cache. The second has one process at a time
//! put them into the runs' pipeline library (loaded if it holds them, made
//! and stored if not), a chunk per process, and save it: a run loads from
//! that library. With the first pass alone (driver 621.40) the driver's cache
//! the runs read stayed empty and the runs made the pipelines again.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use victory_hle::known_pipelines as known;

/// Pipelines a process makes before another takes its place: the compiler's
/// and the translations' memory goes with it.
const AT_ONCE: usize = 40;
/// A pipeline that takes longer is taken for one the compiler hangs on.
const LONGEST: Duration = Duration::from_secs(180);

/// As the runs count one that takes long.
const SLOW_MS: f64 = 1000.0;

fn folder() -> Result<PathBuf> {
    let cfg = victory_core::Config::load()?;
    Ok(cfg.cache_subdir("shaders")?.with_file_name("pipelines"))
}

pub fn run(jobs: Option<usize>, limit: Option<usize>, again: bool, used: bool, these: Option<String>, store: bool) -> Result<()> {
    let dir = folder()?;
    if let Some(these) = these {
        return make(&dir, &these, store);
    }
    let began = Instant::now();
    victory_hle::known_pipelines::use_translations(dir.with_file_name("shaders"));
    let fingerprint = known::fingerprint();
    let done_file = dir.join("warm-done.txt");
    let bad_file = dir.join("warm-bad.txt");
    let keys = known::keys(&dir);
    let listed = std::fs::read_to_string(&done_file).unwrap_or_default();
    let mut done: HashSet<&str> = HashSet::new();
    for line in listed.lines() {
        if let Some((of, key)) = line.split_once(' ')
            && of == fingerprint
        {
            done.insert(key);
        }
    }
    let bad = std::fs::read_to_string(&bad_file).unwrap_or_default();
    let bad: HashSet<&str> = bad.lines().filter_map(|l| l.split(' ').next()).collect();
    let early = std::fs::read_to_string(dir.join("early-pipelines.txt")).unwrap_or_default();
    let early: HashSet<&str> = early.lines().collect();
    // Those earlier runs drew with first, the others in no order (by key).
    let mut all: Vec<usize> = (0..keys.len())
        .filter(|&n| {
            let key = keys[n].as_str();
            (again || !done.contains(key)) && !bad.contains(key) && (!used || early.contains(key))
        })
        .collect();
    all.sort_by_key(|&n| (!early.contains(keys[n].as_str()), &keys[n]));
    all.truncate(limit.unwrap_or(usize::MAX));
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    let jobs = jobs.unwrap_or(cores * 2 / 3).max(1);
    let card = known::adapter().unwrap_or_else(|| "card not known".into());
    println!(
        "{} pipelines known, {} made before ({fingerprint}, {card}), {} bad; {} to make, {jobs} processes",
        keys.len(),
        done.len(),
        bad.len(),
        all.len()
    );
    let chunks: Vec<&[usize]> = all.chunks(AT_ONCE).collect();
    let next = AtomicUsize::new(0);
    let state = Mutex::new((Vec::<f64>::new(), 0usize, 0usize));
    let done_out = Mutex::new(std::fs::OpenOptions::new().create(true).append(true).open(&done_file)?);
    // (Read back by the runs: these are made beside the others, not before.)
    let slow_out = Mutex::new(std::fs::OpenOptions::new().create(true).append(true).open(dir.join("slow-pipelines.txt"))?);
    let bad_out = Mutex::new(std::fs::OpenOptions::new().create(true).append(true).open(&bad_file)?);
    std::thread::scope(|s| {
        for _ in 0..jobs.min(chunks.len()) {
            s.spawn(|| {
                while let Some(&chunk) = chunks.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let mut rest = chunk;
                    while !rest.is_empty() {
                        let (made, ended, _) = child(rest, false, |n, ms, there| {
                            if ms >= SLOW_MS {
                                let _ = writeln!(lock(&slow_out), "\n{} {ms:.0}", keys[n]);
                            }
                            let mut state = lock(&state);
                            state.0.push(ms);
                            state.1 += usize::from(!there);
                            let count = state.0.len();
                            if count.is_multiple_of(1000) {
                                println!("{count} of {}, {:.0} s", all.len(), began.elapsed().as_secs_f64());
                            }
                            drop(state);
                            let _ = writeln!(lock(&done_out), "{fingerprint} {}", keys[n]);
                        });
                        rest = &rest[made.min(rest.len())..];
                        match (ended, rest.split_first()) {
                            (Some(why), Some((&n, after))) => {
                                // The one it was at.
                                println!("pipeline {n} {}: {why}", keys[n]);
                                let _ = writeln!(lock(&bad_out), "{} {why}", keys[n]);
                                lock(&state).2 += 1;
                                rest = after;
                            }
                            _ => break,
                        }
                    }
                }
            });
        }
    });
    let (mut times, refused, crashed) = state.into_inner().unwrap_or_else(|e| e.into_inner());
    times.sort_by(f64::total_cmp);
    let at = |q: f64| times.get((times.len() as f64 * q) as usize).copied().unwrap_or(0.0);
    println!(
        "{} made in {:.0} s ({refused} have no host pipeline, {crashed} ended their process); ms each: median {:.1}, p90 {:.1}, p99 {:.1}, most {:.0}, sum {:.0} s",
        times.len(),
        began.elapsed().as_secs_f64(),
        at(0.5),
        at(0.9),
        at(0.99),
        times.last().copied().unwrap_or(0.0),
        times.iter().sum::<f64>() / 1e3
    );
    // The second pass: every pipeline the first made, now or before, and
    // not one that ended its process, into the library.
    let bad = std::fs::read_to_string(&bad_file).unwrap_or_default();
    let bad: HashSet<&str> = bad.lines().filter_map(|l| l.split(' ').next()).collect();
    let mut stored: Vec<usize> = (0..keys.len())
        .filter(|&n| {
            let key = keys[n].as_str();
            !bad.contains(key) && (!used || early.contains(key))
        })
        .collect();
    stored.sort_by_key(|&n| (!early.contains(keys[n].as_str()), &keys[n]));
    stored.truncate(limit.unwrap_or(usize::MAX));
    let began = Instant::now();
    println!("{} pipelines into the runs' pipeline library, one process at a time", stored.len());
    // A command line of some 7 KB, and one save of the library per process.
    const CHUNK: usize = 1000;
    let (mut count, mut unheard, mut skipped, mut loads, mut makes) = (0usize, 0usize, 0usize, 0u64, 0u64);
    let mut left = stored.clone();
    while !left.is_empty() {
        let len = left.len().min(CHUNK);
        let mut heard = 0;
        let (done, ended, library) = child(&left[..len], true, |_, _, _| heard += 1);
        match ended {
            Some(why) if done < len => {
                // What it stored went with it: the chunk again without the
                // one it ended at. Left out of the library, the run makes it.
                let n = left.remove(done);
                println!("pipeline {n} {} not stored: {why}", keys[n]);
                skipped += 1;
                continue;
            }
            Some(why) => println!("library of {len} pipelines not saved: {why}"),
            None => {}
        }
        if let Some((l, m)) = library {
            loads += l;
            makes += m;
        }
        count += heard;
        unheard += len - heard.min(len);
        left.drain(..len);
        println!("{count} of {}, {:.0} s", stored.len(), began.elapsed().as_secs_f64());
    }
    println!(
        "{count} in the library in {:.0} s: {loads} loaded, {makes} made and stored; {skipped} ended their process",
        began.elapsed().as_secs_f64()
    );
    if unheard > 0 {
        println!("{unheard} not heard of: not in the library");
    }
    Ok(())
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Has a process make `these` (`store`: through the runs' pipeline library);
/// how many it made, how it ended if not well, and with `store` the
/// pipelines it loaded from the library and made.
fn child(
    these: &[usize],
    store: bool,
    mut each: impl FnMut(usize, f64, bool),
) -> (usize, Option<String>, Option<(u64, u64)>) {
    let list: Vec<String> = these.iter().map(usize::to_string).collect();
    let spawned = std::env::current_exe().and_then(|exe| {
        use std::os::windows::process::CommandExt;
        const IDLE_PRIORITY_CLASS: u32 = 0x40;
        let list = list.join(",");
        let mut args = vec!["warm", "--these", list.as_str()];
        if store {
            args.push("--store");
        }
        Command::new(exe)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(IDLE_PRIORITY_CLASS)
            .spawn()
    });
    let Ok(mut process) = spawned else { return (these.len(), None, None) };
    let Some(out) = process.stdout.take() else { return (these.len(), None, None) };
    let process = Mutex::new(process);
    let (heard, over) = (Mutex::new(Instant::now()), AtomicBool::new(false));
    let hung = AtomicBool::new(false);
    let (mut made, mut library) = (0, None);
    std::thread::scope(|s| {
        s.spawn(|| {
            while !over.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(500));
                if lock(&heard).elapsed() > LONGEST {
                    hung.store(true, Ordering::Relaxed);
                    let _ = lock(&process).kill();
                    return;
                }
            }
        });
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            *lock(&heard) = Instant::now();
            let words: Vec<&str> = line.split(' ').collect();
            match words[..] {
                ["made", n, ms, there] => {
                    if let (Ok(n), Ok(ms)) = (n.parse(), ms.parse()) {
                        made += 1;
                        each(n, ms, there == "1");
                    }
                }
                ["library", loaded, "loaded", made, "made"] => {
                    library = loaded.parse().ok().zip(made.parse().ok());
                }
                _ => {}
            }
        }
        over.store(true, Ordering::Relaxed);
    });
    let status = lock(&process).wait();
    let well = status.as_ref().is_ok_and(|s| s.success());
    if well {
        return (these.len(), None, library);
    }
    let why = if hung.load(Ordering::Relaxed) {
        format!("hung for {} s", LONGEST.as_secs())
    } else {
        status.map_or_else(|e| e.to_string(), |s| format!("{s}"))
    };
    (made, Some(why), library)
}

/// What such a process does (`store`: through the runs' pipeline library,
/// saved at the end).
fn make(dir: &Path, these: &str, store: bool) -> Result<()> {
    let these: Vec<usize> = these.split(',').map(|n| n.parse().context("--these")).collect::<Result<_>>()?;
    let cfg = victory_core::Config::load()?;
    victory_hle::known_pipelines::use_translations(cfg.cache_subdir("shaders")?);
    if store {
        // The runs' own folder for it (`use_shader_cache`: beside the translations).
        known::use_library(dir.to_path_buf());
    }
    let mut out = std::io::stdout();
    known::make(dir, &these, |n, came| {
        let told = match came {
            None => writeln!(out, "at {n}"),
            Some((ms, there)) => writeln!(out, "made {n} {ms:.2} {}", u8::from(there)),
        };
        if told.and_then(|()| out.flush()).is_err() {
            // Whoever asked is gone.
            std::process::exit(1);
        }
    });
    if store {
        known::save_library();
        let (loaded, made) = known::library_counts();
        let _ = writeln!(out, "library {loaded} loaded {made} made");
    }
    // Without what a run's end does (the host's threads).
    let _ = out.flush();
    std::process::exit(0)
}
