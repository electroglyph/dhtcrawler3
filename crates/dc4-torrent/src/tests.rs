use std::collections::BTreeMap;

use dc4_bencode::{Limits, OwnedValue as O, encode};
use dc4_core::DhtKey;
use proptest::prelude::*;
use sha1::{Digest, Sha1};
use sha2::Sha256;

use super::*;

type Map = BTreeMap<Vec<u8>, O>;

fn d<const N: usize>(entries: [(&str, O); N]) -> O {
    O::Dict(
        entries
            .into_iter()
            .map(|(k, v)| (k.as_bytes().to_vec(), v))
            .collect(),
    )
}

fn b(s: &str) -> O {
    O::bytes(s)
}

fn i(n: i64) -> O {
    O::Int(n)
}

fn list(items: &[&str]) -> O {
    O::List(items.iter().map(|s| b(s)).collect())
}

fn v1_file(len: i64, path: &[&str]) -> O {
    d([("length", i(len)), ("path", list(path))])
}

fn with(base: O, key: &str, v: O) -> O {
    let O::Dict(mut m) = base else {
        panic!("not a dict")
    };
    m.insert(key.as_bytes().to_vec(), v);
    O::Dict(m)
}

fn without(base: O, key: &str) -> O {
    let O::Dict(mut m) = base else {
        panic!("not a dict")
    };
    m.remove(key.as_bytes());
    O::Dict(m)
}

fn v1_single() -> O {
    d([
        ("name", b("ubuntu.iso")),
        ("length", i(1234)),
        ("piece length", i(16384)),
        ("pieces", O::bytes(vec![0u8; 20])),
    ])
}

fn v1_multi(files: Vec<O>) -> O {
    d([
        ("name", b("dir")),
        ("files", O::List(files)),
        ("piece length", i(16384)),
        ("pieces", O::bytes(vec![0u8; 20])),
    ])
}

fn v2_file(len: i64) -> O {
    d([(
        "",
        d([("length", i(len)), ("pieces root", O::bytes(vec![1u8; 32]))]),
    )])
}

fn v2_only(tree: O) -> O {
    d([
        ("name", b("v2 torrent")),
        ("meta version", i(2)),
        ("piece length", i(65536)),
        ("file tree", tree),
    ])
}

fn parse(v: &O) -> Result<TorrentMeta, ParseError> {
    parse_info(&encode(v))
}

fn paths(m: &TorrentMeta) -> Vec<&str> {
    m.files.iter().map(|f| f.path.as_str()).collect()
}

#[test]
fn v1_single_file() {
    let v = v1_single();
    let raw = encode(&v);
    let m = parse_info(&raw).unwrap();
    assert_eq!(m.name, "ubuntu.iso");
    assert_eq!(m.info_hash_v1, Some(DhtKey(Sha1::digest(&raw).into())));
    assert_eq!(m.info_hash_v2, None);
    assert_eq!(m.total_size, 1234);
    assert_eq!(m.file_count, 1);
    assert_eq!(
        m.files,
        vec![FileEntry {
            path: "ubuntu.iso".into(),
            size: 1234
        }]
    );
    assert!(!m.files_truncated);
    assert_eq!(m.piece_length, Some(16384));
    assert!(!m.private);
}

#[test]
fn v1_empty_pieces_is_zero_length_torrent() {
    let v = d([
        ("name", b("x")),
        ("length", i(0)),
        ("piece length", i(16384)),
        ("pieces", O::bytes(vec![])),
    ]);
    let m = parse(&v).unwrap();
    assert_eq!(m.total_size, 0);
    assert_eq!(m.file_count, 1);
    assert!(m.info_hash_v1.is_some());
}

#[test]
fn v1_short_pieces_is_not_a_torrent() {
    // Non-empty pieces must be a whole number of SHA-1 hashes.
    for len in [1, 5, 19, 21] {
        let v = d([
            ("name", b("x")),
            ("length", i(100)),
            ("piece length", i(16384)),
            ("pieces", O::bytes(vec![0u8; len])),
        ]);
        assert_eq!(parse(&v), Err(ParseError::NotATorrent), "len {len}");
    }
    // Two whole hashes still parse.
    let v = d([
        ("name", b("x")),
        ("length", i(100)),
        ("piece length", i(16384)),
        ("pieces", O::bytes(vec![0u8; 40])),
    ]);
    assert!(parse(&v).is_ok());
}

#[test]
fn v1_multi_file_prefers_utf8_variants() {
    let files = vec![
        v1_file(10, &["b", "file.txt"]),
        with(
            v1_file(20, &["garbage"]),
            "path.utf-8",
            list(&["a", "ünïcode.txt"]),
        ),
        // A wrong-typed path.utf-8 falls back to path.
        with(v1_file(5, &["c.txt"]), "path.utf-8", i(3)),
    ];
    let v = with(v1_multi(files), "name.utf-8", b("Nämé"));
    let m = parse(&v).unwrap();
    assert_eq!(m.name, "Nämé");
    assert_eq!(paths(&m), vec!["a/ünïcode.txt", "b/file.txt", "c.txt"]);
    assert_eq!(m.total_size, 35);
    assert_eq!(m.file_count, 3);
}

