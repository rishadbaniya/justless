//! The persistent lookup store in `.justless/lookup/`: a manifest of file
//! stamps and digests, a base segment, a small delta segment for files that
//! changed since the base was built, and an optional segment of run logs.
//!
//! Every lookup refreshes the store with the repository index's file list,
//! ignore rules, size limit and stamp-then-digest change detection, and parses
//! only changed files. When the store cannot be written, lookup answers from
//! memory instead.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Result, anyhow};
use sha2::{Digest, Sha256};

use super::chunks;
use super::logs;
use super::segment::{
    self, Builder, Input, LOOKUP_VERSION, Segment, Target, check, corrupt, get_varint, is_corrupt,
    put_varint,
};
use crate::index::{self, FileStamp};
use crate::polyglot::Parsers;
use crate::workspace::{Workspace, lock_state, private_dir};

const MANIFEST: &str = "manifest.bin";
const MAGIC: &[u8; 4] = b"JLM1";
const PARALLEL_ABOVE: usize = 64;
const ANNOUNCE_ABOVE: usize = 2_000;
const MAX_WORKERS: usize = 8;
const DELTA_FLOOR: u64 = 2_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    /// The file's chunks are in the base segment.
    Base,
    /// The file changed after the base was built; its chunks are in the delta.
    Delta,
    /// Not UTF-8 text, or contains NUL. Recorded so it is not read again.
    Binary,
}

#[derive(Clone, PartialEq, Eq)]
struct Record {
    status: Status,
    stamp: Option<FileStamp>,
    digest: [u8; 32],
}

#[derive(Default, PartialEq, Eq)]
struct Manifest {
    base: u64,
    delta: u64,
    logs: u64,
    files: BTreeMap<String, Record>,
    runs: Vec<String>,
}

impl Manifest {
    /// "JLM1", the lookup and index versions, the three segment generations,
    /// then each file (path, status, optional stamp, SHA-256) and each run ID.
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&LOOKUP_VERSION.to_le_bytes());
        out.extend_from_slice(&index::VERSION.to_le_bytes());
        for generation in [self.base, self.delta, self.logs] {
            out.extend_from_slice(&generation.to_le_bytes());
        }
        out.extend_from_slice(&(self.files.len() as u32).to_le_bytes());
        for (file, record) in &self.files {
            put_varint(&mut out, file.len() as u64);
            out.extend_from_slice(file.as_bytes());
            out.push(match record.status {
                Status::Base => 0,
                Status::Delta => 1,
                Status::Binary => 2,
            });
            match &record.stamp {
                None => out.push(0),
                Some(stamp) => {
                    out.push(1);
                    for value in [stamp.device, stamp.inode, stamp.bytes] {
                        out.extend_from_slice(&value.to_le_bytes());
                    }
                    for value in [
                        stamp.modified.0,
                        stamp.modified.1,
                        stamp.changed.0,
                        stamp.changed.1,
                    ] {
                        out.extend_from_slice(&value.to_le_bytes());
                    }
                }
            }
            out.extend_from_slice(&record.digest);
        }
        out.extend_from_slice(&(self.runs.len() as u32).to_le_bytes());
        for run in &self.runs {
            put_varint(&mut out, run.len() as u64);
            out.extend_from_slice(run.as_bytes());
        }
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = Reader { bytes, position: 0 };
        check(reader.take(4)? == MAGIC, "bad manifest magic")?;
        check(
            reader.u32()? == LOOKUP_VERSION && reader.u32()? == index::VERSION,
            "manifest from another version",
        )?;
        let mut manifest = Self {
            base: reader.u64()?,
            delta: reader.u64()?,
            logs: reader.u64()?,
            ..Self::default()
        };
        let files = reader.u32()?;
        let mut previous: Option<String> = None;
        for _ in 0..files {
            let file = reader.string()?;
            check(
                previous.as_ref().is_none_or(|previous| *previous < file),
                "manifest out of order",
            )?;
            let status = match reader.take(1)?[0] {
                0 => Status::Base,
                1 => Status::Delta,
                2 => Status::Binary,
                _ => return Err(corrupt("bad file status")),
            };
            let stamp = match reader.take(1)?[0] {
                0 => None,
                1 => Some(FileStamp {
                    device: reader.u64()?,
                    inode: reader.u64()?,
                    bytes: reader.u64()?,
                    modified: (reader.i64()?, reader.i64()?),
                    changed: (reader.i64()?, reader.i64()?),
                }),
                _ => return Err(corrupt("bad stamp flag")),
            };
            let digest = reader.take(32)?.try_into().expect("32 bytes");
            previous = Some(file.clone());
            manifest.files.insert(
                file,
                Record {
                    status,
                    stamp,
                    digest,
                },
            );
        }
        let runs = reader.u32()?;
        for _ in 0..runs {
            manifest.runs.push(reader.string()?);
        }
        check(reader.position == bytes.len(), "trailing manifest bytes")?;
        Ok(manifest)
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(length)
            .filter(|&end| end <= self.bytes.len())
            .ok_or_else(|| corrupt("truncated manifest"))?;
        let bytes = &self.bytes[self.position..end];
        self.position = end;
        Ok(bytes)
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn string(&mut self) -> Result<String> {
        let length = get_varint(self.bytes, &mut self.position)?;
        let length = usize::try_from(length).map_err(|_| corrupt("long manifest string"))?;
        let bytes = self.take(length)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| corrupt("manifest string is not UTF-8"))
    }
}

