# 使い方

get-md の全オプションと、README より多い使用例、セレクタとファイル出力の挙動をまとめています。端末では `get-md --help` で同じオプションの一覧を確認できます。

## 書式

```bash
get-md [OPTIONS] <URL>
```

`<URL>` には取得するページを指定します。ローカルのファイルも `file://` の URL で指定できます。

## オプション

| オプション | 短縮形 | 説明 |
|---|---|---|
| `--selector <SEL>` | `-s` | 変換する要素の CSS セレクタ (複数指定可)。省略するとページ全体 (`body`) を変換 |
| `--output <FILE>` | `-o` | 出力先のファイル (既定: 標準出力) |
| `--chrome-path <PATH>` | | Chrome / Chromium の実行ファイルのパス (既定: 自動で検出) |
| `--wait <SECS>` | `-w` | ページの読み込み後、JavaScript の描画を待つ追加の秒数 (既定: 2) |
| `--timeout <SECS>` | `-t` | ページの読み込みのタイムアウト秒数 (既定: 60) |
| `--no-headless` | | ブラウザのウィンドウを表示する (デバッグ用) |
| `--no-cache` | | ブラウザのキャッシュを無効にする (常に最新の内容を取得) |
| `--ignore-certificate-errors` | | HTTPS の証明書エラーを無視する (危険: 信頼できるサイトのデバッグに限る) |
| `--ignore-date` | | ファイルに書き込むとき、日時だけの差分を変更なしとして扱う |
| `--front-matter` | | 取得元の情報を YAML の front matter として出力の先頭に付ける ([front matter](#front-matter) を参照) |
| `--meta <KEY=VALUE>` | | front matter に自分で決めた項目を足す (複数指定可)。`--front-matter` も有効になる |
| `--quiet` | `-q` | 進捗の表示を止める |
| `--help` | `-h` | ヘルプを表示する |
| `--version` | `-V` | バージョンを表示する |

ブラウザ側のアイドルタイムアウトは、`--timeout` と `--wait` に 30 秒を足した値です。静かなページで JavaScript の描画を待つ間も、Chrome DevTools Protocol の接続を保ちます。この足し算は上限で頭打ちになるため、極端に大きな値を指定してもオーバーフローしません。

## 使用例

```bash
# ページ全体を Markdown に変換して標準出力に書き出す
get-md https://example.com

# 見出しだけを抽出する
get-md https://example.com -s "h1"

# 複数の要素を抽出する (結果は指定した順に --- でつながる)
get-md https://example.com -s "h1" -s "p"

# ファイルに保存する
get-md https://example.com -s "h1" -s "p" -o example.md

# 描画の遅いページに合わせて、読み込み後に 5 秒待ち、読み込み自体は 90 秒まで待つ
get-md https://example.com -w 5 -t 90

# Chrome の実行ファイルを指定する (macOS)
get-md https://example.com --chrome-path "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"

# 証明書に問題がある信頼済みのページを取得する (ここでは公開されている自己署名のテストページ)
get-md https://self-signed.badssl.com/ --ignore-certificate-errors

# 日時だけが変わった場合はファイルを書き換えない
get-md https://example.com -o example.md --ignore-date

# 取得元の URL・タイトル・セレクタ・取得した時刻を YAML の front matter に残す
get-md https://example.com -o example.md --front-matter

# front matter に自分で決めた項目を足す (--meta だけでも front matter が付く)
get-md https://example.com -o example.md --meta topic=example --meta "note=参考に保存"

# スクリプトで保存済みのページを取り直す (ページが本当に変わったときだけファイルが変わる)
get-md https://example.com -o example.md --front-matter --ignore-date --meta topic=example

# 進捗を表示せずに実行する
get-md https://example.com -q -o example.md
```

## セレクタ

- `-s` を省略すると、ページ全体 (`body`) を変換します
- `-s` を複数指定すると、指定した順に結果を並べ、間に `---` の行を挟みます
- 何にも一致しないセレクタがあると、警告を表示します。どのセレクタも一致しなかった場合は、エラーで終了します
- 不正な CSS セレクタは「一致なし」ではなく、エラーとして報告します

## 取得

- **HTTP エラー**: HTTP のステータスを Chrome DevTools Protocol のネットワークイベントから読み、404 などのエラー応答を拒否します。ページのスクリプトがブラウザの Performance API を書き換えても、判定は変わりません
- **証明書**: HTTPS の証明書は既定で検証します。`--ignore-certificate-errors` は、デバッグ中の信頼できるサイトにだけ使ってください
- **キャッシュ**: `--no-cache` を付けるとブラウザのキャッシュを使わず、毎回最新の内容を取得します

## ファイルへの出力

`-o` を付けると、Markdown をファイルに書き込み、そのファイルがどうなったかを表示します。

```text
✨ https://example.com → example.md (created)
```

| 印 | 状態 | 意味 |
|---|---|---|
| ✨ | created | ファイルが無かったので新しく作った |
| 📝 | updated | 内容が変わった |
| ✔ | unchanged | 内容が前と同じ |

- **Git を考慮した判定**: 出力先が Git で追跡されていて、ステージしていない変更がある場合は `updated` と表示します。判定は出力先を含むリポジトリで行い、パスはそのまま照合します (名前に含まれる `[` や `*` などの glob の文字は展開しません)。そのため、削除した追跡ファイルに書き出す場合や、そのリポジトリの外で get-md を実行した場合も正しく判定できます。既存のファイルを読み取れない場合は `updated` として扱います
- **`--ignore-date`**: 日時の文字列だけが変わった場合は、書き換えを省きます。小数秒やタイムゾーン付きの一般的な ISO 8601 の形式も対象です。古い内容と新しい内容の両方に日時が含まれる場合だけ比較し、UTF-8 でないファイルは通常の比較に戻します
- **アトミックな書き込み**: 出力は同じディレクトリの一時ファイルに書いてから rename で置き換えるため、書き込みの途中で I/O エラー (ディスクの容量不足など) が起きても、既存のファイルが切り詰められたり壊れたりしません。既存のパーミッションは内容を書く前に適用し、書き込み権限は先に確かめます。出力先がシンボリックリンクなら実体に書き込み、リンクはそのまま残します。リンク先がまだ無く、その親ディレクトリも無い場合は、親ディレクトリから作ります。rename は inode を置き換えるので、ハードリンクは切れ、ACL と拡張属性は引き継がれません (クラッシュへの強さを優先した仕様です)
- **進捗**: 進捗は標準エラー出力に表示するので、標準出力に書き出す Markdown には混ざりません。完了の表示は、出力の書き込みと flush に成功してから出します
- **front matter**: `--front-matter` を付けると、取得した時刻を比較から外します。詳しくは[既存のファイルとの比較](#既存のファイルとの比較)を参照してください

## front matter

`--front-matter` を付けると、出力の先頭に YAML の front matter を付けます。ファイルだけでなく標準出力にも付くので、保存した写しに取得元の記録が残ります。`--meta KEY=VALUE` は自分で決めた項目を足し、これだけでも front matter が付きます。

```bash
get-md https://example.com -o example.md --meta topic=example
```

```markdown
---
url: "https://example.com"
title: "Example Domain"
selectors: ["body"]
retrieved_at: "2026-09-29T03:10:00Z"
topic: "example"
---

# Example Domain
```

### 項目

項目はいつも次の順に並びます。

| 項目 | 値 | 書く条件 |
|---|---|---|
| `url` | 引数に渡した URL (そのまま) | いつも |
| `final_url` | リダイレクトなどを経て読み込んだ後のページの URL | `url` と違うときだけ。`https://example.com` と `https://example.com/` の末尾の `/` のように、URL の正規化で消える違いは数えない |
| `title` | ページの `document.title` から前後の空白を削ったもの | ページに空でないタイトルがあるときだけ |
| `selectors` | 使ったセレクタの一覧。`-s` を省くと `["body"]` | いつも |
| `retrieved_at` | ページを読んだ時刻。UTC の RFC 3339 で、秒まで | いつも |
| `--meta` の項目 | 指定した値。指定した順に並ぶ | `--meta` を指定したとき |

get-md の版は書きません。get-md を上げるたびに、保存したすべてのファイルが書き換わるのを避けるためです。

### 書式

- `---` の行、項目ごとの `key: value` の行、閉じの `---` の行、空行の順に書き、その後に Markdown が続きます。閉じの `---` の後にはいつも空行があるので、本文が `---` で始まっても区別できます
- 値はすべて YAML のダブルクォートの文字列で、`selectors` はダブルクォートの文字列を並べたフロー形式の列 (`["article", "main"]` など) です。そのため `true` や `2026-09-29`、`a: b` のような値も文字列として読まれます
- クォートの中では、`"` と `\` をエスケープし、改行・復帰・タブを `\n`・`\r`・`\t` に、そのほかの制御文字と U+2028・U+2029・U+FEFF・U+FFFE・U+FFFF を `\uXXXX` にします。どの項目も 1 行に収まり、YAML のパーサで読むと元の文字列に戻ります

### `--meta` の決まり

- 引数は最初の `=` で分けます。`--meta query=a=b` なら、`query` の値は `a=b` です。値は空でもかまいません (`--meta note=`)
- キーは英字か `_` で始め、英数字・`_`・`-` だけで書きます (`^[A-Za-z_][A-Za-z0-9_-]*$`)
- 次の場合はブラウザを起動する前にエラーで終了します: `=` がない、キーが空、キーの書式が違う、同じキーを 2 回以上指定した、組み込みの項目 (`url`・`final_url`・`title`・`selectors`・`retrieved_at`) と同じキー、YAML が文字列ではなく真偽値や null として読む語 (`true`・`false`・`null`・`yes`・`no`・`on`・`off`・`y`・`n`。大文字小文字を問わない)

### 既存のファイルとの比較

`-o` の出力先に get-md の front matter 付きのファイルがすでにあるときは、新しい出力と次のように比べます。

- `retrieved_at` の値は、`--ignore-date` の有無にかかわらず、いつも比較から外します。ほかの項目と本文が同じなら、ファイルを書き換えずに `unchanged` と表示します (Git でステージしていない変更があれば、これまでどおり `updated`)。変わっていないページを取り直しても、Git の差分は出ません
- `--ignore-date` も付けたときでも、front matter (`retrieved_at` を除く) は文字列どおりに比べ、日時の差分を無視する比較は本文にだけ当てます。タイトル・URL・セレクタ・`--meta` の値の変化は必ず書き込みます
- 既存のファイルに front matter がない場合 (初回や、ヘッダの形式が違う場合) は、これまでどおりファイル全体を比べるので、front matter を付けて書き直します
- `retrieved_at` は、ファイルに保存している本文を取得した時刻を表します。ファイルを書き換えなかったときは、前の値が残ります
- 既存のファイルの front matter にしかない項目は引き継ぎません。出力は引数だけで決まります。残したい値は、毎回 `--meta` で渡してください

`--front-matter` も `--meta` も付けないときの出力と比較は、これまでと変わりません。
