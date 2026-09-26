# Markdown conversion

get-md converts the extracted HTML to Markdown with [htmd](https://crates.io/crates/htmd), then post-processes the result: it resolves relative URLs and compacts tables. This page describes how that post-processing treats links, code, and tables. It follows the CommonMark block structure, so text that only looks like Markdown syntax (inside code, for example) is left alone.

## Cleanup

- **Removed elements**: `script`, `style`, `noscript`, and `svg` elements are dropped before conversion

## URL resolution

- **URL resolution**: Converts relative URLs to absolute ones using the rendered document's base URL, including `<base href>`
- **Code-safe URL resolution**: Leaves inline code, fenced code blocks, CommonMark indented code blocks, and code blocks inside blockquotes untouched when resolving Markdown links
- **Markdown link robustness**: Resolves `<...>` style link destinations (including spaces) and ignores bare `](` text that is not real Markdown link syntax
- **Broken link tolerance**: A malformed link candidate without a closing `)` or a malformed `<...>` destination without a closing `>` does not prevent later valid links, including nested links, from being resolved
- **Paragraph-boundary link safety**: Does not combine an unmatched `[` or inline backticks with delimiters beyond blank lines (including blockquote-only blank lines) or code-block boundaries, while preserving valid link text across a single soft line break. Closing-delimiter searches for malformed links stop at the same blockquote-only blank-line boundary
- **Parentheses in angle destinations**: Does not treat `)` inside a `<...>` link destination as the closing delimiter
- **Escaped `>` in angle destinations**: Handles `\>` inside a `<...>` link destination and resolves it as a literal `>` instead of corrupting the path
- **Escaped `<`**: Treats `\<` in a standard link destination as a literal `<` (not the start of the `<...>` form) and resolves it through percent-encoding
- **Escaped parentheses**: Parses link destinations containing `\(` and `\)` and resolves them as literal parentheses
- **Unbalanced parentheses**: Emits resolved URLs with an unbalanced `(` or `)` as `<...>` link destinations, so the Markdown links stay valid
- **Quotes in destinations**: Preserves quotes and apostrophes in standard link destinations
- **Empty destination with a title**: Preserves valid empty-destination links such as `[text]( "title")` or `[text]( 'title')` without treating the title as a URL
- **Escaped whitespace**: Keeps `\ ` in a standard link destination from being split as a title separator and resolves it as a literal space
- **Leading whitespace**: Resolves relative URLs even when a valid link destination starts with whitespace before the URL

## Code blocks and inline code

- **CommonMark fence detection**: Recognizes opening and closing fences relative to the enclosing list item's content indent, including fences that start on the list-marker line. Fence state closes when its list or blockquote container ends, over-indented backtick lines stay indented code, and lines with an info string (such as ` ```rust `) are not closing fences. Table compaction reuses the same block classification, so table-like code inside these fences is preserved and compaction resumes after the fence closes
- **List-aware indented code detection**: Indented code blocks are detected relative to the enclosing list item's content indent, so relative links inside deeply nested lists (three or more levels, which the HTML-to-Markdown conversion indents by four or more spaces) are still resolved, while genuine indented code inside a list item stays untouched. Thematic breaks such as `* * *` are not mistaken for list items
- **Literal backticks**: Treats unmatched inline backticks as literal text, so later Markdown links are still resolved. The lookahead for a closing run also stops at fences opened on a list-marker line
- **Linear-time backtick matching**: Pre-indexes backtick runs of equal length within paragraph and fence boundaries, avoiding repeated suffix scans on documents with many unmatched runs
- **Multiline inline code**: Keeps track of the physical start of each line after multiline inline code closes, so fence-like or indented text later on that line cannot hide subsequent links

## Tables

- **Table compaction**: Removes unnecessary padding in Markdown tables while preserving fenced code blocks and separator-like data cells such as `--` or `:`
- **Escaped pipes**: Keeps escaped cell pipes (`\|`) intact during table compaction, including a final escaped pipe when the optional closing cell delimiter is absent
- **Pipes in inline code**: Keeps pipes inside inline code spans in table cells from being treated as cell separators, preserving the code during compaction
- **Indented code blocks**: Lines indented with four or more spaces (CommonMark indented code blocks) are not treated as table rows, so their leading indentation and inner spacing are preserved
