// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Streaming secret masking for job logs (rust-anything unit U4).
//!
//! # Semantics
//!
//! For a phrase set `P` (empty phrases ignored, duplicates removed) and an input byte stream:
//!
//! 1. Every occurrence of every phrase is found, **including overlapping occurrences** and
//!    occurrences of phrases that are prefixes/suffixes of each other.
//! 2. Each byte covered by at least one occurrence is masked. Each maximal run of consecutive
//!    masked bytes is replaced by a single [`MASK`] (`[MASKED]`).
//! 3. The result depends only on the concatenated input, **not on how it was split** into
//!    [`Masker::write`] calls: a secret split across chunk or write boundaries is still masked.
//!    To achieve this the masker holds back only a trailing part of the input that is a
//!    proper prefix of some phrase (so at most `longest_phrase - 1` bytes) until more input
//!    arrives or [`Masker::flush`] is called; output that cannot start a secret is released
//!    immediately, so live logs are not delayed.
//!
//! Matching is on raw bytes, so multi-byte UTF-8 phrases work and output may be cut inside a
//! UTF-8 sequence (the protocol carries bytes, not strings).
//!
//! Compared with GitLab Runner's stacked longest-first maskers this "union of all occurrences"
//! rule never leaves a fragment of a secret visible when two phrases overlap (e.g. phrases `abc`
//! and `bcd` in `abcd` give `[MASKED]`, not `[MASKED]d`). Like GitLab Runner, the masker does
//! **not** recognise encoded forms of a secret (base64, URL-encoding, hex, shell quoting,
//! reversed text, …); only the literal byte sequences given are masked.

/// Replacement for every masked run.
pub const MASK: &[u8] = b"[MASKED]";

/// A streaming masker: feed bytes with [`write`](Self::write), then call
/// [`flush`](Self::flush) at the end of the stream.
#[derive(Debug, Clone)]
pub struct Masker {
    /// Non-empty, unique phrases, longest first.
    phrases: Vec<Vec<u8>>,
    max_len: usize,
    /// Bytes received but not yet emitted (at most `max_len - 1` after a write).
    pending: Vec<u8>,
    /// Number of leading bytes of `pending` covered by an occurrence that started in bytes
    /// that were already emitted.
    carry: usize,
    /// Whether the last emitted output was a `[MASKED]` whose run may continue.
    in_mask: bool,
}

