//! Unicode のデータファイル (data/unicode/) から、見えない文字を除く処理 (src/invisible.rs) が
//! 使う表を作る。
//!
//! 文字の性質 (一般カテゴリ・Default_Ignorable_Code_Point・Joining_Type など) は icu_properties の
//! 組み込みのデータを使う。ここで作るのは、文字の性質では表せない並びの表だけ:
//!
//! - RGI の絵文字の ZWJ の並び (emoji-zwj-sequences.txt の RGI_Emoji_ZWJ_Sequence)
//! - RGI の絵文字のタグの並び (emoji-sequences.txt の RGI_Emoji_Tag_Sequence。地域の旗)
//! - 絵文字の異体字の並びの基底の文字 (emoji-variation-sequences.txt)
//! - 標準の異体字の並び (StandardizedVariants.txt)
//!
//! データファイルの版は `UNICODE_VERSION` と一致しなければビルドを止める。icu_properties の
//! データ (ICU 78) と同じ Unicode 17.0 にそろえている。更新の手順は docs/development.md。

use std::collections::BTreeSet;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

/// データファイルの Unicode の版。
const UNICODE_VERSION: &str = "17.0.0";
/// 絵文字のデータファイルの版 (Unicode の版の major.minor)。
const EMOJI_VERSION: &str = "17.0";

const DATA_DIR: &str = "data/unicode";
const ZWJ_SEQUENCES: &str = "emoji-zwj-sequences.txt";
const EMOJI_SEQUENCES: &str = "emoji-sequences.txt";
const EMOJI_VARIATION_SEQUENCES: &str = "emoji-variation-sequences.txt";
const STANDARDIZED_VARIANTS: &str = "StandardizedVariants.txt";

const ZWJ: u32 = 0x200D;
const CANCEL_TAG: u32 = 0xE007F;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    for name in [
        ZWJ_SEQUENCES,
        EMOJI_SEQUENCES,
        EMOJI_VARIATION_SEQUENCES,
        STANDARDIZED_VARIANTS,
    ] {
        println!("cargo::rerun-if-changed={DATA_DIR}/{name}");
    }

    let zwj_text = read_data(ZWJ_SEQUENCES);
    require_header(
        ZWJ_SEQUENCES,
        &zwj_text,
        &format!("# Version: {EMOJI_VERSION}"),
    );
    let zwj_sequences = sequences_of_type(ZWJ_SEQUENCES, &zwj_text, "RGI_Emoji_ZWJ_Sequence");
    check(
        zwj_sequences.iter().all(|sequence| sequence.contains(&ZWJ)),
        "every RGI_Emoji_ZWJ_Sequence contains U+200D",
    );

    let emoji_text = read_data(EMOJI_SEQUENCES);
    require_header(
        EMOJI_SEQUENCES,
        &emoji_text,
        &format!("# Version: {EMOJI_VERSION}"),
    );
    let tag_sequences = sequences_of_type(EMOJI_SEQUENCES, &emoji_text, "RGI_Emoji_Tag_Sequence");
    check(
        tag_sequences
            .iter()
            .all(|sequence| sequence.last() == Some(&CANCEL_TAG)),
        "every RGI_Emoji_Tag_Sequence ends with U+E007F",
    );

    let variation_text = read_data(EMOJI_VARIATION_SEQUENCES);
    require_header(
        EMOJI_VARIATION_SEQUENCES,
        &variation_text,
        &format!("# Version: {EMOJI_VERSION}"),
    );
    let variation_bases = emoji_variation_bases(&variation_text);

    let standardized_text = read_data(STANDARDIZED_VARIANTS);
    require_header(
        STANDARDIZED_VARIANTS,
        &standardized_text,
        &format!("# StandardizedVariants-{UNICODE_VERSION}.txt"),
    );
    let standardized = standardized_variants(&standardized_text);

    let mut out = String::new();
    writeln!(
        out,
        "// build.rs が {DATA_DIR} (Unicode {UNICODE_VERSION}) から作る。手で書き換えない。\n"
    )
    .unwrap();
    writeln!(
        out,
        "/// 表を作った Unicode のデータファイルの版 (テストで icu_properties の版と照らす)。\n#[cfg(test)]\npub(crate) const UNICODE_DATA_VERSION: &str = {UNICODE_VERSION:?};\n"
    )
    .unwrap();
    write_sequences(
        &mut out,
        "RGI_EMOJI_ZWJ_SEQUENCES",
        "RGI の絵文字の ZWJ の並び (emoji-zwj-sequences.txt)。",
        &zwj_sequences,
    );
    write_sequences(
        &mut out,
        "RGI_EMOJI_TAG_SEQUENCES",
        "RGI の絵文字のタグの並び (emoji-sequences.txt の RGI_Emoji_Tag_Sequence)。",
        &tag_sequences,
    );
    write_chars(
        &mut out,
        "EMOJI_VARIATION_BASES",
        "U+FE0E・U+FE0F を続けてよい基底の文字 (emoji-variation-sequences.txt。昇順)。",
        &variation_bases,
    );
    write_pairs(
        &mut out,
        "STANDARDIZED_VARIATION_SEQUENCES",
        "標準の異体字の並びの (基底の文字, 異体字セレクタ) (StandardizedVariants.txt。昇順)。",
        &standardized,
    );

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by cargo"));
    fs::write(out_dir.join("unicode_tables.rs"), out).expect("failed to write unicode_tables.rs");
}

