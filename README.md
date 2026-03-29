<h1 align="center">get-md</h1>

<p align="center">
  Fetch web pages with JS rendering and convert to Markdown
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

## Features

- **JS Rendering Support** — uses system Chrome via CDP, handles SPAs and dynamic content
- **CSS Selector Targeting** — extract only the elements you need (multiple selectors supported)
- **No WebDriver Required** — directly controls your installed Chrome/Chromium
- **Flexible Output** — write to file or stdout
- **Auto Chrome Detection** — finds Chrome automatically, or specify a custom path
- **Configurable Wait** — adjustable wait time for JS rendering completion
- **Clean Output** — strips scripts, styles, SVGs automatically
- **URL Resolution** — converts relative URLs to absolute paths in output
- **Code-safe URL Resolution** — leaves inline code and fenced code blocks untouched when resolving Markdown links
- **Markdown Link Robustness** — supports resolving `<...>` style link destinations (including spaces) and ignores bare `](` text that is not real Markdown link syntax
- **Literal Backtick Safety** — treats unmatched inline backticks as literal text, so later Markdown links are still resolved
- **Angle Destination Parentheses Support** — does not treat `)` inside `<...>` link destinations as the closing delimiter
- **Escaped Parentheses Support** — correctly parses link destinations containing `\(` and `\)`
- **Quote-safe URL Parsing** — preserves quotes/apostrophes in standard Markdown link destinations
- **Escaped Whitespace Handling** — keeps `\ ` in standard link destinations from being split as title separators
- **Table Compaction** — removes unnecessary padding in Markdown tables while preserving fenced code blocks
- **Escaped Pipe-safe Tables** — keeps escaped cell pipes (`\|`) intact during table compaction
- **Progress Display** — shows operation progress with quiet mode option, and reports completion only after output succeeds
- **File Status Icons** — shows ✨ (created), 📝 (updated), or ✔ (unchanged) for file output; git-aware change detection and fallback to `updated` when an existing file cannot be read
- **Date-only Diff Ignore** — `--ignore-date` skips rewrites when only timestamp strings changed, including common ISO 8601 forms with fractional seconds and timezone suffixes
- **Timeout Safety** — internal browser idle-timeout buffer uses saturating arithmetic to avoid overflow at extreme `--timeout` values

## Requirements

- **OS**: macOS, Windows
- **Chrome/Chromium**: installed on the system
- **Rust**: 1.85+ (for building from source)

## Installation

### Homebrew (macOS)

```bash
brew install owayo/get-md/get-md
```

### From GitHub Releases

Download the latest binary from [GitHub Releases](https://github.com/owayo/get-md/releases).

| Platform | Asset |
|----------|-------|
| macOS (Apple Silicon) | `get-md-aarch64-apple-darwin.tar.gz` |
| macOS (Intel) | `get-md-x86_64-apple-darwin.tar.gz` |
| Windows (x64) | `get-md-x86_64-pc-windows-msvc.zip` |

### From Source

```bash
git clone https://github.com/owayo/get-md.git
cd get-md
cargo install --path .
```

## Quickstart

```bash
# Convert a page to Markdown
get-md https://example.com

# Extract only the article content and save to file
get-md https://example.com -s "article" -o output.md
```

## Usage

### Basic Syntax

```bash
get-md [OPTIONS] <URL>
```

### Options

| Option | Short | Description |
|--------|-------|-------------|
| `--selector <SEL>` | `-s` | CSS selector for elements to convert (repeatable) |
| `--output <FILE>` | `-o` | Output file path (default: stdout) |
| `--chrome-path <PATH>` | | Path to Chrome binary |
| `--wait <SECS>` | `-w` | Wait time after page load in seconds (default: 2) |
| `--timeout <SECS>` | `-t` | Page load timeout in seconds (default: 60) |
| `--no-headless` | | Run browser visibly (for debugging) |
| `--no-cache` | | Disable browser cache (always fetch latest content) |
| `--ignore-date` | | Treat timestamp-only output diffs as unchanged when writing files |
| `--quiet` | `-q` | Suppress progress display |
| `--help` | `-h` | Show help |
| `--version` | `-V` | Show version |

### Examples

```bash
# Convert entire page to Markdown
get-md https://example.com

# Extract only article content
get-md https://example.com -s "article"

# Extract multiple elements
get-md https://example.com -s "h1" -s ".content"

# Save to file
get-md https://example.com -s "main" -o output.md

# Handle a slow JS-rendered page
get-md https://spa-example.com -s "#app" -w 5 -t 60

# Use a specific Chrome binary
get-md https://example.com --chrome-path /usr/bin/google-chrome

# Skip rewriting when only timestamps changed
get-md https://example.com -o output.md --ignore-date

# Quiet mode (no progress output)
get-md https://example.com -s "article" -q -o output.md
```

## Development

```bash
# Build debug version
make build

# Build release version
make release

# Run tests
make test

# Run release build
cargo build --release

# Run clippy and format check
make check

# Install to /usr/local/bin
make install

# Clean build artifacts
make clean
```

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

## License

[MIT](LICENSE)
