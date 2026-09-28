//! 見えない文字を除く。
//!
//! 取得した Markdown を LLM に渡したとき、人の目には見えない文字 (ゼロ幅スペース・BOM・
//! 双方向の制御文字・タグ文字など) に隠した指示を読ませたり、動きを乱したりできないように、
//! 出力からこれらを除く。見える表記 (`​` など) に置き換えずに消す。
//!
//! 除くのは次の文字:
//!
//! - 制御文字 (一般カテゴリ Cc) のうちタブ・LF・CR 以外 (C0・DEL・C1)
//! - Unicode の Default_Ignorable_Code_Point (DICP。ゼロ幅の文字・双方向の制御文字・
//!   異体字セレクタ・タグ文字・ハングルの埋め字・ソフトハイフンなど)
//! - 行間注記の文字 U+FFF9〜U+FFFB (DICP ではないが表示されない)
//!
//! ただし、次の文脈では表示や綴りに効くので残す:
//!
//! - ZWJ (U+200D) とタグ文字 (U+E0000〜U+E007F): RGI の絵文字の ZWJ の並びとタグの並び
//!   (地域の旗) の中。並びの中の U+FE0F は、完全な形の並びで U+FE0F が来る位置にあれば残す
//! - 異体字セレクタ: 基底の文字の直後の 1 つだけ。U+FE0E・U+FE0F は絵文字の異体字の並びの
//!   基底の後、U+FE00〜U+FE0D とモンゴル文字の自由異体字セレクタ (U+180B〜U+180D・U+180F) は
//!   標準の異体字の並び (StandardizedVariants.txt) の組、U+E0100〜U+E01EF (IVS) は
//!   CJK 統合漢字 (Unified_Ideograph) の後。IVS は IVD の登録を照合しない近似
//! - ZWNJ (U+200C) と、絵文字の並びの外の ZWJ: 綴りに効く文脈だけ (UAX #31 の文脈の規則に
//!   ならう)。前後のどちらかが virama (正準結合クラス 9) のとき (インド系の文字の半字形・
//!   結合の抑止)、または続け書きの文脈のとき (Joining_Type。ZWNJ は前が左に、後ろが右に
//!   つながる文字のとき。ZWJ はどちらかの側がつながる文字のとき)
//!
//! 空白 (NBSP・全角スペースなど)・私用領域の文字・DICP でない未割り当ての文字は触らない。
//! 文字の性質は icu_properties の組み込みのデータ (Unicode 17.0)、並びの表は build.rs が
//! data/unicode/ の Unicode 17.0.0 のデータファイルから作る。

use std::borrow::Cow;
use std::rc::Rc;
use std::sync::LazyLock;

use icu_properties::props::{
    CanonicalCombiningClass, DefaultIgnorableCodePoint, JoiningType, UnifiedIdeograph,
};
use icu_properties::{
    CodePointMapData, CodePointMapDataBorrowed, CodePointSetData, CodePointSetDataBorrowed,
};
use markup5ever_rcdom::{Handle, NodeData};

mod tables {
    include!(concat!(env!("OUT_DIR"), "/unicode_tables.rs"));
}

const ZWNJ: char = '\u{200C}';
const ZWJ: char = '\u{200D}';
const VS16: char = '\u{FE0F}';

static DEFAULT_IGNORABLE: CodePointSetDataBorrowed<'static> =
    CodePointSetData::new::<DefaultIgnorableCodePoint>();
static UNIFIED_IDEOGRAPH: CodePointSetDataBorrowed<'static> =
    CodePointSetData::new::<UnifiedIdeograph>();
static JOINING_TYPE: CodePointMapDataBorrowed<'static, JoiningType> =
    CodePointMapData::<JoiningType>::new();
static COMBINING_CLASS: CodePointMapDataBorrowed<'static, CanonicalCombiningClass> =
    CodePointMapData::<CanonicalCombiningClass>::new();

/// 除いた見えない文字の数 (Unicode のスカラー値の数)。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InvisibleCount {
    /// 除いた文字の数 (`c1_controls` を含む)
    pub(crate) removed: usize,
    /// そのうち C1 制御文字 (U+0080〜U+009F) の数
    pub(crate) c1_controls: usize,
}

