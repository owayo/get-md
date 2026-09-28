# Development

The Development section of the README lists the `make` targets. This page covers the tests.

## Unit tests

`make test` runs the unit tests and compiles the end-to-end tests without running them. The unit tests need no browser.

The unit tests verify the conventional `-h` / `--help` and `-V` / `--version` CLI contracts. They also cover the complete HTML-to-Markdown post-processing pipeline for long code fences, images with spaced URLs and titles, and table rows wider than their headers.

For front matter, the unit tests cover the `--meta` rules, the escaping of each special character, the field order, and the comparison with an existing file. They also read the generated front matter back with a YAML parser ([yaml-rust2](https://crates.io/crates/yaml-rust2), a dev-dependency) and check that every value returns as the original string.

For invisible characters (`src/invisible.rs`), the unit tests check each class of characters that is removed (C0, DEL, C1, and samples of Default_Ignorable_Code_Point) and each class that stays (visible spaces, private use, unassigned code points). They also check that C1 control characters are counted within the total. Every context exception is covered:

- RGI emoji ZWJ sequences, with and without the optional U+FE0F, and a ZWJ between ordinary characters
- Flag tag sequences, and tag characters that smuggle ASCII
- An isolated variation selector, repeated variation selectors, and U+FE0F after an emoji
- Standardized variation sequences (including Mongolian), and an ideographic variation selector after a kanji
- ZWNJ inside and outside Persian words, and joiners next to a virama

Tests in `src/main.rs` run HTML through the whole conversion and check that:

- Decoded character references and invisible characters in link and image attributes are removed, before relative URLs are resolved
- htmd still escapes Markdown syntax after a removed leading character
- Code blocks are cleaned too, and `--keep-invisible` removes nothing
- The front matter title is cleaned while `--meta` values stay as given
- The reported counts read as expected

The parts of hidden-content removal that need no browser (the script arguments and how its result is read) are tested there too.

## End-to-end tests

The end-to-end tests in `tests/e2e.rs` drive a real Chrome or Chromium, so they are marked `#[ignore]` and are not part of `make ci`. Run them with:

```bash
make test-e2e
```

They cover:

- Fetching a real GitHub raw document
- Resolving relative links and images from a local `file://` page, including `<base href>`
- Resolving relative links inside lists nested three levels deep, while leaving links in fenced code unchanged
- Joining multiple selectors with the documented `---` separator
- Rejecting invalid CSS selectors with an explicit error
- Skipping rewrites for `--ignore-date` when only timestamp text changes, while still overwriting non-date content changes
- Writing front matter with `--front-matter` and `--meta`, leaving the file untouched on a re-fetch when only `retrieved_at` would change, and rewriting it when a `--meta` value changes
- Rejecting a real HTTP 404 even when page scripts spoof browser performance APIs
- Removing hidden content by default: `display: none`, the `hidden` attribute, `opacity: 0` (with its children), `content-visibility: hidden`, `visibility: hidden` text, the body of a closed `<details>`, visually hidden elements (`clip` and `clip-path`), elements outside the page to the left or above, `font-size: 0`, and transparent text. At the same time, keeping a child that sets `visibility: visible` again, `display: contents`, an open `<details>` and the summary of a closed one, `aria-hidden` text that is on screen, content far below the viewport, tiny text, gradient text (`background-clip: text`), and transparent text with a shadow. The same page checks that invisible characters are removed from the body and the front matter title while an emoji ZWJ sequence stays
- `--keep-hidden` and `--keep-invisible` each restoring only their own part of the output
- Still removing hidden content when page scripts replace `getComputedStyle`, `getBoundingClientRect`, `JSON.stringify`, and `Array.from` (the extraction runs in an isolated world)
- Failing with a hint about `--keep-hidden` when every selected element is hidden, and warning and continuing when only some are

## Unicode data

Removing invisible characters relies on two sources of Unicode data, which must be for the same Unicode version (currently 17.0):

- Character properties (General_Category, Default_Ignorable_Code_Point, Unified_Ideograph, Joining_Type, Canonical_Combining_Class) come from the compiled data of [icu_properties](https://crates.io/crates/icu_properties). icu_properties 2.3 ships ICU 78 data, which is Unicode 17.0
- Sequences that no character property describes come from the Unicode data files in `data/unicode/`, copied unchanged from unicode.org (the Unicode License v3 is in `data/unicode/LICENSE`):

| File | Source | Used for |
|---|---|---|
| `emoji-zwj-sequences.txt` | https://www.unicode.org/Public/17.0.0/emoji/emoji-zwj-sequences.txt | RGI emoji ZWJ sequences |
| `emoji-sequences.txt` | https://www.unicode.org/Public/17.0.0/emoji/emoji-sequences.txt | RGI emoji tag sequences (flags) |
| `emoji-variation-sequences.txt` | https://www.unicode.org/Public/17.0.0/ucd/emoji/emoji-variation-sequences.txt | Characters that U+FE0E and U+FE0F may follow |
| `StandardizedVariants.txt` | https://www.unicode.org/Public/17.0.0/ucd/StandardizedVariants.txt | Standardized variation sequences, including Mongolian |

`build.rs` reads these files at build time and generates the tables that `src/invisible.rs` includes. It stops the build when a file's version header does not match `UNICODE_VERSION` or `EMOJI_VERSION` in `build.rs`, or when a line has an unexpected shape.

To move to a new Unicode version (for example, after icu_properties starts shipping newer data):

1. Check which Unicode version the icu_properties data is for: `icu_properties_data` names the ICU release in its README (`ICU version release-NN`), and each ICU release names its Unicode version
2. Download the four files for that version from the URLs above (replace `17.0.0` in the path) into `data/unicode/`, without editing them
3. Update `UNICODE_VERSION` and `EMOJI_VERSION` in `build.rs`, and the version check in `table_data_is_unicode_17` in `src/invisible.rs` (rename it, and probe a character that the new version added)
4. Run `make ci`. Look at the changes to the generated tables if something fails, and update the Unicode version in docs/usage.md and docs/usage.ja.md

`markup5ever_rcdom` is pinned (`=0.38.0`) because get-md cleans the DOM that htmd builds and must use the same version as htmd. When you update htmd, set it to the version htmd depends on.
