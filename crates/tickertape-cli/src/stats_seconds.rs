//! Exact second-bucket counts with bounded RAM, including arbitrarily unordered timestamps.

use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

const CHUNK: usize = 8192;

#[derive(Default)]
pub(crate) struct Seconds {
    buffer: Vec<u64>,
    runs: Option<File>,
    count: u64,
    journal: Option<PathBuf>,
}

impl Seconds {
    pub(crate) fn outside_journal(journal: &Path) -> Self {
        Self {
            journal: Some(journal.to_path_buf()),
            ..Self::default()
        }
    }

    fn temporary_file(&self) -> io::Result<File> {
        let directory = std::fs::canonicalize(std::env::temp_dir())?;
        if let Some(journal) = &self.journal {
            if directory.starts_with(std::fs::canonicalize(journal)?) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "temporary directory must be outside the journal",
                ));
            }
        }
        tempfile::tempfile_in(directory)
    }

    pub(crate) fn push(&mut self, second: u64) -> io::Result<()> {
        if self.buffer.len() == CHUNK {
            self.flush_run()?;
        }
        self.buffer.push(second);
        self.count += 1;
        Ok(())
    }

    fn flush_run(&mut self) -> io::Result<()> {
        self.buffer.sort_unstable();
        if self.runs.is_none() {
            self.runs = Some(self.temporary_file()?);
        }
        let file = self.runs.as_mut().expect("temporary file exists");
        let mut out = BufWriter::new(file);
        for second in self.buffer.drain(..) {
            out.write_all(&second.to_le_bytes())?;
        }
        out.flush()
    }

    /// Return (start second, count); ties resolve to the earliest UTC second.
    pub(crate) fn busiest(mut self) -> io::Result<Option<(u64, u64)>> {
        let mut counts = Counts::default();
        if self.runs.is_none() {
            self.buffer.sort_unstable();
            for second in self.buffer {
                counts.push(second);
            }
        } else {
            self.flush_run()?;
            let mut source = self.runs.take().expect("runs exist after flushing");
            let mut destination = self.temporary_file()?;
            let mut width = CHUNK as u64;
            while width < self.count {
                destination.set_len(0)?;
                destination.seek(SeekFrom::Start(0))?;
                merge_pass(&source, &mut destination, self.count, width)?;
                std::mem::swap(&mut source, &mut destination);
                width = width.saturating_mul(2);
            }
            let mut run = Run::new(&source, 0, self.count);
            while let Some(second) = run.next()? {
                counts.push(second);
            }
        }
        Ok(counts.finish())
    }
}

/// A buffered, positioned view of a run; independent views never share a seek cursor.
struct Run<'a> {
    file: &'a File,
    offset: u64,
    end: u64,
    bytes: [u8; 8192],
    position: usize,
    length: usize,
}

impl<'a> Run<'a> {
    fn new(file: &'a File, start: u64, end: u64) -> Self {
        Self {
            file,
            offset: start * 8,
            end: end * 8,
            bytes: [0; 8192],
            position: 0,
            length: 0,
        }
    }

    fn next(&mut self) -> io::Result<Option<u64>> {
        if self.position == self.length {
            self.length = (self.end - self.offset).min(self.bytes.len() as u64) as usize;
            if self.length == 0 {
                return Ok(None);
            }
            self.file
                .read_exact_at(&mut self.bytes[..self.length], self.offset)?;
            self.offset += self.length as u64;
            self.position = 0;
        }
        let second = u64::from_le_bytes(
            self.bytes[self.position..self.position + 8]
                .try_into()
                .expect("eight bytes"),
        );
        self.position += 8;
        Ok(Some(second))
    }
}

fn merge_pass(source: &File, destination: &mut File, count: u64, width: u64) -> io::Result<()> {
    let mut out = BufWriter::new(destination);
    let mut start = 0;
    while start < count {
        let middle = start.saturating_add(width).min(count);
        let end = middle.saturating_add(width).min(count);
        let mut left = Run::new(source, start, middle);
        let mut right = Run::new(source, middle, end);
        let (mut a, mut b) = (left.next()?, right.next()?);
        while a.is_some() || b.is_some() {
            let second = if let Some(value) = a.filter(|&value| b.is_none_or(|b| value <= b)) {
                a = left.next()?;
                value
            } else {
                let value = b.expect("one run still has a value");
                b = right.next()?;
                value
            };
            out.write_all(&second.to_le_bytes())?;
        }
        start = end;
    }
    out.flush()
}

#[derive(Default)]
struct Counts {
    current: Option<(u64, u64)>,
    best: Option<(u64, u64)>,
}

impl Counts {
    fn push(&mut self, second: u64) {
        match self.current.as_mut() {
            Some((previous, count)) if *previous == second => *count += 1,
            _ => {
                self.complete_bucket();
                self.current = Some((second, 1));
            }
        }
    }

    fn complete_bucket(&mut self) {
        if let Some((second, count)) = self.current.take() {
            if self.best.is_none_or(|(_, best)| count > best) {
                self.best = Some((second, count));
            }
        }
    }

    fn finish(mut self) -> Option<(u64, u64)> {
        self.complete_bucket();
        self.best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_boundaries_and_ties() {
        for length in [0, 1, CHUNK - 1, CHUNK, CHUNK + 1, 2 * CHUNK, 4 * CHUNK + 3] {
            let mut seconds = Seconds::default();
            for i in (0..length).rev() {
                seconds.push((i % 13) as u64).unwrap();
            }
            assert_eq!(
                seconds.busiest().unwrap(),
                if length == 0 {
                    None
                } else {
                    Some((0, length.div_ceil(13) as u64))
                }
            );
        }
    }
}
