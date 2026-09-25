# Development

The Development section of the README lists the `make` targets. This page covers the tests.

## Unit tests

`make test` runs the unit tests and compiles the end-to-end tests without running them. The unit tests need no browser.

The unit tests verify the conventional `-h` / `--help` and `-V` / `--version` CLI contracts. They also cover the complete HTML-to-Markdown post-processing pipeline for long code fences, images with spaced URLs and titles, and table rows wider than their headers.

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
- Rejecting a real HTTP 404 even when page scripts spoof browser performance APIs
