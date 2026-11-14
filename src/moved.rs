//! Content an update moved. A new version of a file often holds much of the
//! old one's content at other offsets, or in another file, and Steam's own
//! client copies such chunks instead of downloading them. This finds them in
//! what an older version left on disk, and plans the copying.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::thread;

use sha1::{Digest, Sha1};

use crate::{download, log};

/// The size of the chunks looked for. Steam cuts files into chunks of 1 MiB;
/// only the last chunk of a file, or one beside a change, is smaller, and
/// those few are downloaded.
pub const WINDOW: usize = 1 << 20;
const MODULUS: u32 = 65521;
/// How much of a file a thread reads and looks through at a time.
const SEGMENT: u64 = if cfg!(test) { 1 << 20 } else { 16 << 20 };

/// The checksum a manifest gives for each chunk: Adler-32, but starting from
/// 0 rather than 1. It can be moved along a file one byte at a time, which is
/// what lets every offset be tried.
pub fn adler(data: &[u8]) -> u32 {
    let (mut a, mut b) = (0u32, 0u32);
    for block in data.chunks(5552) { // The most bytes whose sums cannot overflow.
        for &byte in block {
            a += u32::from(byte);
            b += a;
        }
        (a, b) = (a % MODULUS, b % MODULUS);
    }
    b << 16 | a
}

/// Bytes `start..end` of the file `file`, an index into a list of paths.
pub struct Region {
    pub file: usize,
    pub start: u64,
    pub end: u64,
}

/// Look through `regions` of `files` for the chunks in `wanted`, each given
/// by its SHA-1 and checksum and `WINDOW` bytes long, at every offset. Returns
/// the file and offset of each one found. Setting `stop` ends it early.
pub fn search(files: &[PathBuf], regions: &[Region], wanted: &[([u8; 20], u32)], threads: usize, stop: &AtomicBool)
              -> HashMap<[u8; 20], (usize, u64)> {
    // Almost every offset is ruled out by one bit of a table small enough to
    // stay in the processor's cache, without a look at the map.
    let bits = (wanted.len() * 32).next_power_of_two().max(1 << 10);
    let slot = |sum: u32| (sum.wrapping_mul(0x9E37_79B1) >> (32 - bits.trailing_zeros())) as usize;
    let mut table = vec![0u64; bits / 64];
    let mut by_sum: HashMap<u32, Vec<usize>> = HashMap::new();
    for (index, (_, sum)) in wanted.iter().enumerate() {
        by_sum.entry(*sum).or_default().push(index);
        table[slot(*sum) / 64] |= 1 << (slot(*sum) % 64);
    }
    let found: Vec<AtomicBool> = wanted.iter().map(|_| AtomicBool::new(false)).collect();
    let left = AtomicUsize::new(wanted.len());
    let places = Mutex::new(HashMap::new());

    // Each segment covers the chunks starting in its first `SEGMENT` bytes.
    let mut segments = Vec::new();
    for region in regions {
        let mut start = region.start;
        while start + WINDOW as u64 <= region.end {
            segments.push((region.file, start, region.end.min(start + SEGMENT + WINDOW as u64 - 1)));
            start += SEGMENT;
        }
    }
    let next = AtomicUsize::new(0);
    thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| {
                let mut data = Vec::new();
                while !stop.load(Relaxed) && left.load(Relaxed) > 0 {
                    let Some(&(file, start, end)) = segments.get(next.fetch_add(1, Relaxed)) else { break };
                    data.resize((end - start) as usize, 0);
                    let read = File::open(&files[file]).and_then(|file| download::read_at(&file, &mut data, start));
                    if let Err(error) = read {
                        log::write(format!("search: could not read {:?} at {start}: {error}", files[file]));
                        continue;
                    }
                    let mut at = 0;
                    while at + WINDOW <= data.len() {
                        let sum = adler(&data[at..at + WINDOW]);
                        let (mut a, mut b) = (sum & 0xFFFF, sum >> 16);
                        loop {
                            let sum = b << 16 | a;
                            if table[slot(sum) / 64] >> (slot(sum) % 64) & 1 == 1
                                && let Some(same) = by_sum.get(&sum)
                                && same.iter().any(|&index| !found[index].load(Relaxed))
                            {
                                // The checksum says maybe; only the SHA-1 says yes.
                                let sha: [u8; 20] = Sha1::digest(&data[at..at + WINDOW]).into();
                                if let Some(&index) = same.iter().find(|&&index| wanted[index].0 == sha) {
                                    if !found[index].swap(true, Relaxed) {
                                        places.lock().unwrap().insert(sha, (file, start + at as u64));
                                        left.fetch_sub(1, Relaxed);
                                    }
                                    at += WINDOW; // An old file's chunks do not overlap: the next starts after this one.
                                    break;
                                }
                            }
                            if at + WINDOW >= data.len() {
                                at = data.len();
                                break;
                            }
                            // Move the checksum one byte along.
                            let (out, new) = (u32::from(data[at]), u32::from(data[at + WINDOW]));
                            a = (a + MODULUS - out + new) % MODULUS;
                            b = (b + MODULUS - out * (WINDOW as u32 % MODULUS) % MODULUS + a) % MODULUS;
                            at += 1;
                        }
                    }
                }
            });
        }
    });
    places.into_inner().unwrap()
}

