//! Vocabulary correction applied to every transcript, whatever the engine.
//!
//! Parakeet cannot be steered by a prompt the way Whisper can, so a word the
//! user listed ("cleemo") still comes out as it sounds ("climo"). After
//! transcription, a word (or two or three adjacent words, for "clee mo")
//! that sounds like a vocabulary entry is replaced by the entry's spelling.
//! Matching stays strict: identical rough pronunciation, or, for words of
//! five sounds or more that start with the same two sounds, one vowel or
//! similar consonant swapped ("clibo") or one extra consonant ("climbo").

/// Adjacent words tried together, so a split name ("clee mo") still matches.
const MAX_SPAN_WORDS: usize = 3;
/// Shorter keys only match exactly: one sound off is too loose for them.
const MIN_KEY_LEN_FOR_ONE_EDIT: usize = 5;
/// Consonants speech engines confuse with each other.
const SIMILAR_CONSONANTS: [&[char]; 6] = [
    &['m', 'b', 'p'],
    &['m', 'n'],
    &['t', 'd'],
    &['k', 'g'],
    &['f', 'v'],
    &['s', 'x'],
];
/// Keys this short are too ambiguous to correct at all.
const MIN_KEY_LEN: usize = 3;

struct Entry<'a> {
    word: &'a str,
    key: Vec<char>,
}

struct Token {
    start: usize,
    end: usize,
}

/// Replace words that sound like a vocabulary entry with that entry.
pub fn correct<S: AsRef<str>>(text: &str, vocabulary: &[S]) -> String {
    let entries: Vec<Entry> = vocabulary
        .iter()
        .map(|word| word.as_ref().trim())
        .filter(|word| !word.is_empty())
        .map(|word| Entry {
            word,
            key: phonetic_key(word),
        })
        .filter(|entry| entry.key.len() >= MIN_KEY_LEN)
        .collect();
    if entries.is_empty() {
        return text.to_string();
    }

    let tokens = tokenize(text);
    let mut corrected = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut i = 0;
    while i < tokens.len() {
        match find_match(text, &tokens, i, &entries) {
            Some((span, entry)) => {
                let first = &tokens[i];
                let last = &tokens[i + span - 1];
                corrected.push_str(&text[cursor..first.start]);
                corrected.push_str(&with_leading_case(entry.word, &text[first.start..last.end]));
                cursor = last.end;
                i += span;
            }
            None => i += 1,
        }
    }
    corrected.push_str(&text[cursor..]);
    corrected
}

/// Split a comma, semicolon or newline separated list (Windows settings).
#[cfg(any(windows, test))]
pub fn parse_list(list: &str) -> Vec<&str> {
    list.split([',', ';', '\n'])
        .map(str::trim)
        .filter(|word| !word.is_empty())
        .collect()
}

/// Shortest span first, so a correct single word never swallows its
/// neighbour.
fn find_match<'e, 'a>(
    text: &str,
    tokens: &[Token],
    i: usize,
    entries: &'e [Entry<'a>],
) -> Option<(usize, &'e Entry<'a>)> {
    let mut joined = String::new();
    for span in 1..=MAX_SPAN_WORDS.min(tokens.len() - i) {
        let token = &tokens[i + span - 1];
        if span > 1 {
            let gap = &text[tokens[i + span - 2].end..token.start];
            if !gap.chars().all(|c| c == ' ' || c == '-') {
                return None;
            }
        }
        joined.push(' ');
        joined.push_str(&text[token.start..token.end]);
        let key = phonetic_key(&joined);
        if let Some(entry) = entries.iter().find(|entry| keys_match(&key, &entry.key)) {
            return Some((span, entry));
        }
    }
    None
}

