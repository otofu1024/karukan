use std::collections::{BinaryHeap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::Path;

use memmap2::Mmap;
use unicode_normalization::UnicodeNormalization;

/// Errors that can occur during dictionary operations.
#[derive(Debug, thiserror::Error)]
pub enum DictError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("JSON parse error")]
    Json(#[from] serde_json::Error),

    #[error("invalid dictionary format: {0}")]
    Format(String),
}

type Result<T> = std::result::Result<T, DictError>;
use serde::Deserialize;
use yada::DoubleArray;
use yada::builder::DoubleArrayBuilder;

use crate::kana::katakana_to_hiragana;

const MAGIC: &[u8; 4] = b"KRKN";
const VERSION: u32 = 1;

/// A candidate surface form with its score, stored without growable-string capacity.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub surface: Box<str>,
    pub score: f32,
}

/// A dictionary entry with fixed-size reading and candidate storage.
#[derive(Debug, Clone)]
pub struct DictEntry {
    pub reading: Box<str>,
    pub candidates: Box<[Candidate]>,
}

/// A dictionary lookup with a borrowed reading and only the matching candidates decoded.
#[derive(Debug)]
pub struct LookupResult<'a> {
    pub reading: &'a str,
    pub candidates: Vec<Candidate>,
}

/// A predictive match: a candidate whose reading extends the searched prefix.
#[derive(Debug)]
pub struct PredictiveMatch<'a> {
    pub reading: &'a str,
    pub candidate: Candidate,
}

/// An immutable binary dictionary with a double-array trie and a compact entry index.
pub struct Dictionary {
    data: Mmap,
    trie_end: usize,
    entry_offsets: Box<[usize]>,
}

// JSON deserialization types
#[derive(Deserialize)]
struct JsonCandidate {
    surface: String,
    score: f32,
}

#[derive(Deserialize)]
struct JsonEntry {
    reading: String,
    candidates: Vec<JsonCandidate>,
}