impl std::ops::AddAssign for InvisibleCount {
    fn add_assign(&mut self, other: Self) {
        self.removed += other.removed;
        self.c1_controls += other.c1_controls;
    }
}

impl InvisibleCount {
    fn record(&mut self, ch: char) {
        self.removed += 1;
        if ('\u{80}'..='\u{9F}').contains(&ch) {
            self.c1_controls += 1;
        }
    }
}

/// 文字列から見えない文字を除く。除いた数を `count` に足す。
///
/// 何も除かなければ元の文字列をそのまま返す。文脈 (前後の文字) は、この文字列の中だけで見る。
pub(crate) fn remove_invisible<'a>(text: &'a str, count: &mut InvisibleCount) -> Cow<'a, str> {
    if !text.chars().any(is_candidate) {
        return Cow::Borrowed(text);
    }

    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut removed_any = false;
    let mut i = 0;
    while i < chars.len() {
        if let Some(end) = RGI_SEQUENCES.match_end(&chars, i) {
            out.extend(&chars[i..end]);
            i = end;
            continue;
        }
        let ch = chars[i];
        if is_candidate(ch) && !kept_in_context(&chars, i) {
            count.record(ch);
            removed_any = true;
        } else {
            out.push(ch);
        }
        i += 1;
    }

    if removed_any {
        Cow::Owned(out)
    } else {
        Cow::Borrowed(text)
    }
}

/// 除く候補の文字か (文脈を見る前の判定)。
fn is_candidate(ch: char) -> bool {
    if ch.is_ascii() {
        return ch.is_ascii_control() && !matches!(ch, '\t' | '\n' | '\r');
    }
    ch.is_control() || DEFAULT_IGNORABLE.contains(ch) || ('\u{FFF9}'..='\u{FFFB}').contains(&ch)
}

/// 除く候補の文字を、前後の文字の文脈で残すか。
///
/// RGI の絵文字の並びの中の文字は、ここに来る前に並びごと残している。
fn kept_in_context(chars: &[char], i: usize) -> bool {
    let ch = chars[i];
    let prev = i.checked_sub(1).map(|p| chars[p]);
    match ch {
        '\u{FE0E}' | VS16 => prev.is_some_and(is_emoji_variation_base),
        '\u{FE00}'..='\u{FE0D}' | '\u{180B}'..='\u{180D}' | '\u{180F}' => {
            prev.is_some_and(|base| is_standardized_variation(base, ch))
        }
        '\u{E0100}'..='\u{E01EF}' => prev.is_some_and(|base| UNIFIED_IDEOGRAPH.contains(base)),
        ZWNJ | ZWJ => joiner_has_effect(chars, i),
        _ => false,
    }
}

fn is_emoji_variation_base(ch: char) -> bool {
    tables::EMOJI_VARIATION_BASES.binary_search(&ch).is_ok()
}

fn is_standardized_variation(base: char, selector: char) -> bool {
    tables::STANDARDIZED_VARIATION_SEQUENCES
        .binary_search(&(base, selector))
        .is_ok()
}

/// ZWNJ・ZWJ が綴りや字形に効く文脈か。
///
/// UAX #31 (2.3 Layout and Format Control Characters) の文脈の規則にならう。
/// - 前後のどちらかが virama (正準結合クラス 9): インド系の文字の半字形・結合の抑止
///   (クシャ क्‍ष・シンハラ文字 ශ්‍රී・ベンガル文字の ra + ZWJ + virama + ya など)
/// - 続け書きの文脈 (Joining_Type。透過 T の文字は飛ばして見る): ZWNJ は前が左につながる
///   文字 (D か L) で後ろが右につながる文字 (D か R) のとき。ZWJ はどちらかの側が
///   つながる文字のとき (字形を指定して見せる用法)
fn joiner_has_effect(chars: &[char], i: usize) -> bool {
    let prev = i.checked_sub(1).map(|p| chars[p]);
    let next = chars.get(i + 1).copied();
    if prev.is_some_and(is_virama) || next.is_some_and(is_virama) {
        return true;
    }

    let before = chars[..i]
        .iter()
        .rev()
        .map(|&c| JOINING_TYPE.get(c))
        .find(|&jt| jt != JoiningType::Transparent);
    let after = chars[i + 1..]
        .iter()
        .map(|&c| JOINING_TYPE.get(c))
        .find(|&jt| jt != JoiningType::Transparent);
    let joins_forward = matches!(
        before,
        Some(JoiningType::DualJoining | JoiningType::LeftJoining)
    );
    let joins_backward = matches!(
        after,
        Some(JoiningType::DualJoining | JoiningType::RightJoining)
    );
    if chars[i] == ZWNJ {
        joins_forward && joins_backward
    } else {
        joins_forward || joins_backward
    }
}

