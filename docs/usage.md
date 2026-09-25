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
| `--quiet` | `-q` | Suppress the progress display |
| `--help` | `-h` | Show help |
| `--version` | `-V` | Show version |

The browser's own idle timeout is `--timeout` plus 30 seconds. The sum saturates instead of overflowing, so even an extreme `--timeout` value is safe.

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
- **Progress**: Progress is shown on standard error, so it never mixes into Markdown written to standard output. The completion line appears only after the output was written successfully
