//! The restore journal.
//!
//! Before RamFlow changes anything about a process, the original value is written here —
//! atomically — so that a crash, a kill -9 or a power cut can never leave a process stuck at a
//! lowered priority: the next start (or `ramflow restore`) undoes it. Entries are tied to the
//! boot id and to the process start time, so a rebooted machine or a reused pid is never
//! touched by mistake.

use crate::proc::Identity;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const MAGIC: &str = "ramflow-journal 1";

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub id: Identity,
    pub comm: String,
    pub original_nice: i32,
    pub applied_nice: i32,
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
    let mut s = format!("{MAGIC}\nboot\t{boot_id}\n");
    for e in entries {
        s.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\n",
            e.id.pid,
            e.id.start,
            e.original_nice,
            e.applied_nice,
            sanitize(&e.comm)
        ));
    }
    s
}

/// Parse a journal. Returns the boot id it was written under and its entries; malformed
/// entry lines are skipped (a half-written file must not block restoring the rest).
pub fn parse(text: &str) -> Option<(String, Vec<Entry>)> {
    let mut lines = text.lines();
    if lines.next()? != MAGIC {
        return None;
    }
    let boot = lines.next()?.strip_prefix("boot\t")?.to_string();
    let mut entries = Vec::new();
    for line in lines {
        let f: Vec<&str> = line.splitn(5, '\t').collect();
        if f.len() != 5 {
            continue;
        }
        let (Ok(pid), Ok(start), Ok(original), Ok(applied)) =
            (f[0].parse(), f[1].parse(), f[2].parse(), f[3].parse())
        else {
            continue;
        };
        entries.push(Entry {
            id: Identity { pid, start },
            comm: f[4].to_string(),
            original_nice: original,
            applied_nice: applied,
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

    pub fn contains(&self, id: Identity) -> bool {
        self.entries.iter().any(|e| e.id == id)
    }

    pub fn record(&mut self, e: Entry) {
        self.entries.retain(|x| x.id != e.id);
        self.entries.push(e);
    }

    pub fn remove(&mut self, id: Identity) {
        self.entries.retain(|x| x.id != id);
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

    fn entry(pid: i32, start: u64) -> Entry {
        Entry {
            id: Identity { pid, start },
            comm: "ffmpeg".into(),
            original_nice: 0,
            applied_nice: 15,
        }
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
        ];
        let (boot, back) = parse(&serialize("abc", &es)).unwrap();
        assert_eq!(boot, "abc");
        assert_eq!(back.len(), 2);
        assert_eq!(back[0], es[0]);
        assert_eq!(
            back[1].comm, "weird_name",
            "tabs are sanitized so the format stays parseable"
        );
    }

    #[test]
    fn garbage_and_partial_files_are_handled() {
        assert!(parse("").is_none());
        assert!(parse("not a journal").is_none());
        let text = format!("{MAGIC}\nboot\tb1\n10\t100\t0\t15\tgood\nthis is broken\n11\tx\t0\t15\tbad\n12\t102\t-5\t10\talso good\n");
        let (_, es) = parse(&text).unwrap();
        assert_eq!(es.len(), 2);
        assert_eq!(es[1].original_nice, -5);
    }

    #[test]
    fn persists_and_reloads_in_same_boot() {
        let d = tmpdir("same");
        let p = d.join("journal");
        let mut j = Journal::open(&p, "boot1").unwrap();
        j.record(entry(5, 50));
        j.save().unwrap();
        let again = Journal::open(&p, "boot1").unwrap();
        assert_eq!(again.entries(), &[entry(5, 50)]);
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
        j.remove(Identity { pid: 5, start: 50 });
        j.save().unwrap();
        assert!(!p.exists());
        j.save().unwrap(); // idempotent
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn record_replaces_same_identity() {
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
    }

    #[test]
    fn in_memory_journal_never_touches_disk() {
        let mut j = Journal::in_memory("b");
        j.record(entry(1, 1));
        j.save().unwrap();
    }
}
