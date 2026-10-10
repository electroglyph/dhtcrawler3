//! Info-dictionary parsing (v1, v2 and hybrid).

use std::borrow::Cow;
use std::collections::BinaryHeap;

use dc4_bencode::{Dict, Limits, Value};
use dc4_core::text::{sanitize_display, sanitize_path_component};
use encoding_rs::Encoding;

use crate::text::{decode_text, label_encoding};
use crate::{
    FileEntry, MAX_FILE_TREE_DEPTH, MAX_FILES_PARSED, MAX_FILES_STORED, MAX_TEXT_CHARS,
    MIN_TEXT_CHARS, NAME_MAX_CHARS, PATH_MAX_CHARS, ParseError, TEXT_CHARS_PER_BYTE, TorrentMeta,
    UNNAMED, sha1_key, sha256_hash,
};

/// First path component that marks a BEP 47 padding file.
const PAD_DIR: &str = ".pad";
/// Name prefix that legacy clients (BitComet) use for padding files.
const LEGACY_PAD_PREFIX: &str = "_____padding_file";
/// `attr` flag marking a padding file (BEP 47).
const ATTR_PADDING: u8 = b'p';
/// `meta version` value of a BitTorrent v2 torrent (BEP 52).
const META_VERSION_2: i64 = 2;
/// `private` value of a private torrent (BEP 27).
const PRIVATE_FLAG: i64 = 1;
/// Separator between path components in [`FileEntry::path`].
const PATH_SEPARATOR: char = '/';

/// Parses a raw info dictionary into a [`TorrentMeta`].
///
/// Same as [`parse_info_visit`] with a visitor that does nothing.
pub fn parse_info(info: &[u8]) -> Result<TorrentMeta, ParseError> {
    parse_info_visit(info, &mut |_| {})
}

/// Parses a raw info dictionary into a [`TorrentMeta`], showing every
/// sanitised name and path to `visit`.
///
/// The input must be a bencoded dictionary within [`Limits::METADATA`]. It
/// is a v1 torrent if it has a `pieces` byte string, and a v2 torrent if
/// `meta version` is 2 and `file tree` is a dictionary; it may be both
/// (hybrid), in which case the file list comes from the v2 tree, and the v1
/// file fields are checked and visited but not listed or counted.
///
/// `visit` is called first with the sanitised name (or [`UNNAMED`]), then,
/// in parse order, with the sanitised path of every file entry parsed,
/// padding included, including entries left out of [`TorrentMeta::files`]
/// by the [`MAX_FILES_STORED`] cap. A v1 single-file torrent's path is its
/// name, so its only call is the name. If parsing fails part-way, `visit`
/// has already seen the entries before the failure.
///
/// Every sanitised component reaches `visit`, but the work stays linear in
/// the size of `info`: a component cut off by [`PATH_MAX_CHARS`] is also
/// shown on its own, and joined paths are shown only while the text shown
/// stays within `info.len() * TEXT_CHARS_PER_BYTE`, clamped to
/// [`MIN_TEXT_CHARS`]..=[`MAX_TEXT_CHARS`]. Past that, each directory
/// component (once per directory, not per file) and each file name is shown
/// alone, and the result is [`ParseError::TooMuchText`].
///
/// File entries whose path has no valid component after sanitising (for
/// example `[".."]`) are neither visited nor listed, but their size still
/// counts toward `total_size` and `file_count`, because they are real
/// content of the torrent. Padding files are visited but not listed or
/// counted.
///
/// Note: with [`Limits::METADATA`] (1 000 000 items, keys included) the
/// bencode limit is normally reached before [`MAX_FILES_PARSED`]; the file
/// cap is kept as an independent bound.
pub fn parse_info_visit(
    info: &[u8],
    visit: &mut dyn FnMut(&str),
) -> Result<TorrentMeta, ParseError> {
    let root = dc4_bencode::decode(info, &Limits::METADATA)?;
    parse_decoded(info, &root, visit)
}