impl Dictionary {
    /// Build a Dictionary from pre-sorted entries.
    ///
    /// Entries must already be sorted by `reading` bytes and deduplicated.
    /// This is the shared final step for all dictionary builders; it also
    /// sorts each entry's candidates by score ascending, the invariant
    /// every lookup relies on (binary lookups enforce the same on read).
    fn build_from_entries(mut entries: Vec<DictEntry>) -> Result<Self> {
        for entry in &mut entries {
            entry.candidates.sort_by(|a, b| a.score.total_cmp(&b.score));
        }
        let keyset: Vec<(&[u8], u32)> = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.reading.as_bytes(), i as u32))
            .collect();

        let trie_bytes = DoubleArrayBuilder::build(&keyset)
            .map_err(|e| DictError::Format(format!("failed to build double-array trie: {e}")))?;

        let mut snapshot = tempfile::tempfile()?;
        {
            let mut w = BufWriter::new(&mut snapshot);
            w.write_all(MAGIC)?;
            w.write_all(&VERSION.to_le_bytes())?;
            w.write_all(&(trie_bytes.len() as u32).to_le_bytes())?;
            w.write_all(&trie_bytes)?;
            w.write_all(&(entries.len() as u32).to_le_bytes())?;
            for entry in &entries {
                write_string(&mut w, &entry.reading)?;
                let count = u16::try_from(entry.candidates.len())
                    .map_err(|_| DictError::Format("too many candidates for one reading".into()))?;
                w.write_all(&count.to_le_bytes())?;
                for candidate in entry.candidates.iter() {
                    write_string(&mut w, &candidate.surface)?;
                    w.write_all(&candidate.score.to_le_bytes())?;
                }
            }
            w.flush()?;
        }
        drop(entries);
        drop(trie_bytes);
        Self::from_snapshot(snapshot)
    }

    /// Build a Dictionary from a JSON file.
    ///
    /// The JSON format is an array of `{reading, candidates: [{surface, score}]}`.
    /// Readings are converted from katakana to hiragana.
    pub fn build_from_json(path: impl AsRef<Path>) -> Result<Self> {
        let file = File::open(path.as_ref())?;
        let reader = BufReader::new(file);
        let json_entries: Vec<JsonEntry> = serde_json::from_reader(reader)?;

        // Convert to DictEntry with hiragana readings
        let mut entries: Vec<DictEntry> = json_entries
            .into_iter()
            .map(|je| DictEntry {
                reading: katakana_to_hiragana(&je.reading).into_boxed_str(),
                candidates: je
                    .candidates
                    .into_iter()
                    .map(|jc| Candidate {
                        surface: jc.surface.into_boxed_str(),
                        score: jc.score,
                    })
                    .collect(),
            })
            .collect();

        // Sort by reading bytes for the trie builder
        entries.sort_by(|a, b| a.reading.as_bytes().cmp(b.reading.as_bytes()));

        // Deduplicate entries with the same reading (keep the first occurrence)
        entries.dedup_by(|b, a| a.reading == b.reading);

        Self::build_from_entries(entries)
    }

    /// Save the dictionary to a binary file.
    ///
    /// Format:
    /// ```text
    /// [4B] magic "KRKN"
    /// [4B] version (1u32 LE)
    /// [4B] trie_len (u32 LE)
    /// [trie_len B] trie bytes
    /// [4B] num_entries (u32 LE)
    /// For each entry:
    ///   [2B] reading_len (u16 LE)
    ///   [reading_len B] reading (UTF-8)
    ///   [2B] num_candidates (u16 LE)
    ///   For each candidate:
    ///     [2B] surface_len (u16 LE)
    ///     [surface_len B] surface (UTF-8)
    ///     [4B] score (f32 LE)
    /// ```
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        File::create(path)?.write_all(&self.data)?;
        Ok(())
    }

    /// Load an immutable, file-backed snapshot without expanding every entry.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let mut snapshot = tempfile::tempfile()?;
        std::io::copy(&mut File::open(path)?, &mut snapshot)?;
        Self::from_snapshot(snapshot)
    }

    fn from_snapshot(snapshot: File) -> Result<Self> {
        snapshot.sync_data()?;
        // The private temporary file has no exposed path or surviving writable handle.
        let data = unsafe { Mmap::map(&snapshot)? };
        drop(snapshot);
        let mut reader = BinaryReader::new(&data);
        if reader.take(4)? != MAGIC {
            return Err(DictError::Format("invalid magic: expected KRKN".into()));
        }
        let version = reader.u32()?;
        if version != VERSION {
            return Err(DictError::Format(format!("unsupported version: {version}")));
        }
        let trie_len = reader.u32()? as usize;
        if trie_len > 100 * 1024 * 1024 {
            return Err(DictError::Format(format!("trie_len too large: {trie_len}")));
        }
        DoubleArray::new(reader.take(trie_len)?)
            .map_err(|e| DictError::Format(format!("invalid double-array trie: {e}")))?;
        let trie_end = data.len() - reader.remaining.len();
        let num_entries = reader.u32()? as usize;
        if num_entries > 10_000_000 {
            return Err(DictError::Format(format!(
                "num_entries too large: {num_entries}"
            )));
        }
        if num_entries > reader.remaining.len() / 4 {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
        }
        let mut entry_offsets = Vec::with_capacity(num_entries);
        let mut previous = "";
        for _ in 0..num_entries {
            entry_offsets.push(data.len() - reader.remaining.len());
            let reading = reader.string()?;
            if reading < previous {
                return Err(DictError::Format(
                    "dictionary readings are not sorted".into(),
                ));
            }
            previous = reading;
            let num_candidates = reader.u16()?;
            for _ in 0..num_candidates {
                reader.string()?;
                reader.u32()?;
            }
        }
        Ok(Self {
            data,
            trie_end,
            entry_offsets: entry_offsets.into_boxed_slice(),
        })
    }

    fn trie(&self) -> DoubleArray<&[u8]> {
        // Validated once on load; the private snapshot cannot change afterwards.
        unsafe { DoubleArray::new_unchecked(&self.data[12..self.trie_end]) }
    }

    fn entry_reader(&self, index: usize) -> BinaryReader<'_> {
        BinaryReader::new(&self.data[self.entry_offsets[index]..])
    }

    fn reading(&self, index: usize) -> &str {
        self.entry_reader(index)
            .string()
            .expect("validated reading")
    }

    fn candidates(&self, index: usize) -> impl Iterator<Item = (&str, f32)> {
        let mut reader = self.entry_reader(index);
        reader.string().expect("validated reading");
        let count = reader.u16().expect("validated candidate count");
        (0..count).map(move |_| {
            let surface = reader.string().expect("validated candidate");
            let score = f32::from_bits(reader.u32().expect("validated score"));
            (surface, score)
        })
    }

    fn lookup(&self, index: usize) -> Option<LookupResult<'_>> {
        self.entry_offsets.get(index)?;
        let mut candidates: Vec<Candidate> = self
            .candidates(index)
            .map(|(surface, score)| Candidate {
                surface: surface.into(),
                score,
            })
            .collect();
        candidates.sort_by(|a, b| a.score.total_cmp(&b.score));
        Some(LookupResult {
            reading: self.reading(index),
            candidates,
        })
    }

    /// Common prefix search: returns all entries whose reading is a prefix of `input`.
    pub fn common_prefix_search(&self, input: &str) -> Vec<LookupResult<'_>> {
        self.trie()
            .common_prefix_search(input.as_bytes())
            .filter_map(|(value, _len)| self.lookup(value as usize))
            .collect()
    }

    /// Predictive search, ranked by cost plus a completion-length penalty.
    pub fn predictive_search(&self, prefix: &str, limit: usize) -> Vec<PredictiveMatch<'_>> {
        if prefix.is_empty() {
            return Vec::new();
        }
        let pool = self
            .entries_with_prefix(prefix)
            .filter(|(_, reading)| *reading != prefix)
            .map(|(index, _)| index);
        self.rank_predictive(pool, prefix.chars().count(), limit)
    }

    /// Predictive search narrowed by a still-unresolved romaji tail: the
    /// completion after `base` must begin with one of `expansions` (the
    /// kana that tail can become). わせ + `d` matches わせだ… but not
    /// わせり…. Ranking is the same as [`Self::predictive_search`], with
    /// the completion length measured from `base`.
    pub fn predictive_search_expanded(
        &self,
        base: &str,
        expansions: &[String],
        limit: usize,
    ) -> Vec<PredictiveMatch<'_>> {
        let mut seen = HashSet::new();
        let mut pool = Vec::new();
        for expansion in expansions {
            let prefix = format!("{base}{expansion}");
            for (index, _) in self.entries_with_prefix(&prefix) {
                if seen.insert(index) {
                    pool.push(index);
                }
            }
        }
        self.rank_predictive(pool, base.chars().count(), limit)
    }

    /// Entries whose reading starts with `prefix`, in dictionary order.
    fn entries_with_prefix<'s>(
        &'s self,
        prefix: &str,
    ) -> impl Iterator<Item = (usize, &'s str)> + 's {
        let start = self.entry_offsets.partition_point(|&offset| {
            BinaryReader::new(&self.data[offset..])
                .string()
                .expect("validated reading")
                < prefix
        });
        let prefix = prefix.to_string();
        (start..self.entry_offsets.len())
            .map(move |index| (index, self.reading(index)))
            .take_while(move |(_, reading)| reading.starts_with(&prefix))
    }

    fn rank_predictive(
        &self,
        pool: impl IntoIterator<Item = usize>,
        base_chars: usize,
        limit: usize,
    ) -> Vec<PredictiveMatch<'_>> {
        if limit == 0 {
            return Vec::new();
        }
        let mut ranked: BinaryHeap<RankedCandidate<'_>> = BinaryHeap::new();
        for (entry_order, index) in pool.into_iter().enumerate() {
            let reading = self.reading(index);
            let remaining = (reading.chars().count() - base_chars) as f32;
            let penalty = 500.0 * (50.0 * remaining).ln();
            for (candidate_order, (surface, score)) in self.candidates(index).enumerate() {
                let candidate = RankedCandidate {
                    rank: score + penalty,
                    entry_order,
                    candidate_order,
                    reading,
                    surface,
                    score,
                };
                if ranked.len() < limit {
                    ranked.push(candidate);
                } else if candidate < *ranked.peek().unwrap() {
                    *ranked.peek_mut().unwrap() = candidate;
                }
            }
        }
        ranked
            .into_sorted_vec()
            .into_iter()
            .map(|item| PredictiveMatch {
                reading: item.reading,
                candidate: Candidate {
                    surface: item.surface.into(),
                    score: item.score,
                },
            })
            .collect()
    }

    /// Exact match search: returns the entry whose reading exactly matches `input`.
    pub fn exact_match_search(&self, input: &str) -> Option<LookupResult<'_>> {
        self.lookup(self.trie().exact_match_search(input.as_bytes())? as usize)
    }

    /// Write all entries to `writer`, as `reading\tsurface\tscore`.
    pub fn dump_all(&self, writer: &mut dyn std::io::Write) -> std::io::Result<usize> {
        for index in 0..self.entry_offsets.len() {
            let entry = self.lookup(index).expect("valid entry index");
            for candidate in entry.candidates {
                writeln!(
                    writer,
                    "{}\t{}\t{}",
                    entry.reading, candidate.surface, candidate.score
                )?;
            }
        }
        Ok(self.entry_offsets.len())
    }

    /// Search entries by surface substring, returning reading, surface, score.
    pub fn search_by_surface(&self, query: &str) -> Vec<(String, String, f32)> {
        let mut results = Vec::new();
        for index in 0..self.entry_offsets.len() {
            let entry = self.lookup(index).expect("valid entry index");
            for candidate in entry.candidates {
                if candidate.surface.contains(query) {
                    results.push((
                        entry.reading.to_string(),
                        candidate.surface.into_string(),
                        candidate.score,
                    ));
                }
            }
        }
        results
    }

    /// Build a Dictionary from a Mozc/Google IME TSV file.
    ///
    /// The TSV format is `reading\tword\tPOS\tcomment` (tab-separated, 4 columns).
    /// Lines starting with `#` are comments, empty lines are skipped.
    /// Readings are grouped and converted to `DictEntry` with score 0.0.
    pub fn build_from_mozc_tsv(path: impl AsRef<Path>) -> Result<Self> {
        let file = File::open(path.as_ref())?;
        let reader = BufReader::new(file);

        // reading -> Vec<surface> (preserving insertion order)
        let mut groups: HashMap<String, Vec<String>> = HashMap::new();
        let mut order: Vec<String> = Vec::new();

        for line in reader.lines() {
            let line = line?;
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let cols: Vec<&str> = line.split('\t').collect();
            if cols.len() < 2 {
                continue;
            }

            let reading = cols[0].to_string();
            let surface = cols[1].to_string();

            if reading.is_empty() || surface.is_empty() {
                continue;
            }

            let surfaces = groups.entry(reading.clone()).or_insert_with(|| {
                order.push(reading);
                Vec::new()
            });
            // Deduplicate surfaces within the same reading
            if !surfaces.contains(&surface) {
                surfaces.push(surface);
            }
        }

        // Convert to DictEntry
        let mut entries: Vec<DictEntry> = order
            .into_iter()
            .filter_map(|reading| {
                groups.remove(&reading).map(|surfaces| DictEntry {
                    reading: reading.into_boxed_str(),
                    candidates: surfaces
                        .into_iter()
                        .map(|surface| Candidate {
                            surface: surface.into_boxed_str(),
                            score: 0.0,
                        })
                        .collect(),
                })
            })
            .collect();

        // Sort by reading bytes for the trie builder
        entries.sort_by(|a, b| a.reading.as_bytes().cmp(b.reading.as_bytes()));

        Self::build_from_entries(entries)
    }

    /// Load a dictionary with auto-detection of format.
    ///
    /// If the file starts with the `KRKN` magic bytes, it is loaded as binary.
    /// Otherwise, it is parsed as Mozc/Google IME TSV format.
    pub fn load_auto(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let mut file = File::open(path)?;
        let mut magic = [0u8; 4];
        let bytes_read = file.read(&mut magic)?;

        if bytes_read >= 4 && &magic == MAGIC {
            // Binary KRKN format
            Dictionary::load(path)
        } else {
            // Mozc TSV format
            Dictionary::build_from_mozc_tsv(path)
        }
    }

    /// Merge multiple dictionaries into one.
    ///
    /// Dictionaries earlier in the list have higher priority: their candidates
    /// appear first for the same reading. Returns `None` if the input is empty.
    pub fn merge(dicts: Vec<Dictionary>) -> Result<Option<Self>> {
        if dicts.is_empty() {
            return Ok(None);
        }

        // Collect all entries, grouped by reading
        let mut merged: HashMap<Box<str>, Vec<Candidate>> = HashMap::new();
        let mut reading_order: Vec<Box<str>> = Vec::new();

        for dict in dicts {
            for index in 0..dict.entry_offsets.len() {
                let entry = dict.lookup(index).expect("valid entry index");
                let reading: Box<str> = entry.reading.into();
                if !merged.contains_key(&reading) {
                    reading_order.push(reading.clone());
                }
                let candidates = merged.entry(reading).or_default();
                extend_candidates_unique(candidates, entry.candidates);
            }
        }

        let mut entries: Vec<DictEntry> = reading_order
            .into_iter()
            .filter_map(|reading| {
                merged.remove(&reading).map(|candidates| DictEntry {
                    reading,
                    candidates: candidates.into_boxed_slice(),
                })
            })
            .collect();

        // Sort by reading bytes for the trie builder
        entries.sort_by(|a, b| a.reading.as_bytes().cmp(b.reading.as_bytes()));

        Self::build_from_entries(entries).map(Some)
    }
}

