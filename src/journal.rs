//! The restore journal.
//!
//! Before RamFlow changes anything about a process, the original value is written here —
//! atomically — so that a crash, a kill -9 or a power cut can never leave a process stuck at a
//! lowered priority: the next start (or `ramflow restore`) undoes it. Entries are tied to the
//! boot id and to the process start time, so a rebooted machine or a reused pid is never
//! touched by mistake.
//!
//! Two kinds of change are journaled: the nice value of one process (`Task`) and the nice value
//! of a whole scheduler autogroup, i.e. a session (`Group`).

use crate::proc::Identity;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const MAGIC_V2: &str = "ramflow-journal 2";
const MAGIC_V1: &str = "ramflow-journal 1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The nice value of every thread of one process.
    Task,
    /// The nice value of a scheduler autogroup (a whole session).
    Group,
}

/// What an entry is about: a process, or an autogroup id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Task(Identity),
    Group(u64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// For `Task` the process itself; for `Group` a member that was used to address the group.
    pub id: Identity,
    pub comm: String,
    pub kind: Kind,
    /// Autogroup id (only meaningful for `Group`, else 0).
    pub group: u64,
    pub original_nice: i32,
    pub applied_nice: i32,
    /// The value in force before the most recent change. A change of level is journaled before
    /// it is made; if the program dies in between, the process may still be at this value, and
    /// restoring must recognise it as ours.
    pub previous_nice: i32,
}

impl Entry {
    pub fn task(id: Identity, comm: &str, original: i32, applied: i32) -> Entry {
        Entry {
            id,
            comm: comm.to_string(),
            kind: Kind::Task,
            group: 0,
            original_nice: original,
            applied_nice: applied,
            previous_nice: applied,
        }
    }

    pub fn group(id: Identity, comm: &str, group: u64, original: i32, applied: i32) -> Entry {
        Entry {
            id,
            comm: comm.to_string(),
            kind: Kind::Group,
            group,
            original_nice: original,
            applied_nice: applied,
            previous_nice: applied,
        }
    }

    pub fn key(&self) -> Key {
        match self.kind {
            Kind::Task => Key::Task(self.id),
            Kind::Group => Key::Group(self.group),
        }
    }
}

#[derive(Debug)]
pub struct Journal {
    path: Option<PathBuf>,
    boot_id: String,
    entries: Vec<Entry>,
}

fn sanitize(comm: &str) -> String {
    comm.chars()
        .map(|c| {
            if c == '\t' || c == '\n' || c == '\r' {
                '_'
            } else {
                c
            }
        })
        .collect()
}

pub fn serialize(boot_id: &str, entries: &[Entry]) -> String {
    let mut s = format!("{MAGIC_V2}\nboot\t{boot_id}\n");
    for e in entries {
        let kind = match e.kind {
            Kind::Task => "task",
            Kind::Group => "group",
        };
        s.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            e.id.pid,
            e.id.start,
            kind,
            e.group,
            e.original_nice,
            e.applied_nice,
            e.previous_nice,
            sanitize(&e.comm)
        ));
    }
    s
}

/// Parse a journal (current format, or the 0.1 format, whose entries are all `Task`).
/// Returns the boot id it was written under and its entries; malformed entry lines are skipped
/// (a half-written file must not block restoring the rest).
pub fn parse(text: &str) -> Option<(String, Vec<Entry>)> {
    let mut lines = text.lines();
    let magic = lines.next()?;
    if magic != MAGIC_V2 && magic != MAGIC_V1 {
        return None;
    }
    let boot = lines.next()?.strip_prefix("boot\t")?.to_string();
    let mut entries = Vec::new();
    for line in lines {
        if magic == MAGIC_V1 {
            let f: Vec<&str> = line.splitn(5, '\t').collect();
            if f.len() != 5 {
                continue;
            }
            let (Ok(pid), Ok(start), Ok(original), Ok(applied)) =
                (f[0].parse(), f[1].parse(), f[2].parse(), f[3].parse())
            else {
                continue;
            };
            entries.push(Entry::task(
                Identity { pid, start },
                f[4],
                original,
                applied,
            ));
            continue;
        }
        let f: Vec<&str> = line.splitn(8, '\t').collect();
        if f.len() != 8 {
            continue;
        }
        let kind = match f[2] {
            "task" => Kind::Task,
            "group" => Kind::Group,
            _ => continue,
        };
        let (Ok(pid), Ok(start), Ok(group), Ok(original), Ok(applied), Ok(previous)) = (
            f[0].parse(),
            f[1].parse(),
            f[3].parse(),
            f[4].parse(),
            f[5].parse(),
            f[6].parse(),
        ) else {
            continue;
        };
        entries.push(Entry {
            id: Identity { pid, start },
            comm: f[7].to_string(),
            kind,
            group,
            original_nice: original,
            applied_nice: applied,
            previous_nice: previous,
        });
    }
    Some((boot, entries))
}

impl Journal {
    /// A journal that is never written to disk (dry-run and tests).
    pub fn in_memory(boot_id: &str) -> Self {
        Journal {
            path: None,
            boot_id: boot_id.to_string(),
            entries: Vec::new(),
        }
    }

