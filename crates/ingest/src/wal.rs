//! Append-only write-ahead log. The only state M1 persists.
//!
//! Every accepted event is written here *before* the client is told "accepted", so a crash never loses a
//! confirmed event. On restart, replaying the log rebuilds the in-memory run store exactly.
//!
//! Lifecycle, driven by the single writer task (`writer.rs`):
//! 1. [`Wal::open`] at startup: replay all files in order, cut off a half-written tail, reopen for appending.
//! 2. [`Wal::append`] for each event in a group of requests (buffered in memory, not yet durable).
//! 3. [`Wal::sync`] once per group (group commit: one fsync shared by many requests); only then confirm.
//!
//! On disk: `.tracedb/wal/00000000000000001042.wal`, one file per ~64 MB, named by its first `event_seq`
//! (zero-padded so names sort in sequence order). Each file starts with `TDBW` + a u32 version, then records:
//!
//! ```text
//! [u32 len][u32 crc32] [u64 seq][u64 received_us][u8 op][u8 source][u64 body_hash][JSON fields]
//!  └─ header (8 B) ─┘  └──────────────── "rest": len bytes, covered by crc32 ────────────────┘
//! ```
//!
//! All integers are little-endian. `fields` is the event's `RunFields` as JSON.
//!
//! Crash rules: a bad record in the **last** file is a write torn by the crash. It was never synced, so never
//! confirmed, and it's safe to cut off. A bad record in an **earlier** file means real corruption, and `open`
//! refuses to start rather than silently dropping confirmed events.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use tracedb_model::{Op, RunEvent, Source, Ts};

const MAGIC: &[u8; 4] = b"TDBW";
const VERSION: u32 = 1;
/// `TDBW` + u32 version.
const FILE_HEADER_LEN: u64 = 8;
/// The fixed part of "rest" before the JSON: seq + received_us + op + source + body_hash.
const FIXED_LEN: usize = 8 + 8 + 1 + 1 + 8;
/// Larger than any valid record (payload fields are capped at 20 MB each); anything bigger is corruption.
const MAX_RECORD_LEN: u32 = 128 * 1024 * 1024;
/// Start a new file once the current one would pass this size.
pub const DEFAULT_ROLL_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum WalError {
    #[error("wal i/o: {0}")]
    Io(#[from] io::Error),
    #[error("wal file {file} is corrupt at byte {offset}: {why}; it is not the last file, so tracedb won't guess")]
    Corrupt { file: PathBuf, offset: u64, why: String },
}

/// What startup replay found; logged and shown in health.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReplayStats {
    pub records: u64,
    pub files: usize,
    pub last_seq: u64,
    /// Bytes cut from the end of the last file (a write torn by a crash; never confirmed).
    pub truncated_bytes: u64,
}

/// The open log. Owned by the writer task only, so it needs no locking.
pub struct Wal {
    dir: PathBuf,
    /// The current (last) file. Appends collect in this buffer until `sync`.
    out: BufWriter<File>,
    /// Size of the current file including buffered bytes; decides when to roll.
    file_len: u64,
    roll_bytes: u64,
    /// Bytes appended since this `Wal` was opened (for the unflushed-WAL limit).
    appended: u64,
}

impl Wal {
    /// Replays every record in `dir` through `on_event`, truncates a torn tail, and opens the log for appending.
    ///
    /// `on_event` is how the caller rebuilds state: `lib.rs` merges each replayed event into the run store.
    pub fn open(
        dir: &Path,
        roll_bytes: u64,
        mut on_event: impl FnMut(RunEvent),
    ) -> Result<(Wal, ReplayStats), WalError> {
        fs::create_dir_all(dir)?;
        let files = wal_files(dir)?;
        let mut stats = ReplayStats {
            files: files.len(),
            ..Default::default()
        };
        for (i, path) in files.iter().enumerate() {
            let is_last = i + 1 == files.len();
            match replay_file(path, &mut on_event, &mut stats)? {
                FileEnd::Clean => {}
                // Torn tail from a crash: keep everything before `offset`, drop the rest.
                FileEnd::Bad { offset, why } if is_last => {
                    let f = OpenOptions::new().write(true).open(path)?;
                    stats.truncated_bytes = f.metadata()?.len() - offset;
                    tracing::warn!(file = %path.display(), offset, why, "truncating torn wal tail");
                    f.set_len(offset)?;
                    f.sync_all()?;
                }
                // Damage before the last file can't be a torn write; stop instead of losing confirmed events.
                FileEnd::Bad { offset, why } => {
                    return Err(WalError::Corrupt {
                        file: path.clone(),
                        offset,
                        why,
                    });
                }
            }
        }

        // Keep appending to the last file, or start the first one on an empty folder.
        let (out, file_len) = match files.last() {
            Some(path) => {
                let mut f = OpenOptions::new().append(true).open(path)?;
                let len = f.seek(SeekFrom::End(0))?;
                (BufWriter::new(f), len)
            }
            None => (BufWriter::new(new_file(dir, stats.last_seq + 1)?), FILE_HEADER_LEN),
        };
        Ok((
            Wal {
                dir: dir.to_path_buf(),
                out,
                file_len,
                roll_bytes,
                appended: 0,
            },
            stats,
        ))
    }