struct BinaryReader<'a> {
    remaining: &'a [u8],
}

impl<'a> BinaryReader<'a> {
    fn new(remaining: &'a [u8]) -> Self {
        Self { remaining }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let bytes = self
            .remaining
            .get(..len)
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::UnexpectedEof))?;
        self.remaining = &self.remaining[len..];
        Ok(bytes)
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn string(&mut self) -> Result<&'a str> {
        let len = self.u16()? as usize;
        std::str::from_utf8(self.take(len)?)
            .map_err(|e| DictError::Format(format!("invalid dictionary UTF-8: {e}")))
    }
}

struct RankedCandidate<'a> {
    rank: f32,
    entry_order: usize,
    candidate_order: usize,
    reading: &'a str,
    surface: &'a str,
    score: f32,
}

impl Ord for RankedCandidate<'_> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Preserve stable full-sort order even when score + penalty rounds to a tie.
        self.rank
            .total_cmp(&other.rank)
            .then(self.entry_order.cmp(&other.entry_order))
            .then(self.score.total_cmp(&other.score))
            .then(self.candidate_order.cmp(&other.candidate_order))
    }
}

impl PartialOrd for RankedCandidate<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for RankedCandidate<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for RankedCandidate<'_> {}

fn write_string(writer: &mut impl Write, text: &str) -> Result<()> {
    let len = u16::try_from(text.len())
        .map_err(|_| DictError::Format("dictionary string exceeds 65535 bytes".into()))?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(text.as_bytes())?;
    Ok(())
}

