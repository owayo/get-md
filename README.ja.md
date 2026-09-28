<h1 align="center">get-md</h1>

<p align="center">
  インストール済みの Chrome や Chromium で Web ページを描画し、ページ全体か CSS セレクタで選んだ要素を Markdown に変換する CLI
</p>

<!-- standard:badges:start -->
<h3 align="center">対応プラットフォーム</h3>

<p align="center">
  <img src="https://img.shields.io/badge/macOS-000000?logo=apple&amp;logoColor=white" alt="macOS">
  <img src="https://img.shields.io/badge/Windows-0078D6" alt="Windows">
</p>

<p align="center">
  <a href="https://github.com/owayo/get-md/actions/workflows/ci.yml"><img src="https://github.com/owayo/get-md/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI"></a>
  <a href="https://github.com/owayo/get-md/releases/latest"><img src="https://img.shields.io/github/v/release/owayo/get-md" alt="Release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/owayo/get-md" alt="License"></a>
</p>

<p align="center">
  <a href="README.md">English</a> |
  <a href="README.ja.md">日本語</a>
</p>
<!-- standard:badges:end -->

---

get-md は、手元にある Chrome か Chromium で URL を開き、JavaScript の描画が終わるのを待ってから、その内容を Markdown で書き出します。素の HTTP で取得すると中身が空になるシングルページアプリケーションでも、描画後の本文を取り出せます。WebDriver や ChromeDriver の準備は要りません。

ページ全体を変換するほか、CSS セレクタで必要な部分だけを選べます。相対リンクは絶対 URL に直し、ファイルへの出力はアトミックに書き込むので、Web ページの Markdown の写しを更新し続けるスクリプトにも組み込めます。

