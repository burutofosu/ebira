//! Text for the models and for people: clipping by characters, quotes checked against the
//! records, and short project names.

/// At most `max` characters: the head and the tail, with the cut stated in the middle.
pub fn clip_middle(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let head = max * 2 / 3;
    let tail = max - head;
    let mut out: String = text.chars().take(head).collect();
    out.push_str(&format!(
        " … [{} characters left out] … ",
        count - head - tail
    ));
    out.extend(text.chars().skip(count - tail));
    out
}

/// At most `max` characters from the start, with the cut stated.
pub fn clip_head(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push_str(&format!(" … [{} more characters]", count - max));
    out
}

/// The text on one line: line breaks become " / ", runs of spaces and tabs shrink to one, and
/// terminal color codes (in tool output) are dropped.
pub fn one_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_space = false;
    let mut characters = text.trim().chars();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            // An escape sequence: `ESC [` parameters, then a final byte from @ to ~.
            if characters.clone().next() == Some('[') {
                characters.next();
                for code in characters.by_ref() {
                    if ('@'..='~').contains(&code) {
                        break;
                    }
                }
            }
            continue;
        }
        if character == '\n' {
            if !out.ends_with(" / ") {
                out.push_str(" / ");
            }
            last_space = true;
        } else if character == ' ' || character == '\t' || character == '\r' {
            if !last_space {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(character);
            last_space = false;
        }
    }
    out
}

/// A window of the text around the first of `terms` it contains: the opening, then the part
/// around the match.
pub fn window(text: &str, terms: &[String], max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let lead = max / 4;
    let found = terms
        .iter()
        .filter_map(|term| find_folded(text, term))
        .min()
        .map(|byte| text[..byte].chars().count());
    match found {
        Some(position) if position > lead => {
            let span = max - lead;
            let start = position.saturating_sub(span / 3).max(lead);
            let mut out: String = text.chars().take(lead).collect();
            out.push_str(" … ");
            out.extend(text.chars().skip(start).take(span));
            if start + span < count {
                out.push_str(" …");
            }
            out
        }
        _ => clip_head(text, max),
    }
}

/// Where `term` first occurs in `text`, ASCII letters folded as the search folds them.
fn find_folded(text: &str, term: &str) -> Option<usize> {
    text.to_ascii_lowercase().find(&term.to_ascii_lowercase())
}

/// Kana or a kanji, the Japanese that a space may or may not part from Latin letters.
fn is_kana_or_kanji(character: char) -> bool {
    matches!(character,
        '\u{3005}'..='\u{3007}' // 々 〆 〇
        | '\u{3041}'..='\u{3096}' // hiragana
        | '\u{309D}'..='\u{309F}'
        | '\u{30A1}'..='\u{30FA}' // katakana
        | '\u{30FC}'..='\u{30FF}' // ー and the iteration marks
        | '\u{31F0}'..='\u{31FF}'
        | '\u{3400}'..='\u{4DBF}' // kanji
        | '\u{4E00}'..='\u{9FFF}'
        | '\u{F900}'..='\u{FAFF}'
        | '\u{20000}'..='\u{3134F}'
        | '\u{FF66}'..='\u{FF9F}' // half-width katakana
    )
}

/// Whether a space may or may not stand between the two: ASCII letters or digits on one side,
/// kana or kanji on the other.
fn spacing_boundary(left: char, right: char) -> bool {
    (left.is_ascii_alphanumeric() && is_kana_or_kanji(right))
        || (is_kana_or_kanji(left) && right.is_ascii_alphanumeric())
}

/// The term written with a single space at every boundary between ASCII letters or digits and
/// kana or kanji, and with none: Japanese is written both ways (`CSV 取り込み`, `CSV取り込み`),
/// and the search matches spacing literally. Only the forms that differ from the term, the
/// spaced first; none when it has no such boundary. A run of spaces is left as it is.
pub fn spacing_variants(term: &str) -> Vec<String> {
    let characters: Vec<char> = term.chars().collect();
    let mut spaced = String::with_capacity(term.len() + 8);
    let mut joined = String::with_capacity(term.len());
    for (index, &character) in characters.iter().enumerate() {
        let before = index.checked_sub(1).map(|at| characters[at]);
        let after = characters.get(index + 1).copied();
        match (before, after) {
            (Some(left), Some(right)) if character == ' ' && spacing_boundary(left, right) => {
                spaced.push(' ');
                continue;
            }
            (Some(left), _) if spacing_boundary(left, character) => spaced.push(' '),
            _ => {}
        }
        spaced.push(character);
        joined.push(character);
    }
    let mut variants = Vec::new();
    for variant in [spaced, joined] {
        if variant != term && !variants.contains(&variant) {
            variants.push(variant);
        }
    }
    variants
}

/// The text compared without spaces, with full-width ASCII folded and ASCII lowercased.
pub fn normalize(text: &str) -> String {
    text.chars()
        .filter_map(|character| {
            let folded = match character as u32 {
                0xFF01..=0xFF5E => char::from_u32(character as u32 - 0xFEE0).unwrap_or(character),
                _ => character,
            };
            (!folded.is_whitespace()).then(|| folded.to_ascii_lowercase())
        })
        .collect()
}

