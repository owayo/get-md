//! 取得元の情報を書く YAML の front matter。
//!
//! `--meta` の解析と検証、項目の描画と YAML の文字列のエスケープ、既存のファイルと
//! 比べるための分解 (front matter と本文への分割、retrieved_at の値の除外) を扱う。

use std::fmt::Write as _;

use anyhow::{Context, Result, bail};
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};
use url::Url;

use crate::invisible::{self, InvisibleCount};

/// 出力する front matter を組み立てる。
///
/// `strip_invisible` なら、YAML にする前に title から見えない文字を除く。`--meta` の値は
/// 利用者が渡したものなので触らない。
pub(crate) fn front_matter_text(
    mut metadata: SourceMetadata,
    strip_invisible: bool,
    invisible: &mut InvisibleCount,
) -> String {
    if strip_invisible {
        metadata.title = remove_invisible_from_title(metadata.title, invisible);
    }
    render_front_matter(&metadata)
}

/// front matter の title から見えない文字を除く。除いた後に前後の空白を削り、空なら出さない。
fn remove_invisible_from_title(
    title: Option<String>,
    invisible: &mut InvisibleCount,
) -> Option<String> {
    normalize_title(&invisible::remove_invisible(&title?, invisible))
}

/// 取得した時刻の項目の名前。既存のファイルと比べるときは、この項目の値を比べない。
const RETRIEVED_AT_KEY: &str = "retrieved_at";

/// front matter の組み込みの項目の名前 (書く順)。`--meta` のキーには使えない。
const BUILTIN_FRONT_MATTER_KEYS: [&str; 5] =
    ["url", "final_url", "title", "selectors", RETRIEVED_AT_KEY];

/// YAML で文字列ではなく真偽値や null と読まれる語 (大文字小文字を問わない)。
///
/// YAML 1.2 の core schema の `true`・`false`・`null` に、YAML 1.1 のパーサが真偽値と読む
/// `yes`・`no`・`on`・`off`・`y`・`n` を足す。キーにすると、読む側で文字列のキーにならない。
const YAML_NON_STRING_WORDS: [&str; 9] =
    ["true", "false", "null", "yes", "no", "on", "off", "y", "n"];

/// `--meta` のキーの長さの上限。YAML の暗黙のキー (`key: value` の key) は 1024 文字まで
/// (YAML 1.2 の ns-s-implicit-yaml-key)。超えると front matter が YAML として読めない。
const MAX_META_KEY_LEN: usize = 1024;

/// `--meta` で足す front matter の項目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MetaEntry {
    pub(crate) key: String,
    pub(crate) value: String,
}

/// front matter に書く、取得元の情報。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceMetadata {
    /// 引数の URL (そのまま)
    pub(crate) url: String,
    /// 読み込み後のページの URL。`url` と同じなら None
    pub(crate) final_url: Option<String>,
    /// `document.title` の前後の空白を削ったもの。取れない・空なら None
    pub(crate) title: Option<String>,
    /// 実際に使ったセレクタ (既定の body を含む)
    pub(crate) selectors: Vec<String>,
    /// 取得した時刻 (UTC の RFC 3339、秒の精度)
    pub(crate) retrieved_at: String,
    /// `--meta` の項目 (指定した順)
    pub(crate) meta: Vec<MetaEntry>,
}

