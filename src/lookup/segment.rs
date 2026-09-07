//! Lookup's segment format: chunk rows, a sorted term dictionary and
//! delta-encoded postings, with an in-memory builder and a streaming merge.
//!
//! Layout, little-endian:
//! header "JLS1", version, file/chunk/term counts, six section offsets and the
//! five field-length sums; files (varint length and path); 38-byte chunk rows;
//! 22-byte dictionary rows sorted by term; a strings pool (terms in dictionary
//! order, then names, then deduplicated kinds); postings in dictionary order.
//! A segment's bytes depend only on the files and chunks it holds, so a merge
//! and a fresh build of the same files are byte-identical.

use std::collections::HashMap;
use std::fs::{self, File};
use std::hash::{BuildHasherDefault, Hasher};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use super::chunks::{Chunk, Class, FIELDS, PATH};
use super::tokens::{self, Tokenizer};

/// Bump on any tokenizer, chunker or format change.
pub(crate) const LOOKUP_VERSION: u32 = 1;

const MAGIC: &[u8; 4] = b"JLS1";
const HEADER: usize = 108;
const ROW: usize = 38;
const ENTRY: usize = 22;
const MAX_NAME: usize = 100;
const MAX_KIND: usize = 64;

/// A store file that does not decode. The store recovers by rebuilding.
#[derive(Debug)]
pub(crate) struct Corrupt(String);

impl std::fmt::Display for Corrupt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "corrupt lookup index: {}", self.0)
    }
}

impl std::error::Error for Corrupt {}

pub(crate) fn corrupt(what: &str) -> anyhow::Error {
    anyhow::Error::new(Corrupt(what.to_owned()))
}

pub(crate) fn is_corrupt(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<Corrupt>())
}

/// Fails with [`Corrupt`] unless `condition` holds.
pub(crate) fn check(condition: bool, what: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(corrupt(what))
    }
}

pub(crate) fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

pub(crate) fn get_varint(bytes: &[u8], position: &mut usize) -> Result<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes
            .get(*position)
            .ok_or_else(|| corrupt("truncated number"))?;
        *position += 1;
        let bits = u64::from(byte & 0x7f);
        check(shift < 63 || bits <= 1, "number out of range")?;
        value |= bits << shift;
        if byte < 0x80 {
            return Ok(value);
        }
    }
    Err(corrupt("number out of range"))
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap())
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

fn index(value: u64) -> Result<usize> {
    usize::try_from(value).map_err(|_| corrupt("offset out of range"))
}

/// One chunk's row. Line numbers are 1-based.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Row {
    pub(crate) file: u32,
    pub(crate) start: u32,
    pub(crate) end: u32,
    pub(crate) declaration: u32,
    name: (u32, u16),
    kind: (u32, u8),
    flags: u8,
    pub(crate) lengths: [u16; FIELDS],
}

impl Row {
    pub(crate) fn test(&self) -> bool {
        self.flags & 1 != 0
    }

    pub(crate) fn class(&self) -> Class {
        Class::from_bits((self.flags >> 1) & 7).unwrap_or(Class::Text)
    }

    fn decode(bytes: &[u8]) -> Self {
        let mut lengths = [0; FIELDS];
        for (field, length) in lengths.iter_mut().enumerate() {
            *length = u16_at(bytes, 28 + 2 * field);
        }
        Self {
            file: u32_at(bytes, 0),
            start: u32_at(bytes, 4),
            end: u32_at(bytes, 8),
            declaration: u32_at(bytes, 12),
            name: (u32_at(bytes, 16), u16_at(bytes, 20)),
            kind: (u32_at(bytes, 22), bytes[26]),
            flags: bytes[27],
            lengths,
        }
    }
}

/// A chunk on its way into a segment.
pub(crate) struct Draft {
    file: u32,
    start: u32,
    end: u32,
    declaration: u32,
    name: String,
    kind: String,
    flags: u8,
    lengths: [u16; FIELDS],
}

/// One posting: a chunk and its term frequency in each field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Posting {
    pub(crate) chunk: u32,
    pub(crate) frequency: [u32; FIELDS],
}

fn encode_posting(out: &mut Vec<u8>, previous: &mut Option<u32>, posting: &Posting) {
    put_varint(
        out,
        u64::from(posting.chunk - previous.map_or(0, |previous| previous)),
    );
    *previous = Some(posting.chunk);
    let mask = posting
        .frequency
        .iter()
        .enumerate()
        .filter(|(_, frequency)| **frequency > 0)
        .fold(0u8, |mask, (field, _)| mask | 1 << field);
    out.push(mask);
    for &frequency in posting.frequency.iter().filter(|&&frequency| frequency > 0) {
        put_varint(out, u64::from(frequency));
    }
}

