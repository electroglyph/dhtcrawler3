//! Info-dictionary parsing (v1, v2 and hybrid).

use std::borrow::Cow;
use std::collections::BinaryHeap;

use dc3_bencode::{Dict, Limits, Value};
use dc3_core::text::{sanitize_display, sanitize_path_component};
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
    let root = dc3_bencode::decode(info, &Limits::METADATA)?;
    parse_decoded(info, &root, visit)
}

/// Parses an already decoded info dictionary; `info` is its raw bytes.
pub(crate) fn parse_decoded(
    info: &[u8],
    root: &Value<'_>,
    visit: &mut dyn FnMut(&str),
) -> Result<TorrentMeta, ParseError> {
    let dict = root.as_dict().ok_or(ParseError::NotADict)?;

    let is_v1 = matches!(dict.get(b"pieces"), Some(Value::Bytes(b)) if !b.is_empty() && b.len() % 20 == 0);
    let tree = if dict.get_int(b"meta version") == Some(META_VERSION_2) {
        dict.get_dict(b"file tree")
    } else {
        None
    };
    if !is_v1 && tree.is_none() {
        return Err(ParseError::NotATorrent);
    }

    let enc = label_encoding(dict.get_bytes(b"encoding"));
    let name = text_field(dict, b"name.utf-8", b"name")
        .map(|b| sanitize_display(&decode_text(b, enc), NAME_MAX_CHARS))
        .filter(|s| !s.is_empty())
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

/// The byte string under `preferred`, else under `fallback`.
fn text_field<'a>(dict: &Dict<'a>, preferred: &[u8], fallback: &[u8]) -> Option<&'a [u8]> {
    dict.get_bytes(preferred)
        .or_else(|| dict.get_bytes(fallback))
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

    /// From now on, checks and shows entries without recording them.
    fn shadow(&mut self) {
        self.record = false;
        self.parsed = 0;
    }

    /// Shows `text` to the visitor.
    fn show(&mut self, text: &str) {
        self.shown = self.shown.saturating_add(text.chars().count());
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
        self.show(path);
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
    /// Appends a sanitised component.
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
        let taken = comp
            .char_indices()
            .nth(room)
            .and_then(|(at, _)| comp.get(..at))
            .unwrap_or(comp);
        self.cut |= taken.len() < comp.len();
        self.text.push_str(taken);
        self.chars = self.chars.saturating_add(taken.chars().count());
    }

    /// The finished path, without a trailing separator left by the cap;
    /// `None` when no component survived.
    fn finish(mut self) -> Option<String> {
        let trimmed_len = self.text.trim_end_matches(PATH_SEPARATOR).len();
        self.text.truncate(trimmed_len);
        (!self.text.is_empty()).then_some(self.text)
    }
}

/// The path list of a v1 file dictionary: `path.utf-8` if it is a list of
/// byte strings, else `path`, which must be one.
fn path_list<'a>(d: &Dict<'a>) -> Result<Vec<&'a [u8]>, ParseError> {
    let as_bytes_list = |key: &[u8]| -> Option<Vec<&'a [u8]>> {
        d.get_list(key)?.iter().map(Value::as_bytes).collect()
    };
    as_bytes_list(b"path.utf-8")
        .or_else(|| as_bytes_list(b"path"))
        .ok_or(ParseError::InvalidField("path"))
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
        for item in list {
            let fd = item.as_dict().ok_or(ParseError::InvalidField("files"))?;
            let length = file_length(fd)?;
            let components: Vec<Cow<'_, str>> = path_list(fd)?
                .into_iter()
                .map(|b| decode_text(b, enc))
                .collect();
            let sanitised: Vec<String> = components
                .iter()
                .filter_map(|c| sanitize_path_component(c))
                .collect();
            let padding = is_padding(
                components.first().map(AsRef::as_ref),
                components.last().map(AsRef::as_ref),
                fd.get_bytes(b"attr"),
            ) || is_padding(
                sanitised.first().map(String::as_str),
                sanitised.last().map(String::as_str),
                fd.get_bytes(b"attr"),
            );
            // Past the budget the joined path is not needed: parsing fails.
            let mut path = None;
            let mut whole = false;
            if !files.over_budget {
                let mut capped = CappedPath::default();
                for c in &sanitised {
                    capped.push(c);
                }
                whole = !capped.cut;
                path = capped.finish();
                if let Some(p) = &path {
                    whole &= files.show_joined(p);
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
        let padding = is_padding(
            comp.as_deref(),
            comp.as_deref(),
            dict.get_bytes(b"attr"),
        ) || is_padding(Some(name), Some(name), dict.get_bytes(b"attr"));
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
    /// invisible characters cannot hide a padding marker.
    fn is_padding(
        &self,
        raw: &str,
        sanitised: Option<&str>,
        attr: Option<&[u8]>,
    ) -> bool {
        let first_raw = if self.marks.is_empty() {
            raw
        } else if self.top_is_pad {
            PAD_DIR
        } else {
            ""
        };
        if is_padding(Some(first_raw), Some(raw), attr) {
            return true;
        }
        let first_san = if self.marks.is_empty() {
            sanitised.unwrap_or("")
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
    /// or when the cap cut it, the names are shown on their own.
    fn show_file(&mut self, comp: Option<&str>, files: &mut Collector<'_>) -> Option<String> {
        let mut path = None;
        let mut whole = false;
        if !files.over_budget {
            let mut capped = self.path.clone();
            if let Some(comp) = comp {
                capped.push(comp);
            }
            whole = !capped.cut;
            path = capped.finish();
            if let Some(p) = &path {
                whole &= files.show_joined(p);
            }
        }
        if !whole {
            self.show_names(files);
            if let Some(comp) = comp {
                files.show(comp);
            }
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