/// Parses an already decoded info dictionary; `info` is its raw bytes.
pub(crate) fn parse_decoded(
    info: &[u8],
    root: &Value<'_>,
    visit: &mut dyn FnMut(&str),
) -> Result<TorrentMeta, ParseError> {
    let dict = root.as_dict().ok_or(ParseError::NotADict)?;

    let is_v1 = matches!(dict.get(b"pieces"), Some(Value::Bytes(b)) if b.len() % 20 == 0);
    let tree = if dict.get_int(b"meta version") == Some(META_VERSION_2) {
        dict.get_dict(b"file tree")
    } else {
        None
    };
    if !is_v1 && tree.is_none() {
        return Err(ParseError::NotATorrent);
    }

    let enc = label_encoding(dict.get_bytes(b"encoding"));
    let name = sanitised_text_field(dict, b"name.utf-8", b"name", enc)
        .unwrap_or_else(|| UNNAMED.to_owned());
    visit(&name);

    let mut files = Collector::new(visit, info.len());
    if let Some(tree) = tree {
        walk_file_tree(tree, enc, &mut files)?;
        if is_v1 {
            // A hybrid's v1 list could name other files: check and show it too.
            files.shadow();
            v1_files(dict, &name, enc, &mut files)?;
        }
    } else {
        v1_files(dict, &name, enc, &mut files)?;
    }
    if files.over_budget {
        return Err(ParseError::TooMuchText);
    }

    let piece_length = dict
        .get_int(b"piece length")
        .filter(|&n| n > 0)
        .and_then(|n| u64::try_from(n).ok());

    let files_truncated = files.listable > MAX_FILES_STORED;
    Ok(TorrentMeta {
        name,
        info_hash_v1: is_v1.then(|| sha1_key(info)),
        info_hash_v2: tree.is_some().then(|| sha256_hash(info)),
        total_size: files.total_size,
        file_count: files.file_count,
        // Ascending order, the same as sorting everything and truncating.
        files: files.kept.into_sorted_vec(),
        files_truncated,
        piece_length,
        private: dict.get_int(b"private") == Some(PRIVATE_FLAG),
    })
}

