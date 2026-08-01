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
- **無効セレクタのエラー化** — 不正な CSS セレクタを「一致なし」と誤報せず、明示的なエラーとして終了
- **複数セレクタの順序保持結合** — 複数セレクタ指定時は、抽出結果を指定順のまま `---` 区切りで結合
- **WebDriver不要** — インストール済みのChrome/Chromiumを直接制御
- **柔軟な出力** — ファイルまたは標準出力
- **Chrome自動検出** — Chromeを自動検出、またはカスタムパスを指定可能
- **JSレンダリング待機時間の設定** — レンダリング完了までの待機時間を調整可能
- **クリーンな出力** — script、style、SVGを自動除去
- **URL解決** — レンダリング後のドキュメント基準URL（`<base href>` を含む）で相対URLを絶対パスに自動変換
- **コード領域を保護したURL解決** — Markdownリンク解決時にインラインコード、コードフェンス内、CommonMark のインデントコードブロック内、ブロッククォート内のコードブロック内は変更しない
- **CommonMark準拠のフェンス判定** — 開始・終了フェンスは外側のリスト項目の内容インデント基準で最大3スペースまでを許容し、それを超えるバッククォート行はインデントコードとして保持する。リストマーカーと同じ行から始まるフェンスも認識し、リストやブロッククォートの終了時にフェンス状態を正しく閉じる。info string 付きの行（例: ` ```rust `）も閉じフェンスとして扱わない
- **リスト構造を考慮したインデントコード判定** — インデントコードブロックは外側のリスト項目の内容インデント基準で判定するため、3段以上ネストしたリスト（HTML→Markdown 変換では4スペース以上のインデントになる）内の相対リンクも URL 解決される。リスト項目内の本物のインデントコードは従来どおり変換しない。`* * *` のようなテーマ区切りもリスト項目と誤認しない
- **Markdownリンク対応強化** — `<...>` 形式（スペースを含むURL）のリンク先解決に対応し、実際の Markdown リンク構文ではない単独の `](` は変換しない
- **壊れたリンク耐性** — 閉じ `)` が見つからないリンク候補や、閉じ `>` がない壊れた `<...>` リンク先があっても走査を打ち切らず、ネストしたリンクを含む後続の正常なリンクを URL 解決する
- **段落境界を守るリンク解析** — 未閉鎖の `[` やインラインバッククォートを、ブロッククォート記号だけの空行を含む段落境界・コードブロック境界の先にある区切りと結合せず、単一の soft break を含む正規のリンクテキストは維持する
- **未閉鎖バッククォート対応** — 閉じていないインラインバッククォートはリテラルとして扱い、その後ろの Markdown リンク解決を妨げない
- **複数行インラインコードの走査** — 改行をまたぐインラインコードが閉じた後も物理的な行頭状態を正しく追跡し、同じ行のフェンス風・インデント風テキストによって後続リンクが見落とされることを防ぐ
- **山括弧リンク先の括弧対応** — `<...>` 内の `)` をリンク終端として誤認しない
- **山括弧内エスケープ対応** — `<...>` 内の `\>` を終端と誤認せず、URL 解決時も文字 `>` として正しく扱う
- **エスケープ `<` 対応** — 標準形式リンク先の `\<` を山括弧形式の開始と誤認せずに literal `<` として扱い、URL 解決時には percent-encoding して出力する
- **エスケープ括弧対応** — `\(` `\)` を含むリンク先の閉じ括弧を正しく解釈し、URL 解決時も括弧を壊さない
- **アンバランス括弧への安全対応** — 解決後 URL にバランスしない `(` または `)` が含まれる場合は `<...>` 形式で出力し、Markdown リンクの破損を防ぐ
- **クォート安全なURL解析** — 通常のMarkdownリンク先でクォート/アポストロフィを壊さず処理
- **空リンク先title対応** — `[text]( "title")` や `[text]( 'title')` のような空のリンク先 + title の有効な構文で、title を URL と誤認せず保持
- **エスケープ空白対応** — 通常のMarkdownリンク先で `\ ` をタイトル区切りとして誤認せず、URL 解決時は空白として正しく扱う
- **リンク先前の空白対応** — Markdownリンク先の URL 直前に空白がある有効な構文でも、相対 URL を正しく解決
- **テーブル圧縮** — Markdownテーブルの不要なパディングを除去しつつ、コードフェンス内と `--` や `:` のようなセパレータ風データセルは保持
- **エスケープ済みパイプ対応** — テーブル圧縮時にセル内の `\|` を区切り文字として誤認しない
- **インラインコード内パイプ対応** — テーブル圧縮時にインラインコードスパン（` `` `）内に含まれる `|` をセル区切りとして扱わず、コード内容を保持する
- **インデントコードブロック判定** — 4 スペース以上インデントされた行は CommonMark のインデントコードブロックとしてテーブル扱いせず、先頭インデントとセル内空白を保持する
- **プログレス表示** — quietモード対応、完了表示は出力成功後のみ
- **CDPベースのHTTPステータス確認** — ページスクリプトがブラウザの Performance API を改変しても、Chrome DevTools Protocol の実レスポンスでHTTPエラーを拒否
- **証明書検証をデフォルト化** — HTTPS証明書を既定で検証し、信頼済みデバッグ用途に限って `--ignore-certificate-errors` を明示指定可能
- **ファイルステータス表示** — ファイル出力時に ✨（新規作成）、📝（更新）、✔（変更なし）を表示。git管理下のファイルは glob メタ文字を含む名前も対象パスへリテラル一致させて未ステージ変更を検出するため、削除済みの tracked ファイルや repo 外 cwd からの実行でも更新として扱い、既存ファイルの読み取りに失敗した場合も更新にフォールバックする
- **日時差分の無視** — `--ignore-date` で日時文字列だけが変わった場合の上書きを抑止し、小数秒やタイムゾーン付きの一般的な ISO 8601 形式も無視対象にする。双方に日時パターンが含まれる場合のみ比較し、非UTF-8ファイルでは安全にフォールバック
- **タイムアウト安全性** — 極端な `--timeout` 値でも内部のアイドルタイムアウト加算でオーバーフローしない
- **アトミックなファイル書き込み** — 出力は同一ディレクトリの一時ファイルへ書き込んでから rename で置き換えるため、書き込み中の I/O エラー（ディスク容量不足など）で既存ファイルが切り詰め・破損しない。既存ファイルのパーミッションは内容を書き込む前に適用し、書き込み権限を事前確認し、シンボリックリンク出力は実体へ解決してリンクを保ったまま更新する。リンク先が存在せず、その親ディレクトリも未作成の場合は必要な親を作成する。なお rename は inode を置き換えるため、ハードリンクは切れ ACL/拡張属性は保持されない（クラッシュ安全性を優先した仕様）

## 動作要件

- **OS**: macOS、Windows
- **Chrome/Chromium**: システムにインストール済みであること
- **Rust**: 1.88以上（ソースからビルドする場合）

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
| `--ignore-certificate-errors` | | HTTPS証明書エラーを無視（危険: 信頼済みデバッグ用途に限定） |
| `--ignore-date` | | ファイル書き込み時に日時だけの差分を変更なしとして扱う |
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

# 信頼済みサイトの証明書問題をデバッグ
get-md https://example.com --ignore-certificate-errors

# 日時だけが変わった場合は上書きをスキップ
get-md https://example.com -o output.md --ignore-date

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

# リリースビルド
cargo build --release

# フォーマット確認、Clippy、cargo check
make check

# /usr/local/bin にインストール
make install

# ビルド成果物をクリーン
make clean
```

## テスト

```bash
# ユニットテストと ignored E2E のビルド確認
make test

# Chrome/Chromium が必要な E2E テストを実行
cargo test --test e2e -- --ignored
```

ignored の E2E テストでは次を確認します。

- GitHub Raw 上の実ドキュメント取得
- ローカル `file://` ページでの相対リンク・画像 URL 解決（`<base href>` を含む）
- 複数セレクタ指定時の `---` 区切り結合
- 無効な CSS セレクタを明示的なエラーとして拒否すること
- `--ignore-date` 指定時に日時差分だけなら既存ファイルを書き換えず、日時以外の差分は上書きすること
- ページスクリプトが Performance API を偽装しても実際の HTTP 404 を拒否すること

## コントリビュート

コントリビュートを歓迎します！お気軽にプルリクエストをお送りください。

## ライセンス

[MIT](LICENSE)
