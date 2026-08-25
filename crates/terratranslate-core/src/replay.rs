//! Deterministic signatures used to identify already translated turns.
//!
//! Replay signatures deliberately contain only information that is stable across
//! process launches: the producer's stable hook identity and the captured source
//! text.  Event IDs, process IDs, timestamps, and hook ordering are not part of a
//! signature.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

/// A normalized multiset of hook/source pairs for one grouped turn.
///
/// The entries are sorted, rather than stored in capture order.  Keeping duplicate
/// entries is intentional: two occurrences of one hook in the same turn carry
/// different information from one occurrence.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TurnSignature {
    pub entries: Vec<(String, String)>,
}

impl TurnSignature {
    pub fn new(entries: Vec<(String, String)>) -> Self {
        Self::from_pairs(entries)
    }

    /// Construct a signature from `(stable_hook_key, source_text)` pairs.
    pub fn from_pairs<I, K, T>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, T)>,
        K: Into<String>,
        T: AsRef<str>,
    {
        let mut entries = pairs
            .into_iter()
            .map(|(hook, text)| (hook.into(), normalize_replay_text(text.as_ref())))
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| match left.0.cmp(&right.0) {
            Ordering::Equal => left.1.cmp(&right.1),
            ordering => ordering,
        });
        Self { entries }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn normalized_text(text: &str) -> String {
        normalize_replay_text(text)
    }
}

/// Normalize whitespace within each line, trim surrounding whitespace, and apply a small
/// full-Unicode-case-fold compatibility mapping before lowercasing the remaining
/// characters.  Rust's standard library intentionally exposes lowercase rather
/// than a full case-fold operation; the explicit mappings cover the folds that are
/// not representable by `char::to_lowercase` (notably sharp-s and final sigma),
/// while retaining all other Unicode characters losslessly.
pub fn normalize_replay_text(text: &str) -> String {
    let line_normalized = text
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned();

    let mut folded = String::with_capacity(line_normalized.len());
    for character in line_normalized.chars() {
        match character {
            '\u{00df}' | '\u{1e9e}' => folded.push_str("ss"), // sharp s
            '\u{03c2}' => folded.push('\u{03c3}'),            // final sigma
            '\u{0130}' => folded.push_str("i\u{307}"),        // capital I with dot
            '\u{0149}' => folded.push_str("\u{02bc}n"),
            '\u{fb00}' => folded.push_str("ff"),
            '\u{fb01}' => folded.push_str("fi"),
            '\u{fb02}' => folded.push_str("fl"),
            '\u{fb03}' => folded.push_str("ffi"),
            '\u{fb04}' => folded.push_str("ffl"),
            '\u{fb05}' | '\u{fb06}' => folded.push_str("st"),
            _ => folded.extend(character.to_lowercase()),
        }
    }
    folded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_are_order_independent_but_retain_duplicates() {
        let first = TurnSignature::from_pairs([
            ("choice", " YES  "),
            ("dialogue", "Hello\n  world"),
            ("choice", "yes"),
        ]);
        let second = TurnSignature::from_pairs([
            ("choice", "yes"),
            ("choice", "YES"),
            ("dialogue", " hello\n world "),
        ]);
        assert_eq!(first, second);
        assert_eq!(first.len(), 3);
    }

    #[test]
    fn normalization_applies_case_folding_and_line_whitespace() {
        assert_eq!(
            normalize_replay_text("  Straße\n  FOO\t BAR  "),
            "strasse\nfoo bar"
        );
        assert_eq!(normalize_replay_text("ΟΣ ος"), "οσ οσ");
    }

    #[test]
    fn differing_hook_identity_or_text_rejects_match() {
        let expected = TurnSignature::from_pairs([("dialogue", "same")]);
        assert_ne!(expected, TurnSignature::from_pairs([("choice", "same")]));
        assert_ne!(
            expected,
            TurnSignature::from_pairs([("dialogue", "different")])
        );
    }
}