/// Segments ready for ranking, each with the files whose chunks are live.
pub(crate) struct Loaded {
    pub(crate) source: Vec<(Segment, Vec<bool>)>,
    pub(crate) logs: Option<(Segment, Vec<bool>)>,
    files: BTreeMap<String, Record>,
    /// Held until ranking ends, because a refresh elsewhere may delete old segments.
    _lock: Option<File>,
}

impl Loaded {
    /// The SHA-256 of a source file's contents when it was indexed.
    pub(crate) fn digest(&self, file: &str) -> Option<&[u8; 32]> {
        self.files.get(file).map(|record| &record.digest)
    }
}

/// Refreshes the store and returns its segments. Errors only for problems
/// outside the store, such as a broken `.justlessignore` or Git failure.
pub(crate) fn load(workspace: &Workspace, logs: bool) -> Result<Loaded> {
    open(workspace, logs, false)
}

/// Discards the store and builds it again, for damage found only while
/// ranking, such as unreadable postings.
pub(crate) fn rebuild(workspace: &Workspace, logs: bool) -> Result<Loaded> {
    open(workspace, logs, true)
}

fn open(workspace: &Workspace, logs: bool, fresh: bool) -> Result<Loaded> {
    let store = Store {
        workspace,
        directory: workspace.state.join("lookup"),
        max_bytes: workspace.settings()?.max_file_bytes,
        logs,
        workers: None,
    };
    let lock = private_dir(&workspace.state)
        .and_then(|()| private_dir(&store.directory))
        .and_then(|()| {
            // Opening the lock creates it, so never through a link.
            let path = workspace.state.join("lookup.lock");
            if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_symlink()) {
                fs::remove_file(&path)?;
            }
            lock_state(&path)
        });
    match lock {
        Ok(lock) => {
            let mut loaded = if fresh {
                store.rebuild()?
            } else {
                store.writable()?
            };
            loaded._lock = Some(lock);
            Ok(loaded)
        }
        Err(error) => {
            eprintln!("lookup index not saved: {error:#}");
            if fresh {
                store.apply(None, &store.scan(None)?, false)
            } else {
                store.read_only()
            }
        }
    }
}

struct Store<'a> {
    workspace: &'a Workspace,
    directory: PathBuf,
    max_bytes: u64,
    logs: bool,
    /// Overrides the worker count, for tests.
    workers: Option<usize>,
}

struct Stored {
    manifest: Manifest,
    base: Option<Segment>,
    delta: Option<Segment>,
    logs: Option<Segment>,
}

/// Files read in this refresh: new records, the files whose chunks changed,
/// and those chunks as in-memory segments in path order.
struct Scan {
    files: BTreeMap<String, Record>,
    changed: BTreeSet<String>,
    fresh: Vec<Segment>,
}