/// `--meta KEY=VALUE` の並びを解析して検証する。
///
/// 最初の `=` で分け、値は空でもよい。キーは英字か `_` で始め、英数字・`_`・`-` だけで書く。
/// `=` がない・キーが空・キーの書式の誤り・組み込みの項目との衝突・YAML で文字列と
/// 読まれない語・同じキーの重複はエラーにする。ブラウザを起動する前に呼ぶ。
pub(crate) fn parse_meta_entries(args: &[String]) -> Result<Vec<MetaEntry>> {
    let mut entries: Vec<MetaEntry> = Vec::with_capacity(args.len());
    for arg in args {
        let Some((key, value)) = arg.split_once('=') else {
            bail!("--meta {arg:?} に = がありません。KEY=VALUE の形で指定してください");
        };
        if key.is_empty() {
            bail!("--meta {arg:?} のキーが空です。KEY=VALUE の形で指定してください");
        }
        if !is_valid_meta_key(key) {
            bail!(
                "--meta のキー {key:?} は使えません。キーは英字か _ で始め、英数字・_・- だけで書いてください"
            );
        }
        // キーは ASCII だけなので、バイト数が文字数になる
        if key.len() > MAX_META_KEY_LEN {
            bail!(
                "--meta のキーが長すぎます ({} 文字)。YAML のキーにできるのは {MAX_META_KEY_LEN} 文字までです",
                key.len()
            );
        }
        if BUILTIN_FRONT_MATTER_KEYS.contains(&key) {
            bail!(
                "--meta のキー {key:?} は組み込みの項目 ({}) と重なるため使えません",
                BUILTIN_FRONT_MATTER_KEYS.join("・")
            );
        }
        if YAML_NON_STRING_WORDS
            .iter()
            .any(|word| word.eq_ignore_ascii_case(key))
        {
            bail!(
                "--meta のキー {key:?} は YAML で文字列ではなく真偽値や null と読まれるため使えません"
            );
        }
        if entries.iter().any(|entry| entry.key == key) {
            bail!("--meta のキー {key:?} が 2 回以上指定されています");
        }
        entries.push(MetaEntry {
            key: key.to_string(),
            value: value.to_string(),
        });
    }
    Ok(entries)
}

/// `--meta` のキーの書式 (`^[A-Za-z_][A-Za-z0-9_-]*$`) を満たすかを判定する。
fn is_valid_meta_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

/// 取得した時刻を、UTC の RFC 3339 (秒の精度) で書く。例: `2026-09-29T03:10:00Z`
///
/// 秒未満は四捨五入せずに切り捨てる。
pub(crate) fn format_retrieved_at(time: OffsetDateTime) -> Result<String> {
    time.to_offset(UtcOffset::UTC)
        .truncate_to_second()
        .format(&Rfc3339)
        .context("Failed to format the retrieval time")
}

/// 読み込み後の URL が、引数の URL と違うときだけ返す。
///
/// 引数の `https://example.com` がブラウザでは `https://example.com/` になるような、
/// URL の標準 (WHATWG) の正規化だけの違いは同じとみなす。リダイレクトや
/// `history.pushState` で URL が変わったときに返す。
pub(crate) fn distinct_final_url(requested: &str, loaded: &str) -> Option<String> {
    if loaded.is_empty() || loaded == requested {
        return None;
    }
    if let (Ok(requested_url), Ok(loaded_url)) = (Url::parse(requested), Url::parse(loaded))
        && requested_url == loaded_url
    {
        return None;
    }
    Some(loaded.to_string())
}