/// Append candidates to `dst`, skipping any whose surface is already present.
fn extend_candidates_unique(dst: &mut Vec<Candidate>, src: impl IntoIterator<Item = Candidate>) {
    for cand in src {
        if !dst.iter().any(|c| c.surface == cand.surface) {
            dst.push(cand);
        }
    }
}

/// Unescape `\uXXXX` Unicode escape sequences in a string.
///
/// Sudachi CSV files contain literal `\uXXXX` sequences (e.g. `\u0028` for `(`)
/// in surface forms, especially for emoji/kaomoji entries. This function converts
/// them back to actual Unicode characters.
fn unescape_unicode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            // Peek at next char
            let mut clone = chars.clone();
            if clone.next() == Some('u') {
                // Try to read 4 hex digits
                let hex: String = clone.by_ref().take(4).collect();
                if hex.len() == 4
                    && let Ok(code) = u32::from_str_radix(&hex, 16)
                    && let Some(ch) = char::from_u32(code)
                {
                    result.push(ch);
                    // Advance the actual iterator past 'u' + 4 hex digits
                    chars.next(); // 'u'
                    for _ in 0..4 {
                        chars.next();
                    }
                    continue;
                }
            }
            result.push(c);
        } else {
            result.push(c);
        }
    }
    result
}

/// Insert a (surface, cost) pair into a surfaces map, keeping the minimum cost
/// for duplicate surfaces.
fn insert_min_cost(surfaces: &mut HashMap<String, i32>, surface: String, cost: i32) {
    let entry = surfaces.entry(surface).or_insert(cost);
    if cost < *entry {
        *entry = cost;
    }
}

/// Parse a single Sudachi CSV file into a map of reading → {surface → min_cost}.
///
/// Sudachi CSV columns:
/// - 4: surface form (見出し 解析結果表示用)
/// - 3: cost (integer)
/// - 11: reading (katakana)
///
/// Readings are NFKC-normalized. Surface forms have `\uXXXX` Unicode escapes decoded.
/// For duplicate (reading, surface) pairs, the minimum cost is kept.
pub fn parse_sudachi_csv(path: &Path) -> Result<HashMap<String, HashMap<String, i32>>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut map: HashMap<String, HashMap<String, i32>> = HashMap::new();

    for line in reader.lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let cols: Vec<&str> = line.split(',').collect();
        if cols.len() < 12 {
            continue;
        }

        // Skip AA (kaomoji/emoticon) entries — not useful for IME conversion
        if cols.len() > 6 && cols[5] == "補助記号" && cols[6] == "ＡＡ" {
            continue;
        }

        let surface = unescape_unicode(cols[4]);
        // Entries with left/right context IDs of -1 have no reliable cost from Sudachi;
        // assign a large fallback cost so they rank low.
        let cost: i32 = if cols[1] == "-1" && cols[2] == "-1" {
            99999
        } else {
            match cols[3].parse() {
                Ok(v) => v,
                Err(_) => continue,
            }
        };
        let reading: String = cols[11].nfkc().collect();

        if reading.is_empty() || surface.is_empty() {
            continue;
        }

        let surfaces = map.entry(reading).or_default();
        insert_min_cost(surfaces, surface, cost);
    }

    Ok(map)
}

/// Parse multiple Sudachi CSV files and merge into a single map.
///
/// For duplicate (reading, surface) pairs across files, the minimum cost is kept.
pub fn parse_sudachi_csvs(
    paths: &[impl AsRef<Path>],
) -> Result<HashMap<String, HashMap<String, i32>>> {
    let mut merged: HashMap<String, HashMap<String, i32>> = HashMap::new();

    for path in paths {
        let map = parse_sudachi_csv(path.as_ref())?;
        merge_reading_maps(&mut merged, map);
    }

    Ok(merged)
}