#[test]
fn invalid_utf8_preferred_falls_back_to_legacy() {
    // F-09: BEP 3 requires `.utf-8` fields to be valid UTF-8. Bytes that are
    // invalid UTF-8 but decode via GB18030 or lossy UTF-8 must not win over
    // a valid legacy entry.
    let v = with(
        v1_file(1, &["fallback"]),
        "path.utf-8",
        O::List(vec![O::bytes(vec![0x81, 0x30])]),
    );
    let m = parse(&v1_multi(vec![v])).unwrap();
    assert_eq!(paths(&m), vec!["fallback"]);
    // Same for the torrent name.
    let v = with(
        with(v1_single(), "name", b("fallback")),
        "name.utf-8",
        O::bytes(vec![0x81, 0x30]),
    );
    assert_eq!(parse(&v).unwrap().name, "fallback");
}

#[test]
fn name_utf8_wrong_type_falls_back_and_missing_name_is_unnamed() {
    let v = with(v1_single(), "name.utf-8", i(1));
    assert_eq!(parse(&v).unwrap().name, "ubuntu.iso");
    let v = without(v1_single(), "name");
    let m = parse(&v).unwrap();
    assert_eq!(m.name, UNNAMED);
    assert_eq!(paths(&m), vec![UNNAMED]);
    let v = with(v1_single(), "name", b(" \u{202E}\u{0007} "));
    assert_eq!(parse(&v).unwrap().name, UNNAMED);
}

#[test]
fn gbk_name_with_encoding_label() {
    let text = "中文电影";
    let (bytes, _, errors) = encoding_rs::GBK.encode(text);
    assert!(!errors);
    assert!(std::str::from_utf8(&bytes).is_err());
    let v = with(
        with(v1_single(), "name", O::bytes(bytes.to_vec())),
        "encoding",
        b("GBK"),
    );
    assert_eq!(parse(&v).unwrap().name, text);
    // Without a label, GB18030 (a superset of GBK) is tried next.
    let v = with(v1_single(), "name", O::bytes(bytes.to_vec()));
    assert_eq!(parse(&v).unwrap().name, text);
}

#[test]
fn shift_jis_without_label_does_not_panic() {
    let (bytes, _, _) = encoding_rs::SHIFT_JIS.encode("日本語のファイル");
    let v = with(v1_single(), "name", O::bytes(bytes.to_vec()));
    let m = parse(&v).unwrap();
    assert!(!m.name.is_empty());
    // Bytes that no encoding accepts end up lossy but present.
    let v = with(v1_single(), "name", O::bytes(vec![b'a', 0xff, 0xff, 0xff]));
    let m = parse(&v).unwrap();
    assert!(m.name.starts_with('a'));
}

#[test]
fn shift_jis_with_label() {
    let text = "日本語のファイル";
    let (bytes, _, _) = encoding_rs::SHIFT_JIS.encode(text);
    let v = with(
        with(v1_single(), "name", O::bytes(bytes.to_vec())),
        "encoding",
        b("shift_jis"),
    );
    assert_eq!(parse(&v).unwrap().name, text);
}

#[test]
fn padding_files_are_excluded() {
    let files = vec![
        v1_file(100, &["movie.mkv"]),
        with(v1_file(7, &["x"]), "attr", b("p")),
        v1_file(8, &[".pad", "8"]),
        v1_file(9, &["_____padding_file_0_if you see this"]),
        v1_file(10, &["sub", "_____padding_file_1"]),
        // Not padding: ".pad" only matters as the first component.
        v1_file(1, &["sub", ".pad"]),
        // "x" in attr is executable, not padding.
        with(v1_file(2, &["run.sh"]), "attr", b("x")),
    ];
    let m = parse(&v1_multi(files)).unwrap();
    assert_eq!(paths(&m), vec!["movie.mkv", "run.sh", "sub/.pad"]);
    assert_eq!(m.total_size, 103);
    assert_eq!(m.file_count, 3);

    let tree = d([
        ("a.bin", v2_file(50)),
        (
            ".pad",
            d([(
                "16",
                with(
                    v2_file(16),
                    "",
                    with(d([("length", i(16))]), "attr", b("p")),
                ),
            )]),
        ),
        (
            "b",
            d([(
                "c",
                with(v2_file(4), "", d([("length", i(4)), ("attr", b("p"))])),
            )]),
        ),
    ]);
    let m = parse(&v2_only(tree)).unwrap();
    assert_eq!(paths(&m), vec!["a.bin"]);
    assert_eq!((m.total_size, m.file_count), (50, 1));
}

#[test]
fn malicious_paths_are_sanitised() {
    let files = vec![
        v1_file(1, &["..", "..", "etc", "passwd"]),
        v1_file(2, &[".", "", "a/b", "c\\d"]),
        v1_file(4, &["evil\u{202E}gpj.exe"]),
        v1_file(8, &["..", "."]),
        v1_file(16, &[]),
    ];
    let m = parse(&v1_multi(files)).unwrap();
    assert_eq!(paths(&m), vec!["a_b/c_d", "etc/passwd", "evilgpj.exe"]);
    // Unlistable entries still count: they are real content.
    assert_eq!(m.total_size, 31);
    assert_eq!(m.file_count, 5);
    for f in &m.files {
        assert!(!f.path.starts_with('/'));
        assert!(
            f.path
                .split('/')
                .all(|c| !c.is_empty() && c != "." && c != "..")
        );
    }
}