/// Walks one term's postings, calling `visit` with each chunk ID and the
/// byte range of its field mask and frequencies.
fn walk_postings(
    bytes: &[u8],
    chunks: u32,
    mut visit: impl FnMut(u32, Range<usize>),
) -> Result<()> {
    let mut position = 0;
    let mut previous: Option<u32> = None;
    while position < bytes.len() {
        let delta = get_varint(bytes, &mut position)?;
        check(previous.is_none() || delta > 0, "unordered postings")?;
        let chunk = u64::from(previous.unwrap_or(0)) + delta;
        check(chunk < u64::from(chunks), "posting outside the segment")?;
        let chunk = chunk as u32;
        previous = Some(chunk);
        let start = position;
        let mask = *bytes
            .get(position)
            .ok_or_else(|| corrupt("truncated posting"))?;
        position += 1;
        check(mask != 0 && mask < 1 << FIELDS, "bad field mask")?;
        for _ in 0..mask.count_ones() {
            let value = get_varint(bytes, &mut position)?;
            check(
                (1..=u64::from(u32::MAX)).contains(&value),
                "bad term frequency",
            )?;
        }
        visit(chunk, start..position);
    }
    Ok(())
}

fn decode_postings(bytes: &[u8], chunks: u32, out: &mut Vec<Posting>) -> Result<()> {
    out.clear();
    walk_postings(bytes, chunks, |chunk, range| {
        let mask = bytes[range.start];
        let mut position = range.start + 1;
        let mut frequency = [0; FIELDS];
        for (field, slot) in frequency.iter_mut().enumerate() {
            if mask & 1 << field != 0 {
                // Validated by the walk.
                *slot = get_varint(bytes, &mut position).unwrap_or(1) as u32;
            }
        }
        out.push(Posting { chunk, frequency });
    })
}

/// A dictionary row.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Entry {
    text: (u32, u16),
    pub(crate) df: u32,
    offset: u64,
    length: u32,
}

enum Postings {
    /// Postings start at this offset of the segment's own bytes.
    Memory(usize),
    /// Postings start at this offset of the file.
    File(File, u64),
}

/// A decoded, validated segment. Rows and strings are read on demand from
/// the bytes before the postings, which are read per term.
pub(crate) struct Segment {
    data: Vec<u8>,
    files: Vec<String>,
    ranges: Vec<Range<u32>>,
    chunks: u32,
    terms: u32,
    chunks_at: usize,
    dict_at: usize,
    strings: Range<usize>,
    postings: Postings,
    postings_length: u64,
    pub(crate) sums: [u64; FIELDS],
}

