<h1 align="center">get-md</h1>

<p align="center">
  CLI that renders web pages in your installed Chrome or Chromium and converts the whole page or CSS-selected elements to Markdown
</p>

<!-- standard:badges:start -->
<h3 align="center">Supported Platforms</h3>

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

get-md opens a URL in the Chrome or Chromium already on your machine, waits for JavaScript to finish rendering, and writes the result as Markdown. Single-page apps that return an empty shell to a plain HTTP client come out with their real content, and there is no WebDriver or ChromeDriver to set up.

Convert the whole page, or pick the parts you need with CSS selectors. Relative links become absolute URLs and file output is written atomically, so get-md also fits scripts that keep Markdown copies of web pages up to date.

## Features

- **JavaScript rendering**: Drives your installed Chrome or Chromium over the Chrome DevTools Protocol, so single-page apps and dynamic content are converted after they render; `-w` sets the extra wait, which is included in the browser connection's idle timeout
- **No WebDriver**: Needs no ChromeDriver or Selenium; Chrome is found automatically, or set with `--chrome-path`
- **CSS selector extraction**: Converts only the elements you need; repeat `-s` to join several selectors with `---` in the order given, and an invalid selector fails with an explicit error instead of matching nothing
- **Clean Markdown**: Drops scripts, styles, `noscript`, and SVG, and removes the padding from Markdown tables while preserving escaped pipes in cells
- **Absolute URLs**: Resolves relative links and images against the rendered document's base URL, including `<base href>`, and leaves inline code and code blocks untouched
- **HTTP error detection**: Rejects HTTP error responses using Chrome DevTools Protocol network events, even when page scripts tamper with browser performance APIs
- **Certificate checks by default**: Validates HTTPS certificates; `--ignore-certificate-errors` is an explicit opt-out for trusted debugging
- **Safe file output**: Writes through a temporary file and an atomic rename, keeps existing permissions and symlinks, and reports whether the file was created, updated, or unchanged
- **Timestamp-only change detection**: With `--ignore-date`, leaves the file as it is when only dates and times changed
- **Progress display**: Shows each step while fetching and converting; `-q` turns it off. Standard output is flushed before success is reported

How links, code blocks, and tables are handled in detail: [docs/markdown-conversion.md](docs/markdown-conversion.md)

## Requirements

- **Chrome or Chromium**: Installed on the system. get-md starts it in headless mode for each run

## Installation

<!-- standard:install:start -->
### Homebrew (macOS)

```bash
brew install owayo/get-md/get-md
```

### Cargo

Requires Rust 1.98 or later.

```bash
cargo install --git https://github.com/owayo/get-md --locked
```

### From GitHub Releases

Download the archive for your platform from [Releases](https://github.com/owayo/get-md/releases/latest), extract it, and put `get-md` on your `PATH`. Each release also includes `SHA256SUMS` for checking the downloads.

| Platform | Archive |
|---|---|
| macOS (Intel) | `get-md-x86_64-apple-darwin.tar.gz` |
| macOS (Apple Silicon) | `get-md-aarch64-apple-darwin.tar.gz` |
| Windows (x86_64) | `get-md-x86_64-pc-windows-msvc.zip` |

On macOS, if you downloaded the archive with a browser, remove the quarantine attribute before running it: `xattr -d com.apple.quarantine get-md`.

### From Source

Requires [mise](https://mise.jdx.dev/) (the Rust toolchain is pinned in `mise.toml`).

```bash
git clone https://github.com/owayo/get-md.git
cd get-md
make install
```

`make install` installs to `/usr/local/bin`. Set `INSTALL_PATH` to change it (for example `make install INSTALL_PATH="$HOME/.local/bin"`).
<!-- standard:install:end -->

## Usage

```bash
get-md [OPTIONS] <URL>
```

```bash
# Convert the whole page to Markdown on standard output
get-md https://example.com

# Convert only the h1 and p elements and save them to a file
get-md https://example.com -s "h1" -s "p" -o example.md
```

The first command prints:

```markdown
# Example Domain

This domain is for use in documentation examples without needing permission. Avoid use in operations.

[Learn more](https://iana.org/domains/example)
```

Every option, more examples, and how selectors and file output behave: [docs/usage.md](docs/usage.md). `get-md --help` prints the option list in your terminal.

## Development

<!-- standard:dev:start -->
Requires [mise](https://mise.jdx.dev/). Tool versions are pinned in `mise.toml`.

```bash
make setup   # Install the toolchain (mise) and dependencies
make ci      # Run the same checks as CI (no changes)
```

| Command | Description |
|---|---|
| `make setup` | Install the toolchain (mise) and dependencies |
| `make build` | Build a debug binary |
| `make release` | Build a release binary |
| `make run` | Run the debug binary (arguments via ARGS="...") |
| `make test` | Run the tests |
| `make lint` | Run clippy with warnings as errors |
| `make fmt` | Format the code (rewrites files) |
| `make fmt-check` | Check the formatting (no changes) |
| `make check` | Run fmt-check and lint (no changes) |
| `make ci` | Run the same checks as CI (no changes) |
| `make install` | Install the release binary to INSTALL_PATH (default /usr/local/bin) |
| `make uninstall` | Remove the binary from INSTALL_PATH |
| `make clean` | Remove build artifacts |

Run `make` to list every target. Releases are published from GitHub Actions (**Actions → Release → Run workflow**).
<!-- standard:dev:end -->

The end-to-end tests drive a real Chrome or Chromium, so `make test` compiles them without running them. How to run them and what the tests cover: [docs/development.md](docs/development.md)

## License

<!-- standard:license:start -->
[MIT](LICENSE)
<!-- standard:license:end -->