#[test]
fn long_paths_and_names_are_capped() {
    let comp = "x".repeat(255);
    let comps: Vec<&str> = std::iter::repeat_n(comp.as_str(), 40).collect();
    let v = with(
        v1_multi(vec![v1_file(1, &comps)]),
        "name",
        b(&"n".repeat(5000)),
    );
    let m = parse(&v).unwrap();
    assert_eq!(m.name.chars().count(), NAME_MAX_CHARS);
    let p = &m.files[0].path;
    assert!(p.chars().count() <= PATH_MAX_CHARS);
    assert!(!p.ends_with('/'));
}

#[test]
fn cut_paths_show_components_but_not_the_truncated_join() {
    // 40 × 255-char components: the capped join is cut, so the visitor
    // sees each component once — and never the truncated join (showing
    // both would burn the text budget twice for one entry).
    let comp = "y".repeat(255);
    let comps: Vec<&str> = std::iter::repeat_n(comp.as_str(), 40).collect();
    let (m, seen) = parse_visited(&v1_multi(vec![v1_file(1, &comps)]));
    let m = m.unwrap();
    assert!(m.files[0].path.chars().count() <= PATH_MAX_CHARS);
    let joined = comps.join("/");
    let truncated: String = joined.chars().take(PATH_MAX_CHARS).collect();
    assert!(!seen.iter().any(|s| *s == truncated));
    assert_eq!(seen.iter().filter(|s| *s == &comp).count(), 40);
}

#[test]
fn empty_files_list_is_not_a_torrent() {
    assert_eq!(
        parse(&v1_multi(vec![])),
        Err(ParseError::InvalidField("files"))
    );
}

#[test]
fn file_tree_of_only_empty_dirs_is_not_a_torrent() {
    assert_eq!(
        parse(&v2_only(d([("empty", d([]))]))),
        Err(ParseError::InvalidFileTree("no files in file tree"))
    );
    // Nested empty directories: still no file.
    assert_eq!(
        parse(&v2_only(d([("a", d([("b", d([("c", d([]))]))]))]))),
        Err(ParseError::InvalidFileTree("no files in file tree"))
    );
}

#[test]
fn v2_only_torrent() {
    let tree = d([
        ("dir", d([("b.txt", v2_file(3)), ("a.txt", v2_file(2))])),
        ("top.bin", v2_file(10)),
        ("empty", d([])),
    ]);
    let v = v2_only(tree);
    let raw = encode(&v);
    let m = parse_info(&raw).unwrap();
    let expect: [u8; 32] = Sha256::digest(&raw).into();
    assert_eq!(m.info_hash_v2, Some(InfoHashV2(expect)));
    assert_eq!(m.info_hash_v1, None);
    assert_eq!(m.name, "v2 torrent");
    assert_eq!(paths(&m), vec!["dir/a.txt", "dir/b.txt", "top.bin"]);
    assert_eq!((m.total_size, m.file_count), (15, 3));
    assert_eq!(m.piece_length, Some(65536));
}

#[test]
fn hybrid_takes_files_from_tree_and_records_both_hashes() {
    let tree = d([("real.bin", v2_file(42))]);
    let v = with(
        with(
            v2_only(tree),
            "files",
            O::List(vec![v1_file(1, &["v1-only.bin"])]),
        ),
        "pieces",
        O::bytes(vec![0u8; 20]),
    );
    let raw = encode(&v);
    let m = parse_info(&raw).unwrap();
    assert_eq!(m.info_hash_v1, Some(DhtKey(Sha1::digest(&raw).into())));
    assert_eq!(
        m.info_hash_v2,
        Some(InfoHashV2(Sha256::digest(&raw).into()))
    );
    assert_eq!(paths(&m), vec!["real.bin"]);
    assert_eq!(m.total_size, 42);
}

#[test]
fn invalid_filename_does_not_list_the_parent_dir() {
    // A valid directory with an invalid filename counts toward the total
    // but lists no file: the parent directory must not appear as a file.
    let v = v1_multi(vec![v1_file(1, &["dir", ".."])]);
    let m = parse(&v).unwrap();
    assert_eq!(m.file_count, 1);
    assert!(m.files.is_empty());
    assert_eq!(m.total_size, 1);
    let tree = d([("dir", d([("..", v2_file(1))]))]);
    let m = parse(&v2_only(tree)).unwrap();
    assert_eq!(m.file_count, 1);
    assert!(m.files.is_empty());
    assert_eq!(m.total_size, 1);
}

#[test]
fn meta_version_without_tree_or_wrong_version_is_not_v2() {
    let v = without(v2_only(d([("a", v2_file(1))])), "file tree");
    assert_eq!(parse(&v), Err(ParseError::NotATorrent));
    let v = with(v2_only(d([("a", v2_file(1))])), "meta version", i(3));
    assert_eq!(parse(&v), Err(ParseError::NotATorrent));
    let v = with(v2_only(d([("a", v2_file(1))])), "file tree", b("x"));
    assert_eq!(parse(&v), Err(ParseError::NotATorrent));
}

#[test]
fn private_flag() {
    assert!(parse(&with(v1_single(), "private", i(1))).unwrap().private);
    assert!(!parse(&with(v1_single(), "private", i(0))).unwrap().private);
    assert!(
        !parse(&with(v1_single(), "private", b("1")))
            .unwrap()
            .private
    );
}