    /// Open (or start) the journal at `path`. Entries from another boot are discarded: after a
    /// reboot every process has already been reset.
    pub fn open(path: &Path, current_boot: &str) -> io::Result<Self> {
        let mut j = Journal {
            path: Some(path.to_path_buf()),
            boot_id: current_boot.to_string(),
            entries: Vec::new(),
        };
        match fs::read_to_string(path) {
            Ok(text) => {
                if let Some((boot, entries)) = parse(&text) {
                    if boot == current_boot {
                        j.entries = entries;
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok(j)
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn contains(&self, key: Key) -> bool {
        self.entries.iter().any(|e| e.key() == key)
    }

    pub fn get(&self, key: Key) -> Option<&Entry> {
        self.entries.iter().find(|e| e.key() == key)
    }

    pub fn record(&mut self, e: Entry) {
        let key = e.key();
        self.entries.retain(|x| x.key() != key);
        self.entries.push(e);
    }

    pub fn remove(&mut self, key: Key) {
        self.entries.retain(|x| x.key() != key);
    }

    /// Atomically persist: write a temp file, fsync, rename over the journal.
    pub fn save(&self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if self.entries.is_empty() {
            return match fs::remove_file(path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(serialize(&self.boot_id, &self.entries).as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(pid: i32, start: u64) -> Identity {
        Identity { pid, start }
    }

    fn entry(pid: i32, start: u64) -> Entry {
        Entry::task(id(pid, start), "ffmpeg", 0, 15)
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ramflow-journal-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn roundtrip_through_text() {
        let es = vec![
            entry(10, 100),
            Entry {
                comm: "weird\tname".into(),
                ..entry(11, 101)
            },
            Entry::group(id(12, 102), "make", 77, 0, 19),
        ];
        let (boot, back) = parse(&serialize("abc", &es)).unwrap();
        assert_eq!(boot, "abc");
        assert_eq!(back.len(), 3);
        assert_eq!(back[0], es[0]);
        assert_eq!(
            back[1].comm, "weird_name",
            "tabs are sanitized so the format stays parseable"
        );
        assert_eq!(back[2], es[2]);
        assert_eq!(back[2].kind, Kind::Group);
        assert_eq!(back[2].group, 77);
    }

    #[test]
    fn the_first_release_format_is_still_understood() {
        let text = format!(
            "{MAGIC_V1}\nboot\tb1\n10\t100\t0\t15\tgood\nbroken\n12\t102\t-5\t10\talso good\n"
        );
        let (boot, es) = parse(&text).unwrap();
        assert_eq!(boot, "b1");
        assert_eq!(es.len(), 2);
        assert_eq!(es[0].kind, Kind::Task);
        assert_eq!(es[1].original_nice, -5);
    }

    #[test]
    fn garbage_and_partial_files_are_handled() {
        assert!(parse("").is_none());
        assert!(parse("not a journal").is_none());
        let text = format!(
            "{MAGIC_V2}\nboot\tb1\n10\t100\ttask\t0\t0\t15\t15\tgood\nthis is broken\n11\tx\ttask\t0\t0\t15\t15\tbad\n12\t102\tweird\t0\t0\t15\t15\tbad kind\n13\t103\tgroup\t9\t-5\t10\t5\talso good\n"
        );
        let (_, es) = parse(&text).unwrap();
        assert_eq!(es.len(), 2);
        assert_eq!(es[1].original_nice, -5);
        assert_eq!(es[1].group, 9);
        assert_eq!(es[1].previous_nice, 5);
    }

    #[test]
    fn the_in_flight_value_survives_a_roundtrip() {
        let e = Entry {
            previous_nice: 10,
            applied_nice: 15,
            ..entry(3, 30)
        };
        let (_, back) = parse(&serialize("b", std::slice::from_ref(&e))).unwrap();
        assert_eq!(back[0].previous_nice, 10);
        assert_eq!(back[0].applied_nice, 15);
    }

    #[test]
    fn persists_and_reloads_in_same_boot() {
        let d = tmpdir("same");
        let p = d.join("journal");
        let mut j = Journal::open(&p, "boot1").unwrap();
        j.record(entry(5, 50));
        j.record(Entry::group(id(6, 60), "make", 3, 0, 19));
        j.save().unwrap();
        let again = Journal::open(&p, "boot1").unwrap();
        assert_eq!(again.entries().len(), 2);
        assert_eq!(again.entries()[0], entry(5, 50));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn entries_from_another_boot_are_ignored() {
        let d = tmpdir("boot");
        let p = d.join("journal");
        let mut j = Journal::open(&p, "boot1").unwrap();
        j.record(entry(5, 50));
        j.save().unwrap();
        let after_reboot = Journal::open(&p, "boot2").unwrap();
        assert!(after_reboot.is_empty());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn empty_journal_removes_its_file() {
        let d = tmpdir("empty");
        let p = d.join("journal");
        let mut j = Journal::open(&p, "b").unwrap();
        j.record(entry(5, 50));
        j.save().unwrap();
        assert!(p.exists());
        j.remove(Key::Task(id(5, 50)));
        j.save().unwrap();
        assert!(!p.exists());
        j.save().unwrap(); // idempotent
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn record_replaces_same_key_only() {
        let mut j = Journal::in_memory("b");
        j.record(entry(5, 50));
        j.record(Entry {
            applied_nice: 19,
            ..entry(5, 50)
        });
        assert_eq!(j.entries().len(), 1);
        assert_eq!(j.entries()[0].applied_nice, 19);
        // Same pid but a different start time is a different process.
        j.record(entry(5, 51));
        assert_eq!(j.entries().len(), 2);
        // A group entry is keyed by the group, not by the member that addressed it.
        j.record(Entry::group(id(7, 70), "a", 4, 0, 10));
        j.record(Entry::group(id(8, 80), "b", 4, 0, 19));
        assert_eq!(j.entries().len(), 3);
        assert_eq!(j.get(Key::Group(4)).unwrap().applied_nice, 19);
        assert!(j.contains(Key::Task(id(5, 51))));
        assert!(!j.contains(Key::Group(5)));
    }

    #[test]
    fn in_memory_journal_never_touches_disk() {
        let mut j = Journal::in_memory("b");
        j.record(entry(1, 1));
        j.save().unwrap();
    }
}