    /// Buffers one record. Durable only after [`Wal::sync`].
    pub fn append(&mut self, ev: &RunEvent) -> Result<(), WalError> {
        let rec = encode(ev)?;
        // Roll before this record would overflow the file; a lone oversized record still gets written.
        if self.file_len > FILE_HEADER_LEN && self.file_len + rec.len() as u64 > self.roll_bytes {
            self.roll(ev.event_seq)?;
        }
        self.out.write_all(&rec)?;
        self.file_len += rec.len() as u64;
        self.appended += rec.len() as u64;
        Ok(())
    }

    /// Flushes buffered records and fsyncs the current file. Once this returns, the events survive a crash
    /// or power loss, and the writer may confirm them.
    pub fn sync(&mut self) -> Result<(), WalError> {
        self.out.flush()?;
        self.out.get_ref().sync_data()?;
        Ok(())
    }

    pub fn appended_bytes(&self) -> u64 {
        self.appended
    }

    /// Starts a new file at `next_seq` (if the current one has records), so the files before it hold only
    /// events `< next_seq`. Called when a memtable is frozen, so every file belongs to one memtable and can
    /// be deleted whole once that memtable is flushed.
    pub fn roll_at(&mut self, next_seq: u64) -> Result<(), WalError> {
        if self.file_len > FILE_HEADER_LEN {
            self.roll(next_seq)?;
        }
        Ok(())
    }

    /// Syncs and closes the current file, then starts a new one named after the next record's seq.
    fn roll(&mut self, first_seq: u64) -> Result<(), WalError> {
        self.sync()?;
        self.out = BufWriter::new(new_file(&self.dir, first_seq)?);
        self.file_len = FILE_HEADER_LEN;
        Ok(())
    }
}

/// Deletes WAL files whose events are all `≤ flushed_seq` (they're in registered segments) and that were
/// last written before `keep_since` (kept a while so a restart can rebuild the dedup window). The last file
/// is never deleted. Returns how many files were removed.
pub fn trim(dir: &Path, flushed_seq: u64, keep_since: std::time::SystemTime) -> io::Result<usize> {
    let files = wal_files(dir)?;
    let first = |p: &Path| -> Option<u64> { p.file_stem()?.to_str()?.parse().ok() };
    let mut removed = 0;
    for pair in files.windows(2) {
        let (this, next) = (&pair[0], &pair[1]);
        let Some(next_first) = first(next) else { continue };
        // Every record in `this` is < next_first.
        if next_first > flushed_seq + 1 {
            break;
        }
        let modified = fs::metadata(this)?.modified()?;
        if modified < keep_since {
            fs::remove_file(this)?;
            removed += 1;
        }
    }
    if removed > 0 {
        File::open(dir)?.sync_all()?;
    }
    Ok(removed)
}

fn file_name(first_seq: u64) -> String {
    format!("{first_seq:020}.wal")
}

fn new_file(dir: &Path, first_seq: u64) -> io::Result<File> {
    let path = dir.join(file_name(first_seq));
    let mut f = OpenOptions::new().create_new(true).append(true).open(&path)?;
    f.write_all(MAGIC)?;
    f.write_all(&VERSION.to_le_bytes())?;
    f.sync_all()?;
    // Also fsync the folder, so the new file's name itself survives a crash (POSIX doesn't promise that otherwise).
    File::open(dir)?.sync_all()?;
    Ok(f)
}

/// All `*.wal` files, oldest first (zero-padded names sort by seq).
fn wal_files(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "wal"))
        .collect();
    files.sort();
    Ok(files)
}

/// Builds one record: "rest" first, then prefixes its length and checksum.
fn encode(ev: &RunEvent) -> Result<Vec<u8>, WalError> {
    let json = serde_json::to_vec(&ev.fields).map_err(io::Error::other)?;
    let mut rest = Vec::with_capacity(FIXED_LEN + json.len());
    rest.extend_from_slice(&ev.event_seq.to_le_bytes());
    rest.extend_from_slice(&ev.received_at.0.to_le_bytes());
    rest.push(op_byte(ev.op));
    rest.push(source_byte(ev.source));
    rest.extend_from_slice(&ev.body_hash.to_le_bytes());
    rest.extend_from_slice(&json);
    let mut rec = Vec::with_capacity(8 + rest.len());
    rec.extend_from_slice(&(rest.len() as u32).to_le_bytes());
    rec.extend_from_slice(&crc32fast::hash(&rest).to_le_bytes());
    rec.extend_from_slice(&rest);
    Ok(rec)
}