#[test]
fn missing_and_wrong_typed_fields() {
    assert_eq!(parse_info(b"le"), Err(ParseError::NotADict));
    assert!(matches!(
        parse_info(b"d4:name"),
        Err(ParseError::Bencode(_))
    ));
    assert!(matches!(parse_info(b""), Err(ParseError::Bencode(_))));
    assert_eq!(parse(&d([("name", b("x"))])), Err(ParseError::NotATorrent));
    assert_eq!(
        parse(&with(v1_single(), "pieces", i(5))),
        Err(ParseError::NotATorrent)
    );
    assert_eq!(
        parse(&without(v1_single(), "length")),
        Err(ParseError::InvalidField("length"))
    );
    assert_eq!(
        parse(&with(v1_single(), "length", b("12"))),
        Err(ParseError::InvalidField("length"))
    );
    let multi = |files: O| without(with(v1_single(), "files", files), "length");
    assert_eq!(
        parse(&multi(b("x"))),
        Err(ParseError::InvalidField("files"))
    );
    assert_eq!(
        parse(&multi(O::List(vec![i(1)]))),
        Err(ParseError::InvalidField("files"))
    );
    assert_eq!(
        parse(&multi(O::List(vec![d([("path", list(&["a"]))])]))),
        Err(ParseError::InvalidField("length"))
    );
    assert_eq!(
        parse(&multi(O::List(vec![d([("length", i(1))])]))),
        Err(ParseError::InvalidField("path"))
    );
    assert_eq!(
        parse(&multi(O::List(vec![d([
            ("length", i(1)),
            ("path", O::List(vec![i(1)]))
        ])]))),
        Err(ParseError::InvalidField("path"))
    );
    // Non-positive piece length is ignored.
    assert_eq!(
        parse(&with(v1_single(), "piece length", i(0)))
            .unwrap()
            .piece_length,
        None
    );
    assert_eq!(
        parse(&without(v1_single(), "piece length"))
            .unwrap()
            .piece_length,
        None
    );

    // v2 structural errors.
    assert_eq!(
        parse(&v2_only(d([("a", b("x"))]))),
        Err(ParseError::InvalidFileTree("node is not a dictionary"))
    );
    assert_eq!(
        parse(&v2_only(d([("a", d([("", i(1))]))]))),
        Err(ParseError::InvalidFileTree(
            "file entry is not a dictionary"
        ))
    );
    assert_eq!(
        parse(&v2_only(d([("a", d([("", d([]))]))]))),
        Err(ParseError::InvalidField("length"))
    );
    assert_eq!(
        parse(&v2_only(d([("", d([("length", i(1))]))]))),
        Err(ParseError::InvalidFileTree("the root is a file"))
    );
}

#[test]
fn file_entry_with_siblings_is_rejected() {
    let bad = with(v2_file(1), "other", v2_file(2));
    assert_eq!(
        parse(&v2_only(d([("a", bad)]))),
        Err(ParseError::InvalidFileTree("file entry has sibling keys"))
    );
}

#[test]
fn negative_and_overflowing_lengths() {
    assert_eq!(
        parse(&with(v1_single(), "length", i(-1))),
        Err(ParseError::NegativeLength)
    );
    let files = vec![v1_file(1, &["a"]), v1_file(-5, &["b"])];
    assert_eq!(parse(&v1_multi(files)), Err(ParseError::NegativeLength));
    // Negative padding is still an error.
    let files = vec![v1_file(-5, &[".pad", "0"])];
    assert_eq!(parse(&v1_multi(files)), Err(ParseError::NegativeLength));
    let files = vec![
        v1_file(i64::MAX, &["a"]),
        v1_file(i64::MAX, &["b"]),
        v1_file(i64::MAX, &["c"]),
    ];
    assert_eq!(parse(&v1_multi(files)), Err(ParseError::SizeOverflow));
    // Two i64::MAX fit in u64.
    let files = vec![v1_file(i64::MAX, &["a"]), v1_file(i64::MAX, &["b"])];
    assert_eq!(
        parse(&v1_multi(files)).unwrap().total_size,
        (i64::MAX as u64) * 2
    );
    let tree = d([("a", v2_file(-1))]);
    assert_eq!(parse(&v2_only(tree)), Err(ParseError::NegativeLength));
}

/// Parses with looser bencode limits, to reach limits of this crate that
/// `Limits::METADATA` would otherwise pre-empt.
fn parse_loose(v: &O) -> Result<TorrentMeta, ParseError> {
    let raw = encode(v);
    let loose = Limits {
        max_depth: 1000,
        max_items: 10_000_000,
        ..Limits::METADATA
    };
    let value = dc4_bencode::decode(&raw, &loose).unwrap();
    parse::parse_decoded(&raw, &value, &mut |_| {})
}

#[test]
fn too_many_files() {
    let file = v1_file(1, &["f"]);
    let v = v1_multi(vec![file.clone(); MAX_FILES_PARSED + 1]);
    // Through the public entry point the bencode item limit fires first.
    assert!(matches!(parse(&v), Err(ParseError::Bencode(_))));
    assert_eq!(parse_loose(&v), Err(ParseError::TooManyFiles));
    // Padding entries count toward the parse limit too.
    let pad = v1_file(1, &[".pad", "1"]);
    assert_eq!(
        parse_loose(&v1_multi(vec![pad; MAX_FILES_PARSED + 1])),
        Err(ParseError::TooManyFiles)
    );

    // Exactly at the limit: parsed, listing truncated, totals complete.
    let files: Vec<O> = (0..MAX_FILES_PARSED)
        .map(|n| v1_file(1, &[&format!("{:06}", MAX_FILES_PARSED - n)]))
        .collect();
    let m = parse_loose(&v1_multi(files)).unwrap();
    assert_eq!(m.file_count, MAX_FILES_PARSED as u64);
    assert_eq!(m.total_size, MAX_FILES_PARSED as u64);
    assert_eq!(m.files.len(), MAX_FILES_STORED);
    assert!(m.files_truncated);
    assert_eq!(m.files[0].path, "000001");
    assert!(m.files.windows(2).all(|w| w[0] <= w[1]));
}