/// Sanitised text under `preferred`, falling back to `fallback` when the
/// preferred value is missing, is not valid UTF-8 (BEP 3 requires `.utf-8`
/// fields to be valid UTF-8), or sanitises to empty.
fn sanitised_text_field(
    dict: &Dict<'_>,
    preferred: &[u8],
    fallback: &[u8],
    enc: Option<&'static Encoding>,
) -> Option<String> {
    if let Some(raw) = dict.get_bytes(preferred)
        && let Ok(s) = std::str::from_utf8(raw)
    {
        let s = sanitize_display(s, NAME_MAX_CHARS);
        if !s.is_empty() {
            return Some(s);
        }
    }
    if let Some(raw) = dict.get_bytes(fallback) {
        let s = sanitize_display(&decode_text(raw, enc), NAME_MAX_CHARS);
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

/// Accumulates file entries under the parse limits.
struct Collector<'v> {
    visit: &'v mut dyn FnMut(&str),
    /// The smallest [`MAX_FILES_STORED`] listable entries seen so far.
    kept: BinaryHeap<FileEntry>,
    /// Listable (non-padding, with a path) entries seen.
    listable: usize,
    parsed: usize,
    total_size: u64,
    file_count: u64,
    /// False while checking a hybrid's v1 list, whose entries are not recorded.
    record: bool,
    /// Characters shown to `visit` so far.
    shown: usize,
    /// Most characters `visit` may be shown before joined paths stop.
    budget: usize,
    /// A joined path did not fit in `budget`.
    over_budget: bool,
}

impl<'v> Collector<'v> {
    fn new(visit: &'v mut dyn FnMut(&str), info_len: usize) -> Self {
        Self {
            visit,
            kept: BinaryHeap::new(),
            listable: 0,
            parsed: 0,
            total_size: 0,
            file_count: 0,
            record: true,
            shown: 0,
            budget: info_len
                .saturating_mul(TEXT_CHARS_PER_BYTE)
                .clamp(MIN_TEXT_CHARS, MAX_TEXT_CHARS),
            over_budget: false,
        }
    }

    /// From now on, checks and shows entries without recording them. The
    /// per-listing [`MAX_FILES_PARSED`](crate::MAX_FILES_PARSED) count
    /// restarts: the v2 tree and the v1 list are capped independently (see
    /// the constant docs).
    fn shadow(&mut self) {
        self.record = false;
        self.parsed = 0;
    }

    /// Shows `text` to the visitor.
    fn show(&mut self, text: &str) {
        self.show_counted(text, text.chars().count());
    }

    fn show_counted(&mut self, text: &str, chars: usize) {
        self.shown = self.shown.saturating_add(chars);
        (self.visit)(text);
    }

    /// Shows a joined path if it fits in the budget; false if it does not.
    fn show_joined(&mut self, path: &str) -> bool {
        if self.over_budget {
            return false;
        }
        let chars = path.chars().count();
        if self.shown.saturating_add(chars) > self.budget {
            self.over_budget = true;
            return false;
        }
        self.show_counted(path, chars);
        true
    }

    /// Records one file entry. `path` is `None` when it had no valid
    /// component (or was not built because the text budget ran out).
    fn add(&mut self, padding: bool, length: i64, path: Option<String>) -> Result<(), ParseError> {
        self.parsed = self.parsed.saturating_add(1);
        if self.parsed > MAX_FILES_PARSED {
            return Err(ParseError::TooManyFiles);
        }
        let size = u64::try_from(length).map_err(|_| ParseError::NegativeLength)?;
        if padding || !self.record {
            return Ok(());
        }
        self.total_size = self
            .total_size
            .checked_add(size)
            .ok_or(ParseError::SizeOverflow)?;
        self.file_count = self.file_count.saturating_add(1);
        let Some(path) = path else {
            return Ok(());
        };
        self.listable = self.listable.saturating_add(1);
        let entry = FileEntry { path, size };
        if self.kept.len() < MAX_FILES_STORED {
            self.kept.push(entry);
        } else if self.kept.peek().is_some_and(|max| entry < *max) {
            self.kept.pop();
            self.kept.push(entry);
        }
        Ok(())
    }
}

/// The `length` of a file dictionary (sign is checked by [`Collector::add`]).
fn file_length(d: &Dict<'_>) -> Result<i64, ParseError> {
    d.get_int(b"length")
        .ok_or(ParseError::InvalidField("length"))
}

/// BEP 47 padding detection on the first and last path components.
/// Callers pass raw components, sanitised components, or both (OR-ed);
/// the stored path is built from sanitised components, so at least the
/// sanitised form must be checked.
///
/// `first` is the top-level directory, if the path has one: BEP 47 padding
/// lives in a `.pad` *directory* (`.pad/<N>`), so a bare file named `.pad`
/// with no directory part is not padding. Callers pass `None` for `first`
/// when the path is a single component; only the `attr` flag and the legacy
/// name prefix can then mark it as padding.
fn is_padding(first: Option<&str>, last: Option<&str>, attr: Option<&[u8]>) -> bool {
    attr.is_some_and(|a| a.contains(&ATTR_PADDING))
        || first == Some(PAD_DIR)
        || last.is_some_and(|c| c.starts_with(LEGACY_PAD_PREFIX))
}

/// A path being built from sanitised components, capped at
/// [`PATH_MAX_CHARS`]. Capping as components arrive gives the same result
/// as joining everything and truncating once.
#[derive(Clone, Default)]
struct CappedPath {
    text: String,
    chars: usize,
    /// Some component was cut short or left out by the cap.
    cut: bool,
}

impl CappedPath {
    /// Appends a sanitised component in one pass: walks at most `room`
    /// characters and counts while walking, instead of `nth(room)` + recount.
    fn push(&mut self, comp: &str) {
        if self.chars >= PATH_MAX_CHARS {
            self.cut = true;
            return;
        }
        if !self.text.is_empty() {
            self.text.push(PATH_SEPARATOR);
            self.chars = self.chars.saturating_add(1);
        }
        let room = PATH_MAX_CHARS.saturating_sub(self.chars);
        let mut end = comp.len();
        let mut count = 0usize;
        for (at, _) in comp.char_indices() {
            if count >= room {
                end = at;
                break;
            }
            count = count.saturating_add(1);
        }
        let taken = comp.get(..end).unwrap_or(comp);
        self.cut |= taken.len() < comp.len();
        self.text.push_str(taken);
        self.chars = self.chars.saturating_add(count);
    }

    /// The finished path, without a trailing separator left by the cap;
    /// `None` when no component survived.
    fn finish(mut self) -> Option<String> {
        let trimmed_len = self.text.trim_end_matches(PATH_SEPARATOR).len();
        self.text.truncate(trimmed_len);
        (!self.text.is_empty()).then_some(self.text)
    }
}

/// Decodes one path list, or `None` when the key is missing or any element
/// is not a byte string. Decodes inline so the intermediate `Vec<&[u8]>`
/// (`raw_bytes_list`) is gone: one allocation per list, not two.
///
/// With `strict`, entries must be valid UTF-8 (BEP 3 requires `.utf-8`
/// fields to be valid UTF-8); any invalid entry makes the whole list
/// `None` so callers fall back to the legacy list.
fn decoded_list<'a>(
    d: &'a Dict<'a>,
    key: &[u8],
    enc: Option<&'static Encoding>,
    strict: bool,
) -> Option<Vec<Cow<'a, str>>> {
    let list = d.get_list(key)?;
    let mut out = Vec::with_capacity(list.len());
    for v in list {
        let raw = v.as_bytes()?;
        if strict {
            out.push(Cow::Borrowed(std::str::from_utf8(raw).ok()?));
        } else {
            out.push(decode_text(raw, enc));
        }
    }
    Some(out)
}

