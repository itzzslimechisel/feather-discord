//! Discord-flavoured markdown parser.
//!
//! Produces a flat list of styled spans that the UI turns into a single text layout. The parser
//! is linear-ish, allocation-light and never produces markup: untrusted message text can only
//! ever become styled plain text, links (validated http/https) or typed mention tokens.

use crate::model::Id;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub code: bool,
    pub code_block: bool,
    pub spoiler: bool,
    pub quote: bool,
    /// 0 = normal text, 1..=3 heading levels.
    pub heading: u8,
    pub subtext: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Text,
    Link(String),
    User(Id),
    Role(Id),
    Channel(Id),
    Everyone,
    Emoji { name: String, id: Id, animated: bool },
    /// Unix seconds and Discord style letter (t, T, d, D, f, F, R).
    Timestamp(i64, char),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub text: String,
    pub style: Style,
    pub kind: Kind,
}

/// Messages are at most 4000 characters; anything longer is truncated before parsing so a
/// malicious payload cannot make layout arbitrarily expensive.
pub const MAX_INPUT: usize = 6000;

pub fn parse(src: &str) -> Vec<Span> {
    let src = if src.len() > MAX_INPUT {
        let mut end = MAX_INPUT;
        while !src.is_char_boundary(end) {
            end -= 1;
        }
        &src[..end]
    } else {
        src
    };
    let mut out = Vec::new();
    let mut rest = src;
    // Fenced code blocks are verbatim and take priority over everything else.
    while let Some(start) = rest.find("```") {
        let after = &rest[start + 3..];
        let Some(end) = after.find("```") else { break };
        parse_lines(&rest[..start], &mut out);
        let mut body = &after[..end];
        // Optional language tag on the first line.
        if let Some(nl) = body.find('\n') {
            let first = &body[..nl];
            if !first.is_empty() && first.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '#') {
                body = &body[nl + 1..];
            }
        }
        let body = body.strip_suffix('\n').unwrap_or(body);
        push(&mut out, body, Style { code_block: true, ..Style::default() }, Kind::Text);
        rest = &after[end + 3..];
        if let Some(r) = rest.strip_prefix('\n') {
            push(&mut out, "\n", Style::default(), Kind::Text);
            rest = r;
        }
    }
    parse_lines(rest, &mut out);
    out
}

fn push(out: &mut Vec<Span>, text: &str, style: Style, kind: Kind) {
    if text.is_empty() && kind == Kind::Text {
        return;
    }
    if kind == Kind::Text {
        if let Some(last) = out.last_mut() {
            if last.kind == Kind::Text && last.style == style {
                last.text.push_str(text);
                return;
            }
        }
    }
    out.push(Span { text: text.to_string(), style, kind });
}

fn parse_lines(text: &str, out: &mut Vec<Span>) {
    let mut block_quote = false;
    let mut lines = text.split('\n').peekable();
    while let Some(line) = lines.next() {
        let mut style = Style::default();
        let mut body = line;
        if block_quote {
            style.quote = true;
        } else if let Some(b) = body.strip_prefix(">>> ") {
            block_quote = true;
            style.quote = true;
            body = b;
        } else if let Some(b) = body.strip_prefix("> ") {
            style.quote = true;
            body = b;
        }
        if let Some(b) = body.strip_prefix("### ") {
            style.heading = 3;
            body = b;
        } else if let Some(b) = body.strip_prefix("## ") {
            style.heading = 2;
            body = b;
        } else if let Some(b) = body.strip_prefix("# ") {
            style.heading = 1;
            body = b;
        } else if let Some(b) = body.strip_prefix("-# ") {
            style.subtext = true;
            body = b;
        }
        let trimmed = body.trim_start();
        let indent = body.len() - trimmed.len();
        if let Some(b) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
            push(out, &" ".repeat(indent), style, Kind::Text);
            push(out, "• ", style, Kind::Text);
            body = b;
        }
        inline(body, style, out, 0);
        if lines.peek().is_some() {
            push(out, "\n", Style { quote: block_quote, ..Style::default() }, Kind::Text);
        }
    }
}

fn is_word(c: Option<char>) -> bool {
    c.is_some_and(|c| c.is_alphanumeric())
}