/// タイトルの前後の空白を削る。空になれば None。
pub(crate) fn normalize_title(title: &str) -> Option<String> {
    let trimmed = title.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// 取得元の情報を YAML の front matter にする。
///
/// `---` の行で挟み、閉じの `---` の後に空行を 1 つ置く。本文が `---` で始まっても、
/// 閉じの `---` の直後が空行なので区別できる。項目の順序は固定で、値はすべて
/// ダブルクォートの文字列 (selectors は文字列のフロー形式の列) にする。
pub(crate) fn render_front_matter(metadata: &SourceMetadata) -> String {
    let mut out = String::from("---\n");
    push_front_matter_item(&mut out, "url", &yaml_string(&metadata.url));
    if let Some(final_url) = &metadata.final_url {
        push_front_matter_item(&mut out, "final_url", &yaml_string(final_url));
    }
    if let Some(title) = &metadata.title {
        push_front_matter_item(&mut out, "title", &yaml_string(title));
    }
    let selectors = metadata
        .selectors
        .iter()
        .map(|selector| yaml_string(selector))
        .collect::<Vec<_>>()
        .join(", ");
    push_front_matter_item(&mut out, "selectors", &format!("[{selectors}]"));
    push_front_matter_item(
        &mut out,
        RETRIEVED_AT_KEY,
        &yaml_string(&metadata.retrieved_at),
    );
    for entry in &metadata.meta {
        push_front_matter_item(&mut out, &entry.key, &yaml_string(&entry.value));
    }
    out.push_str("---\n\n");
    out
}

fn push_front_matter_item(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push_str(": ");
    out.push_str(value);
    out.push('\n');
}

/// 文字列を YAML のダブルクォートの文字列として書く。
///
/// `"` と `\` をエスケープし、改行・復帰・タブは短縮形 (`\n`・`\r`・`\t`)、そのほかの制御文字
/// (`char::is_control`。C0・DEL・C1) は `\uXXXX` にする。YAML 1.1 のパーサが改行とみなす
/// U+2028・U+2029 と、YAML の印字可能な文字に含まれない U+FFFE・U+FFFF、文書の途中に
/// 置けない U+FEFF (BOM) も `\uXXXX` にする。どんな値も 1 行に収まり、YAML のパーサで
/// 元の文字列に戻る。
fn yaml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{2028}' | '\u{2029}' | '\u{FEFF}' | '\u{FFFE}' | '\u{FFFF}' => {
                push_unicode_escape(&mut out, ch);
            }
            ch if ch.is_control() => push_unicode_escape(&mut out, ch),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// 基本多言語面の文字を `\uXXXX` (16 進数 4 桁、大文字) で書く。
fn push_unicode_escape(out: &mut String, ch: char) {
    // String への書き込みは失敗しない
    let _ = write!(out, "\\u{:04X}", u32::from(ch));
}