/// One chunk of content to copy from where it is on disk, `from`, to every
/// place the new version has it, `to`: each a file and an offset.
pub struct Move {
    pub from: (usize, u64),
    pub size: u32,
    pub to: Vec<(usize, u64)>,
}

#[derive(Debug, PartialEq)]
pub enum Step {
    /// Read the move's content and keep it in memory: what it reads is about
    /// to be overwritten, and its own turn to be written comes later.
    Hold(usize),
    /// Write the move's content to its places, reading it first unless held.
    Copy(usize),
}

/// Put `moves` in an order in which none overwrites what a later one still
/// has to read, since they copy within the same files. Where moves wait on
/// each other in a ring (two chunks swapping places), one is held in memory,
/// up to `budget` bytes in all; past that it is left out, to be downloaded.
pub fn schedule(moves: &[Move], budget: u64) -> Vec<Step> {
    // What is read from each file, by offset, to find what a write there overwrites.
    let mut sources: HashMap<usize, Vec<(u64, u64, usize)>> = HashMap::new();
    for (index, each) in moves.iter().enumerate() {
        sources.entry(each.from.0).or_default().push((each.from.1, each.from.1 + u64::from(each.size), index));
    }
    sources.values_mut().for_each(|list| list.sort_unstable());
    let longest = moves.iter().map(|each| u64::from(each.size)).max().unwrap_or(0);
    // For each move, the moves that must be read before it is written, and the other way round.
    let mut waits: Vec<Vec<usize>> = vec![Vec::new(); moves.len()];
    let mut blocks: Vec<Vec<usize>> = vec![Vec::new(); moves.len()];
    for (index, each) in moves.iter().enumerate() {
        for &(file, offset) in &each.to {
            let (Some(list), end) = (sources.get(&file), offset + u64::from(each.size)) else { continue };
            let first = list.partition_point(|source| source.0 + longest <= offset);
            let over = list[first..].iter().take_while(|source| source.0 < end).filter(|source| source.1 > offset);
            waits[index].extend(over.map(|source| source.2).filter(|&other| other != index));
        }
        waits[index].sort_unstable();
        waits[index].dedup();
        for &other in &waits[index] {
            blocks[other].push(index);
        }
    }

    #[derive(Clone, Copy, PartialEq)]
    enum State { Unread, Held, Finished, GivenUp }
    let mut state = vec![State::Unread; moves.len()];
    let mut waiting: Vec<usize> = waits.iter().map(Vec::len).collect();
    let mut ready: VecDeque<usize> = (0..moves.len()).filter(|&index| waiting[index] == 0).collect();
    let (mut steps, mut held, mut left) = (Vec::new(), 0, moves.len());
    let mut visited = vec![0; moves.len()];
    let (mut round, mut cursor) = (0, 0);
    loop {
        // `read` is a move whose content has just been read, or never will be.
        let mut read = None;
        if let Some(index) = ready.pop_front() {
            steps.push(Step::Copy(index));
            left -= 1;
            if state[index] == State::Held {
                held -= u64::from(moves[index].size);
            } else {
                read = Some(index);
            }
            state[index] = State::Finished;
        } else if left == 0 {
            return steps;
        } else {
            // Every move left waits on another, so some wait in a ring. Walk
            // from one to a move it waits on until a move comes round again:
            // that one is on a ring, and reading it now lets the others go.
            while state[cursor] != State::Unread {
                cursor += 1;
            }
            round += 1;
            let mut index = cursor;
            while visited[index] != round {
                visited[index] = round;
                index = *waits[index].iter().find(|&&other| state[other] == State::Unread).unwrap();
            }
            if held + u64::from(moves[index].size) <= budget {
                steps.push(Step::Hold(index));
                (state[index], held) = (State::Held, held + u64::from(moves[index].size));
            } else {
                (state[index], left) = (State::GivenUp, left - 1); // It is downloaded instead.
            }
            read = Some(index);
        }
        for &other in read.iter().flat_map(|&index| &blocks[index]) {
            waiting[other] -= 1;
            if waiting[other] == 0 && state[other] != State::GivenUp {
                ready.push_back(other);
            }
        }
    }
}