/// Finds a closing delimiter `d` in `s` that is not immediately preceded by a backslash.
fn find_close(s: &str, d: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = s[from..].find(d) {
        let at = from + i;
        if at > 0 && s.as_bytes()[at - 1] == b'\\' {
            from = at + d.len();
            continue;
        }
        if at == 0 {
            // Empty content: not a match (e.g. "****").
            from = at + d.len();
            continue;
        }
        return Some(at);
    }
    None
}

fn inline(s: &str, style: Style, out: &mut Vec<Span>, depth: u8) {
    if depth > 8 {
        push(out, s, style, Kind::Text);
        return;
    }
    let mut text_start = 0;
    let mut i = 0;
    let b = s.as_bytes();
    macro_rules! flush {
        () => {
            push(out, &s[text_start..i], style, Kind::Text);
        };
    }
    while i < s.len() {
        let rest = &s[i..];
        let c = b[i];
        // Escapes.
        if c == b'\\' {
            if let Some(n) = rest[1..].chars().next() {
                if n.is_ascii_punctuation() {
                    flush!();
                    push(out, &rest[1..1 + n.len_utf8()], style, Kind::Text);
                    i += 1 + n.len_utf8();
                    text_start = i;
                    continue;
                }
            }
        }
        // Inline code: verbatim.
        if c == b'`' {
            let ticks = if rest.starts_with("``") { 2 } else { 1 };
            if let Some(end) = rest[ticks..].find(&"`".repeat(ticks)).filter(|e| *e > 0) {
                flush!();
                let code = &rest[ticks..ticks + end];
                let code = if ticks == 2 { code.trim() } else { code };
                push(out, code, Style { code: true, ..style }, Kind::Text);
                i += ticks * 2 + end;
                text_start = i;
                continue;
            }
        }
        // Paired delimiters.
        let pairs: [(&str, fn(&mut Style)); 6] = [
            ("**", |st| st.bold = true),
            ("__", |st| st.underline = true),
            ("~~", |st| st.strike = true),
            ("||", |st| st.spoiler = true),
            ("*", |st| st.italic = true),
            ("_", |st| st.italic = true),
        ];
        let mut matched = false;
        for (d, apply) in pairs {
            if !rest.starts_with(d) {
                continue;
            }
            if d == "_" && is_word(s[..i].chars().next_back()) {
                break;
            }
            if d == "*" && rest[1..].starts_with(' ') {
                break;
            }
            let inner = &rest[d.len()..];
            if let Some(mut end) = find_close(inner, d) {
                // "***" closing a "**" run: the extra '*' belongs to the inner (italic) span.
                if d.len() == 2 {
                    let ch = d.as_bytes()[0];
                    while inner.as_bytes().get(end + 2) == Some(&ch) {
                        end += 1;
                    }
                }
                let after = inner[end + d.len()..].chars().next();
                if d == "_" && is_word(after) {
                    continue;
                }
                if d == "*" && inner[..end].ends_with(' ') {
                    continue;
                }
                flush!();
                let mut st = style;
                apply(&mut st);
                inline(&inner[..end], st, out, depth + 1);
                i += d.len() * 2 + end;
                text_start = i;
                matched = true;
            }
            break;
        }
        if matched {
            continue;
        }
        // Masked link [label](https://...)
        if c == b'[' {
            if let Some((label, url, len)) = masked_link(rest) {
                flush!();
                let mut sub = Vec::new();
                inline(label, style, &mut sub, depth + 1);
                for sp in sub {
                    if sp.kind == Kind::Text {
                        out.push(Span { kind: Kind::Link(url.to_string()), ..sp });
                    } else {
                        out.push(sp);
                    }
                }
                i += len;
                text_start = i;
                continue;
            }
        }
        // Angle-bracket tokens.
        if c == b'<' {
            if let Some(end) = rest.find('>') {
                if let Some(kind) = angle_token(&rest[1..end]) {
                    flush!();
                    let text = match &kind {
                        Kind::Link(u) => u.clone(),
                        Kind::Emoji { name, .. } => format!(":{name}:"),
                        _ => rest[..=end].to_string(),
                    };
                    out.push(Span { text, style, kind });
                    i += end + 1;
                    text_start = i;
                    continue;
                }
            }
        }
        if c == b'@' && (rest.starts_with("@everyone") || rest.starts_with("@here")) {
            flush!();
            let n = if rest.starts_with("@everyone") { 9 } else { 5 };
            out.push(Span { text: rest[..n].to_string(), style, kind: Kind::Everyone });
            i += n;
            text_start = i;
            continue;
        }
        // Bare URLs.
        if (c == b'h') && (rest.starts_with("https://") || rest.starts_with("http://")) && !is_word(s[..i].chars().next_back()) {
            let len = bare_url_len(rest);
            if len > 8 {
                flush!();
                out.push(Span { text: rest[..len].to_string(), style, kind: Kind::Link(rest[..len].to_string()) });
                i += len;
                text_start = i;
                continue;
            }
        }
        i += rest.chars().next().map_or(1, char::len_utf8);
    }
    push(out, &s[text_start..], style, Kind::Text);
}