#[test]
fn truncation_via_public_entry_point() {
    let files: Vec<O> = (0..MAX_FILES_STORED + 1)
        .map(|n| v1_file(2, &[&n.to_string()]))
        .collect();
    let m = parse(&v1_multi(files)).unwrap();
    assert_eq!(m.files.len(), MAX_FILES_STORED);
    assert!(m.files_truncated);
    assert_eq!(m.file_count, MAX_FILES_STORED as u64 + 1);
    assert_eq!(m.total_size, 2 * (MAX_FILES_STORED as u64 + 1));
}

#[test]
fn listing_sorted_by_path_then_size() {
    let files = vec![v1_file(9, &["b"]), v1_file(5, &["a"]), v1_file(3, &["a"])];
    let m = parse(&v1_multi(files)).unwrap();
    let got: Vec<(&str, u64)> = m.files.iter().map(|f| (f.path.as_str(), f.size)).collect();
    assert_eq!(got, vec![("a", 3), ("a", 5), ("b", 9)]);
    assert!(!m.files_truncated);
}

/// Nests `depth` directories and puts one file at the bottom.
fn deep_tree(depth: usize) -> O {
    let mut node = v2_file(1);
    for _ in 1..depth {
        node = d([("d", node)]);
    }
    d([("d", node)])
}

#[test]
fn file_tree_depth_cap() {
    // The bencode depth limit (64) is reached before our cap through
    // parse_info, so exercise the walker directly with looser limits.
    let loose = Limits {
        max_depth: 1000,
        ..Limits::METADATA
    };
    let m = parse_loose(&v2_only(deep_tree(MAX_FILE_TREE_DEPTH + 1)));
    assert_eq!(m, Err(ParseError::FileTreeTooDeep));
    let raw = encode(&deep_tree(MAX_FILE_TREE_DEPTH));
    let v = dc4_bencode::decode(&raw, &loose).unwrap();
    let (count, listing) = parse::walk_file_tree_for_test(v.as_dict().unwrap()).unwrap();
    assert_eq!(count, 1);
    assert_eq!(listing[0].path.split('/').count(), MAX_FILE_TREE_DEPTH);

    let raw = encode(&deep_tree(MAX_FILE_TREE_DEPTH + 1));
    let v = dc4_bencode::decode(&raw, &loose).unwrap();
    assert_eq!(
        parse::walk_file_tree_for_test(v.as_dict().unwrap()),
        Err(ParseError::FileTreeTooDeep)
    );

    // Through the public entry point a deep tree is still an error.
    assert!(parse(&v2_only(deep_tree(200))).is_err());
}

#[test]
fn verify_v1_v2_and_mismatch() {
    let raw = encode(&v1_single());
    let k1 = DhtKey(Sha1::digest(&raw).into());
    assert_eq!(verify(&k1, &raw), Some(Verified::V1));

    let raw2 = encode(&v2_only(d([("a", v2_file(1))])));
    let full: [u8; 32] = Sha256::digest(&raw2).into();
    let k2 = InfoHashV2(full).truncated();
    assert_eq!(verify(&k2, &raw2), Some(Verified::V2Truncated));

    assert_eq!(verify(&k1, &raw2), None);
    assert_eq!(verify(&DhtKey([0; 20]), &raw), None);
    assert_eq!(verify(&DhtKey([0; 20]), b""), None);
}

#[test]
fn deterministic_output() {
    let tree = d([("z", v2_file(1)), ("y", d([("x", v2_file(2))]))]);
    let raw = encode(&v2_only(tree));
    assert_eq!(parse_info(&raw), parse_info(&raw));
}

#[test]
fn unsorted_keys_are_accepted() {
    // Hand-written info dict with keys out of order.
    let raw = b"d6:lengthi5e6:pieces20:aaaaaaaaaaaaaaaaaaaa4:name1:xe";
    let m = parse_info(raw).unwrap();
    assert_eq!((m.name.as_str(), m.total_size), ("x", 5));
    assert_eq!(verify(&m.info_hash_v1.unwrap(), raw), Some(Verified::V1));
}

fn arb_value() -> impl Strategy<Value = O> {
    let leaf = prop_oneof![
        any::<i64>().prop_map(O::Int),
        prop::collection::vec(any::<u8>(), 0..8).prop_map(O::Bytes),
        prop_oneof![
            Just(""),
            Just("p"),
            Just(".pad"),
            Just(".."),
            Just("_____padding_file")
        ]
        .prop_map(b),
    ];
    leaf.prop_recursive(6, 64, 6, |inner| {
        let key = prop_oneof![
            Just(Vec::new()),
            Just(b"length".to_vec()),
            Just(b"path".to_vec()),
            Just(b"path.utf-8".to_vec()),
            Just(b"attr".to_vec()),
            Just(b"a".to_vec()),
            prop::collection::vec(any::<u8>(), 0..4),
        ];
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(O::List),
            prop::collection::btree_map(key, inner, 0..6).prop_map(O::Dict),
        ]
    })
}

