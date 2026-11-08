//! Pure text helpers: session names, the "name:" address, message chunks,
//! the colored tag that heads a session's messages, local timestamps.

use regex::Regex;
use std::sync::OnceLock;
use unicode_normalization::UnicodeNormalization;

/// Telegram cuts at 4096 characters; stay well below.
pub const LIMIT: usize = 3800;
const EMOJIS: [&str; 6] = ["🟦", "🟩", "🟧", "🟪", "🟨", "🟫"];

/// "Acme Backénd" → "acme-backend": lowercase ASCII, no accents or
/// spaces, at most 40 characters.
pub fn normal(name: &str) -> String {
    let plain: String = name.nfkd().filter(char::is_ascii).collect();
    let lowered = plain.to_lowercase();
    let mut out = String::new();
    let mut in_run = false;
    for c in lowered.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-') {
            out.push(c);
            in_run = false;
        } else if !in_run {
            out.push('-');
            in_run = true;
        }
    }
    out.trim_matches(|c| c == '-' || c == '.')
        .chars()
        .take(40)
        .collect()
}

/// Standard CRC-32 (zlib's), so a session keeps the color it had.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// A session's name as Telegram shows it: after its machine's relay prefix
/// ("pc-acme-backend") outside the main machine, whose prefix is empty.
pub fn display(name: &str, prefix: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}-{name}")
    }
}

/// "🟪 acme-backend": the header of a session's messages outside topics.
pub fn tag(name: &str, prefix: &str) -> String {
    format!("{} {}", EMOJIS[crc32(name.as_bytes()) as usize % EMOJIS.len()], display(name, prefix))
}

/// HTML-escapes `&`, `<` and `>` (Python's `html.escape(…, quote=False)`).
pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Paragraph-aligned parts of at most `LIMIT` characters.
pub fn chunks(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    for paragraph in text.split('\n') {
        let mut paragraph: Vec<char> = paragraph.chars().collect();
        while paragraph.len() > LIMIT {
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
            parts.push(paragraph[..LIMIT].iter().collect());
            paragraph.drain(..LIMIT);
        }
        let paragraph: String = paragraph.into_iter().collect();
        if current.chars().count() + paragraph.chars().count() + 1 > LIMIT {
            let full = std::mem::replace(&mut current, paragraph);
            if !full.is_empty() {
                parts.push(full);
            }
        } else if current.is_empty() {
            current = paragraph;
        } else {
            current = format!("{current}\n{paragraph}");
        }
    }
    if !current.trim().is_empty() {
        parts.push(current);
    }
    parts
}

/// A message's address: "name: rest", "name : rest" or "@name rest".
pub struct Address {
    pub typed: String,
    pub rest: String,
}

pub fn address(text: &str) -> Option<Address> {
    static PREFIX: OnceLock<Regex> = OnceLock::new();
    let prefix = PREFIX.get_or_init(|| {
        Regex::new(r"(?s)^\s*(?:@(?P<at>[\w.-]+)[\s,:]+|(?P<name>[\w.-]+)\s*:\s*)(?P<rest>.*)")
            .expect("valid address pattern")
    });
    let found = prefix.captures(text)?;
    let typed = found.name("at").or_else(|| found.name("name"))?.as_str();
    Some(Address {
        typed: typed.to_string(),
        rest: found.name("rest").map_or("", |m| m.as_str()).to_string(),
    })
}

/// Local calendar fields of a timestamp.
pub struct LocalTime {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
}

pub fn local_time(at: f64) -> LocalTime {
    let seconds = at as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::localtime_r(&seconds, &mut tm);
    }
    LocalTime {
        year: tm.tm_year + 1900,
        month: (tm.tm_mon + 1) as u32,
        day: tm.tm_mday as u32,
        hour: tm.tm_hour as u32,
        minute: tm.tm_min as u32,
    }
}

/// "dd/mm HH:MM".
pub fn day_and_time(at: f64) -> String {
    let t = local_time(at);
    format!("{:02}/{:02} {:02}:{:02}", t.day, t.month, t.hour, t.minute)
}

/// "HH:MM" today, "dd/mm HH:MM" another day.
pub fn stamp(at: f64, now: f64) -> String {
    let (moment, today) = (local_time(at), local_time(now));
    if (moment.year, moment.month, moment.day) == (today.year, today.month, today.day) {
        format!("{:02}:{:02}", moment.hour, moment.minute)
    } else {
        day_and_time(at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_plain_ascii() {
        assert_eq!(normal("Acme Backénd"), "acme-backend");
        assert_eq!(normal("  --Nóva / Studio.."), "nova-studio");
        assert_eq!(normal("agents-sleep-preventer"), "agents-sleep-preventer");
        assert_eq!(normal("ŒUVRE"), "uvre");
        assert_eq!(normal(&"x".repeat(60)).len(), 40);
    }

    #[test]
    fn crc32_matches_zlib() {
        assert_eq!(crc32(b"hello"), 0x3610_a686);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn addresses() {
        let found = address("backend : déploie la v336").unwrap();
        assert_eq!((found.typed.as_str(), found.rest.as_str()), ("backend", "déploie la v336"));
        let found = address("@nova relance la campagne").unwrap();
        assert_eq!((found.typed.as_str(), found.rest.as_str()), ("nova", "relance la campagne"));
        assert!(address("pas d'adresse ici").is_none());
    }

    #[test]
    fn chunks_keep_paragraphs_and_cut_long_ones() {
        assert_eq!(chunks("un\ndeux"), vec!["un\ndeux"]);
        let long = "a".repeat(LIMIT + 10);
        let parts = chunks(&format!("début\n{long}"));
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "début");
        assert_eq!(parts[1].chars().count(), LIMIT);
        assert_eq!(parts[2], "a".repeat(10));
    }
}