/// Merge `source` reading map into `target`, keeping minimum costs.
fn merge_reading_maps(
    target: &mut HashMap<String, HashMap<String, i32>>,
    source: HashMap<String, HashMap<String, i32>>,
) {
    for (reading, surfaces) in source {
        let target_surfaces = target.entry(reading).or_default();
        for (surface, cost) in surfaces {
            insert_min_cost(target_surfaces, surface, cost);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_compact_storage_layout() {
        assert!(size_of::<Candidate>() < size_of::<(String, f32)>());
        assert!(size_of::<DictEntry>() < size_of::<(String, Vec<Candidate>)>());
    }

    #[test]
    fn test_snapshot_is_independent_of_source_file() {
        let json = create_test_json();
        let built = Dictionary::build_from_json(json.path()).unwrap();
        let source = NamedTempFile::new().unwrap();
        built.save(source.path()).unwrap();
        let loaded = Dictionary::load(source.path()).unwrap();
        File::create(source.path())
            .unwrap()
            .write_all(b"replaced")
            .unwrap();
        assert_eq!(
            loaded.exact_match_search("きょう").unwrap().candidates[0]
                .surface
                .as_ref(),
            "京"
        );
        loaded.save(source.path()).unwrap();
        assert!(
            Dictionary::load(source.path())
                .unwrap()
                .exact_match_search("とうきょう")
                .is_some()
        );
        source.close().unwrap();
        let candidates = loaded.exact_match_search("きょうと").unwrap().candidates;
        drop(loaded);
        assert_eq!(candidates[0].surface.as_ref(), "京都");
    }

    #[test]
    fn test_binary_validation_before_lazy_lookup() {
        fn load(bytes: &[u8]) -> Result<Dictionary> {
            let mut file = tempfile::tempfile().unwrap();
            file.write_all(bytes).unwrap();
            Dictionary::from_snapshot(file)
        }
        let json = create_test_json();
        let built = Dictionary::build_from_json(json.path()).unwrap();
        let bytes = &built.data[..];
        for len in [0, 1, 4, 8, 11, 12, bytes.len() - 1] {
            assert!(
                load(&bytes[..len]).is_err(),
                "accepted truncated length {len}"
            );
        }
        let trie_len = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
        let first_reading = 12 + trie_len + 4 + 2;
        let reading_len =
            u16::from_le_bytes(bytes[first_reading - 2..first_reading].try_into().unwrap())
                as usize;
        let first_surface = first_reading + reading_len + 2 + 2;
        for position in [first_reading, first_surface] {
            let mut invalid = bytes.to_vec();
            invalid[position] = 0xff;
            assert!(
                load(&invalid).is_err(),
                "accepted invalid UTF-8 at {position}"
            );
        }
        let mut unsorted = bytes.to_vec();
        unsorted[first_reading..first_reading + 3].copy_from_slice("ん".as_bytes());
        assert!(load(&unsorted).is_err());
        for (position, value) in [
            (0, 0u32),
            (4, 2u32),
            (8, 100 * 1024 * 1024 + 1),
            (12 + trie_len, 10_000_001),
        ] {
            let mut invalid = bytes.to_vec();
            invalid[position..position + 4].copy_from_slice(&value.to_le_bytes());
            assert!(load(&invalid).is_err());
        }
    }

    #[test]
    fn test_binary_rejects_oversized_strings() {
        let entries = vec![DictEntry {
            reading: "あ".into(),
            candidates: vec![Candidate {
                surface: "あ".repeat(22_000).into_boxed_str(),
                score: 0.0,
            }]
            .into_boxed_slice(),
        }];
        assert!(matches!(
            Dictionary::build_from_entries(entries),
            Err(DictError::Format(_))
        ));
    }

    #[test]
    fn test_unsorted_scores_keep_stable_predictive_ties() {
        let built = Dictionary::build_from_entries(vec![DictEntry {
            reading: "ああ".into(),
            candidates: vec![
                Candidate {
                    surface: "先".into(),
                    score: 0.0,
                },
                Candidate {
                    surface: "後".into(),
                    score: 0.0,
                },
            ]
            .into_boxed_slice(),
        }])
        .unwrap();
        let mut bytes = built.data.to_vec();
        let score_offset = built.entry_offsets[0] + 2 + "ああ".len() + 2 + 2 + "先".len();
        bytes[score_offset..score_offset + 4].copy_from_slice(&1e-7f32.to_le_bytes());
        let mut snapshot = tempfile::tempfile().unwrap();
        snapshot.write_all(&bytes).unwrap();
        let loaded = Dictionary::from_snapshot(snapshot).unwrap();
        assert_eq!(
            loaded.exact_match_search("ああ").unwrap().candidates[0]
                .surface
                .as_ref(),
            "後"
        );
        assert_eq!(
            loaded.predictive_search("あ", 1)[0]
                .candidate
                .surface
                .as_ref(),
            "後"
        );
    }

    #[test]
    fn test_predictive_top_k_matches_stable_full_sort() {
        let scores = [
            1e-7,
            0.0,
            -0.0,
            100.0,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NAN,
            -f32::NAN,
        ];
        let mut entries: Vec<DictEntry> = (0..32)
            .map(|i| DictEntry {
                reading: format!("あ{i:02}").into_boxed_str(),
                candidates: scores
                    .iter()
                    .enumerate()
                    .map(|(j, &score)| Candidate {
                        surface: format!("候補{i}-{j}").into_boxed_str(),
                        score,
                    })
                    .collect(),
            })
            .collect();
        let dict = Dictionary::build_from_entries(entries.clone()).unwrap();
        let mut expected = Vec::new();
        for entry in &mut entries {
            entry.candidates.sort_by(|a, b| a.score.total_cmp(&b.score));
            let penalty = 500.0 * (50.0 * (entry.reading.chars().count() - 1) as f32).ln();
            for candidate in entry.candidates.iter() {
                expected.push((
                    candidate.score + penalty,
                    entry.reading.to_string(),
                    candidate.surface.to_string(),
                    candidate.score.to_bits(),
                ));
            }
        }
        expected.sort_by(|a, b| a.0.total_cmp(&b.0));
        for limit in [0, 1, 9, 64, 1_000] {
            let actual: Vec<_> = dict
                .predictive_search("あ", limit)
                .into_iter()
                .map(|item| {
                    (
                        item.reading.to_string(),
                        item.candidate.surface.into_string(),
                        item.candidate.score.to_bits(),
                    )
                })
                .collect();
            let expected: Vec<_> = expected
                .iter()
                .take(limit)
                .map(|item| (item.1.clone(), item.2.clone(), item.3))
                .collect();
            assert_eq!(actual, expected, "limit {limit}");
        }
    }

    fn create_test_json() -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        // Note: readings are in katakana (as they come from the JSON)
        let json = r#"[
            {
                "reading": "キョウ",
                "candidates": [
                    {"surface": "今日", "score": 1.5},
                    {"surface": "京", "score": 0.8}
                ]
            },
            {
                "reading": "キョウト",
                "candidates": [
                    {"surface": "京都", "score": 2.0}
                ]
            },
            {
                "reading": "トウキョウ",
                "candidates": [
                    {"surface": "東京", "score": 2.5}
                ]
            }
        ]"#;
        f.write_all(json.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn test_build_from_json() {
        let json_file = create_test_json();
        let dict = Dictionary::build_from_json(json_file.path()).unwrap();

        // Readings should be converted to hiragana
        for reading in ["きょう", "きょうと", "とうきょう"] {
            assert!(dict.exact_match_search(reading).is_some());
        }
    }

    #[test]
    fn test_exact_match_search() {
        let json_file = create_test_json();
        let dict = Dictionary::build_from_json(json_file.path()).unwrap();

        let result = dict.exact_match_search("きょう").unwrap();
        assert_eq!(result.reading, "きょう");
        assert_eq!(result.candidates.len(), 2);
        // Candidates should be sorted by score ascending
        assert_eq!(result.candidates[0].surface.as_ref(), "京");
        assert!((result.candidates[0].score - 0.8).abs() < f32::EPSILON);
        assert_eq!(result.candidates[1].surface.as_ref(), "今日");
        assert!((result.candidates[1].score - 1.5).abs() < f32::EPSILON);

        assert!(dict.exact_match_search("きょうとふ").is_none());
    }

    #[test]
    fn test_predictive_search() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        // 和船 (remaining 1, cost 100)      -> 100 + 500·ln(50)  ≈ 2056
        // 早稲田大学 (remaining 5, cost 1000) -> 1000 + 500·ln(250) ≈ 3761
        // 早稲田 (remaining 1, cost 4000)    -> 4000 + 500·ln(50)  ≈ 5956
        write!(
            f,
            r#"[
                {{"reading":"わせ","candidates":[{{"surface":"和瀬","score":3000.0}}]}},
                {{"reading":"わせだ","candidates":[{{"surface":"早稲田","score":4000.0}}]}},
                {{"reading":"わせだだいがく","candidates":[{{"surface":"早稲田大学","score":1000.0}}]}},
                {{"reading":"わせん","candidates":[{{"surface":"和船","score":100.0}}]}},
                {{"reading":"とうきょう","candidates":[{{"surface":"東京","score":1.0}}]}}
            ]"#
        )
        .unwrap();
        f.flush().unwrap();
        let dict = Dictionary::build_from_json(f.path()).unwrap();

        let matches = dict.predictive_search("わせ", 10);
        let surfaces: Vec<&str> = matches
            .iter()
            .map(|m| m.candidate.surface.as_ref())
            .collect();
        // Exact わせ excluded; common long completion beats rare short one
        assert_eq!(surfaces, ["和船", "早稲田大学", "早稲田"]);
        assert_eq!(matches[1].reading, "わせだだいがく");

        // Limit caps the ranked result
        let top = dict.predictive_search("わせ", 1);
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].candidate.surface.as_ref(), "和船");

        assert!(dict.predictive_search("", 10).is_empty());
        assert!(dict.predictive_search("わせだだいがくいん", 10).is_empty());
    }

    #[test]
    fn test_predictive_search_expanded() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(
            f,
            r#"[
                {{"reading":"わせだ","candidates":[{{"surface":"早稲田","score":1000.0}}]}},
                {{"reading":"わせだだいがく","candidates":[{{"surface":"早稲田大学","score":500.0}}]}},
                {{"reading":"わせりん","candidates":[{{"surface":"ワセリン","score":10.0}}]}}
            ]"#
        )
        .unwrap();
        f.flush().unwrap();
        let dict = Dictionary::build_from_json(f.path()).unwrap();

        // Pending `d` narrows to だ/で/ど… readings: ワセリン drops out.
        // 早稲田 1000+500·ln(50·1)≈2956 ranks above 早稲田大学 500+500·ln(50·5)≈3261
        let expansions = vec!["だ".to_string(), "で".to_string(), "ど".to_string()];
        let matches = dict.predictive_search_expanded("わせ", &expansions, 10);
        let surfaces: Vec<&str> = matches
            .iter()
            .map(|m| m.candidate.surface.as_ref())
            .collect();
        assert_eq!(surfaces, ["早稲田", "早稲田大学"]);

        // Overlapping expansions don't duplicate entries
        let overlapping = vec!["だ".to_string(), "だだ".to_string()];
        let matches = dict.predictive_search_expanded("わせ", &overlapping, 10);
        assert_eq!(matches.len(), 2);

        assert!(dict.predictive_search_expanded("わせ", &[], 10).is_empty());
    }

    #[test]
    fn test_common_prefix_search() {
        let json_file = create_test_json();
        let dict = Dictionary::build_from_json(json_file.path()).unwrap();

        // "きょうと" should match both "きょう" and "きょうと"
        let results = dict.common_prefix_search("きょうと");
        assert_eq!(results.len(), 2);
        let readings: Vec<&str> = results.iter().map(|r| r.reading).collect();
        assert!(readings.contains(&"きょう"));
        assert!(readings.contains(&"きょうと"));
    }

    #[test]
    fn test_save_and_load() {
        let json_file = create_test_json();
        let dict = Dictionary::build_from_json(json_file.path()).unwrap();

        let bin_file = NamedTempFile::new().unwrap();
        dict.save(bin_file.path()).unwrap();

        let loaded = Dictionary::load(bin_file.path()).unwrap();

        // Verify loaded dictionary works the same
        let result = loaded.exact_match_search("きょう").unwrap();
        assert_eq!(result.reading, "きょう");
        assert_eq!(result.candidates.len(), 2);
        assert_eq!(result.candidates[0].surface.as_ref(), "京");
        assert!((result.candidates[0].score - 0.8).abs() < f32::EPSILON);

        let results = loaded.common_prefix_search("きょうと");
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_no_match() {
        let json_file = create_test_json();
        let dict = Dictionary::build_from_json(json_file.path()).unwrap();

        assert!(dict.exact_match_search("おおさか").is_none());
        assert!(dict.common_prefix_search("おおさか").is_empty());
    }

    fn create_test_sudachi_csv() -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        // Sudachi CSV format: col0,col1,col2,cost,surface(解析結果表示用),...,col11=reading(katakana)
        // Columns 0-11 (12 columns minimum), surface is taken from column 4
        let csv = "\
col0,col1,col2,5000,今日,col5,col6,col7,col8,col9,col10,キョウ
col0,col1,col2,6000,京,col5,col6,col7,col8,col9,col10,キョウ
col0,col1,col2,4000,京都,col5,col6,col7,col8,col9,col10,キョウト
col0,col1,col2,3000,東京,col5,col6,col7,col8,col9,col10,トウキョウ
col0,col1,col2,4500,今日,col5,col6,col7,col8,col9,col10,キョウ
";
        f.write_all(csv.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn test_parse_sudachi_csv() {
        let csv_file = create_test_sudachi_csv();
        let map = parse_sudachi_csv(csv_file.path()).unwrap();

        // Check reading groups
        assert!(map.contains_key("キョウ"));
        assert!(map.contains_key("キョウト"));
        assert!(map.contains_key("トウキョウ"));

        // Check surfaces for キョウ
        let kyou = &map["キョウ"];
        assert_eq!(kyou.len(), 2); // 今日, 京
        assert_eq!(kyou["今日"], 4500); // min(5000, 4500) = 4500
        assert_eq!(kyou["京"], 6000);

        // Check surfaces for キョウト
        let kyouto = &map["キョウト"];
        assert_eq!(kyouto.len(), 1);
        assert_eq!(kyouto["京都"], 4000);
    }

    #[test]
    fn test_parse_sudachi_csvs_merge() {
        let csv1 = create_test_sudachi_csv();

        let mut csv2 = NamedTempFile::new().unwrap();
        csv2.write_all(
            "col0,col1,col2,3500,大阪,col5,col6,col7,col8,col9,col10,オオサカ\n".as_bytes(),
        )
        .unwrap();
        csv2.write_all(
            "col0,col1,col2,4000,今日,col5,col6,col7,col8,col9,col10,キョウ\n".as_bytes(),
        )
        .unwrap();
        csv2.flush().unwrap();

        let paths = vec![csv1.path().to_path_buf(), csv2.path().to_path_buf()];
        let map = parse_sudachi_csvs(&paths).unwrap();

        assert!(map.contains_key("オオサカ"));
        assert_eq!(map["オオサカ"]["大阪"], 3500);

        // 今日 should have min cost across both files: min(4500, 4000) = 4000
        assert_eq!(map["キョウ"]["今日"], 4000);
    }

    #[test]
    fn test_unescape_unicode() {
        // Basic escapes
        assert_eq!(unescape_unicode(r"\u0028"), "(");
        assert_eq!(unescape_unicode(r"\u0029"), ")");

        // Mixed content (kaomoji-like)
        assert_eq!(
            unescape_unicode(r"ムカ!σ\u0028`・ω・ ́;\u0029"),
            "ムカ!σ(`・ω・ ́;)"
        );

        // No escapes
        assert_eq!(unescape_unicode("hello"), "hello");

        // Incomplete escape (should be left as-is)
        assert_eq!(unescape_unicode(r"\u00"), r"\u00");

        // Backslash not followed by 'u'
        assert_eq!(unescape_unicode(r"\n"), r"\n");
    }

    #[test]
    fn test_parse_sudachi_csv_unicode_unescape() {
        let mut f = NamedTempFile::new().unwrap();
        // Kaomoji entry with \u0028 and \u0029 escapes in column 4
        f.write_all(
            b"col0,col1,col2,5000,\\u0028*\\u0029,col5,col6,col7,col8,col9,col10,\xE3\x82\xAD\xE3\x82\xB4\xE3\x82\xA6\n",
        )
        .unwrap();
        f.flush().unwrap();

        let map = parse_sudachi_csv(f.path()).unwrap();
        let surfaces = &map["キゴウ"];
        // \u0028 → (, \u0029 → )
        assert!(
            surfaces.contains_key("(*)"),
            "Expected (*), got: {:?}",
            surfaces.keys().collect::<Vec<_>>()
        );
    }

    fn create_test_mozc_tsv() -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        let tsv = "# Comment line\n\
                    きょう\t今日\t名詞\t\n\
                    きょう\t京\t名詞\t\n\
                    きょうと\t京都\t名詞\tcity\n\
                    とうきょう\t東京\t名詞\tcapital\n";
        f.write_all(tsv.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn test_build_from_mozc_tsv() {
        let tsv_file = create_test_mozc_tsv();
        let dict = Dictionary::build_from_mozc_tsv(tsv_file.path()).unwrap();

        // Should have 3 readings
        assert_eq!(dict.entry_offsets.len(), 3);

        // Check exact match
        let result = dict.exact_match_search("きょう").unwrap();
        assert_eq!(result.candidates.len(), 2);
        assert_eq!(result.candidates[0].surface.as_ref(), "今日");
        assert_eq!(result.candidates[1].surface.as_ref(), "京");
        assert!((result.candidates[0].score - 0.0).abs() < f32::EPSILON);

        let result = dict.exact_match_search("きょうと").unwrap();
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(result.candidates[0].surface.as_ref(), "京都");
    }

    #[test]
    fn test_build_from_mozc_tsv_skips_invalid() {
        let mut f = NamedTempFile::new().unwrap();
        let tsv = "# Comment\n\
                    \n\
                    single_column\n\
                    \t\t名詞\t\n\
                    きょう\t今日\t名詞\t\n";
        f.write_all(tsv.as_bytes()).unwrap();
        f.flush().unwrap();

        let dict = Dictionary::build_from_mozc_tsv(f.path()).unwrap();
        assert_eq!(dict.entry_offsets.len(), 1);
        assert_eq!(
            dict.exact_match_search("きょう").unwrap().candidates[0]
                .surface
                .as_ref(),
            "今日"
        );
    }

    #[test]
    fn test_build_from_mozc_tsv_dedup_surfaces() {
        let mut f = NamedTempFile::new().unwrap();
        let tsv = "きょう\t今日\t名詞\t\n\
                    きょう\t今日\t副詞\t\n\
                    きょう\t京\t名詞\t\n";
        f.write_all(tsv.as_bytes()).unwrap();
        f.flush().unwrap();

        let dict = Dictionary::build_from_mozc_tsv(f.path()).unwrap();
        let result = dict.exact_match_search("きょう").unwrap();
        // "今日" should appear only once (deduplicated)
        assert_eq!(result.candidates.len(), 2);
        assert_eq!(result.candidates[0].surface.as_ref(), "今日");
        assert_eq!(result.candidates[1].surface.as_ref(), "京");
    }

    #[test]
    fn test_load_auto_binary() {
        let json_file = create_test_json();
        let dict = Dictionary::build_from_json(json_file.path()).unwrap();

        let bin_file = NamedTempFile::new().unwrap();
        dict.save(bin_file.path()).unwrap();

        // load_auto should detect KRKN magic and load as binary
        let loaded = Dictionary::load_auto(bin_file.path()).unwrap();
        let result = loaded.exact_match_search("きょう").unwrap();
        assert_eq!(result.candidates.len(), 2);
    }

    #[test]
    fn test_load_auto_mozc_tsv() {
        let tsv_file = create_test_mozc_tsv();

        // load_auto should detect non-KRKN and parse as Mozc TSV
        let dict = Dictionary::load_auto(tsv_file.path()).unwrap();
        let result = dict.exact_match_search("きょう").unwrap();
        assert_eq!(result.candidates.len(), 2);
        assert_eq!(result.candidates[0].surface.as_ref(), "今日");
    }

    #[test]
    fn test_merge_dictionaries() {
        // Create two TSV dictionaries
        let mut f1 = NamedTempFile::new().unwrap();
        f1.write_all("きょう\t今日\t名詞\t\nきょうと\t京都\t名詞\t\n".as_bytes())
            .unwrap();
        f1.flush().unwrap();

        let mut f2 = NamedTempFile::new().unwrap();
        f2.write_all("きょう\t教\t名詞\t\nおおさか\t大阪\t名詞\t\n".as_bytes())
            .unwrap();
        f2.flush().unwrap();

        let dict1 = Dictionary::build_from_mozc_tsv(f1.path()).unwrap();
        let dict2 = Dictionary::build_from_mozc_tsv(f2.path()).unwrap();

        let merged = Dictionary::merge(vec![dict1, dict2]).unwrap().unwrap();

        // "きょう" should have candidates from both, dict1 first
        let result = merged.exact_match_search("きょう").unwrap();
        assert_eq!(result.candidates.len(), 2);
        assert_eq!(result.candidates[0].surface.as_ref(), "今日");
        assert_eq!(result.candidates[1].surface.as_ref(), "教");

        // "きょうと" from dict1
        assert!(merged.exact_match_search("きょうと").is_some());
        // "おおさか" from dict2
        assert!(merged.exact_match_search("おおさか").is_some());
    }

    #[test]
    fn test_merge_empty() {
        let result = Dictionary::merge(vec![]).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_sudachi_csv_skips_invalid_lines() {
        let mut f = NamedTempFile::new().unwrap();
        // Too few columns
        f.write_all(b"short,line,only\n").unwrap();
        // Empty line
        f.write_all(b"\n").unwrap();
        // Invalid cost
        f.write_all(b"surface,col1,col2,notanumber,col4,col5,col6,col7,col8,col9,col10,reading\n")
            .unwrap();
        // Valid line
        f.write_all("OK,col1,col2,100,col4,col5,col6,col7,col8,col9,col10,オッケー\n".as_bytes())
            .unwrap();
        f.flush().unwrap();

        let map = parse_sudachi_csv(f.path()).unwrap();
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("オッケー"));
    }
}
