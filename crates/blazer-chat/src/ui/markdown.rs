//! Message text → renderable spans: Rocket.Chat markdown (`*bold*`,
//! `_italic_`, `` `code` ``, `~strike~`, links, `@mentions`) plus
//! `:custom_emoji:` tokens resolved through the caller's emoji cache.
//!
//! Nesting combines style flags on flat spans (`_курсив~страйк~_` yields
//! italic and italic+strike runs): the parser keeps a style stack with
//! look-ahead closers, and the renderer stays flat. Code spans win over
//! markers inside them; unclosed markers stay literal.

/// One renderable piece of a message line.
#[derive(Clone, Debug, PartialEq)]
pub enum Span {
    /// Plain or styled text.
    Text {
        text: String,
        bold: bool,
        italic: bool,
        code: bool,
        strike: bool,
    },
    /// A link: the label shows, the url is the destination.
    Link { label: String, url: String },
    /// `@user` mention.
    Mention(String),
    /// A custom emoji resolved from the cache: the image path.
    Emoji { path: String },
    /// A fenced code block: the verbatim body, rendered as its own
    /// monospace block — markers inside are never styled.
    CodeBlock { body: String },
}

/// One open emphasis: the marker that opened it and its contribution.
#[derive(Clone, Copy)]
struct OpenMarker {
    marker: char,
    bold: bool,
    italic: bool,
    code: bool,
    strike: bool,
}

impl OpenMarker {
    fn style(&self) -> Style {
        Style {
            bold: self.bold,
            italic: self.italic,
            code: self.code,
            strike: self.strike,
        }
    }
}

/// The emphasis flags in effect at the current position.
#[derive(Clone, Copy, Default)]
struct Style {
    bold: bool,
    italic: bool,
    code: bool,
    strike: bool,
}

impl Style {
    fn merge(self, marker: &OpenMarker) -> Style {
        Style {
            bold: self.bold || marker.bold,
            italic: self.italic || marker.italic,
            code: self.code || marker.code,
            strike: self.strike || marker.strike,
        }
    }
}

/// The emphasis markers, closest first.
const MARKERS: [OpenMarker; 4] = [
    OpenMarker {
        marker: '*',
        bold: true,
        italic: false,
        code: false,
        strike: false,
    },
    OpenMarker {
        marker: '_',
        bold: false,
        italic: true,
        code: false,
        strike: false,
    },
    OpenMarker {
        marker: '`',
        bold: false,
        italic: false,
        code: true,
        strike: false,
    },
    OpenMarker {
        marker: '~',
        bold: false,
        italic: false,
        code: false,
        strike: true,
    },
];

/// Parses one line of message text into spans. `emoji_path` resolves a
/// custom-emoji name to its cached image path; unknown names stay literal.
/// A message split by [`split_fences`]: prose goes through the inline
/// parser line by line; fence bodies never do.
pub enum Segment<'a> {
    Prose(&'a str),
    Fence { body: String },
}

/// Splits a message into prose lines and fenced code blocks. A fence
/// opens on a line whose trimmed form starts with ``` and runs to the
/// next such line — or to the end of the text, so an unterminated fence
/// keeps its body verbatim instead of swallowing nothing. A line that
/// both opens and closes with ``` on the same line stays prose (inline
/// code). The fence language tag after the backticks is ignored for
/// now.
pub fn split_fences(text: &str) -> Vec<Segment<'_>> {
    let mut segments: Vec<Segment<'_>> = Vec::new();
    let mut fence: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim_start();
        let opens = trimmed.starts_with("```");
        let closes_same_line = opens && trimmed[3..].trim_end().ends_with("```");
        match fence.as_mut() {
            None => {
                if opens && !closes_same_line {
                    fence = Some(String::new());
                } else {
                    segments.push(Segment::Prose(line));
                }
            }
            Some(body) => {
                if opens {
                    segments.push(Segment::Fence {
                        body: std::mem::take(body),
                    });
                    fence = None;
                } else {
                    body.push_str(line);
                    body.push('\n');
                }
            }
        }
    }
    if let Some(body) = fence {
        segments.push(Segment::Fence { body });
    }
    segments
}