impl Segment {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path)?;
        let length = file.metadata()?.len();
        let mut header = [0u8; HEADER];
        file.read_exact(&mut header)
            .map_err(|_| corrupt("truncated header"))?;
        let postings_at = u64_at(&header, 52);
        check(
            postings_at >= HEADER as u64 && postings_at <= length,
            "bad postings offset",
        )?;
        let mut data = vec![0; index(postings_at)?];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut data)?;
        Self::decode(data, length, Some(file))
    }

    pub(crate) fn from_bytes(data: Vec<u8>) -> Result<Self> {
        let length = data.len() as u64;
        Self::decode(data, length, None)
    }

    fn decode(data: Vec<u8>, length: u64, file: Option<File>) -> Result<Self> {
        check(data.len() >= HEADER, "truncated header")?;
        check(&data[..4] == MAGIC, "bad magic")?;
        check(u32_at(&data, 4) == LOOKUP_VERSION, "unsupported version")?;
        let files = u32_at(&data, 8);
        let chunks = u32_at(&data, 12);
        let terms = u32_at(&data, 16);
        let offsets = (0..6)
            .map(|slot| u64_at(&data, 20 + 8 * slot))
            .collect::<Vec<_>>();
        let mut sums = [0; FIELDS];
        for (field, sum) in sums.iter_mut().enumerate() {
            *sum = u64_at(&data, 68 + 8 * field);
        }
        check(
            offsets[0] == HEADER as u64
                && offsets.windows(2).all(|pair| pair[0] <= pair[1])
                && offsets[5] == length,
            "bad section offsets",
        )?;
        let files_at = index(offsets[0])?;
        let chunks_at = index(offsets[1])?;
        let dict_at = index(offsets[2])?;
        let strings_at = index(offsets[3])?;
        let postings_at = offsets[4];
        check(
            (dict_at - chunks_at) as u64 == u64::from(chunks) * ROW as u64
                && (strings_at - dict_at) as u64 == u64::from(terms) * ENTRY as u64
                && index(postings_at)? <= data.len(),
            "bad section sizes",
        )?;
        let strings = strings_at..index(postings_at)?;
        // Every file name takes at least one byte, which bounds the count.
        check(files as usize <= chunks_at - files_at, "bad file count")?;
        let mut names = Vec::with_capacity(files as usize);
        let mut position = files_at;
        for _ in 0..files {
            let length = index(get_varint(&data[..chunks_at], &mut position)?)?;
            let end = position
                .checked_add(length)
                .filter(|&end| end <= chunks_at)
                .ok_or_else(|| corrupt("truncated file name"))?;
            let name = std::str::from_utf8(&data[position..end])
                .map_err(|_| corrupt("file name is not UTF-8"))?;
            names.push(name.to_owned());
            position = end;
        }
        check(position == chunks_at, "bad file table")?;
        let postings_length = offsets[5] - offsets[4];
        let postings = match file {
            Some(file) => Postings::File(file, postings_at),
            None => Postings::Memory(index(postings_at)?),
        };
        let mut segment = Self {
            data,
            files: names,
            ranges: Vec::new(),
            chunks,
            terms,
            chunks_at,
            dict_at,
            strings,
            postings,
            postings_length,
            sums,
        };
        segment.validate()?;
        Ok(segment)
    }

    fn string(&self, (offset, length): (u32, usize)) -> Result<&str> {
        let start = self
            .strings
            .start
            .checked_add(offset as usize)
            .ok_or_else(|| corrupt("string offset"))?;
        let end = start
            .checked_add(length)
            .filter(|&end| end <= self.strings.end)
            .ok_or_else(|| corrupt("string outside the pool"))?;
        std::str::from_utf8(&self.data[start..end]).map_err(|_| corrupt("string is not UTF-8"))
    }

    fn validate(&mut self) -> Result<()> {
        let mut ranges = vec![0..0; self.files.len()];
        let mut previous_file = 0;
        for id in 0..self.chunks {
            let row = self.row(id);
            check(
                (row.file as usize) < self.files.len() && row.file >= previous_file,
                "chunk file out of order",
            )?;
            check(
                1 <= row.start
                    && row.start <= row.end
                    && Class::from_bits((row.flags >> 1) & 7).is_some(),
                "bad chunk row",
            )?;
            self.string((row.name.0, row.name.1.into()))?;
            self.string((row.kind.0, row.kind.1.into()))?;
            let range = &mut ranges[row.file as usize];
            if range.start == range.end {
                *range = id..id + 1;
            } else {
                range.end = id + 1;
            }
            previous_file = row.file;
        }
        self.ranges = ranges;
        let mut expected = 0u64;
        let mut previous: Option<&[u8]> = None;
        for slot in 0..self.terms {
            let entry = self.entry(slot);
            let text = self.string((entry.text.0, entry.text.1.into()))?.as_bytes();
            check(
                previous.is_none_or(|previous| previous < text) && entry.df > 0,
                "dictionary out of order",
            )?;
            check(entry.offset == expected, "postings out of order")?;
            expected += u64::from(entry.length);
            previous = Some(text);
        }
        check(expected == self.postings_length, "bad postings size")?;
        Ok(())
    }

    /// The whole encoded segment, for a segment held in memory.
    #[cfg(test)]
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.data
    }

    pub(crate) fn files(&self) -> &[String] {
        &self.files
    }

    /// The chunk IDs of each file, indexed like [`Self::files`].
    pub(crate) fn ranges(&self) -> &[Range<u32>] {
        &self.ranges
    }

    pub(crate) fn chunk_count(&self) -> u32 {
        self.chunks
    }

    pub(crate) fn row(&self, id: u32) -> Row {
        let at = self.chunks_at + id as usize * ROW;
        Row::decode(&self.data[at..at + ROW])
    }

    pub(crate) fn name(&self, row: &Row) -> &str {
        self.string((row.name.0, row.name.1.into()))
            .unwrap_or_default()
    }

    pub(crate) fn kind(&self, row: &Row) -> &str {
        self.string((row.kind.0, row.kind.1.into()))
            .unwrap_or_default()
    }

    fn entry(&self, slot: u32) -> Entry {
        let at = self.dict_at + slot as usize * ENTRY;
        let bytes = &self.data[at..at + ENTRY];
        Entry {
            text: (u32_at(bytes, 0), u16_at(bytes, 4)),
            df: u32_at(bytes, 6),
            offset: u64_at(bytes, 10),
            length: u32_at(bytes, 18),
        }
    }

    /// A term's bytes; validated as UTF-8 when the segment was opened.
    fn term(&self, entry: &Entry) -> &[u8] {
        let start = self.strings.start + entry.text.0 as usize;
        &self.data[start..start + usize::from(entry.text.1)]
    }

    pub(crate) fn find(&self, term: &str) -> Option<Entry> {
        let (mut low, mut high) = (0, self.terms);
        while low < high {
            let middle = low + (high - low) / 2;
            let entry = self.entry(middle);
            match self.term(&entry).cmp(term.as_bytes()) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return Some(entry),
            }
        }
        None
    }

    /// Reads and decodes one term's postings.
    pub(crate) fn postings(&self, entry: &Entry, out: &mut Vec<Posting>) -> Result<()> {
        let mut bytes = vec![0; entry.length as usize];
        match &self.postings {
            Postings::Memory(start) => {
                let at = start + index(entry.offset)?;
                bytes.copy_from_slice(&self.data[at..at + entry.length as usize]);
            }
            Postings::File(file, start) => {
                let mut file = file;
                file.seek(SeekFrom::Start(start + entry.offset))?;
                file.read_exact(&mut bytes)
                    .map_err(|_| corrupt("truncated postings"))?;
            }
        }
        decode_postings(&bytes, self.chunks, out)?;
        check(out.len() == entry.df as usize, "document count mismatch")?;
        Ok(())
    }

    /// Reads every term's postings in dictionary order.
    fn sequential(&self) -> Result<Box<dyn Read + '_>> {
        Ok(match &self.postings {
            Postings::Memory(start) => Box::new(&self.data[*start..]),
            Postings::File(file, start) => {
                let mut file = file;
                file.seek(SeekFrom::Start(*start))?;
                Box::new(BufReader::with_capacity(1 << 16, file))
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Writing

/// Writes a segment. `dict` holds (term, df, postings length) in term order and
/// `postings` writes the postings in the same order.
fn write_segment<W: Write>(
    out: &mut W,
    files: &[String],
    chunks: &[Draft],
    dict: &[(Vec<u8>, u32, u32)],
    postings: impl FnOnce(&mut W) -> io::Result<()>,
) -> Result<()> {
    let mut file_table = Vec::new();
    for file in files {
        put_varint(&mut file_table, file.len() as u64);
        file_table.extend_from_slice(file.as_bytes());
    }
    let terms_length = dict
        .iter()
        .map(|(term, _, _)| term.len() as u64)
        .sum::<u64>();
    let names_length = chunks
        .iter()
        .map(|chunk| chunk.name.len() as u64)
        .sum::<u64>();
    let mut kinds = Vec::<&str>::new();
    let mut kind_at = HashMap::<&str, u64>::new();
    let mut kinds_length = 0;
    for chunk in chunks {
        if !kind_at.contains_key(chunk.kind.as_str()) {
            kind_at.insert(&chunk.kind, terms_length + names_length + kinds_length);
            kinds_length += chunk.kind.len() as u64;
            kinds.push(&chunk.kind);
        }
    }
    let postings_length = dict
        .iter()
        .map(|(_, _, length)| u64::from(*length))
        .sum::<u64>();
    let files_at = HEADER as u64;
    let chunks_at = files_at + file_table.len() as u64;
    let dict_at = chunks_at + (chunks.len() * ROW) as u64;
    let strings_at = dict_at + (dict.len() * ENTRY) as u64;
    let postings_at = strings_at + terms_length + names_length + kinds_length;
    let end = postings_at + postings_length;
    let mut sums = [0u64; FIELDS];
    for chunk in chunks {
        for (sum, &length) in sums.iter_mut().zip(&chunk.lengths) {
            *sum += u64::from(length);
        }
    }
    let count = |value: usize| u32::try_from(value).context("lookup segment is too large");
    let mut header = Vec::with_capacity(HEADER);
    header.extend_from_slice(MAGIC);
    header.extend_from_slice(&LOOKUP_VERSION.to_le_bytes());
    for value in [
        count(files.len())?,
        count(chunks.len())?,
        count(dict.len())?,
    ] {
        header.extend_from_slice(&value.to_le_bytes());
    }
    for value in [files_at, chunks_at, dict_at, strings_at, postings_at, end] {
        header.extend_from_slice(&value.to_le_bytes());
    }
    for sum in sums {
        header.extend_from_slice(&sum.to_le_bytes());
    }
    out.write_all(&header)?;
    out.write_all(&file_table)?;
    let mut name_at = terms_length;
    let mut row = Vec::with_capacity(ROW);
    for chunk in chunks {
        row.clear();
        for value in [chunk.file, chunk.start, chunk.end, chunk.declaration] {
            row.extend_from_slice(&value.to_le_bytes());
        }
        row.extend_from_slice(&count(name_at as usize)?.to_le_bytes());
        row.extend_from_slice(&(chunk.name.len() as u16).to_le_bytes());
        row.extend_from_slice(&count(kind_at[chunk.kind.as_str()] as usize)?.to_le_bytes());
        row.push(chunk.kind.len() as u8);
        row.push(chunk.flags);
        for length in chunk.lengths {
            row.extend_from_slice(&length.to_le_bytes());
        }
        out.write_all(&row)?;
        name_at += chunk.name.len() as u64;
    }
    let mut term_at = 0u64;
    let mut posting_at = 0u64;
    for (term, df, length) in dict {
        row.clear();
        row.extend_from_slice(&count(term_at as usize)?.to_le_bytes());
        row.extend_from_slice(&(term.len() as u16).to_le_bytes());
        row.extend_from_slice(&df.to_le_bytes());
        row.extend_from_slice(&posting_at.to_le_bytes());
        row.extend_from_slice(&length.to_le_bytes());
        out.write_all(&row)?;
        term_at += term.len() as u64;
        posting_at += u64::from(*length);
    }
    for (term, _, _) in dict {
        out.write_all(term)?;
    }
    for chunk in chunks {
        out.write_all(chunk.name.as_bytes())?;
    }
    for kind in kinds {
        out.write_all(kind.as_bytes())?;
    }
    postings(out)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Building

/// A fast, deterministic hasher for interning terms.
#[derive(Default)]
struct Fx(u64);

impl Hasher for Fx {
    fn write(&mut self, bytes: &[u8]) {
        for part in bytes.chunks(8) {
            let mut word = [0u8; 8];
            word[..part.len()].copy_from_slice(part);
            self.0 = (self.0.rotate_left(5) ^ u64::from_le_bytes(word))
                .wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

#[derive(Default)]
struct List {
    previous: Option<u32>,
    df: u32,
    bytes: Vec<u8>,
}

/// Builds a segment in memory from chunked files, which arrive in path order.
#[derive(Default)]
pub(crate) struct Builder {
    files: Vec<String>,
    chunks: Vec<Draft>,
    terms: HashMap<Box<str>, u32, BuildHasherDefault<Fx>>,
    lists: Vec<List>,
    tokenizer: Tokenizer,
    pairs: Vec<(u32, u8)>,
}

fn intern(
    terms: &mut HashMap<Box<str>, u32, BuildHasherDefault<Fx>>,
    lists: &mut Vec<List>,
    term: &str,
) -> u32 {
    if let Some(&id) = terms.get(term) {
        return id;
    }
    let id = lists.len() as u32;
    terms.insert(term.into(), id);
    lists.push(List::default());
    id
}

impl Builder {
    /// Adds one file's chunks. `path` adds the file path's words to every chunk.
    pub(crate) fn add(&mut self, file: &str, chunks: &[Chunk], path: bool) {
        if chunks.is_empty() {
            return;
        }
        debug_assert!(self.files.last().is_none_or(|last| last.as_str() < file));
        let file_id = self.files.len() as u32;
        self.files.push(file.to_owned());
        let Self {
            chunks: drafts,
            terms,
            lists,
            tokenizer,
            pairs,
            ..
        } = self;
        let mut path_terms = Vec::new();
        if path {
            tokens::words(file, |_, word| {
                tokenizer.word(word, |token| path_terms.push(intern(terms, lists, token)));
            });
        }
        for chunk in chunks {
            pairs.clear();
            let mut lengths = [0u16; FIELDS];
            for (field, pieces) in chunk.fields.iter().enumerate() {
                for piece in pieces {
                    tokens::words(piece, |_, word| {
                        tokenizer.word(word, |token| {
                            pairs.push((intern(terms, lists, token), field as u8));
                        });
                    });
                }
            }
            pairs.extend(path_terms.iter().map(|&term| (term, PATH as u8)));
            for &(_, field) in pairs.iter() {
                let length = &mut lengths[field as usize];
                *length = length.saturating_add(1);
            }
            pairs.sort_unstable();
            let id = drafts.len() as u32;
            let mut index = 0;
            while index < pairs.len() {
                let term = pairs[index].0;
                let mut frequency = [0u32; FIELDS];
                while index < pairs.len() && pairs[index].0 == term {
                    frequency[pairs[index].1 as usize] += 1;
                    index += 1;
                }
                let list = &mut lists[term as usize];
                encode_posting(
                    &mut list.bytes,
                    &mut list.previous,
                    &Posting {
                        chunk: id,
                        frequency,
                    },
                );
                list.df += 1;
            }
            drafts.push(Draft {
                file: file_id,
                start: chunk.start as u32,
                end: chunk.end as u32,
                declaration: chunk.declaration as u32,
                name: super::chunks::clip(&chunk.name, MAX_NAME),
                kind: super::chunks::clip(&chunk.kind, MAX_KIND),
                flags: u8::from(chunk.test) | chunk.class.bits() << 1,
                lengths,
            });
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// Encodes the segment in memory.
    pub(crate) fn finish(self) -> Result<Segment> {
        let mut order = self
            .terms
            .iter()
            .map(|(term, &id)| (term.as_bytes(), id))
            .collect::<Vec<_>>();
        order.sort_unstable();
        let dict = order
            .iter()
            .map(|&(term, id)| {
                let list = &self.lists[id as usize];
                (term.to_vec(), list.df, list.bytes.len() as u32)
            })
            .collect::<Vec<_>>();
        let mut bytes = Vec::new();
        write_segment(&mut bytes, &self.files, &self.chunks, &dict, |out| {
            for &(_, id) in &order {
                out.write_all(&self.lists[id as usize].bytes)?;
            }
            Ok(())
        })?;
        Segment::from_bytes(bytes)
    }
}

// ---------------------------------------------------------------------------
// Merging

/// A merge input and which of its files survive.
pub(crate) struct Input<'a> {
    pub(crate) segment: &'a Segment,
    /// Indexed like [`Segment::files`].
    pub(crate) live: Vec<bool>,
}

impl<'a> Input<'a> {
    pub(crate) fn all(segment: &'a Segment) -> Self {
        Self {
            segment,
            live: vec![true; segment.files.len()],
        }
    }
}

/// Where a merged segment goes.
#[derive(Clone, Copy)]
pub(crate) enum Target<'a> {
    Memory,
    /// Written beside its final path through temporary files, then renamed.
    Disk(&'a Path),
}

fn temporary(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// Merges the live files of every input into one path-ordered segment. Inputs
/// must not share a live file. Postings are streamed one term at a time, so
/// memory grows with chunks and terms, not with postings.
pub(crate) fn merge(inputs: &[Input], target: Target) -> Result<Segment> {
    let mut files = Vec::<(&str, usize, usize)>::new();
    for (input, source) in inputs.iter().enumerate() {
        for (file, name) in source.segment.files.iter().enumerate() {
            if source.live.get(file).copied().unwrap_or(false) {
                files.push((name, input, file));
            }
        }
    }
    files.sort_unstable();
    ensure!(
        files.windows(2).all(|pair| pair[0].0 != pair[1].0),
        "a file appears in two lookup segments"
    );
    let mut remap = inputs
        .iter()
        .map(|input| vec![u32::MAX; input.segment.chunks as usize])
        .collect::<Vec<_>>();
    let mut drafts = Vec::new();
    for (new_file, &(_, input, file)) in files.iter().enumerate() {
        let segment = inputs[input].segment;
        for old in segment.ranges[file].clone() {
            remap[input][old as usize] = drafts.len() as u32;
            let row = segment.row(old);
            drafts.push(Draft {
                file: new_file as u32,
                start: row.start,
                end: row.end,
                declaration: row.declaration,
                name: segment.name(&row).to_owned(),
                kind: segment.kind(&row).to_owned(),
                flags: row.flags,
                lengths: row.lengths,
            });
        }
    }
    let names = files
        .iter()
        .map(|(name, _, _)| (*name).to_owned())
        .collect::<Vec<_>>();
    let mut sink = match target {
        Target::Memory => Sink::Memory(Vec::new()),
        Target::Disk(path) => {
            let postings = temporary(path, ".postings.tmp");
            let file = File::create(&postings)?;
            Sink::File(BufWriter::with_capacity(1 << 16, file), postings)
        }
    };
    let mut dict = Vec::new();
    let mut readers = inputs
        .iter()
        .map(|input| input.segment.sequential())
        .collect::<Result<Vec<_>>>()?;
    let mut cursors = vec![0u32; inputs.len()];
    let mut raws = vec![Vec::new(); inputs.len()];
    // New chunk ID, input, and the byte range of its mask and frequencies.
    let mut merged = Vec::<(u32, usize, Range<usize>)>::new();
    let mut encoded = Vec::new();
    loop {
        let term = inputs
            .iter()
            .zip(&cursors)
            .filter(|(input, cursor)| **cursor < input.segment.terms)
            .map(|(input, cursor)| input.segment.term(&input.segment.entry(*cursor)))
            .min()
            .map(<[u8]>::to_vec);
        let Some(term) = term else {
            break;
        };
        merged.clear();
        for (input, source) in inputs.iter().enumerate() {
            let cursor = cursors[input];
            if cursor >= source.segment.terms {
                continue;
            }
            let entry = source.segment.entry(cursor);
            if source.segment.term(&entry) != term.as_slice() {
                continue;
            }
            cursors[input] += 1;
            let raw = &mut raws[input];
            raw.resize(entry.length as usize, 0);
            readers[input]
                .read_exact(raw)
                .map_err(|_| corrupt("truncated postings"))?;
            let remap = &remap[input];
            walk_postings(raw, source.segment.chunks, |chunk, range| {
                let chunk = remap[chunk as usize];
                if chunk != u32::MAX {
                    merged.push((chunk, input, range));
                }
            })?;
        }
        if merged.is_empty() {
            continue;
        }
        if !merged.windows(2).all(|pair| pair[0].0 < pair[1].0) {
            merged.sort_unstable_by_key(|(chunk, _, _)| *chunk);
        }
        // Frequencies are copied as encoded; only the chunk deltas change.
        encoded.clear();
        let mut previous = 0;
        for (chunk, input, range) in &merged {
            put_varint(&mut encoded, u64::from(chunk - previous));
            encoded.extend_from_slice(&raws[*input][range.clone()]);
            previous = *chunk;
        }
        match &mut sink {
            Sink::Memory(bytes) => bytes.extend_from_slice(&encoded),
            Sink::File(writer, _) => writer.write_all(&encoded)?,
        }
        dict.push((term, merged.len() as u32, encoded.len() as u32));
    }
    drop(readers);
    match (sink, target) {
        (Sink::Memory(postings), _) => {
            let mut bytes = Vec::new();
            write_segment(&mut bytes, &names, &drafts, &dict, |out| {
                out.write_all(&postings)
            })?;
            Segment::from_bytes(bytes)
        }
        (Sink::File(mut writer, postings), Target::Disk(path)) => {
            writer.flush()?;
            drop(writer);
            let partial = temporary(path, ".tmp");
            let mut out = BufWriter::with_capacity(1 << 16, File::create(&partial)?);
            write_segment(&mut out, &names, &drafts, &dict, |out| {
                io::copy(&mut File::open(&postings)?, out).map(|_| ())
            })?;
            out.flush()?;
            drop(out);
            fs::remove_file(&postings)?;
            fs::rename(&partial, path)?;
            Segment::open(path)
        }
        (Sink::File(..), Target::Memory) => unreachable!("the sink follows the target"),
    }
}

enum Sink {
    Memory(Vec<u8>),
    File(BufWriter<File>, PathBuf),
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::super::chunks::{BODY, Chunk, Class, NAME};
    use super::*;

    fn chunk(start: usize, name: &'static str, body: &'static str) -> Chunk<'static> {
        let mut chunk = Chunk {
            start,
            end: start + 1,
            declaration: start,
            name: name.to_owned(),
            kind: "function".into(),
            test: start.is_multiple_of(2),
            class: Class::Code,
            fields: Default::default(),
        };
        chunk.fields[NAME].push(Cow::Borrowed(name));
        chunk.fields[BODY].push(Cow::Borrowed(body));
        chunk
    }

    fn add(builder: &mut Builder, file: &'static str, index: usize) {
        let chunks = [
            chunk(1, "retryPolicy", "retries are configured here"),
            chunk(index + 3, file, "a body with retry and backoff words"),
        ];
        builder.add(file, &chunks, true);
    }

    fn build(files: &[&'static str]) -> Segment {
        let mut builder = Builder::default();
        for (index, file) in files.iter().enumerate() {
            add(&mut builder, file, index);
        }
        builder.finish().unwrap()
    }

    fn postings(segment: &Segment, term: &str) -> Vec<Posting> {
        let mut out = Vec::new();
        segment
            .postings(&segment.find(term).unwrap(), &mut out)
            .unwrap();
        out
    }

    #[test]
    fn varint_edges() {
        for value in [
            0,
            1,
            127,
            128,
            16_383,
            16_384,
            u64::from(u32::MAX),
            u64::MAX,
        ] {
            let mut bytes = Vec::new();
            put_varint(&mut bytes, value);
            let mut position = 0;
            assert_eq!(get_varint(&bytes, &mut position).unwrap(), value);
            assert_eq!(position, bytes.len());
        }
        let too_large = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02];
        for bytes in [&[0x80][..], &[0xff; 10][..], &too_large[..]] {
            let error = get_varint(bytes, &mut 0).unwrap_err();
            assert!(is_corrupt(&error), "{bytes:?}");
        }
    }

    #[test]
    fn segments_round_trip_through_memory_and_disk() {
        let segment = build(&["a.rs", "b/c.py"]);
        assert_eq!(segment.files(), ["a.rs", "b/c.py"]);
        assert_eq!(segment.chunk_count(), 4);
        assert_eq!(segment.ranges(), [0..2, 2..4]);
        let row = segment.row(3);
        assert_eq!((row.file, row.start, row.end), (1, 4, 5));
        assert_eq!(segment.name(&row), "b/c.py");
        assert_eq!(segment.kind(&row), "function");
        assert!(row.test());
        assert_eq!(row.class(), Class::Code);
        let retry = postings(&segment, "retri");
        let chunks = retry
            .iter()
            .map(|posting| posting.chunk)
            .collect::<Vec<_>>();
        assert_eq!(chunks, [0, 1, 2, 3]);
        assert_eq!(retry[0].frequency, [1, 0, 0, 1, 0]);
        assert!(segment.find("absent").is_none());
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("base-1.seg");
        fs::write(&path, segment.bytes()).unwrap();
        let opened = Segment::open(&path).unwrap();
        assert_eq!(opened.files(), segment.files());
        assert_eq!(opened.sums, segment.sums);
        assert_eq!(postings(&opened, "retri"), retry);
        assert_eq!(postings(&opened, "backoff"), postings(&segment, "backoff"));
    }

    #[test]
    fn merging_away_dead_files_matches_a_fresh_build() {
        let old = build(&["a.rs", "b.rs", "d.rs"]);
        let new = build(&["c.rs"]);
        let inputs = [
            Input {
                segment: &old,
                live: vec![true, false, true],
            },
            Input::all(&new),
        ];
        let merged = merge(&inputs, Target::Memory).unwrap();
        let mut builder = Builder::default();
        for (file, index) in [("a.rs", 0), ("c.rs", 0), ("d.rs", 2)] {
            add(&mut builder, file, index);
        }
        let fresh = builder.finish().unwrap();
        assert_eq!(merged.bytes(), fresh.bytes());
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("base-2.seg");
        let on_disk = merge(&inputs, Target::Disk(&path)).unwrap();
        assert_eq!(fs::read(&path).unwrap(), fresh.bytes());
        assert_eq!(on_disk.files(), ["a.rs", "c.rs", "d.rs"]);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        let overlapping = [Input::all(&old), Input::all(&old)];
        assert!(merge(&overlapping, Target::Memory).is_err());
    }

    #[test]
    fn damaged_segments_fail_without_panicking() {
        let bytes = build(&["a.rs", "b.rs"]).bytes().to_vec();
        for length in 0..bytes.len() {
            let error = Segment::from_bytes(bytes[..length].to_vec()).err().unwrap();
            assert!(is_corrupt(&error), "prefix {length}: {error:#}");
        }
        for at in 0..bytes.len() {
            let mut damaged = bytes.clone();
            damaged[at] ^= 0xff;
            let Ok(segment) = Segment::from_bytes(damaged) else {
                continue;
            };
            for term in ["retri", "backoff", "rs", "a"] {
                if let Some(entry) = segment.find(term) {
                    let _ = segment.postings(&entry, &mut Vec::new());
                }
            }
            for id in 0..segment.chunk_count() {
                let row = segment.row(id);
                let _ = (segment.name(&row), segment.kind(&row), row.class());
            }
        }
    }
}
