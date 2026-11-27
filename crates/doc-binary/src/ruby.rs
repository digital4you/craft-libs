//! Phonetic Guide (ruby) in a Word 97-2003 document: Word stores it as an EQ field without a
//! result, `EQ \* jc2 \* "Font:MS Mincho" \* hps11 \o\ad(\s\up 10(とうきょう),東京)`, which it
//! saves to `.docx` as `w:ruby`. The field is read into the same [`InlineObject::Ruby`].
//!
//! `\* jcN` is the alignment (0 centred, 1 distributed, 2 distributed with space, 3 left, 4
//! right, 5 right over vertical text), `\* hpsN` the ruby size in half-points, `\* "Font:…"` the
//! ruby font, and `\s\upN` the raise of the ruby text in points.

use craft_wordmodel::para::{InlineObject, RubyAlign, ruby_raise};
use craft_wordmodel::props::CharProps;

/// Longest base or ruby text kept (as the DOCX reader).
const MAX: usize = 4096;

/// The ruby of an EQ field code; `None` for any other field. `base` is the base text's
/// formatting (its size is the base size).
pub(crate) fn from_field(instr: &str, base: &CharProps) -> Option<InlineObject> {
    let t = instr.trim_start();
    let name_end = t.find(|c: char| c.is_whitespace() || c == '\\').unwrap_or(t.len());
    if !t.get(..name_end)?.eq_ignore_ascii_case("eq") {
        return None;
    }
    let mut rest = t.get(name_end..)?;
    let (mut align, mut size, mut font) = (RubyAlign::Center, None, None);
    // The `\*` options, then `\o` with its alignment switches.
    loop {
        rest = rest.trim_start();
        let r = rest.strip_prefix('\\')?;
        if let Some(r) = r.strip_prefix('*') {
            let r = r.trim_start();
            if let Some(q) = r.strip_prefix('"') {
                let end = q.find('"')?;
                let v = &q[..end];
                if let Some(f) = v.strip_prefix("Font:").or_else(|| v.strip_prefix("font:")) {
                    font = Some(f.trim().to_string()).filter(|f| !f.is_empty());
                }
                rest = &q[end + 1..];
            } else {
                let end = r.find(|c: char| c.is_whitespace() || c == '\\').unwrap_or(r.len());
                let v = r[..end].to_ascii_lowercase();
                if let Some(n) = v.strip_prefix("jc").and_then(|n| n.parse::<u8>().ok()) {
                    align = match n {
                        1 => RubyAlign::DistributeLetter,
                        2 => RubyAlign::DistributeSpace,
                        3 => RubyAlign::Left,
                        4 => RubyAlign::Right,
                        5 => RubyAlign::RightVertical,
                        _ => RubyAlign::Center,
                    };
                } else if let Some(n) = v.strip_prefix("hps").and_then(|n| n.parse::<f32>().ok()) {
                    size = Some(n / 2.0);
                }
                rest = &r[end..];
            }
            continue;
        }
        let o = r.chars().next()?;
        if !o.eq_ignore_ascii_case(&'o') {
            return None;
        }
        rest = &r[1..];
        break;
    }
    loop {
        rest = rest.trim_start();
        if let Some(r) = rest.strip_prefix('(') {
            rest = r;
            break;
        }
        // `\al`, `\ac`, `\ar`, `\ad`, `\ao`: the place of the arguments, given by `\* jc` too.
        let r = rest.strip_prefix('\\')?;
        let sw = r.get(..2)?;
        if !sw.to_ascii_lowercase().starts_with('a') {
            return None;
        }
        rest = &r[2..];
    }
    let args = split_args(rest)?;
    let [ruby_arg, base_arg] = args.as_slice() else { return None };
    // The ruby argument is `\s\upN(text)`; an enclosure (`\o\ac(○,字)`) has none.
    let (raise, ruby) = raised(ruby_arg)?;
    let base_text: String = plain(base_arg).chars().take(MAX).collect();
    let ruby: String = plain(ruby).chars().take(MAX).collect();
    if base_text.is_empty() || ruby.is_empty() {
        return None;
    }
    let base_size = base.size.filter(|s| s.is_finite() && *s > 0.0).unwrap_or(0.0).min(1584.0);
    let size = size.filter(|s: &f32| s.is_finite() && *s > 0.0).unwrap_or(if base_size > 0.0 { base_size / 2.0 } else { 5.0 }).clamp(1.0, 1584.0);
    let raise = raise.filter(|r| r.is_finite() && *r >= 0.0).unwrap_or_else(|| ruby_raise(0.0, base_size.max(size), size)).min(1584.0);
    let mut props = CharProps::default();
    if let Some(f) = font {
        props.font = Some(f.clone());
        props.font_ea = Some(f);
    }
    Some(InlineObject::Ruby { base: base_text, ruby, align, size, raise, base_size, lang: String::new(), props: Box::new(props) })
}

/// The arguments up to the matching `)`, split at top-level `,` or `;`, as written.
fn split_args(s: &str) -> Option<Vec<&str>> {
    let mut args = Vec::new();
    let (mut depth, mut start) = (0usize, 0usize);
    let mut it = s.char_indices();
    while let Some((i, c)) = it.next() {
        match c {
            '\\' => {
                it.next();
            }
            '(' => depth += 1,
            ')' if depth == 0 => {
                args.push(&s[start..i]);
                return (args.len() <= 2).then_some(args);
            }
            ')' => depth -= 1,
            ',' | ';' if depth == 0 => {
                args.push(&s[start..i]);
                start = i + 1;
                if args.len() > 2 {
                    return None;
                }
            }
            _ => {}
        }
    }
    None
}

/// `\s\upN(text)`: the raise in points (if given) and the text inside the brackets.
fn raised(arg: &str) -> Option<(Option<f32>, &str)> {
    let mut r = arg.trim_start().strip_prefix('\\')?;
    r = r.strip_prefix('s').or_else(|| r.strip_prefix('S'))?;
    let mut raise = None;
    loop {
        r = r.trim_start();
        if let Some(inner) = r.strip_prefix('(') {
            let close = inner.rfind(')')?;
            if !inner[close + 1..].trim().is_empty() {
                return None;
            }
            return Some((raise, &inner[..close]));
        }
        let s = r.strip_prefix('\\')?;
        let end = s.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(s.len());
        let name = s[..end].to_ascii_lowercase();
        let num = s[end..].trim_start();
        let num_end = num.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(num.len());
        let n = num[..num_end].parse::<f32>().ok();
        match name.as_str() {
            "up" => raise = n,
            "do" => raise = n.map(|n| -n),
            "ai" | "di" => {}
            _ => return None,
        }
        r = &num[num_end..];
    }
}

/// An argument's text: escapes (`\,` `\(` `\)` `\\` `\;`) become the character; other switches
/// and brackets are dropped.
fn plain(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\\' => match it.peek().copied() {
                Some(n @ (',' | '(' | ')' | '\\' | ';')) => {
                    out.push(n);
                    it.next();
                }
                Some(_) => {
                    while it.next_if(char::is_ascii_alphabetic).is_some() {}
                    while it.next_if(|n| *n == ' ').is_some() {}
                    while it.next_if(|n| n.is_ascii_digit() || *n == '-' || *n == '.').is_some() {}
                }
                None => {}
            },
            '(' | ')' => {}
            '\t' | '\r' | '\n' => out.push(' '),
            c => out.push(c),
        }
    }
    out.trim().to_string()
}