/// How reading a file ended.
enum FileEnd {
    Clean,
    /// Every record before `offset` was good; the one at `offset` wasn't.
    Bad {
        offset: u64,
        why: String,
    },
}

/// Reads one file and feeds each good record to `on_event`, stopping at the first bad one. A bad record
/// returns `Ok(FileEnd::Bad)`, not an error: `open` decides whether that's a torn tail or corruption.
fn replay_file(path: &Path, on_event: &mut impl FnMut(RunEvent), stats: &mut ReplayStats) -> io::Result<FileEnd> {
    let mut data = Vec::new();
    File::open(path)?.read_to_end(&mut data)?;
    if data.len() < FILE_HEADER_LEN as usize || &data[..4] != MAGIC {
        return Ok(FileEnd::Bad {
            offset: 0,
            why: "missing file header".into(),
        });
    }
    let version = u32::from_le_bytes(data[4..8].try_into().unwrap());
    if version != VERSION {
        return Ok(FileEnd::Bad {
            offset: 0,
            why: format!("unknown version {version}"),
        });
    }
    let mut pos = FILE_HEADER_LEN as usize;
    while pos < data.len() {
        let bad = |why: &str| {
            Ok(FileEnd::Bad {
                offset: pos as u64,
                why: why.to_string(),
            })
        };
        let Some(head) = data.get(pos..pos + 8) else {
            return bad("short record header");
        };
        let len = u32::from_le_bytes(head[..4].try_into().unwrap());
        let crc = u32::from_le_bytes(head[4..].try_into().unwrap());
        // Check the length before using it, so a garbage length can't cause a huge read.
        if len > MAX_RECORD_LEN || (len as usize) < FIXED_LEN {
            return bad("bad record length");
        }
        let Some(rest) = data.get(pos + 8..pos + 8 + len as usize) else {
            return bad("short record");
        };
        if crc32fast::hash(rest) != crc {
            return bad("checksum mismatch");
        }
        match decode(rest) {
            Some(ev) => {
                stats.records += 1;
                stats.last_seq = stats.last_seq.max(ev.event_seq);
                on_event(ev);
            }
            None => return bad("undecodable record"),
        }
        pos += 8 + len as usize;
    }
    Ok(FileEnd::Clean)
}

/// The reverse of `encode` for an already length- and checksum-verified "rest".
fn decode(rest: &[u8]) -> Option<RunEvent> {
    let u64_at = |i: usize| u64::from_le_bytes(rest[i..i + 8].try_into().unwrap());
    Some(RunEvent {
        event_seq: u64_at(0),
        received_at: Ts(u64_at(8) as i64),
        op: byte_op(rest[16])?,
        source: byte_source(rest[17])?,
        body_hash: u64_at(18),
        fields: serde_json::from_slice(&rest[FIXED_LEN..]).ok()?,
    })
}

// The byte values below are part of the file format: never renumber them, only add new ones.

fn op_byte(op: Op) -> u8 {
    match op {
        Op::Post => 0,
        Op::Patch => 1,
    }
}

fn byte_op(b: u8) -> Option<Op> {
    match b {
        0 => Some(Op::Post),
        1 => Some(Op::Patch),
        _ => None,
    }
}

fn source_byte(s: Source) -> u8 {
    match s {
        Source::LsBatch => 0,
        Source::LsSingle => 1,
        Source::Native => 2,
    }
}

fn byte_source(b: u8) -> Option<Source> {
    match b {
        0 => Some(Source::LsBatch),
        1 => Some(Source::LsSingle),
        2 => Some(Source::Native),
        _ => None,
    }
}

/// Bytes in WAL files that may hold events after `flushed_seq` (every file whose successor starts after
/// `flushed_seq + 1`, i.e. the files a restart would have to replay).
pub fn unflushed_bytes(dir: &Path, flushed_seq: u64) -> io::Result<u64> {
    let files = wal_files(dir)?;
    let first = |p: &Path| -> Option<u64> { p.file_stem()?.to_str()?.parse().ok() };
    let mut total = 0;
    for (i, f) in files.iter().enumerate() {
        let all_flushed = files
            .get(i + 1)
            .and_then(|n| first(n))
            .is_some_and(|next| next <= flushed_seq + 1);
        if !all_flushed {
            total += fs::metadata(f)?.len().saturating_sub(FILE_HEADER_LEN);
        }
    }
    Ok(total)
}