fn arb_info() -> impl Strategy<Value = O> {
    let keys = [
        "name",
        "name.utf-8",
        "length",
        "files",
        "pieces",
        "piece length",
        "meta version",
        "file tree",
        "private",
        "encoding",
        "attr",
    ];
    let entries = keys.map(|k| prop::option::of(arb_value()).prop_map(move |v| (k, v)));
    (entries, any::<bool>(), any::<bool>()).prop_map(|(entries, v1, v2)| {
        let mut m: Map = entries
            .into_iter()
            .filter_map(|(k, v)| v.map(|v| (k.as_bytes().to_vec(), v)))
            .collect();
        if v1 {
            m.insert(b"pieces".to_vec(), O::bytes(vec![0u8; 20]));
        }
        if v2 {
            m.insert(b"meta version".to_vec(), i(2));
        }
        O::Dict(m)
    })
}

proptest! {
    #[test]
    fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        let _ = parse_info(&bytes);
        let _ = verify(&DhtKey([0; 20]), &bytes);
    }

    #[test]
    fn arbitrary_info_dicts_never_panic(v in arb_info()) {
        let raw = encode(&v);
        if let Ok(m) = parse_info(&raw) {
            prop_assert!(m.files.len() <= MAX_FILES_STORED);
            prop_assert!(m.files.len() as u64 <= m.file_count);
            prop_assert!(!m.name.is_empty());
            prop_assert!(m.info_hash_v1.is_some() || m.info_hash_v2.is_some());
            let listed: u64 = m.files.iter().map(|f| f.size).sum();
            prop_assert!(listed <= m.total_size);
            for f in &m.files {
                prop_assert!(!f.path.is_empty());
                prop_assert!(f.path.split('/').all(|c| !c.is_empty() && c != "." && c != ".."));
            }
            prop_assert_eq!(parse_info(&raw), Ok(m));
        }
    }
}

/// Parses `v` and returns the result plus every string passed to the visitor.
fn parse_visited(v: &O) -> (Result<TorrentMeta, ParseError>, Vec<String>) {
    let mut seen = Vec::new();
    let r = parse_info_visit(&encode(v), &mut |s| seen.push(s.to_owned()));
    (r, seen)
}

#[test]
fn visit_v1_single_file_sees_name_once() {
    let (m, seen) = parse_visited(&v1_single());
    assert_eq!(seen, vec!["ubuntu.iso"]);
    assert_eq!(paths(&m.unwrap()), vec!["ubuntu.iso"]);
    // Even a padding-looking single file shows its name exactly once.
    let (_, seen) = parse_visited(&with(v1_single(), "attr", b("p")));
    assert_eq!(seen, vec!["ubuntu.iso"]);
}

#[test]
fn visit_v1_multi_file_in_parse_order() {
    let files = vec![
        v1_file(1, &["z", "last.txt"]),
        v1_file(2, &["..", "."]),
        v1_file(3, &["a\u{202E}.txt"]),
        with(v1_file(4, &["x"]), "path.utf-8", list(&["utf8.txt"])),
    ];
    let (m, seen) = parse_visited(&v1_multi(files));
    assert_eq!(seen, vec!["dir", "z/last.txt", "a.txt", "utf8.txt"]);
    let m = m.unwrap();
    assert_eq!(paths(&m), vec!["a.txt", "utf8.txt", "z/last.txt"]);
    assert_eq!(m.file_count, 4);
}

#[test]
fn visit_v2_tree_paths() {
    let tree = d([
        ("dir", d([("b.txt", v2_file(3)), ("a.txt", v2_file(2))])),
        ("top.bin", v2_file(10)),
        ("empty", d([])),
        ("..", v2_file(1)),
    ]);
    let (m, seen) = parse_visited(&v2_only(tree));
    // Encoded dictionaries are key-sorted, so this is the byte order.
    assert_eq!(
        seen,
        vec!["v2 torrent", "dir/a.txt", "dir/b.txt", "top.bin"]
    );
    assert_eq!(m.unwrap().file_count, 4);
}

#[test]
fn visit_sees_paths_beyond_the_stored_cap() {
    const N: usize = 3_000;
    let files: Vec<O> = (0..N)
        .map(|n| v1_file(1, &["d", &format!("{:05}", N - n)]))
        .collect();
    let (m, seen) = parse_visited(&v1_multi(files));
    let m = m.unwrap();
    assert_eq!(seen.len(), N + 1);
    assert_eq!(seen[0], "dir");
    assert_eq!(seen[1], "d/03000");
    assert_eq!(seen[N], "d/00001");
    assert_eq!(m.files.len(), MAX_FILES_STORED);
    assert!(m.files_truncated);
    assert_eq!(m.file_count, N as u64);
    // The kept entries are the smallest, in order.
    assert_eq!(m.files[0].path, "d/00001");
    assert_eq!(
        m.files[MAX_FILES_STORED - 1].path,
        format!("d/{MAX_FILES_STORED:05}")
    );
    assert!(m.files.windows(2).all(|w| w[0] <= w[1]));
}