/// Whether the quote appears in the text, its parts in order, allowing `…` (or `...`) for
/// what it left out, and ` / ` for a line break (as `one_line` shows one).
pub fn quote_found(quote: &str, text: &str) -> bool {
    let quote = quote
        .trim()
        .trim_start_matches(['「', '『', '"'])
        .trim_end_matches(['」', '』', '"']);
    let parts: Vec<String> = quote
        .split('…')
        .flat_map(|part| part.split("..."))
        .flat_map(|part| part.split(" / "))
        .map(normalize)
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        return false;
    }
    let haystack = normalize(text);
    let mut from = 0;
    for part in parts {
        match haystack[from..].find(&part) {
            Some(index) => from += index + part.len(),
            None => return false,
        }
    }
    true
}

/// The last two parts of a working directory (`C:\Users\me\work\game` is `work/game`), a
/// file URI decoded.
pub fn project_from_cwd(cwd: &str) -> String {
    let path = cwd
        .strip_prefix("file:///")
        .map(percent_decode)
        .unwrap_or_else(|| cwd.to_string());
    let parts: Vec<&str> = path
        .split(['\\', '/'])
        .filter(|part| !part.is_empty())
        .collect();
    match parts.len() {
        0 => String::new(),
        1 => parts[0].to_string(),
        n => format!("{}/{}", parts[n - 2], parts[n - 1]),
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                out.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clips_by_characters_not_bytes() {
        let text = "木".repeat(50);
        let clipped = clip_middle(&text, 30);
        assert!(clipped.starts_with(&"木".repeat(20)));
        assert!(clipped.contains("[20 characters left out]"));
        assert!(clipped.ends_with(&"木".repeat(10)));
        assert_eq!(clip_head("短い", 10), "短い");
        assert_eq!(clip_head("abcdef", 4), "abcd … [2 more characters]");
    }

    #[test]
    fn finds_quotes_despite_spacing_width_and_ellipsis() {
        let record = "いくつか見つかりました　凡例が消えてたり　拡大の後ずれたりするのは急務かな　ＪＳＯＮにするべき？";
        assert!(quote_found("凡例が消えてたり 拡大の後ずれたり", record));
        assert!(quote_found("「いくつか見つかりました…急務かな」", record));
        assert!(quote_found("jsonにするべき", record));
        assert!(!quote_found("凡例が残ってたり", record));
        assert!(!quote_found("急務かな…いくつか", record));
        assert!(!quote_found("…", record));
        // A quote copied from a line shown with `one_line` marks the break with " / ".
        let message = "古い地図は残す方がいい可能性がある\n色こそが邪魔な可能性がある";
        assert!(quote_found(&one_line(message), message));
        assert!(quote_found("可能性がある / 色こそが", message));
        assert!(!quote_found(
            "色こそが / 可能性がある",
            "可能性がある色こそが"
        ));
    }

    #[test]
    fn names_projects() {
        assert_eq!(project_from_cwd("C:\\Users\\me\\work\\game"), "work/game");
        assert_eq!(
            project_from_cwd("file:///C:/Users/me/work/%E5%9C%B0%E5%9B%B3"),
            "work/地図"
        );
        assert_eq!(project_from_cwd(""), "");
    }

    #[test]
    fn windows_show_the_opening_and_the_match() {
        let text = format!("{}凡例が消えてる{}", "あ".repeat(100), "い".repeat(100));
        let shown = window(&text, &["消えて".to_string()], 40);
        assert!(shown.starts_with("ああああああああああ"));
        assert!(shown.contains("凡例が消えてる"));
        let english = format!("{}The Codex reply{}", "x".repeat(100), "y".repeat(100));
        assert!(window(&english, &["codex".to_string()], 40).contains("Codex reply"));
    }

    #[test]
    fn spaces_latin_letters_from_japanese_both_ways() {
        let variants = spacing_variants;
        assert_eq!(variants("csv取り込み"), vec!["csv 取り込み"]);
        assert_eq!(variants("CSV 取り込み"), vec!["CSV取り込み"]);
        assert_eq!(variants("取り込みCSV"), vec!["取り込み CSV"]);
        assert_eq!(variants("v2の設定"), vec!["v2 の設定"], "digits count too");
        assert_eq!(variants("ｶﾀﾛｸﾞAPI"), vec!["ｶﾀﾛｸﾞ API"], "half-width katakana");
        assert_eq!(
            variants("サーバーAPI"),
            vec!["サーバー API"],
            "the long vowel mark is kana"
        );
        // Both boundaries get a space, or neither does; the mixed form is the term itself.
        assert_eq!(
            variants("Rust製のCLI"),
            vec!["Rust 製の CLI"],
            "one form is the term"
        );
        assert_eq!(
            variants("Rust 製のCLI"),
            vec!["Rust 製の CLI", "Rust製のCLI"]
        );
        assert_eq!(variants("Rust 製の CLI"), vec!["Rust製のCLI"]);
    }

    #[test]
    fn leaves_other_spacing_alone() {
        for term in [
            "dark mode",
            "ダーク モード",
            "取り込み",
            "README.md",
            "CSV  取り込み",
            "C++ の",
            "ＣＳＶ取り込み",
            "csv「取り込み」",
            "",
        ] {
            assert!(spacing_variants(term).is_empty(), "{term:?}");
        }
        assert_eq!(
            spacing_variants("ダーク モードをON"),
            vec!["ダーク モードを ON"],
            "a space between two Japanese words stays"
        );
        assert_eq!(spacing_variants("a 取 b"), vec!["a取b"]);
    }

    #[test]
    fn writes_one_line() {
        assert_eq!(one_line("a\n\nb  c\t d"), "a / b c d");
        assert_eq!(
            one_line("\u{1b}[32m➜\u{1b}[39m  \u{1b}[1mLocal\u{1b}[22m: ok"),
            "➜ Local: ok"
        );
    }
}