fn tokenize(text: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut start = None;
    for (index, c) in text.char_indices() {
        match (c.is_alphanumeric(), start) {
            (true, None) => start = Some(index),
            (false, Some(s)) => {
                tokens.push(Token { start: s, end: index });
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        tokens.push(Token {
            start: s,
            end: text.len(),
        });
    }
    tokens
}

fn keys_match(candidate: &[char], target: &[char]) -> bool {
    if candidate == target {
        return true;
    }
    if candidate.len().min(target.len()) < MIN_KEY_LEN_FOR_ONE_EDIT
        || candidate[..2] != target[..2]
    {
        return false;
    }
    if candidate.len() == target.len() {
        let mut differences = candidate.iter().zip(target).filter(|(a, b)| a != b);
        return matches!(
            (differences.next(), differences.next()),
            (Some((a, b)), None) if sounds_similar(*a, *b)
        );
    }
    let (longer, shorter) = if candidate.len() > target.len() {
        (candidate, target)
    } else {
        (target, candidate)
    };
    if longer.len() != shorter.len() + 1 {
        return false;
    }
    // One extra consonant ("climbo" for "climo"), anywhere after the start.
    let split = shorter.iter().zip(longer).take_while(|(a, b)| a == b).count();
    !is_vowel(longer[split]) && longer[split + 1..] == shorter[split..]
}

fn sounds_similar(a: char, b: char) -> bool {
    (is_vowel(a) && is_vowel(b))
        || SIMILAR_CONSONANTS
            .iter()
            .any(|group| group.contains(&a) && group.contains(&b))
}

fn is_vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u')
}

/// Keep the vocabulary spelling, but capitalize it when the spoken word
/// started a sentence ("Climo is…" → "Cleemo is…" for an entry "cleemo").
fn with_leading_case(word: &str, original: &str) -> String {
    let starts_upper = original.chars().next().is_some_and(char::is_uppercase);
    let mut chars = word.chars();
    match chars.next() {
        Some(first) if starts_upper && first.is_lowercase() => {
            first.to_uppercase().chain(chars).collect()
        }
        _ => word.to_string(),
    }
}

fn fold(c: char) -> Option<char> {
    let c = c.to_lowercase().next()?;
    Some(match c {
        'à' | 'á' | 'â' | 'ä' | 'ã' | 'å' => 'a',
        'ç' => 'c',
        'è' | 'é' | 'ê' | 'ë' => 'e',
        'ì' | 'í' | 'î' | 'ï' => 'i',
        'ñ' => 'n',
        'ò' | 'ó' | 'ô' | 'ö' | 'õ' => 'o',
        'ù' | 'ú' | 'û' | 'ü' => 'u',
        'ý' | 'ÿ' => 'y',
        c if c.is_ascii_alphanumeric() => c,
        _ => return None,
    })
}

/// Rough pronunciation key (English and French spellings): letters that
/// sound alike map to one letter, silent `h` and word-final `e` drop, and
/// doubled sounds collapse. Words are keyed separately, then joined.
fn phonetic_key(text: &str) -> Vec<char> {
    let mut key = Vec::new();
    for word in text.split(|c: char| !c.is_alphanumeric()) {
        for sound in word_key(word) {
            if key.last() != Some(&sound) {
                key.push(sound);
            }
        }
    }
    key
}

fn word_key(word: &str) -> Vec<char> {
    let letters: Vec<char> = word.chars().filter_map(fold).collect();
    let mut sounds = Vec::with_capacity(letters.len());
    let mut i = 0;
    while i < letters.len() {
        let next = letters.get(i + 1).copied();
        let (sound, consumed): (&[char], usize) = match (letters[i], next) {
            ('p', Some('h')) => (&['f'], 2),
            ('c', Some('k')) | ('q', Some('u')) => (&['k'], 2),
            ('c' | 's', Some('h')) => (&['x'], 2),
            ('e', Some('e' | 'a' | 'i' | 'y')) | ('i', Some('e')) => (&['i'], 2),
            ('o', Some('o' | 'u')) => (&['u'], 2),
            ('a', Some('i' | 'y')) => (&['e'], 2),
            ('a', Some('u')) => (&['o'], 2),
            ('c', Some('e' | 'i' | 'y')) => (&['s'], 1),
            ('c' | 'q', _) => (&['k'], 1),
            ('z', _) => (&['s'], 1),
            ('x', _) => (&['k', 's'], 1),
            ('y', _) => (&['i'], 1),
            ('w', _) => (&['u'], 1),
            ('h', _) => (&[], 1),
            _ => (std::slice::from_ref(&letters[i]), 1),
        };
        for &s in sound {
            if sounds.last() != Some(&s) {
                sounds.push(s);
            }
        }
        i += consumed;
    }
    // Silent final e ("code", "Claude"), but not in "je" or "ce".
    if sounds.len() > 2 && sounds.last() == Some(&'e') && !is_vowel(sounds[sounds.len() - 2]) {
        sounds.pop();
    }
    sounds
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixes_a_misheard_vocabulary_word() {
        assert_eq!(
            correct("je travaille sur climo ce soir", &["cleemo"]),
            "je travaille sur cleemo ce soir"
        );
        assert_eq!(correct("Clemo is ready.", &["cleemo"]), "Cleemo is ready.");
        assert_eq!(correct("open Kleemo, then", &["cleemo"]), "open Cleemo, then");
        // Real Parakeet outputs for "Cleemo"
        assert_eq!(correct("Oui, Climbo. C'est bon", &["cleemo"]), "Oui, Cleemo. C'est bon");
        assert_eq!(correct("écoute, Clibo ça marche", &["cleemo"]), "écoute, Cleemo ça marche");
    }

    #[test]
    fn joins_a_name_split_into_several_words() {
        assert_eq!(correct("deploy clee mo now", &["cleemo"]), "deploy cleemo now");
        assert_eq!(correct("ask cloud code", &["Claude Code"]), "ask Claude Code");
        assert_eq!(correct("ask Claude code", &["Claude Code"]), "ask Claude Code");
    }

    #[test]
    fn keeps_the_listed_spelling_and_leaves_other_words_alone() {
        let text = "Climb the clean climate, calm client: clam, come on.";
        assert_eq!(correct(text, &["cleemo"]), text);
        let text = "Le climat, une Clio, un clip, limbo, clinique, Kimbo, clément.";
        assert_eq!(correct(text, &["cleemo"]), text);
        assert_eq!(correct("Marius et Lamdera", &["Marius", "Lamdera"]), "Marius et Lamdera");
    }

    #[test]
    fn does_not_join_across_punctuation_or_swallow_neighbours() {
        assert_eq!(correct("clee, mo", &["cleemo"]), "clee, mo");
        assert_eq!(correct("climo o", &["cleemo"]), "cleemo o");
    }

    #[test]
    fn ignores_empty_and_too_short_entries() {
        assert_eq!(correct("on y va", &["", "  ", "va"]), "on y va");
        assert_eq!(correct("", &["cleemo"]), "");
    }

    #[test]
    fn parses_the_windows_list_format() {
        assert_eq!(parse_list("Marius, Cleemo;Lamdera\n\n"), ["Marius", "Cleemo", "Lamdera"]);
    }
}