fn bare_url_len(s: &str) -> usize {
    let mut end = s.find(|c: char| c.is_whitespace() || c == '<' || c == '>').unwrap_or(s.len());
    // Trim trailing punctuation, keeping balanced parentheses (Wikipedia-style URLs).
    loop {
        let Some(last) = s[..end].chars().next_back() else { break };
        let opens = s[..end].matches('(').count();
        let closes = s[..end].matches(')').count();
        if ".,:;!?'\"*_~|".contains(last) || (last == ')' && closes > opens) {
            end -= last.len_utf8();
        } else {
            break;
        }
    }
    end
}

pub fn safe_url(u: &str) -> bool {
    (u.starts_with("https://") || u.starts_with("http://"))
        && u.len() <= 2048
        && !u.chars().any(|c| c.is_control() || c.is_whitespace())
        && url::Url::parse(u).is_ok_and(|p| p.host_str().is_some())
}

fn masked_link(s: &str) -> Option<(&str, &str, usize)> {
    let close = s.find("](")?;
    let label = &s[1..close];
    if label.is_empty() || label.contains('\n') || label.len() > 256 {
        return None;
    }
    let url_start = close + 2;
    let url_end = url_start + s[url_start..].find(')')?;
    let url = s[url_start..url_end].trim();
    let url = url.strip_prefix('<').and_then(|u| u.strip_suffix('>')).unwrap_or(url);
    safe_url(url).then_some((label, url, url_end + 1))
}

