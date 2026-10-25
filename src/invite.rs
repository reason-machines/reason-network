//! Invite codes: four words, like `plum-tiger-lake-orbit`. The code itself derives a temporary iroh key that
//! the inviting machine listens on, and the secret the joiner proves, so joining needs nothing but the code: no
//! server of ours, no long string to paste. Four words from 1295 give ~41 bits; a code works once, for a day,
//! while the inviting machine is online.
//!
//! Words: the EFF short wordlist (https://www.eff.org/dice), CC BY 3.0 US.

use data_encoding::HEXLOWER;
use iroh::SecretKey;

const WORDS: &str = include_str!("words.txt");
const LEN: usize = 4;

fn words() -> Vec<&'static str> {
    WORDS.lines().filter(|w| !w.is_empty()).collect()
}

pub fn generate() -> String {
    let words = words();
    let n = words.len() as u32;
    let mut out = Vec::with_capacity(LEN);
    while out.len() < LEN {
        let mut b = [0u8; 4];
        getrandom::fill(&mut b).expect("system randomness");
        let r = u32::from_le_bytes(b);
        // Rejection sampling keeps every word equally likely.
        if r < u32::MAX - u32::MAX % n {
            out.push(words[(r % n) as usize]);
        }
    }
    out.join("-")
}

/// `Plum Tiger lake-ORBIT` -> `plum-tiger-lake-orbit`, if it is four known words.
pub fn normalize(input: &str) -> Option<String> {
    let words = words();
    let parts: Vec<String> = input
        .split(|c: char| !c.is_ascii_alphabetic())
        .filter(|p| !p.is_empty())
        .map(|p| p.to_ascii_lowercase())
        .collect();
    (parts.len() == LEN && parts.iter().all(|p| words.contains(&p.as_str()))).then(|| parts.join("-"))
}

/// The key the inviting machine listens on while the code is valid. Kept out of the debug log, even in part: the code
/// is short enough to find from it offline.
pub fn key(code: &str) -> SecretKey {
    let key = SecretKey::from_bytes(&blake3::derive_key("tincan 2026 invite endpoint", code.as_bytes()));
    crate::debuglog::hide(&key.public());
    key
}

/// The invite secret the joiner proves (its hash is in the team state, like any invite's). Kept out of the debug log
/// too.
pub fn secret(code: &str) -> String {
    let secret = HEXLOWER.encode(&blake3::derive_key("tincan 2026 invite secret", code.as_bytes())[..16]);
    crate::debuglog::hide_text(&secret);
    secret
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes() {
        let c = generate();
        assert_eq!(c.split('-').count(), 4);
        assert_eq!(normalize(&c.to_uppercase().replace('-', " ")).as_deref(), Some(c.as_str()));
        assert_eq!(normalize("acid acorn acre"), None);
        assert_eq!(normalize("acid acorn acre notaword"), None);
        assert_eq!(key(&c).public(), key(&c).public());
        assert_ne!(key("acid-acorn-acre-acid").public(), key(&c).public());
        assert_eq!(secret(&c).len(), 32);
        assert_eq!(words().len(), 1295);
    }
}
