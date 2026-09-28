# Usage

Every option of get-md, more examples than the README, and how selectors and file output behave. `get-md --help` prints the same option list in your terminal.

## Synopsis

```bash
get-md [OPTIONS] <URL>
```

`<URL>` is the page to fetch. Local files work too, as `file://` URLs.

## Options

| Option | Short | Description |
|---|---|---|
| `--selector <SEL>` | `-s` | CSS selector for the elements to convert (repeatable). Without it, the whole `body` is converted |
| `--output <FILE>` | `-o` | Output file path (default: standard output) |
| `--chrome-path <PATH>` | | Path to the Chrome or Chromium binary (default: detected automatically) |
| `--wait <SECS>` | `-w` | Extra wait after the page loads, for JavaScript rendering (default: 2) |
| `--timeout <SECS>` | `-t` | Page load timeout in seconds (default: 60) |
| `--no-headless` | | Show the browser window (for debugging) |
| `--no-cache` | | Disable the browser cache (always fetch the latest content) |
| `--ignore-certificate-errors` | | Ignore HTTPS certificate errors (dangerous; use only for trusted debugging) |
| `--ignore-date` | | Treat timestamp-only differences as unchanged when writing a file |
| `--front-matter` | | Prepend YAML front matter that records where the Markdown came from (see [Front matter](#front-matter)) |
| `--meta <KEY=VALUE>` | | Add your own field to the front matter (repeatable). Turns on `--front-matter` |
| `--quiet` | `-q` | Suppress the progress display |
| `--help` | `-h` | Show help |
| `--version` | `-V` | Show version |

The browser's own idle timeout is `--timeout` plus `--wait` plus 30 seconds. This keeps the Chrome DevTools Protocol connection open while a quiet page waits for JavaScript rendering. The sum saturates instead of overflowing, so even extreme values are safe.

## Examples

```bash
# Convert the whole page to Markdown on standard output
get-md https://example.com

# Extract only the heading
get-md https://example.com -s "h1"

# Extract several elements; the results are joined with --- in the order given
get-md https://example.com -s "h1" -s "p"

# Save to a file
get-md https://example.com -s "h1" -s "p" -o example.md

# Wait 5 seconds after the page loads for slow JavaScript rendering, and allow 90 seconds for loading
get-md https://example.com -w 5 -t 90

# Use a specific Chrome binary (macOS)
get-md https://example.com --chrome-path "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"

# Fetch a trusted page whose certificate is broken (here, a public self-signed test page)
get-md https://self-signed.badssl.com/ --ignore-certificate-errors

# Skip rewriting the file when only timestamps changed
get-md https://example.com -o example.md --ignore-date

# Record the source URL, title, selectors, and retrieval time in YAML front matter
get-md https://example.com -o example.md --front-matter

# Add your own fields to the front matter (--meta alone also turns it on)
get-md https://example.com -o example.md --meta topic=example --meta "note=kept for reference"

# Re-fetch a saved page from a script: the file changes only when the page really changed
get-md https://example.com -o example.md --front-matter --ignore-date --meta topic=example

# Quiet mode (no progress output)
get-md https://example.com -q -o example.md
```

## Selectors

- Without `-s`, get-md converts the whole `body`
- With several `-s` options, the results are joined with a `---` line in the order the selectors were given
- A selector that matches nothing prints a warning. If no selector matches anything, get-md exits with an error
- An invalid CSS selector is reported as an error, not as a selector that matched nothing

## Fetching

- **HTTP errors**: get-md reads the HTTP status from Chrome DevTools Protocol network events and rejects error responses such as 404, even when page scripts alter the browser's performance APIs
- **Certificates**: HTTPS certificates are validated by default. Use `--ignore-certificate-errors` only for a trusted site you are debugging
- **Cache**: `--no-cache` disables the browser cache, so every run fetches the latest content

## Writing to a file

With `-o`, get-md writes the Markdown to the file and reports what happened to it:

```text
✨ https://example.com → example.md (created)
```

| Mark | Status | Meaning |
|---|---|---|
| ✨ | created | The file did not exist before |
| 📝 | updated | The content changed |
| ✔ | unchanged | The content is the same as before |

- **Git-aware status**: If the file is tracked by Git and has unstaged changes, the status is `updated`. The check runs in the repository that contains the output file and matches its literal path (glob characters such as `[` or `*` in the name are not expanded), so it also holds when you write to a tracked file that was deleted and when get-md runs from outside that repository. If the existing file cannot be read, the status falls back to `updated`
- **`--ignore-date`**: Skips the rewrite when only date and time strings changed, including common ISO 8601 forms with fractional seconds and timezone suffixes. Both the old and the new content must contain a date for this to apply, and non-UTF-8 files fall back to the normal comparison
- **Atomic writes**: The output is written to a temporary file in the same directory and renamed into place, so an I/O error in the middle of writing (a full disk, for example) never truncates or corrupts an existing file. Existing permissions are applied before the content is written, write permission is checked up front, and a symlinked output resolves to the real target so the link stays intact, even when a dangling target has missing parent directories. The rename replaces the inode, so hard links are broken and ACLs and extended attributes are not carried over (a deliberate trade-off for crash safety)
- **Progress**: Progress is shown on standard error, so it never mixes into Markdown written to standard output. The completion line appears only after the output was written and flushed successfully
- **Front matter**: With `--front-matter`, the retrieval time is left out of the comparison. See [Comparing with the existing file](#comparing-with-the-existing-file)

## Front matter

`--front-matter` puts YAML front matter at the top of the output, on standard output as well as in a file, so a saved copy keeps a record of where it came from. `--meta KEY=VALUE` adds your own fields and turns front matter on by itself.

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

### Fields

The fields always appear in this order:

| Field | Value | Written |
|---|---|---|
| `url` | The URL given on the command line, as is | Always |
| `final_url` | The page URL after loading, for example after a redirect | Only when it differs from `url`. Differences that URL normalization removes, such as the trailing `/` in `https://example.com/`, do not count |
| `title` | The page's `document.title` with leading and trailing whitespace removed | Only when the page has a non-empty title |
| `selectors` | The selectors used, as a list. Without `-s` this is `["body"]` | Always |
| `retrieved_at` | When get-md read the page: UTC, RFC 3339, whole seconds | Always |
| `--meta` fields | Your values, in the order you gave them | When `--meta` is given |

The get-md version is not recorded, so upgrading get-md does not rewrite every saved file.

### Format

- The front matter is a `---` line, one `key: value` line per field, a closing `---` line, and a blank line. The Markdown follows. Because a blank line always follows the closing `---`, a body that itself starts with `---` stays distinguishable
- Every value is a double-quoted YAML string, and `selectors` is a flow sequence of double-quoted strings, such as `["article", "main"]`. So values like `true`, `2026-09-29`, or `a: b` stay strings
- Inside the quotes, `"` and `\` are escaped, line feeds, carriage returns, and tabs become `\n`, `\r`, and `\t`, and the other control characters, U+2028, U+2029, U+FEFF, U+FFFE, and U+FFFF become `\uXXXX`. Every field fits on one line, and a YAML parser reads back the original string

### `--meta` rules

- The argument is split at the first `=`: `--meta query=a=b` sets `query` to `a=b`. The value may be empty (`--meta note=`)
- Keys start with a letter or `_`, followed by letters, digits, `_`, or `-` (`^[A-Za-z_][A-Za-z0-9_-]*$`)
- These are errors, reported before the browser starts: a missing `=`, an empty key, a key in another form, the same key twice, a built-in field name (`url`, `final_url`, `title`, `selectors`, `retrieved_at`), and words that YAML reads as a boolean or null instead of a string (`true`, `false`, `null`, `yes`, `no`, `on`, `off`, `y`, `n`, in any letter case)

### Comparing with the existing file

When `-o` points at a file that already has get-md's front matter, get-md compares the new output with it like this:

- The `retrieved_at` value is always left out of the comparison, with or without `--ignore-date`. When the rest of the front matter and the body are the same, get-md leaves the file as it is and reports `unchanged` (`updated` if Git has unstaged changes, as usual). Re-fetching a page that did not change produces no Git diff
- With `--ignore-date` as well, the front matter (apart from `retrieved_at`) is still compared exactly, and only the body gets the timestamp-insensitive comparison. A changed title, URL, selector, or `--meta` value is always written
- If the existing file has no front matter, because this is the first run or its header has another format, the usual comparison of the whole file applies, and the file is rewritten with front matter
- `retrieved_at` is the time the body stored in the file was retrieved. When get-md keeps the file, the old value stays
- Fields that exist only in the existing file are not carried over, because the output depends only on the arguments. Pass any value you want to keep with `--meta` on every run

Without `--front-matter` and `--meta`, the output and the comparison are the same as before.