struct Job {
    file: String,
    stamp: Option<FileStamp>,
    previous: Option<Record>,
}

enum Outcome {
    Skip,
    Keep(Record),
    Changed(Record),
}

struct Slice {
    outcomes: Vec<Outcome>,
    segment: Option<Segment>,
}

fn status(files: &BTreeMap<String, Record>, file: &str) -> Option<Status> {
    files.get(file).map(|record| record.status)
}

fn segment_name(kind: &str, generation: u64) -> String {
    format!("{kind}-{generation}.seg")
}

/// Whether a name in the store directory is one of the store's own files.
fn owned_name(name: &str) -> bool {
    name == MANIFEST
        || name.ends_with(".tmp")
        || ["base-", "delta-", "logs-"].iter().any(|prefix| {
            name.strip_prefix(prefix)
                .and_then(|rest| rest.strip_suffix(".seg"))
                .is_some_and(|generation| {
                    !generation.is_empty() && generation.bytes().all(|byte| byte.is_ascii_digit())
                })
        })
}

impl Store<'_> {
    fn writable(&self) -> Result<Loaded> {
        let stored = self.read().unwrap_or_else(|_| {
            self.clean(&[]);
            None
        });
        let cold = stored.is_none();
        let scan = self.scan(stored.as_ref().map(|stored| &stored.manifest))?;
        let error = match self.apply(stored, &scan, true) {
            Ok(loaded) => return Ok(loaded),
            Err(error) => error,
        };
        if is_corrupt(&error) {
            return self.rebuild();
        }
        eprintln!("lookup index not saved: {error:#}");
        // The scan was made against the manifest read above; answer from the
        // same store, or from a fresh scan if it can no longer be read.
        match self.read().ok().flatten() {
            Some(stored) if !cold => match self.apply(Some(stored), &scan, false) {
                Ok(loaded) => Ok(loaded),
                Err(_) => self.apply(None, &self.scan(None)?, false),
            },
            _ if cold => self.apply(None, &scan, false),
            _ => self.apply(None, &self.scan(None)?, false),
        }
    }

    /// Removes every store file and builds the store from scratch, answering
    /// from memory if it cannot be saved.
    fn rebuild(&self) -> Result<Loaded> {
        self.clean(&[]);
        let scan = self.scan(None)?;
        match self.apply(None, &scan, true) {
            Ok(loaded) => Ok(loaded),
            Err(error) => {
                eprintln!("lookup index not saved: {error:#}");
                self.apply(None, &scan, false)
            }
        }
    }

    /// Answers without writing. An existing store is used only if it decodes.
    fn read_only(&self) -> Result<Loaded> {
        let stored = self.read().ok().flatten();
        let scan = self.scan(stored.as_ref().map(|stored| &stored.manifest))?;
        match self.apply(stored, &scan, false) {
            Ok(loaded) => Ok(loaded),
            Err(_) => self.apply(None, &self.scan(None)?, false),
        }
    }

    fn read(&self) -> Result<Option<Stored>> {
        let path = self.directory.join(MANIFEST);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => check(metadata.is_file(), "manifest is not a regular file")?,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let bytes = fs::read(path)?;
        let manifest = Manifest::decode(&bytes)?;
        let open = |kind: &str, generation: u64| -> Result<Option<Segment>> {
            if generation == 0 {
                return Ok(None);
            }
            Segment::open(&self.directory.join(segment_name(kind, generation)))
                .map(Some)
                .map_err(|error| corrupt(&format!("{kind} segment: {error:#}")))
        };
        let base = open("base", manifest.base)?;
        let delta = open("delta", manifest.delta)?;
        let logs = open("logs", manifest.logs)?;
        if let Some(delta) = &delta {
            check(
                delta.files().iter().all(|file| {
                    manifest
                        .files
                        .get(file)
                        .is_some_and(|record| record.status == Status::Delta)
                }),
                "delta segment does not match the manifest",
            )?;
        }
        Ok(Some(Stored {
            manifest,
            base,
            delta,
            logs,
        }))
    }

    /// Removes the store's own files except `keep`, and links at the store's
    /// names without following them. Other files are left alone.
    fn clean(&self, keep: &[String]) {
        let Ok(entries) = fs::read_dir(&self.directory) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !owned_name(name) || keep.iter().any(|kept| kept == name) {
                continue;
            }
            let path = entry.path();
            if fs::symlink_metadata(&path)
                .is_ok_and(|metadata| metadata.is_file() || metadata.is_symlink())
            {
                let _ = fs::remove_file(path);
            }
        }
    }

    /// Stamps every listed file and reads, hashes and chunks only those whose
    /// stamp differs from the manifest.
    fn scan(&self, previous: Option<&Manifest>) -> Result<Scan> {
        let candidates = index::inputs(self.workspace)?;
        let mut files = BTreeMap::new();
        let mut jobs = Vec::new();
        for file in candidates {
            // lstat rather than a canonical path check: canonicalizing every file
            // costs far more. Files that are read are checked in full.
            let Ok(metadata) = fs::symlink_metadata(self.workspace.root.join(&file)) else {
                continue;
            };
            if !metadata.is_file() || metadata.len() > self.max_bytes {
                continue;
            }
            let stamp = index::file_stamp(&metadata);
            let previous = previous.and_then(|manifest| manifest.files.get(&file));
            if let Some(record) = previous
                && stamp.is_some()
                && record.stamp == stamp
            {
                files.insert(file, record.clone());
                continue;
            }
            jobs.push(Job {
                file,
                stamp,
                previous: previous.cloned(),
            });
        }
        if previous.is_none() && jobs.len() > ANNOUNCE_ABOVE {
            eprintln!("Building lookup index for {} files…", jobs.len());
        }
        let mut changed = BTreeSet::new();
        let mut fresh = Vec::new();
        let mut done = jobs.iter();
        for slice in self.run(&jobs)? {
            for outcome in slice.outcomes {
                let job = done.next().expect("one outcome per job");
                match outcome {
                    Outcome::Skip => {}
                    Outcome::Keep(record) => {
                        files.insert(job.file.clone(), record);
                    }
                    Outcome::Changed(record) => {
                        changed.insert(job.file.clone());
                        files.insert(job.file.clone(), record);
                    }
                }
            }
            fresh.extend(slice.segment);
        }
        Ok(Scan {
            files,
            changed,
            fresh,
        })
    }

    /// Processes jobs in contiguous slices on up to eight threads. Output does
    /// not depend on the number of threads.
    fn run(&self, jobs: &[Job]) -> Result<Vec<Slice>> {
        if jobs.is_empty() {
            return Ok(Vec::new());
        }
        let workers = self.workers.unwrap_or_else(|| {
            if jobs.len() > PARALLEL_ABOVE {
                std::thread::available_parallelism()
                    .map_or(1, |count| count.get())
                    .min(MAX_WORKERS)
            } else {
                1
            }
        });
        let size = (jobs.len() / (workers * 4)).clamp(16, 512);
        let slices = jobs.chunks(size).collect::<Vec<_>>();
        let next = AtomicUsize::new(0);
        let results = Mutex::new((0..slices.len()).map(|_| None).collect::<Vec<_>>());
        let work = || {
            let mut parsers = Parsers::default();
            loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some(slice) = slices.get(index) else {
                    break;
                };
                let result = self.process(slice, &mut parsers);
                results.lock().expect("lookup worker panicked")[index] = Some(result);
            }
        };
        std::thread::scope(|scope| {
            for _ in 1..workers {
                // If a thread cannot start, the remaining slices run here. Parsers
                // recurse, so workers get the main thread's usual stack size.
                if std::thread::Builder::new()
                    .stack_size(8 << 20)
                    .spawn_scoped(scope, work)
                    .is_err()
                {
                    break;
                }
            }
            work();
        });
        results
            .into_inner()
            .expect("lookup worker panicked")
            .into_iter()
            .map(|result| result.unwrap_or_else(|| Err(anyhow!("lookup slice was not processed"))))
            .collect()
    }

    fn process(&self, jobs: &[Job], parsers: &mut Parsers) -> Result<Slice> {
        let mut builder = Builder::default();
        let mut outcomes = Vec::with_capacity(jobs.len());
        for job in jobs {
            let Some(bytes) = self
                .workspace
                .source_path(&job.file)
                .ok()
                .and_then(|path| fs::read(path).ok())
            else {
                outcomes.push(Outcome::Skip);
                continue;
            };
            let digest: [u8; 32] = Sha256::digest(&bytes).into();
            if let Some(previous) = &job.previous
                && previous.digest == digest
            {
                outcomes.push(Outcome::Keep(Record {
                    stamp: job.stamp,
                    ..previous.clone()
                }));
                continue;
            }
            let record = |status| Record {
                status,
                stamp: job.stamp,
                digest,
            };
            match std::str::from_utf8(&bytes) {
                Ok(text) if !text.contains('\0') => {
                    let chunks = chunks::chunk(&job.file, text, parsers);
                    builder.add(&job.file, &chunks, true);
                    outcomes.push(Outcome::Changed(record(Status::Delta)));
                }
                _ => outcomes.push(Outcome::Keep(record(Status::Binary))),
            }
        }
        let segment = if builder.is_empty() {
            None
        } else {
            Some(builder.finish()?)
        };
        Ok(Slice { outcomes, segment })
    }

    /// Folds the scan into new segments and a new manifest, writing them when
    /// `write` is set.
    fn apply(&self, stored: Option<Stored>, scan: &Scan, write: bool) -> Result<Loaded> {
        let cold = stored.is_none();
        let Stored {
            manifest: previous,
            base,
            delta,
            logs,
        } = stored.unwrap_or(Stored {
            manifest: Manifest::default(),
            base: None,
            delta: None,
            logs: None,
        });
        let mut files = scan.files.clone();
        let mut generation = previous.base.max(previous.delta).max(previous.logs);
        let mut next = || {
            generation += 1;
            generation
        };
        let mut manifest = Manifest {
            base: previous.base,
            delta: previous.delta,
            logs: previous.logs,
            files: BTreeMap::new(),
            runs: previous.runs.clone(),
        };
        // Old chunks survive unless their file changed in this scan.
        let kept = |files: &BTreeMap<String, Record>, segment: &Segment, wanted: Status| {
            segment
                .files()
                .iter()
                .map(|file| !scan.changed.contains(file) && status(files, file) == Some(wanted))
                .collect::<Vec<_>>()
        };
        let fresh_chunks = scan
            .fresh
            .iter()
            .map(|segment| u64::from(segment.chunk_count()))
            .sum::<u64>();
        let live_chunks = |segment: &Segment, live: &[bool]| {
            segment
                .ranges()
                .iter()
                .zip(live)
                .filter(|(_, live)| **live)
                .map(|(range, _)| u64::from(range.end - range.start))
                .sum::<u64>()
        };
        let (mut base, mut delta) = (base, delta);
        if cold {
            for record in files.values_mut() {
                if record.status == Status::Delta {
                    record.status = Status::Base;
                }
            }
            let fresh = scan.fresh.iter().map(Input::all).collect::<Vec<_>>();
            (base, manifest.base) = self.merge(&fresh, "base", next(), write)?;
            (delta, manifest.delta) = (None, 0);
        } else {
            let base_live = base
                .as_ref()
                .map(|base| kept(&files, base, Status::Base))
                .unwrap_or_default();
            let delta_live = delta
                .as_ref()
                .map(|delta| kept(&files, delta, Status::Delta))
                .unwrap_or_default();
            let delta_dirty = delta_live.iter().any(|live| !live);
            if !scan.changed.is_empty() || delta_dirty {
                let base_chunks = base
                    .as_ref()
                    .map_or(0, |base| u64::from(base.chunk_count()));
                let base_kept = base
                    .as_ref()
                    .map_or(0, |base| live_chunks(base, &base_live));
                let delta_chunks = delta
                    .as_ref()
                    .map_or(0, |delta| live_chunks(delta, &delta_live))
                    + fresh_chunks;
                let mut inputs = Vec::new();
                let compact = delta_chunks > DELTA_FLOOR.max(base_kept / 16)
                    || base_chunks - base_kept > base_chunks / 8;
                if compact && let Some(base) = &base {
                    inputs.push(Input {
                        segment: base,
                        live: base_live,
                    });
                }
                if let Some(delta) = &delta {
                    inputs.push(Input {
                        segment: delta,
                        live: delta_live,
                    });
                }
                inputs.extend(scan.fresh.iter().map(Input::all));
                if compact {
                    for record in files.values_mut() {
                        if record.status == Status::Delta {
                            record.status = Status::Base;
                        }
                    }
                    let (merged, generation) = self.merge(&inputs, "base", next(), write)?;
                    drop(inputs);
                    (base, manifest.base) = (merged, generation);
                    (delta, manifest.delta) = (None, 0);
                } else {
                    let (merged, generation) = if delta_chunks == 0 {
                        (None, 0)
                    } else {
                        self.merge(&inputs, "delta", next(), write)?
                    };
                    drop(inputs);
                    (delta, manifest.delta) = (merged, generation);
                }
            }
        }
        let mut logs = logs;
        if self.logs {
            let runs = logs::recent(self.workspace);
            let mut ids = runs.iter().map(|run| run.id.clone()).collect::<Vec<_>>();
            ids.sort();
            if ids != previous.runs {
                let recent = ids.iter().map(String::as_str).collect::<BTreeSet<_>>();
                let mut builder = Builder::default();
                let mut added = runs
                    .iter()
                    .filter(|run| !previous.runs.contains(&run.id))
                    .collect::<Vec<_>>();
                added.sort_by(|left, right| left.id.cmp(&right.id));
                for run in added {
                    logs::add(self.workspace, run, &mut builder);
                }
                let added = if builder.is_empty() {
                    None
                } else {
                    Some(builder.finish()?)
                };
                let mut inputs = Vec::new();
                if let Some(old) = &logs {
                    let live = old
                        .files()
                        .iter()
                        .map(|file| recent.contains(logs::run_of(file)))
                        .collect::<Vec<_>>();
                    inputs.push(Input { segment: old, live });
                }
                inputs.extend(added.iter().map(Input::all));
                let total = inputs
                    .iter()
                    .map(|input| live_chunks(input.segment, &input.live))
                    .sum::<u64>();
                let (merged, generation) = if total == 0 {
                    (None, 0)
                } else {
                    self.merge(&inputs, "logs", next(), write)?
                };
                drop(inputs);
                (logs, manifest.logs) = (merged, generation);
                manifest.runs = ids;
            }
        }
        manifest.files = files;
        if write && manifest != previous {
            // A new, exclusively created file renamed over the manifest, so a
            // link at either name is replaced rather than followed.
            let mut partial = tempfile::Builder::new()
                .prefix("manifest.bin.")
                .suffix(".tmp")
                .tempfile_in(&self.directory)?;
            partial.write_all(&manifest.encode())?;
            partial.persist(self.directory.join(MANIFEST))?;
            let keep = [
                ("base", manifest.base),
                ("delta", manifest.delta),
                ("logs", manifest.logs),
            ]
            .into_iter()
            .filter(|(_, generation)| *generation != 0)
            .map(|(kind, generation)| segment_name(kind, generation))
            .chain([MANIFEST.to_owned()])
            .collect::<Vec<_>>();
            self.clean(&keep);
        }
        let live = |segment: &Segment, wanted: Status| {
            segment
                .files()
                .iter()
                .map(|file| status(&manifest.files, file) == Some(wanted))
                .collect::<Vec<_>>()
        };
        let mut source = Vec::new();
        if let Some(base) = base {
            let live = live(&base, Status::Base);
            source.push((base, live));
        }
        if let Some(delta) = delta {
            let live = live(&delta, Status::Delta);
            source.push((delta, live));
        }
        let logs = logs.filter(|_| self.logs).map(|logs| {
            let live = vec![true; logs.files().len()];
            (logs, live)
        });
        Ok(Loaded {
            source,
            logs,
            files: manifest.files,
            _lock: None,
        })
    }

    /// Merges into a new segment named for `kind` and `generation`, or returns
    /// no segment when nothing survives.
    fn merge(
        &self,
        inputs: &[Input],
        kind: &str,
        generation: u64,
        write: bool,
    ) -> Result<(Option<Segment>, u64)> {
        if inputs.is_empty() {
            return Ok((None, 0));
        }
        let path = self.directory.join(segment_name(kind, generation));
        let target = if write {
            Target::Disk(&path)
        } else {
            Target::Memory
        };
        let merged = segment::merge(inputs, target)?;
        if merged.chunk_count() == 0 {
            return Ok((None, 0));
        }
        Ok((Some(merged), if write { generation } else { 0 }))
    }
}

