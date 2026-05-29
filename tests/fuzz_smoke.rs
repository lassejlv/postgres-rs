//! Deterministic smoke fuzzer for the lexer, parser, and protocol reader.
//!
//! A seeded LCG (no external crates) generates many random byte strings and
//! random SQL-ish strings and feeds them to `Parser::parse_sql`, the lexer, and
//! the protocol message reader. The only assertion is that the process never
//! PANICS — returning an `Err` is a perfectly acceptable outcome.
//!
//! This is a CI-runnable smoke fuzzer, not a coverage-guided one.

use std::io::Cursor;

use postgres_rs::protocol::{read_message, read_startup};
use postgres_rs::sql::Parser;
use postgres_rs::sql::lexer::Lexer;

/// A tiny linear-congruential generator (glibc constants) for reproducible
/// pseudo-randomness without any dependency.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Lcg(seed)
    }

    fn next_u32(&mut self) -> u32 {
        // Numerical Recipes LCG; we take the high bits which mix better.
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }

    fn below(&mut self, n: u32) -> u32 {
        if n == 0 { 0 } else { self.next_u32() % n }
    }

    fn byte(&mut self) -> u8 {
        self.next_u32() as u8
    }
}

/// Build a random byte string up to `max` bytes.
fn random_bytes(rng: &mut Lcg, max: usize) -> Vec<u8> {
    let n = rng.below(max as u32 + 1) as usize;
    (0..n).map(|_| rng.byte()).collect()
}

/// SQL fragment alphabet: keywords, punctuation, literals, identifiers. Mixing
/// these produces strings that exercise more parser states than raw bytes.
const FRAGMENTS: &[&str] = &[
    "SELECT", "FROM", "WHERE", "INSERT", "INTO", "VALUES", "UPDATE", "SET",
    "DELETE", "CREATE", "TABLE", "DROP", "JOIN", "ON", "GROUP", "BY", "ORDER",
    "HAVING", "LIMIT", "AND", "OR", "NOT", "NULL", "IN", "(", ")", ",", ";",
    "*", "=", "<", ">", "<=", ">=", "+", "-", "/", ".", "'", "\"", "$1", "1",
    "42", "-3.14", "'abc'", "t", "x", "integer", "text", "true", "false",
    "COUNT", "SUM", "DISTINCT", "AS", "CASE", "WHEN", "THEN", "ELSE", "END",
    "  ", "\n", "\t", "''''", "--c", "/*", "*/", "0x", "1e10",
];

/// Build a random SQL-ish string by concatenating random fragments.
fn random_sql(rng: &mut Lcg, max_fragments: usize) -> String {
    let n = rng.below(max_fragments as u32) as usize + 1;
    let mut s = String::new();
    for _ in 0..n {
        let frag = FRAGMENTS[rng.below(FRAGMENTS.len() as u32) as usize];
        s.push_str(frag);
        if rng.below(3) == 0 {
            s.push(' ');
        }
    }
    s
}

#[test]
fn fuzz_lexer_and_parser_never_panic() {
    let mut rng = Lcg::new(0x1234_5678_9abc_def0);
    for _ in 0..5000 {
        let sql = random_sql(&mut rng, 24);
        // Lexer must not panic.
        let _ = Lexer::new(&sql).tokenize();
        // Parser must not panic (errors are fine).
        let _ = Parser::parse_sql(&sql);
    }
}

#[test]
fn fuzz_parser_on_raw_bytes_never_panics() {
    let mut rng = Lcg::new(0xdead_beef_cafe_babe);
    for _ in 0..3000 {
        let bytes = random_bytes(&mut rng, 64);
        // Treat arbitrary bytes as a UTF-8-lossy string and lex/parse it.
        let s = String::from_utf8_lossy(&bytes);
        let _ = Lexer::new(&s).tokenize();
        let _ = Parser::parse_sql(&s);
    }
}

#[test]
fn fuzz_protocol_reader_never_panics() {
    let mut rng = Lcg::new(0x0f0f_0f0f_f0f0_f0f0);
    for _ in 0..5000 {
        // A random tag byte followed by a self-described length and a random
        // body. The reader frames on the length, so feed plausible framings.
        let body = random_bytes(&mut rng, 48);
        let tag = match rng.below(20) {
            // Bias toward the message types with the trickiest body parsing.
            0 => b'P',
            1 => b'B',
            2 => b'E',
            3 => b'D',
            4 => b'C',
            5 => b'Q',
            6 => b'X',
            other => 0x40u8.wrapping_add(other as u8),
        };
        let len = (body.len() + 4) as i32;
        let mut packet = vec![tag];
        packet.extend_from_slice(&len.to_be_bytes());
        packet.extend_from_slice(&body);
        let mut cur = Cursor::new(packet);
        let _ = read_message(&mut cur);

        // Also fuzz the startup reader: a length-prefixed random body.
        let sbody = random_bytes(&mut rng, 48);
        let slen = (sbody.len() + 4) as i32;
        let mut spacket = Vec::new();
        spacket.extend_from_slice(&slen.to_be_bytes());
        spacket.extend_from_slice(&sbody);
        let mut scur = Cursor::new(spacket);
        let _ = read_startup(&mut scur);
    }
}

/// A handful of hand-picked adversarial protocol packets that previously could
/// index out of bounds (truncated Bind/Execute/Parse bodies).
#[test]
fn fuzz_protocol_known_truncations() {
    let cases: &[&[u8]] = &[
        &[b'B', 0, 0, 0, 5, 0],
        &[b'E', 0, 0, 0, 5, 0],
        &[b'P', 0, 0, 0, 5, 0],
        &[b'B', 0, 0, 0, 6, 0, 0],
        &[b'D', 0, 0, 0, 4],
        &[b'C', 0, 0, 0, 4],
        &[b'E', 0, 0, 0, 6, b'p', 0],
    ];
    for case in cases {
        let mut cur = Cursor::new(case.to_vec());
        // Must return (Ok or Err) without panicking.
        let _ = read_message(&mut cur);
    }
}