impl Masker {
    /// Creates a masker for `phrases`. Empty phrases are ignored.
    pub fn new<I, S>(phrases: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<[u8]>,
    {
        let mut list: Vec<Vec<u8>> = phrases
            .into_iter()
            .map(|p| p.as_ref().to_vec())
            .filter(|p| !p.is_empty())
            .collect();
        list.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        list.dedup();
        let max_len = list.first().map_or(0, Vec::len);
        Self {
            phrases: list,
            max_len,
            pending: Vec::new(),
            carry: 0,
            in_mask: false,
        }
    }

    /// Returns `true` when there is nothing to mask (output equals input).
    pub fn is_empty(&self) -> bool {
        self.phrases.is_empty()
    }

    /// Feeds `data` and returns the bytes that are now final (masked).
    pub fn write(&mut self, data: &[u8]) -> Vec<u8> {
        if self.phrases.is_empty() {
            return data.to_vec();
        }
        self.pending.extend_from_slice(data);
        let ready = self.ready_len();
        self.emit(ready)
    }

    /// Length of the prefix of `pending` that is final: everything before the earliest suffix
    /// that is a proper prefix of some phrase (a match that may still complete).
    fn ready_len(&self) -> usize {
        let len = self.pending.len();
        let lowest = len.saturating_sub(self.max_len.saturating_sub(1));
        (lowest..len)
            .find(|&s| {
                let tail = &self.pending[s..];
                self.phrases
                    .iter()
                    .any(|p| p.len() > tail.len() && p.starts_with(tail))
            })
            .unwrap_or(len)
    }

    /// Ends the stream: returns all held-back bytes (masked) and resets the state, so the
    /// masker can be reused for an independent stream.
    pub fn flush(&mut self) -> Vec<u8> {
        let all = self.pending.len();
        let out = self.emit(all);
        self.carry = 0;
        self.in_mask = false;
        out
    }

    /// Masks `data` as one complete stream (convenience for strings such as error messages).
    pub fn mask_all(&self, data: &[u8]) -> Vec<u8> {
        let mut fresh = Self {
            phrases: self.phrases.clone(),
            max_len: self.max_len,
            pending: Vec::new(),
            carry: 0,
            in_mask: false,
        };
        let mut out = fresh.write(data);
        out.extend(fresh.flush());
        out
    }

    /// Emits the first `n` bytes of `pending`. Every occurrence that covers one of these bytes
    /// starts at an index `< n` and is fully contained in `pending` (guaranteed by the
    /// hold-back of `max_len - 1` bytes, or by the end of the stream in `flush`).
    fn emit(&mut self, n: usize) -> Vec<u8> {
        if n == 0 {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(n);
        let mut covered_until = self.carry;
        for i in 0..n {
            let rest = &self.pending[i..];
            // Longest phrase first: the first match gives the furthest end for this start.
            if let Some(p) = self.phrases.iter().find(|p| rest.starts_with(p)) {
                covered_until = covered_until.max(i + p.len());
            }
            if i < covered_until {
                if !self.in_mask {
                    out.extend_from_slice(MASK);
                    self.in_mask = true;
                }
            } else {
                out.push(self.pending[i]);
                self.in_mask = false;
            }
        }
        self.carry = covered_until.saturating_sub(n);
        self.pending.drain(..n);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Independent reference: mark every covered byte of the whole input, collapse runs.
    fn reference(phrases: &[&[u8]], input: &[u8]) -> Vec<u8> {
        let mut covered = vec![false; input.len()];
        for p in phrases.iter().filter(|p| !p.is_empty()) {
            if p.len() > input.len() {
                continue;
            }
            for s in 0..=input.len() - p.len() {
                if &input[s..s + p.len()] == *p {
                    covered[s..s + p.len()].iter_mut().for_each(|c| *c = true);
                }
            }
        }
        let mut out = Vec::new();
        let mut prev = false;
        for (i, &b) in input.iter().enumerate() {
            if covered[i] {
                if !prev {
                    out.extend_from_slice(MASK);
                }
                prev = true;
            } else {
                out.push(b);
                prev = false;
            }
        }
        out
    }

    fn stream(phrases: &[&[u8]], chunks: &[&[u8]]) -> Vec<u8> {
        let mut m = Masker::new(phrases.iter().copied());
        let mut out = Vec::new();
        for c in chunks {
            out.extend(m.write(c));
        }
        out.extend(m.flush());
        out
    }

    fn s(v: Vec<u8>) -> String {
        String::from_utf8(v).expect("utf8")
    }

    #[test]
    fn simple_and_repeated() {
        let p: &[&[u8]] = &[b"hunter2"];
        assert_eq!(s(stream(p, &[b"pw=hunter2 ok"])), "pw=[MASKED] ok");
        assert_eq!(s(stream(p, &[b"hunter2hunter2"])), "[MASKED]");
        assert_eq!(s(stream(p, &[b"hunter2 hunter2"])), "[MASKED] [MASKED]");
        assert_eq!(s(stream(p, &[b"hunter"])), "hunter");
    }

    #[test]
    fn split_across_every_boundary() {
        let p: &[&[u8]] = &[b"s3cr3t"];
        let input = b"a s3cr3t b";
        for cut in 0..=input.len() {
            let (l, r) = input.split_at(cut);
            assert_eq!(s(stream(p, &[l, r])), "a [MASKED] b", "cut {cut}");
        }
        let bytes: Vec<&[u8]> = input.chunks(1).collect();
        assert_eq!(s(stream(p, &bytes)), "a [MASKED] b");
    }

    #[test]
    fn overlapping_and_prefix_phrases() {
        let p: &[&[u8]] = &[b"abc", b"bcd"];
        assert_eq!(s(stream(p, &[b"xabcdx"])), "x[MASKED]x");
        let p: &[&[u8]] = &[b"ab", b"abcdef"];
        assert_eq!(
            s(stream(p, &[b"abcdeX ab abcdef"])),
            "[MASKED]cdeX [MASKED] [MASKED]"
        );
        let p: &[&[u8]] = &[b"aa"];
        assert_eq!(s(stream(p, &[b"aaa"])), "[MASKED]");
    }

    #[test]
    fn empty_and_one_byte_phrases() {
        let none: &[&[u8]] = &[b""];
        assert_eq!(s(stream(none, &[b"abc"])), "abc");
        assert!(Masker::new(none.iter().copied()).is_empty());
        let one: &[&[u8]] = &[b"x"];
        assert_eq!(s(stream(one, &[b"axbxx", b"c"])), "a[MASKED]b[MASKED]c");
        // Single-byte phrases need no hold-back: output is immediate.
        let mut m = Masker::new(one.iter().copied());
        assert_eq!(m.write(b"ab"), b"ab");
    }

    #[test]
    fn utf8_phrases() {
        let p: &[&[u8]] = &["pässwörd😀".as_bytes()];
        let input = "x=pässwörd😀!".as_bytes();
        for cut in 0..=input.len() {
            let (l, r) = input.split_at(cut);
            assert_eq!(s(stream(p, &[l, r])), "x=[MASKED]!");
        }
    }

    #[test]
    fn mask_all_and_reuse_after_flush() {
        let m = Masker::new(["tok"]);
        assert_eq!(m.mask_all(b"a tok b"), b"a [MASKED] b");
        let mut m = Masker::new(["tok"]);
        assert_eq!(m.write(b"to"), b"");
        assert_eq!(m.flush(), b"to");
        let mut out = m.write(b"tok!");
        out.extend(m.flush());
        assert_eq!(out, b"[MASKED]!");
    }

    #[test]
    fn hold_back_only_possible_prefixes() {
        let mut m = Masker::new(["0123456789"]);
        // Nothing can start a phrase: released immediately.
        assert_eq!(m.write(&[b'x'; 100]).len(), 100);
        // A possible beginning of the phrase is held back...
        assert_eq!(m.write(b"ab012"), b"ab");
        // ...and released once it can no longer match.
        assert_eq!(m.write(b"x"), b"012x");
        let mut out = m.write(b"012345");
        out.extend(m.write(b"6789!"));
        assert_eq!(out, b"[MASKED]!");
    }

    fn alphabet() -> impl Strategy<Value = Vec<u8>> {
        // Small alphabet (plus multi-byte UTF-8) to force overlaps and near-misses.
        prop::collection::vec(
            prop::sample::select(vec!["a", "b", "c", "é", "😀", "\n"]),
            0..40,
        )
        .prop_map(|v| v.concat().into_bytes())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2000))]

        #[test]
        fn any_split_equals_reference(
            phrases in prop::collection::vec(alphabet(), 0..4),
            input in alphabet(),
            cuts in prop::collection::vec(any::<prop::sample::Index>(), 0..8),
        ) {
            let phrase_refs: Vec<&[u8]> = phrases.iter().map(Vec::as_slice).collect();
            let mut points: Vec<usize> = cuts.iter().map(|c| c.index(input.len() + 1)).collect();
            points.sort_unstable();
            let mut chunks: Vec<&[u8]> = Vec::new();
            let mut last = 0;
            for p in points {
                chunks.push(&input[last..p]);
                last = p;
            }
            chunks.push(&input[last..]);
            let expected = reference(&phrase_refs, &input);
            prop_assert_eq!(stream(&phrase_refs, &chunks), expected.clone());
            prop_assert_eq!(stream(&phrase_refs, &[&input]), expected);
        }
    }
}
