<h1 align="center">get-md</h1>

<p align="center">
  Webページを取得し、JSレンダリング後にMarkdownへ変換するCLIツール
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

---

## 特徴

- **JSレンダリング対応** — システムChromeをCDP経由で使用し、SPAや動的コンテンツに対応
- **CSSセレクタによる要素指定** — 必要な要素のみ抽出（複数指定可）
- **WebDriver不要** — インストール済みのChrome/Chromiumを直接制御
- **柔軟な出力** — ファイルまたは標準出力
- **Chrome自動検出** — Chromeを自動検出、またはカスタムパスを指定可能
- **JSレンダリング待機時間の設定** — レンダリング完了までの待機時間を調整可能
- **クリーンな出力** — script、style、SVGを自動除去
- **URL解決** — 相対URLを絶対パスに自動変換
- **Markdownリンク対応強化** — `<...>` 形式（スペースを含むURL）のリンク先解決に対応
- **エスケープ括弧対応** — `\(` `\)` を含むリンク先の閉じ括弧を正しく解釈
- **クォート安全なURL解析** — 通常のMarkdownリンク先でクォート/アポストロフィを壊さず処理
- **エスケープ空白対応** — 通常のMarkdownリンク先で `\ ` をタイトル区切りとして誤認しない
- **テーブル圧縮** — Markdownテーブルの不要なパディングを除去しつつ、コードフェンス内は保持
- **プログレス表示** — quietモード対応、完了表示は出力成功後のみ
- **タイムアウト安全性** — 極端な `--timeout` 値でも内部のアイドルタイムアウト加算でオーバーフローしない

## 動作要件

- **OS**: macOS、Windows
- **Chrome/Chromium**: システムにインストール済みであること
- **Rust**: 1.85以上（ソースからビルドする場合）

## インストール

### Homebrew (macOS)

```bash
brew install owayo/get-md/get-md
```

### GitHubリリースから

[GitHubリリース](https://github.com/owayo/get-md/releases)から最新のバイナリをダウンロードしてください。

| プラットフォーム | アセット |
|----------|-------|
| macOS (Apple Silicon) | `get-md-aarch64-apple-darwin.tar.gz` |
| macOS (Intel) | `get-md-x86_64-apple-darwin.tar.gz` |
| Windows (x64) | `get-md-x86_64-pc-windows-msvc.zip` |

### ソースから

```bash
git clone https://github.com/owayo/get-md.git
cd get-md
cargo install --path .
```

## クイックスタート

```bash
# ページをMarkdownに変換
get-md https://example.com

# 記事コンテンツのみ抽出してファイルに保存
get-md https://example.com -s "article" -o output.md
```

## 使い方

### 基本構文

```bash
get-md [OPTIONS] <URL>
```

### オプション

| オプション | 短縮形 | 説明 |
|-----------|-------|------|
| `--selector <SEL>` | `-s` | CSSセレクタ（複数指定可） |
| `--output <FILE>` | `-o` | 出力先ファイル（デフォルト: 標準出力） |
| `--chrome-path <PATH>` | | Chromeバイナリのパス |
| `--wait <SECS>` | `-w` | ページ読み込み後の待機秒数 [デフォルト: 2] |
| `--timeout <SECS>` | `-t` | ページ読み込みタイムアウト秒数 [デフォルト: 60] |
| `--no-headless` | | ブラウザを表示（デバッグ用） |
| `--no-cache` | | ブラウザキャッシュを無効化（常に最新を取得） |
| `--quiet` | `-q` | プログレス表示を抑止 |
| `--help` | `-h` | ヘルプ表示 |
| `--version` | `-V` | バージョン表示 |

### 使用例

```bash
# ページ全体をMarkdownに変換
get-md https://example.com

# 記事コンテンツのみ抽出
get-md https://example.com -s "article"

# 複数の要素を抽出
get-md https://example.com -s "h1" -s ".content"

# ファイルに保存
get-md https://example.com -s "main" -o output.md

# JSレンダリングが遅いページに対応
get-md https://spa-example.com -s "#app" -w 5 -t 60

# Chromeバイナリを指定
get-md https://example.com --chrome-path /usr/bin/google-chrome

# プログレス表示を抑止して実行
get-md https://example.com -s "article" -q -o output.md
```

## 開発

```bash
# デバッグビルド
make build

# リリースビルド
make release

# テスト実行
make test

# Clippy + フォーマットチェック
make check

# /usr/local/bin にインストール
make install

# ビルド成果物をクリーン
make clean
```

## コントリビュート

コントリビュートを歓迎します！お気軽にプルリクエストをお送りください。

## ライセンス

[MIT](LICENSE)