pub fn parse(text: &str, emoji_path: &dyn Fn(&str) -> Option<String>) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut plain = String::new();
    let mut rest = text;
    // Open emphasis, outermost first; the effective style is the merge.
    let mut stack: Vec<OpenMarker> = Vec::new();

    'scan: while !rest.is_empty() {
        let effective = style_of(&stack);
        // Custom emoji: :name: with a cache hit wins over everything else.
        if let Some(after) = rest.strip_prefix(':')
            && let Some(end) = after.find(':')
        {
            let name = &after[..end];
            if is_emoji_name(name)
                && let Some(path) = emoji_path(name)
            {
                flush_plain(&mut plain, &mut spans, Some(effective));
                spans.push(Span::Emoji { path });
                rest = &after[end + 1..];
                continue 'scan;
            }
        }

        // Links: bare urls and [label](url).
        if let Some(url) = prefix_url(rest) {
            flush_plain(&mut plain, &mut spans, Some(effective));
            let length = url.len();
            spans.push(Span::Link {
                label: url.to_owned(),
                url: url.to_owned(),
            });
            rest = &rest[length..];
            continue 'scan;
        }
        if let Some(rest_after) = rest.strip_prefix('[')
            && let Some((label, url, tail)) = labeled_link(rest_after)
        {
            flush_plain(&mut plain, &mut spans, Some(effective));
            spans.push(Span::Link { label, url });
            rest = tail;
            continue 'scan;
        }

        // Mentions: @name at a token boundary.
        if let Some(after) = rest.strip_prefix('@')
            && let Some(name) = prefix_token(after)
        {
            flush_plain(&mut plain, &mut spans, Some(effective));
            spans.push(Span::Mention(format!("@{name}")));
            rest = &after[name.len()..];
            continue 'scan;
        }

        // Emphasis: *bold*, _italic_, `code`, ~strike~ — nestable. A
        // marker already on the stack closes the innermost run of its
        // kind; otherwise it opens when a closer exists ahead (unclosed
        // markers stay literal). Inside code, only the closing backtick
        // acts.
        for open in MARKERS {
            if !rest.starts_with(open.marker) {
                continue;
            }
            let after = &rest[open.marker.len_utf8()..];
            let closes = stack
                .iter()
                .any(|open_on_stack| open_on_stack.marker == open.marker);
            let has_closer =
                !open.code && after.contains(open.marker) || open.code && after.contains('`');
            // Inside code, only the closing backtick acts.
            if closes && (!effective.code || open.code) {
                // The text before this closer sits at the merged style of
                // everything open, including the marker being closed.
                flush_plain(&mut plain, &mut spans, Some(effective));
                // Pop inner levels up to and including this marker.
                while let Some(open_on_stack) = stack.pop() {
                    if open_on_stack.marker == open.marker {
                        break;
                    }
                }
                rest = after;
                continue 'scan;
            }
            if has_closer && (!effective.code || open.code) {
                flush_plain(&mut plain, &mut spans, Some(style_of(&stack)));
                stack.push(open);
                rest = after;
                continue 'scan;
            }
        }

        // Nothing matched: one character into the plain buffer.
        let boundary = rest
            .char_indices()
            .nth(1)
            .map(|(index, _)| index)
            .unwrap_or(rest.len());
        plain.push_str(&rest[..boundary]);
        rest = &rest[boundary..];
    }
    flush_plain(&mut plain, &mut spans, stack.last().map(OpenMarker::style));
    spans
}

/// The merged style of the open markers.
fn style_of(stack: &[OpenMarker]) -> Style {
    stack
        .iter()
        .fold(Style::default(), |style, open| style.merge(open))
}

fn flush_plain(plain: &mut String, spans: &mut Vec<Span>, style: Option<Style>) {
    if !plain.is_empty() {
        let style = style.unwrap_or_default();
        spans.push(Span::Text {
            text: std::mem::take(plain),
            bold: style.bold,
            italic: style.italic,
            code: style.code,
            strike: style.strike,
        });
    }
}

/// Emoji names: letters, digits, `_`, `-`, `+` — custom and unicode sets.
fn is_emoji_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '+')
        })
}

/// A bare url at the start of `rest`, if any.
fn prefix_url(rest: &str) -> Option<&str> {
    let scheme_length = if rest.starts_with("https://") {
        8
    } else if rest.starts_with("http://") {
        7
    } else {
        return None;
    };
    let end = rest[scheme_length..]
        .find(|character: char| character.is_whitespace() || matches!(character, ')' | '<'))
        .map(|index| scheme_length + index)
        .unwrap_or(rest.len());
    if end > scheme_length {
        Some(&rest[..end])
    } else {
        None
    }
}

/// `[label](url)` after the opening bracket: returns label, url, tail.
fn labeled_link(rest: &str) -> Option<(String, String, &str)> {
    let label_end = rest.find(']')?;
    let label = &rest[..label_end];
    let after_label = &rest[label_end + 1..];
    let full_url = after_label.strip_prefix('(')?;
    let url_end = full_url.find(')')?;
    let url = &full_url[..url_end];
    if label.is_empty() || url.is_empty() {
        return None;
    }
    Some((label.to_owned(), url.to_owned(), &full_url[url_end + 1..]))
}