#[test]
fn visit_sees_unnamed_and_stops_at_error() {
    let files = vec![v1_file(1, &["a"]), v1_file(-1, &["b"]), v1_file(1, &["c"])];
    let (m, seen) = parse_visited(&without(v1_multi(files), "name"));
    assert_eq!(m, Err(ParseError::NegativeLength));
    // The failing entry's path is shown before its length is checked.
    assert_eq!(seen, vec![UNNAMED, "a", "b"]);
    let (_, seen) = parse_visited(&d([("name", b("x"))]));
    assert!(seen.is_empty());
}

#[test]
fn v2_paths_match_v1_paths_under_the_length_cap() {
    // 16 full-size components fill 4 095 characters, so the 17th is cut
    // by the path cap. v2 builds paths incrementally, v1 joins a component
    // list; both must give the same capped path. One component is
    // multi-byte (byte-capped on its own) to cover non-ASCII joins.
    let mut comps: Vec<String> = (0..16)
        .map(|n| format!("{n:02}") + &"a".repeat(253))
        .collect();
    // A multi-byte component that survives the byte cap (253 bytes), so the
    // join still covers non-ASCII text.
    comps[7] = format!("07{}東東", "a".repeat(245));
    comps.push("tail.txt".into());
    let refs: Vec<&str> = comps.iter().map(String::as_str).collect();
    let v1 = parse(&v1_multi(vec![v1_file(1, &refs)])).unwrap();

    let mut node = d([("tail.txt", v2_file(1))]);
    for c in refs[..16].iter().rev() {
        node = d([(*c, node)]);
    }
    let v2 = parse(&v2_only(node)).unwrap();
    assert_eq!(v1.files[0].path, v2.files[0].path);
    assert!(v1.files[0].path.chars().count() <= PATH_MAX_CHARS);
    assert!(!v1.files[0].path.ends_with('/'));
    assert!(!v1.files[0].path.ends_with("tail.txt"));
}

#[test]
fn many_files_under_long_directories_stay_bounded() {
    // Every file shares a ~3 840-character directory prefix that fits the
    // path cap, so each joined path is shown: 20 000 of them exceed the
    // text budget, so parsing fails closed after showing each directory
    // once and every file name.
    let dir = "目".repeat(255);
    let files: Vec<(Vec<u8>, O)> = (0..20_000)
        .map(|n| (format!("{n}").into_bytes(), v2_file(1)))
        .collect();
    let mut node = O::Dict(files.into_iter().collect());
    for _ in 0..15 {
        node = d([(dir.as_str(), node)]);
    }
    let raw = encode(&v2_only(node));
    let mut calls = 0usize;
    let r = parse_info_visit(&raw, &mut |p| {
        calls += 1;
        assert!(p.chars().count() <= PATH_MAX_CHARS);
    });
    assert_eq!(r, Err(ParseError::TooMuchText));
    // name + 15 directories + 20 000 file names, plus the joined paths
    // that fit in the budget.
    let base = 1 + 15 + 20_000;
    // Exactly one visit per file (the joined path or the components,
    // never both), plus the name and each directory once.
    assert_eq!(calls, base);
}

#[test]
fn deep_tree_within_budget_shows_joined_paths() {
    // Phrases across a directory/file boundary still reach the visitor.
    let tree = d([("blocked", d([("phrase.mkv", v2_file(1))]))]);
    let (m, seen) = parse_visited(&v2_only(tree));
    assert!(m.is_ok());
    assert_eq!(seen, vec!["v2 torrent", "blocked/phrase.mkv"]);
}

proptest! {
    #[test]
    fn visitor_matches_listing(v in arb_info()) {
        let (r, seen) = parse_visited(&v);
        if let Ok(m) = r {
            prop_assert_eq!(&seen[0], &m.name);
            // Every kept path was visited (the single-file path is the name).
            for f in &m.files {
                prop_assert!(seen.contains(&f.path));
            }
        }
    }
}

/// The visited-text budget for `raw`, as documented on [`parse_info_visit`].
fn text_budget(raw: &[u8]) -> usize {
    raw.len()
        .saturating_mul(TEXT_CHARS_PER_BYTE)
        .clamp(MIN_TEXT_CHARS, MAX_TEXT_CHARS)
}

#[test]
fn shared_long_prefix_is_not_rescanned_per_file() {
    // F1: many tiny files under one ~3 840-character directory prefix that
    // fits the path cap, so every joined path is shown and the total
    // exceeds the text budget.
    let dir = "d".repeat(255);
    let files: Vec<(Vec<u8>, O)> = (0..20_000)
        .map(|n| (format!("{n}").into_bytes(), v2_file(1)))
        .collect();
    let mut files: Map = files.into_iter().collect();
    files.insert(b"zz blocked".to_vec(), v2_file(1));
    let mut node = O::Dict(files);
    for _ in 0..15 {
        node = d([(dir.as_str(), node)]);
    }
    let raw = encode(&v2_only(node));
    let mut chars = 0usize;
    let mut seen_blocked = false;
    let r = parse_info_visit(&raw, &mut |p| {
        chars += p.chars().count();
        seen_blocked |= p.contains("blocked");
    });
    assert_eq!(r, Err(ParseError::TooMuchText));
    // Bounded by the budget plus text that is present in the bytes.
    assert!(chars <= text_budget(&raw) + raw.len(), "{chars}");
    // The file name past the budget (and past the path cap) is still seen.
    assert!(seen_blocked);
}