/// front matter 付きの内容を、front matter (開きと閉じの `---` の間の行) と、
/// 閉じの `---` の行より後ろ (区切りの空行と本文) に分ける。
///
/// 先頭が `---\n` でない、または閉じの `---` の行がなければ None。get-md が書く値は
/// すべてクォートした 1 行なので、front matter の中に `---` だけの行は現れない。
pub(crate) fn split_front_matter(content: &str) -> Option<(&str, &str)> {
    let rest = content.strip_prefix("---\n")?;
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line == "---\n" {
            return Some((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

/// front matter の 1 行が retrieved_at の項目なら、値を除いた項目名だけにする。
///
/// 行の位置と数は比べたまま、取得した時刻の違いだけを比較から外す。
pub(crate) fn mask_retrieved_at(line: &str) -> &str {
    match line.strip_prefix(RETRIEVED_AT_KEY) {
        Some(rest) if rest.starts_with(':') => RETRIEVED_AT_KEY,
        _ => line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{meta_entry, sample_metadata};
    use regex::Regex;

    fn meta_args(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn parse_meta_entries_splits_on_first_equals_and_keeps_order() {
        let entries = parse_meta_entries(&meta_args(&[
            "source=manual",
            "query=a=b",
            "_id=42",
            "Topic-2=x y",
        ]))
        .unwrap();
        assert_eq!(
            entries,
            vec![
                meta_entry("source", "manual"),
                meta_entry("query", "a=b"),
                meta_entry("_id", "42"),
                meta_entry("Topic-2", "x y"),
            ]
        );
    }

    #[test]
    fn parse_meta_entries_accepts_empty_value() {
        let entries = parse_meta_entries(&meta_args(&["note="])).unwrap();
        assert_eq!(entries, vec![meta_entry("note", "")]);
    }

    #[test]
    fn parse_meta_entries_accepts_no_entries() {
        assert!(parse_meta_entries(&[]).unwrap().is_empty());
    }

    #[test]
    fn parse_meta_entries_rejects_missing_equals() {
        let err = parse_meta_entries(&meta_args(&["source"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("\"source\""), "{err}");
        assert!(err.contains("KEY=VALUE"), "{err}");
    }

    #[test]
    fn parse_meta_entries_rejects_empty_key() {
        let err = parse_meta_entries(&meta_args(&["=value"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("キーが空"), "{err}");
    }

    #[test]
    fn parse_meta_entries_rejects_invalid_keys() {
        for arg in [
            "1st=x", "-key=x", "a.b=x", "a b=x", "a:b=x", "キー=x", "a/b=x",
        ] {
            let err = parse_meta_entries(&meta_args(&[arg]))
                .unwrap_err()
                .to_string();
            assert!(err.contains("英字か _ で始め"), "{arg}: {err}");
        }
    }

    #[test]
    fn parse_meta_entries_rejects_duplicate_keys() {
        let err = parse_meta_entries(&meta_args(&["source=a", "note=x", "source=b"]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("\"source\""), "{err}");
        assert!(err.contains("2 回以上"), "{err}");
    }

    #[test]
    fn parse_meta_entries_rejects_builtin_keys() {
        for key in BUILTIN_FRONT_MATTER_KEYS {
            let err = parse_meta_entries(&[format!("{key}=x")])
                .unwrap_err()
                .to_string();
            assert!(err.contains("組み込みの項目"), "{key}: {err}");
        }
    }

    #[test]
    fn parse_meta_entries_rejects_yaml_boolean_and_null_words() {
        for key in ["true", "False", "NULL", "yes", "No", "on", "OFF", "y", "N"] {
            let err = parse_meta_entries(&[format!("{key}=x")])
                .unwrap_err()
                .to_string();
            assert!(err.contains("真偽値や null"), "{key}: {err}");
        }
        // 語を一部に含むだけのキーは文字列として読まれるので使える
        assert!(parse_meta_entries(&meta_args(&["yes_no=x", "null-ish=x", "online=x"])).is_ok());
    }

    #[test]
    fn parse_meta_entries_rejects_keys_longer_than_yaml_implicit_key_limit() {
        use yaml_rust2::YamlLoader;

        // 1024 文字のキーは使え、YAML として読み直せる
        let longest = "k".repeat(1024);
        let metadata = SourceMetadata {
            meta: parse_meta_entries(&[format!("{longest}=x")]).unwrap(),
            ..sample_metadata()
        };
        let rendered = render_front_matter(&metadata);
        let (header, _) = split_front_matter(&rendered).expect("front matter is found");
        let docs = YamlLoader::load_from_str(header).expect("front matter is valid YAML");
        assert_eq!(docs[0][longest.as_str()].as_str(), Some("x"));

        // 1025 文字のキーは YAML のパーサが読めないので、ブラウザを起動する前に断る
        let err = parse_meta_entries(&[format!("{longest}k=x")])
            .unwrap_err()
            .to_string();
        assert!(err.contains("1024"), "{err}");
    }

    #[test]
    fn is_valid_meta_key_follows_documented_pattern() {
        for key in ["a", "Z", "_", "_a", "source", "source-id", "A_b-9", "x--"] {
            assert!(is_valid_meta_key(key), "{key:?} は使えるキー");
        }
        for key in ["", "9a", "-a", "a.b", "a b", "a:b", "é", "キー", "a\n"] {
            assert!(!is_valid_meta_key(key), "{key:?} は使えないキー");
        }
    }

    #[test]
    fn yaml_string_quotes_plain_text() {
        assert_eq!(yaml_string("Example"), r#""Example""#);
        assert_eq!(yaml_string(""), r#""""#);
    }

    #[test]
    fn yaml_string_escapes_double_quote_and_backslash() {
        assert_eq!(yaml_string(r#"say "hi" \o/"#), r#""say \"hi\" \\o/""#);
    }

    #[test]
    fn yaml_string_uses_short_escapes_for_newline_carriage_return_and_tab() {
        assert_eq!(yaml_string("a\nb\rc\td"), r#""a\nb\rc\td""#);
    }

    #[test]
    fn yaml_string_escapes_line_and_paragraph_separators() {
        assert_eq!(yaml_string("a\u{2028}b\u{2029}c"), r#""a\u2028b\u2029c""#);
    }

    #[test]
    fn yaml_string_escapes_other_control_characters() {
        // C0 (NUL・ESC)・DEL・C1 (NEL を含む)
        assert_eq!(
            yaml_string("\u{0}\u{1b}\u{7f}\u{85}\u{9f}"),
            r#""\u0000\u001B\u007F\u0085\u009F""#
        );
    }

    #[test]
    fn yaml_string_escapes_characters_yaml_does_not_allow_unescaped() {
        // U+FFFE・U+FFFF は YAML の印字可能な文字でなく、U+FEFF (BOM) は文書の途中に置けない
        assert_eq!(
            yaml_string("\u{feff}\u{fffe}\u{ffff}"),
            r#""\uFEFF\uFFFE\uFFFF""#
        );
    }

    #[test]
    fn yaml_string_keeps_emoji_colon_and_other_text() {
        let value = "注意: 😀 #tag - [x] {y} 'z' --- & * ! | > % @ `";
        assert_eq!(yaml_string(value), format!("\"{value}\""));
    }

    #[test]
    fn render_front_matter_writes_items_in_fixed_order() {
        assert_eq!(
            render_front_matter(&sample_metadata()),
            concat!(
                "---\n",
                "url: \"https://example.com/docs\"\n",
                "final_url: \"https://example.com/docs/intro\"\n",
                "title: \"Example Docs\"\n",
                "selectors: [\"article\", \"main\"]\n",
                "retrieved_at: \"2026-09-29T03:10:00Z\"\n",
                "source: \"manual\"\n",
                "note: \"\"\n",
                "---\n",
                "\n",
            )
        );
    }

    #[test]
    fn render_front_matter_omits_missing_final_url_and_title() {
        let metadata = SourceMetadata {
            final_url: None,
            title: None,
            meta: Vec::new(),
            ..sample_metadata()
        };
        assert_eq!(
            render_front_matter(&metadata),
            concat!(
                "---\n",
                "url: \"https://example.com/docs\"\n",
                "selectors: [\"article\", \"main\"]\n",
                "retrieved_at: \"2026-09-29T03:10:00Z\"\n",
                "---\n",
                "\n",
            )
        );
    }

    #[test]
    fn render_front_matter_escapes_selectors_and_values() {
        let metadata = SourceMetadata {
            title: Some("A \"quoted\"\ntitle".to_string()),
            selectors: vec![r#"a[href="x"]"#.to_string(), "div\\:hover".to_string()],
            meta: vec![meta_entry("note", "line1\nline2")],
            ..sample_metadata()
        };
        let rendered = render_front_matter(&metadata);
        assert!(
            rendered.contains("\ntitle: \"A \\\"quoted\\\"\\ntitle\"\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("\nselectors: [\"a[href=\\\"x\\\"]\", \"div\\\\:hover\"]\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("\nnote: \"line1\\nline2\"\n"),
            "{rendered}"
        );
    }

    #[test]
    fn split_front_matter_requires_opening_and_closing_lines() {
        assert_eq!(
            split_front_matter("---\nurl: \"x\"\n---\n\nBody"),
            Some(("url: \"x\"\n", "\nBody"))
        );
        // 先頭が区切りでない・閉じの行がない・CRLF の区切りは front matter とみなさない
        assert_eq!(split_front_matter("# Title\n\n---\n"), None);
        assert_eq!(split_front_matter("---\nurl: \"x\"\n"), None);
        assert_eq!(split_front_matter("---\nurl: \"x\"\n---"), None);
        assert_eq!(
            split_front_matter("---\r\nurl: \"x\"\r\n---\r\n\r\nBody"),
            None
        );
    }

    #[test]
    fn front_matter_round_trips_through_yaml_parser() {
        use yaml_rust2::YamlLoader;

        let values = [
            "",
            "plain text",
            "quote \" backslash \\ and a literal \\n",
            "line\nbreak\r\nand\ttab",
            "separators \u{2028} \u{2029}",
            "controls \u{0}\u{7}\u{1b}\u{7f}\u{85}\u{9f}",
            "bom \u{feff} and noncharacters \u{fffe}\u{ffff}",
            "emoji 😀 and colon: value # not a comment",
            "  leading and trailing spaces  ",
            "---",
            "- not a list",
            "{not: a map}",
            "[not, a, list]",
            "'single quoted'",
            "&anchor *alias !tag %directive @reserved `backtick`",
            "true",
            "null",
            "~",
            "123",
            "1e3",
            "0x1F",
            "2026-09-29",
            "日本語のタイトル｜サイト名",
        ];
        let metadata = SourceMetadata {
            url: "https://example.com/a?b=c&d=\"e\"#f".to_string(),
            final_url: Some("https://example.com/b?x=1#frag".to_string()),
            title: Some(values.join(" / ")),
            selectors: values.iter().map(|value| value.to_string()).collect(),
            retrieved_at: "2026-09-29T03:10:00Z".to_string(),
            meta: values
                .iter()
                .enumerate()
                .map(|(i, value)| meta_entry(&format!("v{i}"), value))
                .collect(),
        };

        let rendered = render_front_matter(&metadata);
        let (header, _) = split_front_matter(&rendered).expect("front matter is found");
        let docs = YamlLoader::load_from_str(header).expect("front matter is valid YAML");
        assert_eq!(docs.len(), 1);
        let doc = &docs[0];

        let keys: Vec<&str> = doc
            .as_hash()
            .expect("front matter is a mapping")
            .keys()
            .map(|key| key.as_str().expect("keys are strings"))
            .collect();
        let mut expected_keys = BUILTIN_FRONT_MATTER_KEYS.to_vec();
        let meta_keys: Vec<String> = (0..values.len()).map(|i| format!("v{i}")).collect();
        expected_keys.extend(meta_keys.iter().map(String::as_str));
        assert_eq!(keys, expected_keys);

        assert_eq!(doc["url"].as_str(), Some(metadata.url.as_str()));
        assert_eq!(doc["final_url"].as_str(), metadata.final_url.as_deref());
        assert_eq!(doc["title"].as_str(), metadata.title.as_deref());
        assert_eq!(
            doc["retrieved_at"].as_str(),
            Some(metadata.retrieved_at.as_str())
        );
        let selectors: Vec<&str> = doc["selectors"]
            .as_vec()
            .expect("selectors is a sequence")
            .iter()
            .map(|selector| selector.as_str().expect("selectors are strings"))
            .collect();
        assert_eq!(selectors, values);
        for (key, value) in meta_keys.iter().zip(values) {
            assert_eq!(doc[key.as_str()].as_str(), Some(value), "{key}");
        }
    }

    #[test]
    fn format_retrieved_at_truncates_to_seconds_in_utc() {
        let date = time::Date::from_calendar_date(2026, time::Month::September, 29).unwrap();
        let utc = date
            .with_hms_nano(3, 10, 0, 999_999_999)
            .unwrap()
            .assume_utc();
        assert_eq!(format_retrieved_at(utc).unwrap(), "2026-09-29T03:10:00Z");

        // UTC 以外の時刻は UTC に直す
        let jst = date
            .with_hms_nano(12, 10, 0, 500_000_000)
            .unwrap()
            .assume_offset(UtcOffset::from_hms(9, 0, 0).unwrap());
        assert_eq!(format_retrieved_at(jst).unwrap(), "2026-09-29T03:10:00Z");
    }

    #[test]
    fn format_retrieved_at_writes_current_time_as_rfc3339_seconds() {
        let formatted = format_retrieved_at(OffsetDateTime::now_utc()).unwrap();
        let pattern = Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$").unwrap();
        assert!(pattern.is_match(&formatted), "{formatted}");
    }

    #[test]
    fn distinct_final_url_omits_same_url() {
        assert_eq!(
            distinct_final_url("https://example.com/a", "https://example.com/a"),
            None
        );
        // URL の標準の正規化だけの違い (末尾の /・スキームとホストの大文字) は同じとみなす
        assert_eq!(
            distinct_final_url("https://example.com", "https://example.com/"),
            None
        );
        assert_eq!(
            distinct_final_url("HTTPS://EXAMPLE.com/a", "https://example.com/a"),
            None
        );
        // 読み込み後の URL が取れなかったときは出さない
        assert_eq!(distinct_final_url("https://example.com/a", ""), None);
    }

    #[test]
    fn distinct_final_url_returns_redirected_url() {
        assert_eq!(
            distinct_final_url("http://example.com/a", "https://example.com/a"),
            Some("https://example.com/a".to_string())
        );
        assert_eq!(
            distinct_final_url("https://example.com/old", "https://example.com/new#top"),
            Some("https://example.com/new#top".to_string())
        );
        // 引数を URL として読めなければ文字列で比べる
        assert_eq!(
            distinct_final_url("example.com", "https://example.com/"),
            Some("https://example.com/".to_string())
        );
    }

    #[test]
    fn normalize_title_trims_whitespace() {
        assert_eq!(
            normalize_title("  Example Docs \n"),
            Some("Example Docs".to_string())
        );
        assert_eq!(
            normalize_title("\u{3000}全角の空白\u{3000}"),
            Some("全角の空白".to_string())
        );
        assert_eq!(normalize_title(""), None);
        assert_eq!(normalize_title(" \t\n\u{3000}"), None);
    }

    #[test]
    fn mask_retrieved_at_hides_only_retrieved_at_value() {
        assert_eq!(
            mask_retrieved_at("retrieved_at: \"2026-09-29T03:10:00Z\"\n"),
            "retrieved_at"
        );
        assert_eq!(
            mask_retrieved_at("retrieved_at_note: \"x\"\n"),
            "retrieved_at_note: \"x\"\n"
        );
        assert_eq!(
            mask_retrieved_at("title: \"retrieved_at: x\"\n"),
            "title: \"retrieved_at: x\"\n"
        );
    }

    #[test]
    fn remove_invisible_from_title_trims_after_removal() {
        let mut count = InvisibleCount::default();
        assert_eq!(
            remove_invisible_from_title(
                Some("\u{200B} Ti\u{202E}tle \u{FEFF}".to_string()),
                &mut count
            ),
            Some("Title".to_string())
        );
        assert_eq!(count.removed, 3);
        // 見えない文字だけのタイトルは出さない
        assert_eq!(
            remove_invisible_from_title(Some("\u{200B}\u{2060}".to_string()), &mut count),
            None
        );
        assert_eq!(remove_invisible_from_title(None, &mut count), None);
        assert_eq!(count.removed, 5);
    }

    #[test]
    fn front_matter_text_cleans_title_but_not_meta_values() {
        let metadata = SourceMetadata {
            title: Some("Exa\u{200B}mple".to_string()),
            meta: vec![meta_entry("note", "kept\u{200B}as-is")],
            ..sample_metadata()
        };

        let mut count = InvisibleCount::default();
        let cleaned = front_matter_text(metadata.clone(), true, &mut count);
        assert!(cleaned.contains("\ntitle: \"Example\"\n"), "{cleaned}");
        // --meta の値は利用者が渡したものなので触らない
        assert!(
            cleaned.contains("\nnote: \"kept\u{200B}as-is\"\n"),
            "{cleaned}"
        );
        assert_eq!(count.removed, 1);

        let mut count = InvisibleCount::default();
        let kept = front_matter_text(metadata, false, &mut count);
        assert!(kept.contains("\ntitle: \"Exa\u{200B}mple\"\n"), "{kept}");
        assert_eq!(count, InvisibleCount::default());
    }
}