/// An identifier at the start of `rest` (mentions, token names).
fn prefix_token(rest: &str) -> Option<&str> {
    let end = rest
        .find(|character: char| {
            !(character.is_alphanumeric() || matches!(character, '_' | '.' | '-'))
        })
        .unwrap_or(rest.len());
    if end > 0 { Some(&rest[..end]) } else { None }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests assert the expected path
mod tests {
    use super::*;

    fn no_emoji(_: &str) -> Option<String> {
        None
    }

    fn cached(name: &str) -> Option<String> {
        (name == "heart_21").then(|| "/emoji/heart_21.gif".to_owned())
    }

    #[test]
    fn plain_text_stays_one_span() {
        let spans = parse("просто сообщение", &no_emoji);
        assert_eq!(spans.len(), 1);
        assert!(
            matches!(&spans[0], Span::Text { text, bold: false, .. } if text == "просто сообщение")
        );
    }

    #[test]
    fn emphasis_and_code_decode() {
        let spans = parse("*важно* и `код` и _наклон_", &no_emoji);
        assert_eq!(spans.len(), 5);
        assert!(matches!(&spans[0], Span::Text { bold: true, text, .. } if text == "важно"));
        assert!(matches!(&spans[2], Span::Text { code: true, text, .. } if text == "код"));
        assert!(matches!(&spans[4], Span::Text { italic: true, text, .. } if text == "наклон"));
    }

    #[test]
    fn links_and_mentions_decode() {
        let spans = parse("см. https://example.com/x и @someuser", &no_emoji);
        assert!(matches!(&spans[1], Span::Link { url, .. } if url == "https://example.com/x"));
        assert!(matches!(&spans[3], Span::Mention(name) if name == "@someuser"));
    }

    #[test]
    fn labeled_links_decode() {
        let spans = parse("[док](https://docs.rs)", &no_emoji);
        assert!(matches!(
            &spans[0],
            Span::Link { label, url } if label == "док" && url == "https://docs.rs"
        ));
    }

    #[test]
    fn cached_emoji_becomes_an_image_span() {
        let spans = parse("держи :heart_21: от меня", &cached);
        assert_eq!(spans.len(), 3);
        assert!(matches!(
            &spans[1],
            Span::Emoji { path } if path == "/emoji/heart_21.gif"
        ));
    }

    #[test]
    fn unknown_emoji_names_stay_literal() {
        let spans = parse(":не-эмодзи:", &cached);
        assert_eq!(spans.len(), 1);
        assert!(matches!(&spans[0], Span::Text { text, .. } if text == ":не-эмодзи:"));
    }

    #[test]
    fn nested_emphasis_combines_flags() {
        // Inner strike inside outer italic: two runs, flags combined.
        let spans = parse("_курсив~страйк~_", &no_emoji);
        assert_eq!(spans.len(), 2);
        assert!(
            matches!(&spans[0], Span::Text { italic: true, strike: false, text, .. } if text == "курсив")
        );
        assert!(
            matches!(&spans[1], Span::Text { italic: true, strike: true, text, .. } if text == "страйк")
        );

        // Bold+italic on one run.
        let spans = parse("*_обе_*", &no_emoji);
        assert_eq!(spans.len(), 1);
        assert!(matches!(
            &spans[0],
            Span::Text { bold: true, italic: true, text, .. } if text == "обе"
        ));
    }

    #[test]
    fn code_swallows_markers_inside_it() {
        // Inside code, markers stay literal: the `*` in the caption does
        // not open bold, and the backtick run holds until its closer.
        let spans = parse("*`код * не жирный`*", &no_emoji);
        assert_eq!(spans.len(), 1);
        assert!(
            matches!(&spans[0], Span::Text { code: true, text, .. } if text == "код * не жирный")
        );
    }

    #[test]
    fn unclosed_markers_stay_literal() {
        let spans = parse("2 * 3 = 6 и _незакрыт", &no_emoji);
        // "2 " plain; "* 3 = 6 и _незакрыт" — no closing '*' → literal.
        assert!(spans.iter().all(|span| matches!(span, Span::Text { .. })));
    }

    #[test]
    fn fences_split_verbatim_bodies() {
        let text = "before\n```rust\nlet x = *bold*;\n```\nafter";
        let segments = split_fences(text);
        assert_eq!(segments.len(), 3);
        assert!(matches!(segments[0], Segment::Prose("before")));
        assert!(
            matches!(&segments[1], Segment::Fence { body } if body == "let x = *bold*;\n"),
            "the fence body is verbatim — markers inside never reach the inline parser"
        );
        assert!(matches!(segments[2], Segment::Prose("after")));
    }

    #[test]
    fn unterminated_fence_keeps_the_rest_verbatim() {
        let segments = split_fences("```py\nprint(1)\nprint(2)");
        assert_eq!(segments.len(), 1);
        assert!(matches!(&segments[0], Segment::Fence { body } if body == "print(1)\nprint(2)\n"));
    }

    #[test]
    fn single_line_triple_backticks_stay_prose() {
        // A line that opens AND closes on the same line never opens a
        // fence — only a fence-opening line does.
        let segments = split_fences("```code```");
        assert_eq!(segments.len(), 1);
        assert!(matches!(segments[0], Segment::Prose("```code```")));
    }
}