#[test]
fn visited_bound_is_budget_plus_metadata_len() {
    // F-05: the documented bound on visited text is budget + info length
    // (fail-closed with TooMuchText), not the budget alone. The prefix fits
    // the path cap so every joined path is shown.
    let dir = "e".repeat(255);
    let files: Vec<(Vec<u8>, O)> = (0..20_000)
        .map(|n| (format!("{n}").into_bytes(), v2_file(1)))
        .collect();
    let mut files: Map = files.into_iter().collect();
    files.insert(b"zz blocked".to_vec(), v2_file(1));
    let mut node = O::Dict(files);
    for _ in 0..15 {
        node = d([(dir.as_str(), node)]);
    }
    let raw = encode(&v2_only(node));
    let budget = text_budget(&raw);
    let mut chars = 0usize;
    let r = parse_info_visit(&raw, &mut |p| {
        chars += p.chars().count();
    });
    assert_eq!(r, Err(ParseError::TooMuchText));
    assert!(chars <= budget + raw.len(), "{chars}");
}

#[test]
fn components_beyond_the_path_cap_are_visited() {
    let long = "x".repeat(255);
    let mut comps: Vec<&str> = vec![long.as_str(); 17];
    comps.push("blocked dir");
    comps.push("blocked file");
    let (m, seen) = parse_visited(&v1_multi(vec![v1_file(1, &comps)]));
    assert!(m.is_ok());
    assert!(seen.iter().any(|s| s.contains("blocked dir")));
    assert!(seen.iter().any(|s| s.contains("blocked file")));

    let mut node = d([("blocked file", v2_file(1))]);
    for c in comps[..18].iter().rev() {
        node = d([(*c, node)]);
    }
    let (m, seen) = parse_visited(&v2_only(node));
    assert!(m.is_ok());
    assert!(seen.iter().any(|s| s.contains("blocked dir")));
    assert!(seen.iter().any(|s| s.contains("blocked file")));
}

#[test]
fn visit_sees_padding_paths() {
    // F2: padding stays out of the listing but its path is still shown.
    let files = vec![
        v1_file(100, &["movie.mkv"]),
        with(v1_file(7, &["blocked a"]), "attr", b("p")),
        v1_file(8, &[".pad", "blocked b"]),
        v1_file(9, &["_____padding_file blocked c"]),
    ];
    let (m, seen) = parse_visited(&v1_multi(files));
    assert_eq!(
        seen,
        vec![
            "dir",
            "movie.mkv",
            "blocked a",
            ".pad/blocked b",
            "_____padding_file blocked c"
        ]
    );
    let m = m.unwrap();
    assert_eq!(paths(&m), vec!["movie.mkv"]);
    assert_eq!((m.total_size, m.file_count), (100, 1));

    let tree = d([
        ("a.bin", v2_file(50)),
        (".pad", d([("blocked d", v2_file(16))])),
        (
            "b",
            d([(
                "blocked e",
                with(v2_file(4), "", d([("length", i(4)), ("attr", b("p"))])),
            )]),
        ),
    ]);
    let (m, seen) = parse_visited(&v2_only(tree));
    assert_eq!(
        seen,
        vec!["v2 torrent", ".pad/blocked d", "a.bin", "b/blocked e"]
    );
    let m = m.unwrap();
    assert_eq!(paths(&m), vec!["a.bin"]);
    assert_eq!((m.total_size, m.file_count), (50, 1));
}

fn hybrid(tree: O, files: O) -> O {
    with(
        with(v2_only(tree), "files", files),
        "pieces",
        O::bytes(vec![0u8; 20]),
    )
}

#[test]
fn visit_hybrid_sees_tree_and_v1_list() {
    // F3: the v1 list of a hybrid is shown but not stored.
    let tree = d([("real", d([("one.bin", v2_file(42))]))]);
    let v = hybrid(
        tree,
        O::List(vec![
            v1_file(1, &["dir", "blocked 01.mp4"]),
            v1_file(2, &[".pad", "blocked pad"]),
        ]),
    );
    let (m, seen) = parse_visited(&v);
    assert_eq!(
        seen,
        vec![
            "v2 torrent",
            "real/one.bin",
            "dir/blocked 01.mp4",
            ".pad/blocked pad"
        ]
    );
    let m = m.unwrap();
    assert_eq!(paths(&m), vec!["real/one.bin"]);
    assert_eq!((m.total_size, m.file_count), (42, 1));
}

#[test]
fn malformed_hybrid_v1_list_is_rejected() {
    let tree = || d([("a", v2_file(1))]);
    assert_eq!(
        parse(&hybrid(tree(), b("x"))),
        Err(ParseError::InvalidField("files"))
    );
    assert_eq!(
        parse(&hybrid(tree(), O::List(vec![d([("length", i(1))])]))),
        Err(ParseError::InvalidField("path"))
    );
    assert_eq!(
        parse(&hybrid(tree(), O::List(vec![v1_file(-1, &["a"])]))),
        Err(ParseError::NegativeLength)
    );
    // A single-file hybrid needs a v1 `length`.
    let single = with(v2_only(tree()), "pieces", O::bytes(vec![0u8; 20]));
    assert_eq!(parse(&single), Err(ParseError::InvalidField("length")));
    assert!(parse(&with(single, "length", i(1))).is_ok());
}