/// Decoded components plus sanitised components for one v1 file, preferring
/// `path.utf-8` but falling back to `path` when the preferred value
/// sanitises to nothing. When both lists exist with equal length, falls back
/// per component so one blank `utf-8` entry does not hide a valid `path`
/// entry.
///
/// The preferred list must be valid UTF-8 per BEP 3: an entry that is not
/// valid UTF-8 never wins over the legacy entry, even when it would decode
/// through the legacy code page or lossy UTF-8.
///
/// Each list is decoded at most once and moved (never re-decoded): the old
/// code re-decoded the legacy list (`leg_dec2`) and decoded the preferred
/// list again in the counted-but-unlisted fallback (up to 4 list-decodes).
///
/// Returns the decoded components, the sanitised survivors, and whether the
/// filename (last component) sanitised to something. A surviving directory
/// prefix with an invalid filename must not be listed as a file.
fn sanitised_path_components<'a>(
    fd: &'a Dict<'a>,
    enc: Option<&'static Encoding>,
) -> Result<(Vec<Cow<'a, str>>, Vec<String>, bool), ParseError> {
    let pref_dec = decoded_list(fd, b"path.utf-8", enc, true);
    let leg_dec = decoded_list(fd, b"path", enc, false);
    if pref_dec.is_none() && leg_dec.is_none() {
        return Err(ParseError::InvalidField("path"));
    }
    // Equal-length per-component fallback preserves the most valid entries.
    if let (Some(p), Some(l)) = (&pref_dec, &leg_dec) {
        if p.len() == l.len() {
            let mut comps = Vec::with_capacity(p.len());
            let mut san = Vec::new();
            for (a, b) in p.iter().zip(l.iter()) {
                let s = sanitize_path_component(a).or_else(|| sanitize_path_component(b));
                if let Some(s) = s {
                    san.push(s);
                }
                // For padding checks keep the preferred raw when present.
                comps.push(a.clone());
            }
            if !san.is_empty() {
                let last_valid = p.last().zip(l.last()).is_some_and(|(a, b)| {
                    sanitize_path_component(a)
                        .or_else(|| sanitize_path_component(b))
                        .is_some()
                });
                return Ok((comps, san, last_valid));
            }
            // Preferred sanitised to nothing: fall through to legacy below.
        }
    }
    // Whole-list preference with post-sanitisation fallback.
    if let Some(comps) = pref_dec {
        let san: Vec<String> = comps
            .iter()
            .filter_map(|c| sanitize_path_component(c))
            .collect();
        if !san.is_empty() {
            let last_valid = comps
                .last()
                .is_some_and(|c| sanitize_path_component(c).is_some());
            return Ok((comps, san, last_valid));
        }
        // Preferred sanitised to nothing: fall back to the already-decoded
        // legacy list.
        if let Some(comps) = leg_dec {
            let san: Vec<String> = comps
                .iter()
                .filter_map(|c| sanitize_path_component(c))
                .collect();
            if !san.is_empty() {
                let last_valid = comps
                    .last()
                    .is_some_and(|c| sanitize_path_component(c).is_some());
                return Ok((comps, san, last_valid));
            }
        }
        // Both sanitise to nothing: the preferred decoded list is counted
        // but unlisted, matching existing unlistable handling.
        return Ok((comps, Vec::new(), false));
    }
    // Only the legacy list exists (preferred was missing).
    let comps = leg_dec.unwrap_or_default();
    let san: Vec<String> = comps
        .iter()
        .filter_map(|c| sanitize_path_component(c))
        .collect();
    if !san.is_empty() {
        let last_valid = comps
            .last()
            .is_some_and(|c| sanitize_path_component(c).is_some());
        return Ok((comps, san, last_valid));
    }
    // Legacy sanitised to nothing: counted but unlisted.
    Ok((comps, Vec::new(), false))
}