fn is_virama(ch: char) -> bool {
    COMBINING_CLASS.get(ch) == CanonicalCombiningClass::Virama
}

/// RGI の絵文字の ZWJ の並びとタグの並びを引く木。
///
/// 辺は U+FE0F を除いた並びの文字で張り、完全な形の並びで U+FE0F が続く位置の節に
/// `optional_vs16` を立てる。入力のその位置に U+FE0F があれば並びの一部として読み、
/// なければ (U+FE0F を省いた形) 飛ばして続ける。
struct SequenceTrie {
    nodes: Vec<TrieNode>,
}

#[derive(Default)]
struct TrieNode {
    /// (文字, 子の節の番号)。文字の昇順
    children: Vec<(char, usize)>,
    /// ここで並びが終わる
    terminal: bool,
    /// 完全な形の並びでは、この節の文字の後に U+FE0F が来る
    optional_vs16: bool,
}

static RGI_SEQUENCES: LazyLock<SequenceTrie> = LazyLock::new(|| {
    SequenceTrie::build(
        tables::RGI_EMOJI_ZWJ_SEQUENCES
            .iter()
            .chain(tables::RGI_EMOJI_TAG_SEQUENCES),
    )
});

impl SequenceTrie {
    fn build<'a>(sequences: impl Iterator<Item = &'a &'static [char]>) -> Self {
        let mut trie = SequenceTrie {
            nodes: vec![TrieNode::default()],
        };
        for sequence in sequences {
            let mut node = 0;
            for &ch in sequence.iter() {
                if ch == VS16 {
                    trie.nodes[node].optional_vs16 = true;
                    continue;
                }
                node = match trie.child(node, ch) {
                    Some(child) => child,
                    None => trie.add_child(node, ch),
                };
            }
            trie.nodes[node].terminal = true;
        }
        trie
    }

    fn child(&self, node: usize, ch: char) -> Option<usize> {
        let children = &self.nodes[node].children;
        children
            .binary_search_by_key(&ch, |&(c, _)| c)
            .ok()
            .map(|index| children[index].1)
    }

    fn add_child(&mut self, node: usize, ch: char) -> usize {
        let child = self.nodes.len();
        self.nodes.push(TrieNode::default());
        let children = &mut self.nodes[node].children;
        let index = children.partition_point(|&(c, _)| c < ch);
        children.insert(index, (ch, child));
        child
    }

    /// `start` から始まる最も長い並びの終わり (排他的な位置) を返す。
    fn match_end(&self, chars: &[char], start: usize) -> Option<usize> {
        let mut node = 0;
        let mut j = start;
        let mut end = None;
        while let Some(&ch) = chars.get(j) {
            let Some(child) = self.child(node, ch) else {
                break;
            };
            node = child;
            j += 1;
            if self.nodes[node].optional_vs16 && chars.get(j) == Some(&VS16) {
                j += 1;
            }
            if self.nodes[node].terminal {
                end = Some(j);
            }
        }
        end
    }
}

