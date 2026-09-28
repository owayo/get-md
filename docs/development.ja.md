# 開発

`make` のターゲットは README の開発の節に並べています。このページではテストを説明します。

## ユニットテスト

`make test` はユニットテストを実行し、E2E テストはビルドだけして実行しません。ユニットテストにブラウザは要りません。

ユニットテストでは、標準的な `-h` / `--help` と `-V` / `--version` の CLI の契約も確認します。また、長いコードフェンス、空白を含む URL と title を持つ画像、ヘッダーより列の多い表の行について、HTML を変換してから Markdown を後処理するまでの流れを通して確認します。

front matter については、`--meta` の決まり、特殊な文字ごとのエスケープ、項目の順序、既存のファイルとの比較を確かめます。生成した front matter は YAML のパーサ ([yaml-rust2](https://crates.io/crates/yaml-rust2)。dev-dependency) で読み直し、どの値も元の文字列に戻ることも確かめます。

見えない文字 (`src/invisible.rs`) のテストでは、除く文字と残す文字を種類ごとに確かめます。除く文字は C0・DEL・C1 と Default_Ignorable_Code_Point の代表、残す文字は見える空白・私用領域・未割り当ての文字です。C1 制御文字を総数の内数として数えることも確かめます。文脈で残す例外は、次のものをすべて確かめます。

- RGI の絵文字が作る ZWJ の並び (U+FE0F を省いた形を含む) と、普通の文字に挟まれた ZWJ
- 旗を表すタグの並びと、ASCII を密輸するタグ文字
- 単独の異体字セレクタ、続けて並べた異体字セレクタ、絵文字の後の U+FE0F
- 標準の異体字の並び (モンゴル文字を含む) と、漢字の後の IVS
- ペルシア語の語の中と外の ZWNJ、virama の隣の接合子

`src/main.rs` のテストでは、HTML を変換の全体に通して次を確かめます。

- 文字参照から戻した文字と、リンクや画像の属性に入った見えない文字を除くこと。相対 URL を解決する前に除くこと
- 先頭の文字を除いた後も、htmd が Markdown の記号を escape すること
- コードブロックでも除くこと。`--keep-invisible` では何も除かないこと
- front matter の title から除き、`--meta` の値は渡したとおりに残すこと
- 表示する数の文言

見えなくされた内容を除く処理のうち、ブラウザの要らない部分 (スクリプトに渡す引数と、結果の読み取り) もここで確かめます。

## E2E テスト

`tests/e2e.rs` の E2E テストは実際の Chrome / Chromium を操作するため、`#[ignore]` を付けてあり、`make ci` には含まれません。次のコマンドで実行します。

```bash
make test-e2e
```

E2E テストでは次を確認します。

- GitHub Raw 上の実際のドキュメントの取得
- ローカルの `file://` ページでの相対リンクと画像の URL の解決 (`<base href>` を含む)
- 3 段ネストしたリストの中の相対リンクの解決と、コードフェンスの中のリンクを変換しないこと
- 複数のセレクタを指定したときの `---` 区切りの結合
- 無効な CSS セレクタを明示的なエラーとして拒否すること
- `--ignore-date` を付けたとき、日時の差分だけなら既存のファイルを書き換えず、日時以外の差分は上書きすること
- `--front-matter` と `--meta` で front matter を書くこと。取り直して `retrieved_at` しか変わらないならファイルを書き換えず、`--meta` の値が変わったら書き換えること
- ページのスクリプトが Performance API を偽装しても、実際の HTTP 404 を拒否すること
- 既定で見えなくされた内容を除くこと: `display: none`、`hidden` 属性、`opacity: 0` (子を含む)、`content-visibility: hidden`、`visibility: hidden` の文字、閉じた `<details>` の本文、画面から隠す形の要素 (`clip` と `clip-path`)、ページの左や上の外に出した要素、`font-size: 0`、透明な文字。同時に、`visibility: visible` に戻した子、`display: contents`、開いた `<details>` と閉じた `<details>` の summary、画面に出ている `aria-hidden` の文字、表示領域のずっと下の内容、小さい文字、グラデーションの文字 (`background-clip: text`)、影の付いた透明な文字は残すこと。同じページで、本文と front matter の title から見えない文字を除き、絵文字の ZWJ の並びは残すこと
- `--keep-hidden` と `--keep-invisible` が、それぞれ自分の受け持つ部分だけを元に戻すこと
- ページのスクリプトが `getComputedStyle`・`getBoundingClientRect`・`JSON.stringify`・`Array.from` を書き換えても、見えなくされた内容を除くこと (抽出は isolated world で動く)
- 選んだ要素がどれも見えないときは `--keep-hidden` を案内するエラーで終わり、一部だけのときは警告して続けること

## Unicode のデータ

見えない文字を除く処理は、2 つの Unicode のデータに基づきます。2 つは同じ Unicode の版 (今は 17.0) にそろえます。

- 文字の性質 (General_Category・Default_Ignorable_Code_Point・Unified_Ideograph・Joining_Type・Canonical_Combining_Class) は、[icu_properties](https://crates.io/crates/icu_properties) の組み込みのデータを使います。icu_properties 2.3 のデータは ICU 78 のもので、Unicode 17.0 です
- 文字の性質では表せない並びは、`data/unicode/` に置いた Unicode のデータファイルから作ります。ファイルは unicode.org から手を加えずに写したものです (Unicode License v3 は `data/unicode/LICENSE`)

| ファイル | 出どころ | 使い道 |
|---|---|---|
| `emoji-zwj-sequences.txt` | https://www.unicode.org/Public/17.0.0/emoji/emoji-zwj-sequences.txt | RGI の絵文字の ZWJ の並び |
| `emoji-sequences.txt` | https://www.unicode.org/Public/17.0.0/emoji/emoji-sequences.txt | RGI の絵文字のタグの並び (旗) |
| `emoji-variation-sequences.txt` | https://www.unicode.org/Public/17.0.0/ucd/emoji/emoji-variation-sequences.txt | U+FE0E・U+FE0F を続けてよい文字 |
| `StandardizedVariants.txt` | https://www.unicode.org/Public/17.0.0/ucd/StandardizedVariants.txt | 標準の異体字の並び (モンゴル文字を含む) |

`build.rs` がビルドのときにこれらを読み、`src/invisible.rs` が取り込む表を作ります。各ファイルの先頭にある版の行が `build.rs` の `UNICODE_VERSION`・`EMOJI_VERSION` と合わないときや、想定しない形の行があるときは、ビルドを止めます。

新しい Unicode の版に移る手順 (icu_properties のデータが新しくなったときなど):

1. icu_properties のデータが対応する Unicode の版を確かめる。`icu_properties_data` の README に ICU の版 (`ICU version release-NN`) があり、ICU の各リリースが対応する Unicode の版を公表している
2. 上の URL (パスの `17.0.0` を置き換える) から、その版の 4 つのファイルを `data/unicode/` に手を加えずに置く
3. `build.rs` の `UNICODE_VERSION` と `EMOJI_VERSION`、`src/invisible.rs` の版の確認 (`table_data_is_unicode_17`。名前を変え、新しい版で追加された文字を調べる) を更新する
4. `make ci` を通す。失敗したら生成した表の変化を確かめ、docs/usage.md と docs/usage.ja.md の Unicode の版も直す

`markup5ever_rcdom` は版を固定しています (`=0.38.0`)。get-md は htmd が組み立てた DOM から見えない文字を除くので、htmd と同じ版でなければなりません。htmd を上げるときは、htmd が依存する版にそろえてください。