/// Collects the files of a v1 torrent: `files` (multi-file) or `length` (single file).
fn v1_files(
    dict: &Dict<'_>,
    name: &str,
    enc: Option<&'static Encoding>,
    files: &mut Collector<'_>,
) -> Result<(), ParseError> {
    if let Some(list) = dict.get(b"files") {
        let list = list.as_list().ok_or(ParseError::InvalidField("files"))?;
        // BEP 3 multi-file torrents name at least one file: an empty list
        // is not a torrent.
        if list.is_empty() {
            return Err(ParseError::InvalidField("files"));
        }
        for item in list {
            let fd = item.as_dict().ok_or(ParseError::InvalidField("files"))?;
            let length = file_length(fd)?;
            let (components, sanitised, filename_valid) = sanitised_path_components(fd, enc)?;
            let attr = fd.get_bytes(b"attr");
            // The `.pad`-directory rule needs a directory part: a lone file
            // named `.pad` is real content, not BEP 47 padding. An invalid
            // filename contributes no `last` to the check.
            let raw_dir = components.len() >= 2;
            let san_dir = sanitised.len() >= 2;
            let raw_last = filename_valid
                .then(|| components.last().map(AsRef::as_ref))
                .flatten();
            let san_last = filename_valid
                .then(|| sanitised.last().map(String::as_str))
                .flatten();
            let padding = is_padding(
                components.first().filter(|_| raw_dir).map(AsRef::as_ref),
                raw_last,
                attr,
            ) || is_padding(
                sanitised.first().filter(|_| san_dir).map(String::as_str),
                san_last,
                attr,
            );
            // Past the budget the joined path is not needed: parsing fails.
            // An invalid filename yields no path: the surviving directory
            // prefix must not be listed as a file.
            let mut path = None;
            let mut whole = false;
            if filename_valid && !files.over_budget {
                let mut capped = CappedPath::default();
                for c in &sanitised {
                    capped.push(c);
                }
                whole = !capped.cut;
                path = capped.finish();
                // A cut path is shown component-wise below: showing the
                // truncated join too would burn the text budget twice for
                // the same entry.
                if whole {
                    if let Some(p) = &path {
                        whole &= files.show_joined(p);
                    }
                }
            }
            if !whole {
                for c in &sanitised {
                    files.show(c);
                }
            }
            files.add(padding, length, path)?;
        }
        Ok(())
    } else {
        let length = file_length(dict)?;
        let comp = sanitize_path_component(name);
        let attr = dict.get_bytes(b"attr");
        // A single file has no directory part, so the `.pad`-directory rule
        // cannot apply; only `attr` or the legacy padding name mark it.
        let padding = is_padding(None, comp.as_deref(), attr) || is_padding(None, Some(name), attr);
        if let Some(p) = &comp {
            if p != name && !files.show_joined(p) {
                files.show(p);
            }
        }
        files.add(padding, length, comp)
    }
}

/// The directories currently open during a `file tree` walk.
#[derive(Default)]
struct OpenDirs {
    path: CappedPath,
    /// `path` before each open directory, and that directory's sanitised
    /// name until it has been shown on its own.
    marks: Vec<DirMark>,
    /// Whether the outermost open directory is [`PAD_DIR`].
    top_is_pad: bool,
}

struct DirMark {
    len: usize,
    chars: usize,
    cut: bool,
    unshown: Option<String>,
}

impl OpenDirs {
    fn open(&mut self, raw: &str) {
        let comp = sanitize_path_component(raw);
        if self.marks.is_empty() {
            self.top_is_pad = raw == PAD_DIR || comp.as_deref() == Some(PAD_DIR);
        }
        self.marks.push(DirMark {
            len: self.path.text.len(),
            chars: self.path.chars,
            cut: self.path.cut,
            unshown: comp.clone(),
        });
        if let Some(comp) = comp {
            self.path.push(&comp);
        }
    }

    fn close(&mut self) {
        if let Some(mark) = self.marks.pop() {
            // Cannot panic: `len` was the text length when this directory
            // was opened, and the text has only been appended to since, so
            // it is a char boundary.
            self.path.text.truncate(mark.len);
            self.path.chars = mark.chars;
            self.path.cut = mark.cut;
        }
    }

    /// Shows each open directory's name on its own, once per directory.
    fn show_names(&mut self, files: &mut Collector<'_>) {
        for mark in &mut self.marks {
            if let Some(comp) = mark.unshown.take() {
                files.show(&comp);
            }
        }
    }