/// htmd が組み立てた DOM のうち、Markdown に出る文字列から見えない文字を除く。
///
/// テキストノード (`skip_tags` の要素の中は htmd が捨てるので除く) と、Markdown に出る属性
/// (リンクの href・title、画像の src・alt・title、コードの言語を決める pre・code の class) を
/// 対象にする。htmd は Markdown の記号 (`#`・`-`・`>`・`~` など) をテキストノードの先頭の
/// 文字だけで escape するかを決めるため、Markdown にしてから除くと、先頭のゼロ幅スペースの
/// 後ろの `# ` や `~~~` が見出しやコードフェンスに化ける。Markdown にする前に木の段で除けば、
/// htmd は除いた後の文字列で escape を決める。文脈は 1 つのテキストノード・属性の中だけで見る。
pub(crate) fn remove_invisible_from_tree(
    root: &Handle,
    skip_tags: &[&str],
    count: &mut InvisibleCount,
) {
    let mut stack = vec![Rc::clone(root)];
    while let Some(node) = stack.pop() {
        match &node.data {
            NodeData::Text { contents } => {
                let cleaned = match remove_invisible(&contents.borrow(), count) {
                    Cow::Owned(cleaned) => Some(cleaned),
                    Cow::Borrowed(_) => None,
                };
                if let Some(cleaned) = cleaned {
                    *contents.borrow_mut() = cleaned.into();
                }
            }
            NodeData::Element { name, attrs, .. } => {
                let tag: &str = &name.local;
                if skip_tags.contains(&tag) {
                    continue;
                }
                for attr in attrs.borrow_mut().iter_mut() {
                    if !is_markdown_attribute(tag, &attr.name.local) {
                        continue;
                    }
                    if let Cow::Owned(cleaned) = remove_invisible(&attr.value, count) {
                        attr.value = cleaned.into();
                    }
                }
            }
            _ => {}
        }
        stack.extend(node.children.borrow().iter().rev().cloned());
    }
}