#[cfg(test)]
pub(crate) fn cold_bytes(workspace: &Workspace, workers: usize) -> Vec<u8> {
    let store = Store {
        workspace,
        directory: workspace.state.join("lookup"),
        max_bytes: u64::MAX,
        logs: false,
        workers: Some(workers),
    };
    let scan = store.scan(None).unwrap();
    let inputs = scan.fresh.iter().map(Input::all).collect::<Vec<_>>();
    segment::merge(&inputs, Target::Memory)
        .unwrap()
        .bytes()
        .to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(status: Status, stamp: bool) -> Record {
        Record {
            status,
            stamp: stamp.then_some(FileStamp {
                device: 1,
                inode: u64::MAX,
                bytes: 300,
                modified: (-5, 999_999_999),
                changed: (1_700_000_000, 1),
            }),
            digest: [7; 32],
        }
    }

    fn manifest() -> Manifest {
        Manifest {
            base: 3,
            delta: 4,
            logs: 0,
            files: BTreeMap::from([
                ("a.rs".to_owned(), record(Status::Base, true)),
                ("b/é.md".to_owned(), record(Status::Delta, false)),
                ("c.bin".to_owned(), record(Status::Binary, true)),
            ]),
            runs: vec!["run-1".into(), "run-2".into()],
        }
    }

    #[test]
    fn manifest_round_trips_and_rejects_every_prefix() {
        let bytes = manifest().encode();
        assert!(Manifest::decode(&bytes).unwrap() == manifest());
        for length in 0..bytes.len() {
            assert!(
                Manifest::decode(&bytes[..length]).is_err(),
                "prefix {length}"
            );
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(Manifest::decode(&longer).is_err());
    }

    #[test]
    fn manifest_from_another_version_is_rejected() {
        for at in [4, 8] {
            let mut bytes = manifest().encode();
            bytes[at] ^= 1;
            let error = Manifest::decode(&bytes).err().unwrap();
            assert!(is_corrupt(&error), "{error:#}");
        }
    }

    #[test]
    fn store_names() {
        for name in [
            "manifest.bin",
            "base-1.seg",
            "delta-22.seg",
            "logs-3.seg",
            "x.tmp",
        ] {
            assert!(owned_name(name), "{name}");
        }
        for name in [
            "notes.txt",
            "base-.seg",
            "base-1a.seg",
            "base-1.segment",
            "lookup.lock",
        ] {
            assert!(!owned_name(name), "{name}");
        }
    }

    #[test]
    fn worker_count_does_not_change_the_segment() {
        let directory = tempfile::TempDir::new().unwrap();
        let root = directory.path().canonicalize().unwrap();
        for index in 0..150 {
            let path = root.join(format!("src/m{:03}/lib.rs", index));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(
                &path,
                format!(
                    "/// Retries request {index}.\npub fn retry_{index}(attempt: u32) -> u32 {{\n    attempt + {index}\n}}\n"
                ),
            )
            .unwrap();
        }
        fs::write(root.join("README.md"), "# Retries\nRetry budget.\n").unwrap();
        fs::write(root.join("data.csv"), "a,b\n1,2\n").unwrap();
        let workspace = Workspace {
            root: root.clone(),
            cwd: root.clone(),
            state: root.join(".justless"),
        };
        let single = cold_bytes(&workspace, 1);
        assert_eq!(single, cold_bytes(&workspace, 4));
        assert_eq!(single, cold_bytes(&workspace, 3));
        let segment = Segment::from_bytes(single).unwrap();
        assert_eq!(segment.files().len(), 152);
    }
}