fn angle_token(t: &str) -> Option<Kind> {
    if t.starts_with("http") {
        return safe_url(t).then(|| Kind::Link(t.to_string()));
    }
    let id = |s: &str| s.parse::<u64>().ok().map(Id);
    if let Some(r) = t.strip_prefix("@&") {
        return id(r).map(Kind::Role);
    }
    if let Some(r) = t.strip_prefix("@!").or_else(|| t.strip_prefix('@')) {
        return id(r).map(Kind::User);
    }
    if let Some(r) = t.strip_prefix('#') {
        return id(r).map(Kind::Channel);
    }
    if let Some(r) = t.strip_prefix("t:") {
        let mut it = r.splitn(2, ':');
        let secs: i64 = it.next()?.parse().ok()?;
        let style = it.next().and_then(|s| s.chars().next()).unwrap_or('f');
        return "tTdDfFR".contains(style).then_some(Kind::Timestamp(secs, style));
    }
    let (animated, r) = match t.strip_prefix("a:") {
        Some(r) => (true, r),
        None => (false, t.strip_prefix(':')?),
    };
    let (name, eid) = r.rsplit_once(':')?;
    if !name.is_empty() && name.len() <= 32 && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return id(eid).map(|id| Kind::Emoji { name: name.to_string(), id, animated });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(v: &[Span]) -> Vec<(&str, Style)> {
        v.iter().map(|s| (s.text.as_str(), s.style)).collect()
    }

    #[test]
    fn plain_text_is_one_span() {
        let v = parse("hello world");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].text, "hello world");
    }

    #[test]
    fn emphasis() {
        let v = parse("a **b** *c* __d__ ~~e~~ ||f|| `g`");
        let t = texts(&v);
        assert!(t.iter().any(|(s, st)| *s == "b" && st.bold));
        assert!(t.iter().any(|(s, st)| *s == "c" && st.italic));
        assert!(t.iter().any(|(s, st)| *s == "d" && st.underline));
        assert!(t.iter().any(|(s, st)| *s == "e" && st.strike));
        assert!(t.iter().any(|(s, st)| *s == "f" && st.spoiler));
        assert!(t.iter().any(|(s, st)| *s == "g" && st.code));
    }

    #[test]
    fn nested_and_unclosed() {
        let v = parse("**bold *both***");
        assert!(v.iter().any(|s| s.text == "both" && s.style.bold && s.style.italic));
        let v = parse("2 * 3 = 6 and **unclosed");
        assert_eq!(v.iter().map(|s| s.text.as_str()).collect::<String>(), "2 * 3 = 6 and **unclosed");
    }

    #[test]
    fn snake_case_is_not_italic() {
        let v = parse("call some_function_name now");
        assert!(v.iter().all(|s| !s.style.italic));
    }

    #[test]
    fn code_is_verbatim() {
        let v = parse("`**not bold**` and\n```rust\nfn main() { <@1> }\n```");
        assert!(v.iter().any(|s| s.text == "**not bold**" && s.style.code));
        assert!(v.iter().any(|s| s.text == "fn main() { <@1> }" && s.style.code_block));
        assert!(!v.iter().any(|s| matches!(s.kind, Kind::User(_))));
    }

    #[test]
    fn mentions_emoji_timestamps() {
        let v = parse("hi <@123> <@!4> <@&5> <#6> <:wave:77> <a:dance:78> <t:1618953630:R> @everyone");
        let kinds: Vec<&Kind> = v.iter().map(|s| &s.kind).filter(|k| **k != Kind::Text).collect();
        assert_eq!(kinds[0], &Kind::User(Id(123)));
        assert_eq!(kinds[1], &Kind::User(Id(4)));
        assert_eq!(kinds[2], &Kind::Role(Id(5)));
        assert_eq!(kinds[3], &Kind::Channel(Id(6)));
        assert_eq!(kinds[4], &Kind::Emoji { name: "wave".into(), id: Id(77), animated: false });
        assert_eq!(kinds[5], &Kind::Emoji { name: "dance".into(), id: Id(78), animated: true });
        assert_eq!(kinds[6], &Kind::Timestamp(1618953630, 'R'));
        assert_eq!(kinds[7], &Kind::Everyone);
        let v = parse("quiet <https://example.com/x>");
        assert_eq!(v[1].kind, Kind::Link("https://example.com/x".into()));
    }

    #[test]
    fn links_are_validated() {
        let v = parse("see https://example.com/a_(b). and [docs](https://x.org/y) and [bad](javascript:alert(1))");
        let links: Vec<&str> = v
            .iter()
            .filter_map(|s| if let Kind::Link(u) = &s.kind { Some(u.as_str()) } else { None })
            .collect();
        assert_eq!(links, vec!["https://example.com/a_(b)", "https://x.org/y"]);
        assert!(v.iter().any(|s| s.text.contains("[bad](javascript:alert(1))")));
    }

    #[test]
    fn block_elements() {
        let v = parse("# Title\n> quoted\n- item\n-# small");
        assert!(v.iter().any(|s| s.text == "Title" && s.style.heading == 1));
        assert!(v.iter().any(|s| s.text == "quoted" && s.style.quote));
        assert!(v.iter().any(|s| s.text.contains("• item")));
        assert!(v.iter().any(|s| s.text == "small" && s.style.subtext));
    }

    #[test]
    fn escapes() {
        let v = parse(r"\*not italic\*");
        assert_eq!(v.iter().map(|s| s.text.as_str()).collect::<String>(), "*not italic*");
        assert!(v.iter().all(|s| !s.style.italic));
    }

    #[test]
    fn pathological_input_is_bounded() {
        let evil = "*_~|`".repeat(5000);
        let t = std::time::Instant::now();
        let _ = parse(&evil);
        let _ = parse(&"**a".repeat(3000));
        let _ = parse(&"[".repeat(6000));
        assert!(t.elapsed() < std::time::Duration::from_millis(500), "{:?}", t.elapsed());
    }

    #[test]
    fn utf8_safe() {
        let v = parse("héllo *wörld* 🎉 <@1>");
        assert!(v.iter().any(|s| s.text == "wörld" && s.style.italic));
    }
}