/// htmd が Markdown に書き出す属性か。
fn is_markdown_attribute(tag: &str, attr: &str) -> bool {
    match tag {
        "a" => matches!(attr, "href" | "title"),
        "img" => matches!(attr, "src" | "href" | "alt" | "title"),
        "pre" | "code" => attr == "class",
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(text: &str) -> (String, InvisibleCount) {
        let mut count = InvisibleCount::default();
        let cleaned = remove_invisible(text, &mut count).into_owned();
        (cleaned, count)
    }

    fn cleaned(text: &str) -> String {
        clean(text).0
    }

    fn removed(text: &str) -> usize {
        clean(text).1.removed
    }

    #[test]
    fn table_data_is_unicode_17() {
        assert_eq!(tables::UNICODE_DATA_VERSION, "17.0.0");
        // icu_properties のデータも Unicode 17.0 であること
        // (CJK 統合漢字拡張 J の U+323B0 は Unicode 17.0 で追加された)
        assert!(UNIFIED_IDEOGRAPH.contains('\u{323B0}'));
    }

    #[test]
    fn plain_text_is_borrowed_unchanged() {
        let mut count = InvisibleCount::default();
        let text = "Plain text, 日本語、emoji 😀 and\ttab\r\nnewline";
        assert!(matches!(
            remove_invisible(text, &mut count),
            Cow::Borrowed(borrowed) if borrowed == text
        ));
        assert_eq!(count, InvisibleCount::default());
    }

    #[test]
    fn removes_c0_controls_but_keeps_tab_lf_and_cr() {
        assert_eq!(
            cleaned("a\u{0}b\u{1}c\u{8}d\u{b}e\u{c}f\u{1b}g\u{1f}h"),
            "abcdefgh"
        );
        assert_eq!(cleaned("a\tb\nc\r\nd"), "a\tb\nc\r\nd");
    }

    #[test]
    fn removes_delete() {
        assert_eq!(cleaned("a\u{7f}b"), "ab");
    }

    #[test]
    fn removes_c1_controls_and_counts_them_within_the_total() {
        let (text, count) = clean("a\u{80}b\u{85}c\u{9f}d\u{200b}e");
        assert_eq!(text, "abcde");
        // C1 は総数の内数 (二重に数えない)
        assert_eq!(
            count,
            InvisibleCount {
                removed: 4,
                c1_controls: 3
            }
        );
    }

    #[test]
    fn removes_zero_width_and_bidi_and_other_default_ignorables() {
        let samples = [
            '\u{00AD}',  // SOFT HYPHEN
            '\u{034F}',  // COMBINING GRAPHEME JOINER
            '\u{061C}',  // ARABIC LETTER MARK
            '\u{115F}',  // HANGUL CHOSEONG FILLER
            '\u{1160}',  // HANGUL JUNGSEONG FILLER
            '\u{17B4}',  // KHMER VOWEL INHERENT AQ
            '\u{180E}',  // MONGOLIAN VOWEL SEPARATOR
            '\u{200B}',  // ZERO WIDTH SPACE
            '\u{200E}',  // LEFT-TO-RIGHT MARK
            '\u{200F}',  // RIGHT-TO-LEFT MARK
            '\u{202A}',  // LEFT-TO-RIGHT EMBEDDING
            '\u{202B}',  // RIGHT-TO-LEFT EMBEDDING
            '\u{202C}',  // POP DIRECTIONAL FORMATTING
            '\u{202D}',  // LEFT-TO-RIGHT OVERRIDE
            '\u{202E}',  // RIGHT-TO-LEFT OVERRIDE
            '\u{2060}',  // WORD JOINER
            '\u{2061}',  // FUNCTION APPLICATION
            '\u{2062}',  // INVISIBLE TIMES
            '\u{2063}',  // INVISIBLE SEPARATOR
            '\u{2064}',  // INVISIBLE PLUS
            '\u{2065}',  // 未割り当てだが DICP
            '\u{2066}',  // LEFT-TO-RIGHT ISOLATE
            '\u{2067}',  // RIGHT-TO-LEFT ISOLATE
            '\u{2068}',  // FIRST STRONG ISOLATE
            '\u{2069}',  // POP DIRECTIONAL ISOLATE
            '\u{206A}',  // INHIBIT SYMMETRIC SWAPPING
            '\u{206F}',  // NOMINAL DIGIT SHAPES
            '\u{3164}',  // HANGUL FILLER
            '\u{FEFF}',  // ZERO WIDTH NO-BREAK SPACE (BOM)
            '\u{FFA0}',  // HALFWIDTH HANGUL FILLER
            '\u{1BCA0}', // SHORTHAND FORMAT LETTER OVERLAP
            '\u{1D173}', // MUSICAL SYMBOL BEGIN BEAM
            '\u{E0001}', // LANGUAGE TAG
            '\u{E0020}', // TAG SPACE
            '\u{E0080}', // 未割り当てだが DICP
        ];
        for ch in samples {
            let text = format!("a{ch}b");
            assert_eq!(cleaned(&text), "ab", "U+{:04X}", u32::from(ch));
            assert_eq!(removed(&text), 1, "U+{:04X}", u32::from(ch));
        }
    }

    #[test]
    fn removes_interlinear_annotation_characters() {
        assert_eq!(cleaned("\u{FFF9}base\u{FFFA}ruby\u{FFFB}"), "baseruby");
    }

    #[test]
    fn keeps_visible_spaces() {
        let text = "a\u{A0}b\u{2009}c\u{200A}d\u{202F}e\u{3000}f\u{2028}g\u{2029}h";
        assert_eq!(cleaned(text), text);
    }

    #[test]
    fn keeps_private_use_and_unassigned_characters() {
        let text = "a\u{E000}b\u{F8FF}c\u{F0000}d\u{0378}e\u{FFFE}f\u{FDD0}g";
        assert_eq!(cleaned(text), text);
    }

    #[test]
    fn keeps_visible_format_characters_outside_default_ignorable() {
        // 数字の前に付けて表示するアラビア文字の記号 (Prepended_Concatenation_Mark) と、
        // エジプト聖刻文字の配置の制御は Cf でも DICP ではない
        let text = "\u{0600}123 \u{06DD}4 \u{13430}";
        assert_eq!(cleaned(text), text);
    }

    #[test]
    fn keeps_rgi_zwj_sequences() {
        for emoji in [
            "👨\u{200D}👩\u{200D}👧",           // family: man, woman, girl
            "👨\u{200D}👩\u{200D}👧\u{200D}👦", // family: man, woman, girl, boy
            "🏳\u{FE0F}\u{200D}🌈",              // rainbow flag
            "❤\u{FE0F}\u{200D}🔥",              // heart on fire
            "🧑🏽\u{200D}💻",                     // technologist: medium skin tone
            "👁\u{FE0F}\u{200D}🗨\u{FE0F}",       // eye in speech bubble
            "🏃\u{200D}➡\u{FE0F}",              // person running facing right
        ] {
            let text = format!("[{emoji}]");
            assert_eq!(cleaned(&text), text, "{emoji:?}");
        }
    }

    #[test]
    fn keeps_rgi_zwj_sequences_without_optional_vs16() {
        // U+FE0F を省いた形 (minimally-qualified) も同じ並びとして残す
        let text = "🏳\u{200D}🌈 👁\u{200D}🗨";
        assert_eq!(cleaned(text), text);
    }

    #[test]
    fn removes_zwj_outside_rgi_sequences() {
        assert_eq!(cleaned("A\u{200D}B"), "AB");
        assert_eq!(cleaned("日\u{200D}本"), "日本");
        assert_eq!(cleaned("😀\u{200D}😀"), "😀😀");
        // 並びの一部だけ一致しても、ZWJ は残さない
        assert_eq!(cleaned("👨\u{200D}x"), "👨x");
        // 5 人家族は RGI にないので、最長一致の 4 人家族の後ろの ZWJ は除く
        assert_eq!(
            cleaned("👨\u{200D}👩\u{200D}👧\u{200D}👦\u{200D}👦"),
            "👨\u{200D}👩\u{200D}👧\u{200D}👦👦"
        );
    }

    #[test]
    fn keeps_rgi_tag_sequences() {
        let scotland = "🏴\u{E0067}\u{E0062}\u{E0073}\u{E0063}\u{E0074}\u{E007F}";
        let text = format!("flag {scotland}!");
        assert_eq!(cleaned(&text), text);
    }

    #[test]
    fn removes_tag_characters_used_to_smuggle_ascii() {
        // "hi" をタグ文字で書いたもの (ASCII の密輸)
        let text = "visible\u{E0068}\u{E0069}\u{E007F} text";
        assert_eq!(cleaned(text), "visible text");
        assert_eq!(removed(text), 3);
        // 黒い旗の後でも、RGI の旗でなければ除く
        assert_eq!(cleaned("🏴\u{E0078}\u{E0079}\u{E007A}\u{E007F}"), "🏴");
    }

    #[test]
    fn keeps_emoji_presentation_selector_after_its_base() {
        let text = "❤\u{FE0F} ☺\u{FE0E} #\u{FE0F}\u{20E3} ©\u{FE0F}";
        assert_eq!(cleaned(text), text);
    }

    #[test]
    fn removes_isolated_and_repeated_variation_selectors() {
        // 基底の文字がない
        assert_eq!(cleaned("\u{FE0F}start"), "start");
        assert_eq!(cleaned("a \u{FE0F}"), "a ");
        // 異体字の並びが定義されていない文字の後
        assert_eq!(cleaned("x\u{FE0F}y"), "xy");
        assert_eq!(cleaned("😀\u{FE0F}"), "😀");
        // 2 つ目からは除く (異体字セレクタにデータを埋め込む手口)
        assert_eq!(cleaned("❤\u{FE0F}\u{FE0F}\u{FE0E}"), "❤\u{FE0F}");
        assert_eq!(removed("❤\u{FE0F}\u{FE0F}\u{FE0E}"), 2);
        assert_eq!(cleaned("a\u{FE00}\u{FE01}\u{FE02}b"), "ab");
    }

    #[test]
    fn keeps_standardized_variation_sequences() {
        // DIGIT ZERO + VS1 (short diagonal stroke form)
        assert_eq!(cleaned("0\u{FE00}"), "0\u{FE00}");
        // CJK 互換漢字の標準の異体字 (U+8C48 + VS1)
        assert_eq!(cleaned("\u{8C48}\u{FE00}"), "\u{8C48}\u{FE00}");
        // モンゴル文字の自由異体字セレクタ (U+1820 + FVS1)
        assert_eq!(cleaned("\u{1820}\u{180B}"), "\u{1820}\u{180B}");
        // 組が定義されていなければ除く
        assert_eq!(cleaned("1\u{FE00}"), "1");
        assert_eq!(cleaned("A\u{180B}"), "A");
        assert_eq!(cleaned("\u{1820}\u{180F}"), "\u{1820}");
    }

    #[test]
    fn keeps_ideographic_variation_selector_after_unified_ideograph() {
        // 葛 + VS17 (IVS)
        assert_eq!(cleaned("\u{845B}\u{E0100}城"), "\u{845B}\u{E0100}城");
        // 2 つ目は除く
        assert_eq!(cleaned("\u{845B}\u{E0100}\u{E0101}"), "\u{845B}\u{E0100}");
        // 漢字でない文字の後の IVS は除く
        assert_eq!(cleaned("A\u{E0100}"), "A");
        assert_eq!(cleaned("あ\u{E0100}"), "あ");
    }

    #[test]
    fn keeps_zwnj_inside_persian_words() {
        // می‌خواهم (ZWNJ で「می」と動詞を分ける)
        let word = "\u{0645}\u{06CC}\u{200C}\u{062E}\u{0648}\u{0627}\u{0647}\u{0645}";
        assert_eq!(cleaned(word), word);
        // 母音記号 (透過) を挟んでも続け書きの文脈とみなす
        let with_mark = "\u{0628}\u{0650}\u{200C}\u{0647}";
        assert_eq!(cleaned(with_mark), with_mark);
    }

    #[test]
    fn removes_zwnj_where_it_has_no_effect() {
        // ラテン文字の間 (合字の抑止は対象外)
        assert_eq!(cleaned("Auf\u{200C}lage"), "Auflage");
        // 右にしかつながらない文字の後では続け書きに効かない
        assert_eq!(cleaned("\u{0632}\u{200C}\u{0647}"), "\u{0632}\u{0647}");
        // 語の外 (空白の隣)
        assert_eq!(cleaned("\u{0645}\u{06CC} \u{200C}x"), "\u{0645}\u{06CC} x");
        // 2 つ続くものは、どちらも隣が続け書きの文字にならない
        assert_eq!(
            cleaned("\u{0645}\u{06CC}\u{200C}\u{200C}\u{062E}"),
            "\u{0645}\u{06CC}\u{062E}"
        );
    }

    #[test]
    fn keeps_joiners_next_to_virama_in_indic_scripts() {
        // デーヴァナーガリーの半字形 (क्‍ष) と結合の抑止 (क्‌ष)
        let half_form = "\u{0915}\u{094D}\u{200D}\u{0937}";
        let explicit_virama = "\u{0915}\u{094D}\u{200C}\u{0937}";
        // シンハラ文字の ශ්‍රී
        let sri = "\u{0DC1}\u{0DCA}\u{200D}\u{0DBB}\u{0DD3}";
        // ベンガル文字の ra + ZWJ + virama + ya (র‍্য)
        let ra_ya = "\u{09B0}\u{200D}\u{09CD}\u{09AF}";
        for text in [half_form, explicit_virama, sri, ra_ya] {
            assert_eq!(cleaned(text), text, "{text:?}");
        }
        // virama の隣でなければ除く
        assert_eq!(cleaned("\u{0915}\u{200D}\u{0937}"), "\u{0915}\u{0937}");
    }

    #[test]
    fn keeps_zwj_that_shows_a_joining_form() {
        // アラビア文字の頭字形・尾字形を単独で見せる用法
        let text = "\u{0639}\u{200D} \u{200D}\u{0639}";
        assert_eq!(cleaned(text), text);
    }

    #[test]
    fn counts_each_removed_scalar_value_once() {
        let (text, count) = clean("\u{200B}\u{200B}a\u{FEFF}\u{80}\u{E0041}");
        assert_eq!(text, "a");
        assert_eq!(
            count,
            InvisibleCount {
                removed: 5,
                c1_controls: 1
            }
        );
    }

    #[test]
    fn removal_accumulates_across_calls() {
        let mut count = InvisibleCount::default();
        let _ = remove_invisible("a\u{200B}", &mut count);
        let _ = remove_invisible("\u{85}b", &mut count);
        assert_eq!(
            count,
            InvisibleCount {
                removed: 2,
                c1_controls: 1
            }
        );
    }

    #[test]
    fn crlf_and_tabs_survive_between_removed_characters() {
        assert_eq!(cleaned("a\u{200B}\r\n\u{200B}\tb"), "a\r\n\tb");
    }

    #[test]
    fn removes_inside_code_fences_too() {
        let markdown = "```\nlet x\u{200B} = 1;\u{202E}\n```";
        assert_eq!(cleaned(markdown), "```\nlet x = 1;\n```");
    }
}