fn read_data(name: &str) -> String {
    let path = Path::new(DATA_DIR).join(name);
    fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()))
}

fn check(condition: bool, what: &str) {
    assert!(condition, "Unicode data check failed: {what}");
}

/// ファイルの先頭のコメントに、期待する版の行があることを確かめる。
fn require_header(name: &str, text: &str, expected: &str) {
    let found = text
        .lines()
        .take_while(|line| line.starts_with('#') || line.trim().is_empty())
        .any(|line| line.trim_end() == expected);
    assert!(
        found,
        "{DATA_DIR}/{name} does not have the header line {expected:?}. \
         Update UNICODE_VERSION / EMOJI_VERSION in build.rs together with the data files"
    );
}

/// 行からコメント (`#` 以降) を除き、`;` で分けた欄にする。空行は None。
fn fields(line: &str) -> Option<Vec<&str>> {
    let body = line.split('#').next().unwrap_or("").trim();
    if body.is_empty() {
        return None;
    }
    Some(body.split(';').map(str::trim).collect())
}

fn parse_code_points(field: &str, name: &str) -> Vec<u32> {
    field
        .split_whitespace()
        .map(|hex| {
            let value = u32::from_str_radix(hex, 16)
                .unwrap_or_else(|err| panic!("{name}: invalid code point {hex:?}: {err}"));
            assert!(
                char::from_u32(value).is_some(),
                "{name}: U+{value:04X} is not a Unicode scalar value"
            );
            value
        })
        .collect()
}

/// 型の欄が `type_field` の行の並びを集める (重複は除き、並びの順に並べる)。
fn sequences_of_type(name: &str, text: &str, type_field: &str) -> Vec<Vec<u32>> {
    let mut sequences = BTreeSet::new();
    for line in text.lines() {
        let Some(fields) = fields(line) else {
            continue;
        };
        if fields.get(1) != Some(&type_field) {
            continue;
        }
        let sequence = parse_code_points(fields[0], name);
        assert!(sequence.len() >= 2, "{name}: too short sequence: {line}");
        sequences.insert(sequence);
    }
    assert!(!sequences.is_empty(), "{name}: no {type_field} found");
    sequences.into_iter().collect()
}

/// 絵文字の異体字の並び (基底 + U+FE0E か U+FE0F) の基底の文字を集める。
fn emoji_variation_bases(text: &str) -> Vec<u32> {
    let mut bases = BTreeSet::new();
    for line in text.lines() {
        let Some(fields) = fields(line) else {
            continue;
        };
        let sequence = parse_code_points(fields[0], EMOJI_VARIATION_SEQUENCES);
        assert!(
            sequence.len() == 2 && matches!(sequence[1], 0xFE0E | 0xFE0F),
            "{EMOJI_VARIATION_SEQUENCES}: unexpected sequence: {line}"
        );
        bases.insert(sequence[0]);
    }
    assert!(
        !bases.is_empty(),
        "{EMOJI_VARIATION_SEQUENCES}: no sequences"
    );
    bases.into_iter().collect()
}

/// 標準の異体字の並び (基底 + 異体字セレクタ) を集める。
fn standardized_variants(text: &str) -> Vec<(u32, u32)> {
    let mut pairs = BTreeSet::new();
    for line in text.lines() {
        let Some(fields) = fields(line) else {
            continue;
        };
        let sequence = parse_code_points(fields[0], STANDARDIZED_VARIANTS);
        assert!(
            sequence.len() == 2
                && matches!(sequence[1], 0x180B..=0x180D | 0x180F | 0xFE00..=0xFE0D),
            "{STANDARDIZED_VARIANTS}: unexpected sequence: {line}"
        );
        pairs.insert((sequence[0], sequence[1]));
    }
    assert!(!pairs.is_empty(), "{STANDARDIZED_VARIANTS}: no sequences");
    pairs.into_iter().collect()
}

fn char_literal(code_point: u32) -> String {
    format!("'\\u{{{code_point:X}}}'")
}

fn write_sequences(out: &mut String, name: &str, doc: &str, sequences: &[Vec<u32>]) {
    writeln!(out, "/// {doc}\npub(crate) static {name}: &[&[char]] = &[").unwrap();
    for sequence in sequences {
        let chars: Vec<String> = sequence.iter().map(|&cp| char_literal(cp)).collect();
        writeln!(out, "    &[{}],", chars.join(", ")).unwrap();
    }
    writeln!(out, "];\n").unwrap();
}

fn write_chars(out: &mut String, name: &str, doc: &str, chars: &[u32]) {
    writeln!(out, "/// {doc}\npub(crate) static {name}: &[char] = &[").unwrap();
    for &cp in chars {
        writeln!(out, "    {},", char_literal(cp)).unwrap();
    }
    writeln!(out, "];\n").unwrap();
}

fn write_pairs(out: &mut String, name: &str, doc: &str, pairs: &[(u32, u32)]) {
    writeln!(
        out,
        "/// {doc}\npub(crate) static {name}: &[(char, char)] = &["
    )
    .unwrap();
    for &(base, selector) in pairs {
        writeln!(
            out,
            "    ({}, {}),",
            char_literal(base),
            char_literal(selector)
        )
        .unwrap();
    }
    writeln!(out, "];\n").unwrap();
}