    /// Whether a file named `raw` in the current directory is padding.
    /// Checks both the raw and sanitised file name, so whitespace or
    /// invisible characters cannot hide a padding marker. A file at the
    /// root has no directory part, so the `.pad`-directory rule cannot
    /// apply to it; only `attr` or the legacy padding name can.
    fn is_padding(&self, raw: &str, sanitised: Option<&str>, attr: Option<&[u8]>) -> bool {
        let first_raw = if self.marks.is_empty() {
            ""
        } else if self.top_is_pad {
            PAD_DIR
        } else {
            ""
        };
        if is_padding(Some(first_raw), Some(raw), attr) {
            return true;
        }
        let first_san = if self.marks.is_empty() {
            ""
        } else if self.top_is_pad {
            PAD_DIR
        } else {
            ""
        };
        is_padding(Some(first_san), sanitised, attr)
    }

    /// Shows the file `comp` in the current directory and returns its path.
    ///
    /// The joined path is built and shown while the budget lasts; otherwise,
    /// or when the cap cut it, the names are shown on their own. An invalid
    /// filename (`None`) yields no path: the surviving directory prefix is
    /// shown for the text budget but never listed as a file.
    fn show_file(&mut self, comp: Option<&str>, files: &mut Collector<'_>) -> Option<String> {
        let Some(comp) = comp else {
            self.show_names(files);
            return None;
        };
        let mut path = None;
        let mut whole = false;
        if !files.over_budget {
            let mut capped = self.path.clone();
            capped.push(comp);
            whole = !capped.cut;
            path = capped.finish();
            // As above: a cut path is shown component-wise below.
            if whole {
                if let Some(p) = &path {
                    whole &= files.show_joined(p);
                }
            }
        }
        if !whole {
            self.show_names(files);
            files.show(comp);
        }
        path
    }
}

/// Walks a v2 `file tree` iteratively, with at most [`MAX_FILE_TREE_DEPTH`]
/// path components per file.
///
/// Each key is a path component and each value a dictionary. A dictionary
/// whose only key is `""` is a file; that key holds `{length, pieces root?, attr?}`.
/// Each directory name is decoded and sanitised once, not once per file.
fn walk_file_tree(
    root: &Dict<'_>,
    enc: Option<&'static Encoding>,
    files: &mut Collector<'_>,
) -> Result<(), ParseError> {
    if root.contains_key(b"") {
        return Err(ParseError::InvalidFileTree("the root is a file"));
    }
    if root.len() == 0 {
        return Err(ParseError::InvalidFileTree("empty file tree"));
    }
    let recorded = files.file_count;
    let mut stack = vec![root.iter()];
    // Directories on the stack (the root has none).
    let mut dirs = OpenDirs::default();
    while let Some(top) = stack.last_mut() {
        let Some((key, value)) = top.next() else {
            stack.pop();
            dirs.close();
            continue;
        };
        let child = value
            .as_dict()
            .ok_or(ParseError::InvalidFileTree("node is not a dictionary"))?;
        let name = decode_text(key, enc);
        if let Some(file) = child.get(b"") {
            if child.len() != 1 {
                return Err(ParseError::InvalidFileTree("file entry has sibling keys"));
            }
            let fd = file.as_dict().ok_or(ParseError::InvalidFileTree(
                "file entry is not a dictionary",
            ))?;
            let length = file_length(fd)?;
            let comp = sanitize_path_component(&name);
            let padding = dirs.is_padding(&name, comp.as_deref(), fd.get_bytes(b"attr"));
            let path = dirs.show_file(comp.as_deref(), files);
            files.add(padding, length, path)?;
        } else {
            // Files inside `child` would have `stack.len() + 1` components.
            if stack.len() >= MAX_FILE_TREE_DEPTH {
                return Err(ParseError::FileTreeTooDeep);
            }
            dirs.open(&name);
            stack.push(child.iter());
        }
    }
    // Directories alone are not a torrent (mirrors the empty `files`
    // rejection for v1): padding-only trees count no files either.
    if files.file_count == recorded {
        return Err(ParseError::InvalidFileTree("no files in file tree"));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn walk_file_tree_for_test(
    root: &Dict<'_>,
) -> Result<(u64, Vec<FileEntry>), ParseError> {
    let mut visit = |_: &str| {};
    let mut files = Collector::new(&mut visit, 0);
    walk_file_tree(root, None, &mut files)?;
    Ok((files.file_count, files.kept.into_sorted_vec()))
}
