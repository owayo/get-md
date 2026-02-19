# get-md

URL をブラウザで取得し、指定要素を Markdown に変換する CLI ツール。

## Tech Stack

- Rust (Edition 2024)
- headless_chrome (CDP 経由のブラウザ制御)
- htmd (HTML -> Markdown 変換、skip_tags/spacing オプション使用)
- clap (CLI 引数解析、derive feature)
- indicatif (プログレス表示)
- url (相対URL -> 絶対URL 変換)
- anyhow (エラーハンドリング)

## Architecture

- WebDriver を使用せず、システムの Chrome/Chromium を CDP で直接制御
- JS レンダリング対応（SPA、動的コンテンツ）
- CSS セレクタでブラウザ内 JS 実行により要素の outerHTML を取得
- htmd で HTML -> Markdown 変換（script, style, noscript, svg は skip_tags で除去）
- Rust 側で相対 URL を絶対パスに変換（Markdown リンク・画像の `[text](url)` パターン）
- テーブルのセルパディングとセパレータダッシュを圧縮
- 対応 OS: macOS, Windows

## Project Structure

```
src/
  main.rs       # CLI 定義、ブラウザ起動、HTML 取得、Markdown 変換
  progress.rs   # indicatif ベースのプログレス表示
Makefile        # build, release, test, fmt, check, install ターゲット
.github/
  workflows/
    ci.yml      # CI (test, clippy, fmt, build)
    release.yml # リリース (バージョンバンプ、ビルド、GitHub Release、Homebrew更新)
```

## Development

```bash
make build    # デバッグビルド
make release  # リリースビルド
make test     # テスト
make check    # clippy + check
make fmt      # フォーマット
make install  # /usr/local/bin にインストール
```

## Testing

- ユニットテストは Chrome 不要（CLI パース、JS エスケープ、Markdown変換、プログレス表示のテスト）
- E2E テストは実際の Chrome/Chromium が必要
- `make test` または `cargo test` で実行

## Key Design Decisions

- Chrome/Chromium はシステムにインストール済みであることを前提とする
- セレクタ未指定時は body 全体を対象とする
- 複数セレクタ指定時は `---` で区切って結合
- ファイル出力時は末尾改行を保証
- バージョニングは CalVer (YY.M.counter) 形式