> [!IMPORTANT]
> get-md は、見えなくされた内容と見えない文字を既定で出力から除くようになりました。そのため、以前の版とは出力が変わることがあります。以前と同じ出力にするには `--keep-hidden --keep-invisible` を付けてください。詳しくは[見えなくされた内容と見えない文字](docs/usage.ja.md#見えなくされた内容と見えない文字)を参照してください。

## 機能

- **JavaScript の描画に対応**: インストール済みの Chrome / Chromium を Chrome DevTools Protocol で操作し、シングルページアプリケーションや動的なコンテンツも描画を待ってから変換します。追加の待ち時間は `-w` で指定し、ブラウザ接続のアイドルタイムアウトにも算入します
- **WebDriver が不要**: ChromeDriver や Selenium を用意する必要はありません。Chrome は自動で見つけ、`--chrome-path` で場所を指定することもできます
- **CSS セレクタで抽出**: 必要な要素だけを変換します。`-s` を繰り返すと、複数のセレクタの結果を指定した順に `---` でつなぎます。不正なセレクタは「一致なし」として扱わず、エラーで終了します
- **余計なものを除いた Markdown**: script・style・`noscript`・SVG を取り除き、セル内のエスケープ済みパイプを保ちながら Markdown の表の余分な空白を詰めます
- **見えなくされた内容を除く**: `display: none`・`opacity: 0`・`visibility: hidden`・閉じた `<details>` の中身・支援技術向けに画面から隠したテキスト (visually-hidden)・ページの左や上の外へ追い出した要素など、描画後のページで見えない要素とテキストを出力しません。判定は取得した時点の表示を基準にします。人が画面で見る内容と LLM が読む内容をそろえ、人には見えない指示 (プロンプトインジェクション) を渡さないためです。`--keep-hidden` で残せます
- **見えない文字を除く**: ゼロ幅の文字・BOM・双方向の制御文字・タグ文字・制御文字を、コードブロック・リンク先・画像の alt と title・front matter の title を含む Markdown 全体から除きます。絵文字の ZWJ の並び・旗・正しい異体字セレクタは残します。`--keep-invisible` で残せます
- **絶対 URL への変換**: 相対リンクと画像を、描画後の文書の基準 URL (`<base href>` を含む) で絶対 URL に直します。インラインコードとコードブロックの中は書き換えません
- **HTTP エラーの検出**: HTTP のエラー応答を Chrome DevTools Protocol のネットワークイベントで検出して拒否します。ページのスクリプトがブラウザの Performance API を書き換えても、判定は変わりません
- **証明書を既定で検証**: HTTPS の証明書を検証します。信頼できるサイトのデバッグに限り、`--ignore-certificate-errors` で明示的に無視できます
- **安全なファイル出力**: 一時ファイルに書いてからアトミックな rename で置き換え、既存のパーミッションとシンボリックリンクを保ちます。ファイルを新しく作ったか、更新したか、変更しなかったかも表示します
- **日時だけの変更を無視**: `--ignore-date` を付けると、日時だけが変わった場合はファイルを書き換えません
- **取得元を front matter に記録**: `--front-matter` を付けると、取得元の URL・ページのタイトル・セレクタ・取得した時刻を YAML の front matter として先頭に付けます。`--meta KEY=VALUE` で自分で決めた項目も足せます。既存のファイルとは取得した時刻を除いて比べるので、変わっていないページを取り直してもファイルは書き換わりません
- **進捗の表示**: 取得と変換の各段階を表示します。`-q` で表示を止められます。標準出力への書き込みは flush に成功してから完了を表示します

リンク・コードブロック・表を具体的にどう扱うかは [docs/markdown-conversion.ja.md](docs/markdown-conversion.ja.md) にまとめています。

## 動作環境

- **Chrome または Chromium**: システムにインストールされていること。get-md は実行のたびにヘッドレスモードで起動します

## インストール

<!-- standard:install:start -->
### Homebrew (macOS)

```bash
brew install owayo/get-md/get-md
```

### Cargo

Rust 1.98 以上が必要です。

```bash
cargo install --git https://github.com/owayo/get-md --locked
```

### GitHub Releases から

[Releases](https://github.com/owayo/get-md/releases/latest) から自分の環境のアーカイブを取得して展開し、`get-md` を `PATH` の通った場所に置きます。各リリースには、取得したファイルを確かめるための `SHA256SUMS` も添付しています。

| プラットフォーム | ファイル |
|---|---|
| macOS (Intel) | `get-md-x86_64-apple-darwin.tar.gz` |
| macOS (Apple Silicon) | `get-md-aarch64-apple-darwin.tar.gz` |
| Windows (x86_64) | `get-md-x86_64-pc-windows-msvc.zip` |

macOS でブラウザから取得した場合は、実行の前に隔離属性を外します: `xattr -d com.apple.quarantine get-md`。

### ソースから

[mise](https://mise.jdx.dev/) が必要です (Rust のツールチェーンは `mise.toml` で固定しています)。

```bash
git clone https://github.com/owayo/get-md.git
cd get-md
make install
```

`make install` は `/usr/local/bin` に入れます。場所を変えるときは `INSTALL_PATH` を指定します (例: `make install INSTALL_PATH="$HOME/.local/bin"`)。
<!-- standard:install:end -->

## 使い方

```bash
get-md [OPTIONS] <URL>
```

```bash
# ページ全体を Markdown に変換して標準出力に書き出す
get-md https://example.com

# h1 要素と p 要素だけを変換してファイルに保存する
get-md https://example.com -s "h1" -s "p" -o example.md

# 取得元の情報と自分で決めた項目を YAML の front matter に残して保存する
get-md https://example.com -o example.md --front-matter --meta topic=example

# 見えなくされた内容と見えない文字も残す (以前の版と同じ出力)
get-md https://example.com --keep-hidden --keep-invisible
```

1 つ目のコマンドは次の内容を出力します。

```markdown
# Example Domain

This domain is for use in documentation examples without needing permission. Avoid use in operations.

[Learn more](https://iana.org/domains/example)
```

`--front-matter` を付けると、出力の先頭に取得元の情報が付きます。

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

見えなくされた内容や見えない文字を除いたときは、進捗の表示の最後に、除いた数を 1 行で表示します (`-q` では表示せず、出力には書きません)。何を除いて何を残すか、判定の限界は[見えなくされた内容と見えない文字](docs/usage.ja.md#見えなくされた内容と見えない文字)にまとめています。

全オプションとほかの使用例、セレクタとファイル出力の細かな挙動、front matter の決まりは [docs/usage.ja.md](docs/usage.ja.md) にあります。端末では `get-md --help` でオプションの一覧を確認できます。

## 開発

<!-- standard:dev:start -->
[mise](https://mise.jdx.dev/) が必要です。ツールの版は `mise.toml` で固定しています。

```bash
make setup   # ツールチェーン (mise) と依存を取得する
make ci      # CI と同じ検査 (書き換えない)
```

| コマンド | 説明 |
|---|---|
| `make setup` | ツールチェーン (mise) と依存を取得する |
| `make build` | デバッグ版をビルドする |
| `make release` | リリース版をビルドする |
| `make run` | デバッグ版を実行する (引数は ARGS="...") |
| `make test` | テストを実行する |
| `make lint` | clippy を警告ゼロで通す |
| `make fmt` | コードを整形する (書き換える) |
| `make fmt-check` | 整形済みかを確かめる (書き換えない) |
| `make check` | 整形と静的検査 (書き換えない) |
| `make ci` | CI と同じ検査 (書き換えない) |
| `make install` | リリース版を INSTALL_PATH (既定 /usr/local/bin) に入れる |
| `make uninstall` | INSTALL_PATH から取り除く |
| `make clean` | ビルド成果物を消す |

`make` でターゲットの一覧を表示します。リリースは GitHub Actions で行います (**Actions → Release → Run workflow**)。
<!-- standard:dev:end -->

E2E テストは実際の Chrome / Chromium を操作するため、`make test` ではビルドだけして実行しません。実行方法とテストの範囲は [docs/development.ja.md](docs/development.ja.md) を参照してください。

## ライセンス

<!-- standard:license:start -->
[MIT](LICENSE)
<!-- standard:license:end -->
