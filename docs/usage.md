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
| `--keep-hidden` | | Also extract elements and text that are hidden on the page (the previous behavior; see [Hidden content and invisible characters](#hidden-content-and-invisible-characters)) |
| `--keep-invisible` | | Keep invisible characters such as zero-width spaces and bidirectional controls in the output (the previous behavior) |
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

# Include collapsed sections, closed tabs, and other content that is hidden on the page
get-md https://example.com --keep-hidden

# Get the output of earlier versions: keep hidden content and invisible characters
get-md https://example.com --keep-hidden --keep-invisible

# Quiet mode (no progress output)
get-md https://example.com -q -o example.md
```

## Selectors

- Without `-s`, get-md converts the whole `body`
- With several `-s` options, the results are joined with a `---` line in the order the selectors were given
- A selector that matches nothing prints a warning. If no selector matches anything, get-md exits with an error
- An invalid CSS selector is reported as an error, not as a selector that matched nothing
- A matched element that is itself hidden on the page, or sits inside a hidden element, is left out with a warning. If every matched element is hidden, get-md exits with an error that suggests `--keep-hidden` (see [Hidden content](#hidden-content))

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
- **Progress**: Progress is shown on standard error, so it never mixes into Markdown written to standard output. The completion line appears only after the output was written and flushed successfully. When hidden content or invisible characters were removed, a line with the counts follows it (see [What get-md reports](#what-get-md-reports))
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
| `title` | The page's `document.title` without invisible characters (unless `--keep-invisible`) and with leading and trailing whitespace removed | Only when the page has a non-empty title |
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
- The value is written as you gave it. Unlike the page title, invisible characters are not removed from it
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

## Hidden content and invisible characters

Text that people cannot see on a page can still reach an AI (LLM) that reads the Markdown: instructions hidden with CSS or with invisible characters can steer the model (prompt injection), and stray control characters can disturb it. So get-md removes two kinds of content by default, to bring what the model reads closer to what a person sees on screen:

- **Hidden content**: elements and text that are hidden on the rendered page. `--keep-hidden` keeps them
- **Invisible characters**: zero-width spaces, BOMs, bidirectional controls, tag characters, control characters, and similar characters that are not displayed. `--keep-invisible` keeps them

Both are deleted, not replaced with a visible notation. The two options are independent.

> **This changes the default output.** Earlier versions of get-md kept both. To get the previous output, use `--keep-hidden --keep-invisible`.

### Hidden content

get-md decides what is hidden in the browser, from the computed styles of the rendered page at the moment it extracts the HTML (after `--wait`). It removes the hidden parts from a copy of the selected elements, so the page itself is not changed. The check runs apart from the page's own scripts (in an isolated world, as browser extensions do), so a page cannot switch it off or fake its result by replacing JavaScript functions.

These are removed together with everything inside them:

| Condition | Examples |
|---|---|
| `display: none` | Elements with the `hidden` attribute (unless CSS overrides it), a closed `<dialog>`, a popover that is not shown, closed menus and tab panels |
| `content-visibility: hidden` | `hidden="until-found"` |
| `opacity: 0` on the element | Content that fades in later |
| The children of a closed `<details>` other than its `<summary>` | The summary stays |
| Visually hidden: absolutely or fixed positioned, clipped with a zero-area `clip: rect(...)` or with a `clip-path: inset(...)` that covers the whole box, and no larger than 1px | `.sr-only` and `.visually-hidden` classes |
| Outside the page to the left or above: absolutely or fixed positioned and entirely left of or above the page, where scrolling cannot reach | `left: -9999px`, `top: -500px` |

These are removed as text. The element stays, so a child can still show its own text:

| Condition | Note |
|---|---|
| `visibility: hidden` or `collapse` | A child with `visibility: visible` is kept |
| `font-size: 0` | |
| A transparent text color (`color` or `-webkit-text-fill-color` with an alpha of 0) | Kept when a text stroke, a text shadow, or a background clipped to the text (`background-clip: text`, as in gradient headings) makes the letters visible |
| Text directly inside a closed `<details>` | |

- Text that is only whitespace is not judged. `script`, `style`, `noscript`, `template`, `svg`, and head elements such as `title` and `meta` are left as they are (the Markdown conversion drops script, style, noscript, and SVG anyway)
- The rule for content outside the page applies to horizontal pages only, and the left edge only to left-to-right pages: right-to-left and vertical pages can scroll to the left or up. An element whose overflowing content still reaches into the page is kept
- If a selected element is itself hidden, or sits inside a hidden element, it is left out with a warning. If every selected element is hidden, get-md exits with an error that suggests `--keep-hidden`

**The page as it is when get-md reads it is what counts.** Closed tabs, accordions, and `<details>`, menus that are not open, and content that a scroll animation has not revealed yet (it stays at `opacity: 0` until you scroll to it, and get-md does not scroll) are all removed. Use `--keep-hidden` when you need them.

These are not removed, because they do not hide the text, or because get-md cannot tell cheaply and reliably whether the text is visible:

- `aria-hidden="true"`: it hides the element from assistive technology only, and the text is still on screen
- Very small text (any `font-size` above 0), and elements of 1px or less that are not clipped
- Content beyond the right or bottom edge of the page, which scrolling can reach, and text moved away with `text-indent`
- Text in the same color as its background: the real background can come from images, gradients, or overlapping elements
- Content cut off by `overflow: hidden` or a zero height, such as an accordion collapsed with `max-height: 0`

Some content never appears in the output, with or without this feature, because it is not part of the HTML that get-md extracts: text generated by CSS pseudo-elements (`::before`, `::after`, and counters), drawings on a `<canvas>`, the contents of `<iframe>` elements, and shadow DOM contents.

### Invisible characters

get-md removes these characters from the Markdown, before it resolves relative URLs:

- Control characters other than tab, line feed, and carriage return: the C0 controls, DEL, and the C1 controls (U+0080 to U+009F)
- Characters with the Unicode property Default_Ignorable_Code_Point, for example:
  - The zero-width space U+200B, the word joiner U+2060 and the invisible operators U+2061 to U+2064, the BOM (zero-width no-break space) U+FEFF, the soft hyphen U+00AD, and the combining grapheme joiner U+034F
  - Bidirectional controls: LRM U+200E, RLM U+200F, ALM U+061C, the embeddings and overrides U+202A to U+202E, the isolates U+2066 to U+2069, and the deprecated format characters U+206A to U+206F
  - The Hangul fillers U+115F, U+1160, U+3164, and U+FFA0, and the Mongolian vowel separator U+180E
  - Variation selectors, the zero-width joiner and non-joiner, and the tag characters U+E0000 to U+E007F, except in the contexts below, and unassigned code points reserved as default ignorable
- The interlinear annotation characters U+FFF9 to U+FFFB

These are kept:

- Visible spaces, such as the no-break space, thin space, hair space, narrow no-break space, and ideographic space U+3000, and the line and paragraph separators
- Private use characters (such as icon font glyphs) and unassigned code points that are not default ignorable
- Format characters that are displayed, such as the Arabic number sign U+0600

A default-ignorable character is also kept where it changes what is displayed:

| Character | Kept |
|---|---|
| The zero-width joiner U+200D and the tag characters U+E0000 to U+E007F | Inside an RGI emoji ZWJ sequence (such as 👨‍👩‍👧) or an RGI emoji tag sequence (the flags of England, Scotland, and Wales). A U+FE0F in the sequence may be left out, as in the minimally qualified forms |
| U+FE0E and U+FE0F | Right after a character that has emoji variation sequences (❤️, #️⃣) |
| U+FE00 to U+FE0D, and the Mongolian free variation selectors U+180B to U+180D and U+180F | Right after a base character, when the pair is a standardized variation sequence |
| The ideographic variation selectors U+E0100 to U+E01EF | Right after a CJK unified ideograph (葛󠄀). get-md does not check whether the pair is registered in the Ideographic Variation Database |
| The zero-width non-joiner U+200C, and the zero-width joiner U+200D outside emoji | Where they change how letters join, following the contexts in UAX #31: next to a virama (Devanagari क्‍ष, Sinhala ශ්‍රී), the non-joiner between two letters that would otherwise join (Persian می‌خواهم), and the joiner next to a joining letter. They are removed elsewhere, for example between Latin letters |

Only one variation selector directly after its base is kept. A second one in a row, or one with no base in front of it, is removed, because data can be hidden in runs of variation selectors.

- The removal covers the whole Markdown: code blocks and inline code, link destinations, and the alt text and titles of images. Character references in the page, such as `&#x200B;`, are decoded before this, so they are removed too
- Invisible characters in a link destination are removed before the relative URL is resolved, so they never end up percent-encoded in the absolute URL
- The front matter `title` is cleaned the same way. `--meta` values are written as given
- The character properties come from Unicode 17.0 (the Unicode data built into get-md), and the emoji sequences and standardized variation sequences from the Unicode 17.0.0 data files

### What get-md reports

When it removed something, get-md prints a line with the counts to standard error after the completion line:

```text
🧹 隠れた要素 3 個・隠れたテキスト 2 か所・見えない文字 12 個 (うち C1 制御文字 4 個) を除いた
```

The line reads: 3 hidden elements, hidden text in 2 places, and 12 invisible characters (4 of them C1 control characters) were removed.

- **Hidden elements** counts the outermost removed elements that contained text or an image; hidden form fields and empty decorations are not counted. The count is taken per extracted fragment, so an element matched by two selectors is counted twice
- **Hidden text** counts places: hidden text that follows other hidden text is the same place
- **Invisible characters** counts Unicode scalar values, including those removed from the front matter title. The C1 control characters are part of the total, not added to it
- Counts of zero are left out, and nothing is printed when nothing was removed
- Like the progress display, the line is not shown with `--quiet`, or when standard error is not a terminal. The counts are never written to the front matter, so a small change in how a page renders does not rewrite a saved file just because a count changed

When the text removed for `opacity: 0` or `visibility: hidden` is at least as long as the text that remains, a second line (💡) suggests `--keep-hidden`: the page probably reveals its content with scroll animations.
