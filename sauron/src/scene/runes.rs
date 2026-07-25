//! The engraving above the board: canonical Sindarin in Elder Futhark, with a
//! faint English gloss above it.
//!
//! Real runes (U+16A0 block), because they render out of the box. True Tengwar
//! lives in the Unicode private-use area and needs a font installed, so a
//! terminal without it would show a row of tofu where the engraving should be --
//! runes stand in for it and always draw.
//!
//! grep targets:
//!   const VERSES -- the engraved lines, Sindarin plus gloss
//!   fn verse     -- which line is showing at time `ms`
//!   fn runic     -- latin -> Elder Futhark, thorn digraph included

/// The engraved lines: canonical Sindarin (the West-gate of Moria), shown in
/// runes with a faint English gloss. Real Elvish words, real script glyphs.
pub(super) const VERSES: [(&str, &str); 3] = [
    ("pedo mellon a minno", "speak, friend, and enter"),
    ("ennyn durin aran moria", "the doors of durin, lord of moria"),
    ("celebrimbor teithant", "celebrimbor drew these signs"),
];

pub(super) fn verse(ms: u64) -> (&'static str, &'static str) {
    // A slow engraving: each line lingers for half a minute.
    VERSES[((ms / 30_000) % VERSES.len() as u64) as usize]
}

/// Latin -> Elder Futhark. `th` becomes the thorn rune; unknown chars pass
/// through so punctuation and the like survive.
pub(super) fn runic(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if i + 1 < chars.len() && chars[i] == 't' && chars[i + 1] == 'h' {
            out.push('ᚦ');
            i += 2;
            continue;
        }
        out.push(rune(chars[i]));
        i += 1;
    }
    out
}

fn rune(c: char) -> char {
    match c.to_ascii_lowercase() {
        'a' => 'ᚨ', 'b' => 'ᛒ', 'c' => 'ᚲ', 'd' => 'ᛞ', 'e' => 'ᛖ', 'f' => 'ᚠ',
        'g' => 'ᚷ', 'h' => 'ᚺ', 'i' => 'ᛁ', 'j' => 'ᛃ', 'k' => 'ᚲ', 'l' => 'ᛚ',
        'm' => 'ᛗ', 'n' => 'ᚾ', 'o' => 'ᛟ', 'p' => 'ᛈ', 'q' => 'ᚲ', 'r' => 'ᚱ',
        's' => 'ᛊ', 't' => 'ᛏ', 'u' => 'ᚢ', 'v' => 'ᚹ', 'w' => 'ᚹ', 'x' => 'ᛉ',
        'y' => 'ᛃ', 'z' => 'ᛉ', ' ' => '᛬', other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runic_transliterates_and_keeps_the_thorn_digraph() {
        assert_eq!(runic("a b"), "ᚨ᛬ᛒ");
        // "th" collapses to a single thorn rune rather than two glyphs.
        assert_eq!(runic("th"), "ᚦ");
        assert_eq!(runic("teithant").chars().filter(|&c| c == 'ᚦ').count(), 1);
    }

    #[test]
    fn every_verse_is_transliterable_and_cycles() {
        assert_ne!(verse(0), verse(30_000));
        assert_eq!(verse(0), verse(90_000));
        for (words, gloss) in VERSES {
            assert_eq!(runic(words).chars().count() <= words.chars().count(), true);
            assert!(!gloss.is_empty());
        }
    }
}
