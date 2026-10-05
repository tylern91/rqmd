//! Tantivy token filter that makes CJK text searchable by substring.
//!
//! The default tokenizer treats Han, kana and Hangul as ordinary letters, so a
//! sentence with no spaces becomes one token — dropped outright when it
//! exceeds `RemoveLongFilter`'s 40 bytes, and otherwise matchable only as a
//! whole. This filter splits each CJK run into overlapping bigrams (a lone
//! character stays a unigram), which is what lets a query for `東京都` find
//! `私は昨日東京都庁を訪れました`. Latin and other tokens pass through untouched.

use std::collections::VecDeque;

use tantivy::tokenizer::{Token, TokenFilter, TokenStream, Tokenizer};

/// Hiragana, Katakana (incl. halfwidth), CJK Han (unified, extension A,
/// compatibility) and Hangul syllables.
fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{3040}'..='\u{30FF}'
        | '\u{3400}'..='\u{4DBF}'
        | '\u{4E00}'..='\u{9FFF}'
        | '\u{AC00}'..='\u{D7AF}'
        | '\u{F900}'..='\u{FAFF}'
        | '\u{FF66}'..='\u{FF9F}')
}

#[derive(Clone)]
pub(crate) struct CjkBigramFilter;

impl TokenFilter for CjkBigramFilter {
    type Tokenizer<T: Tokenizer> = CjkBigramWrapper<T>;

    fn transform<T: Tokenizer>(self, tokenizer: T) -> CjkBigramWrapper<T> {
        CjkBigramWrapper { inner: tokenizer }
    }
}

#[derive(Clone)]
pub(crate) struct CjkBigramWrapper<T> {
    inner: T,
}

impl<T: Tokenizer> Tokenizer for CjkBigramWrapper<T> {
    type TokenStream<'a> = CjkBigramStream<T::TokenStream<'a>>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        CjkBigramStream {
            tail: self.inner.token_stream(text),
            pending: VecDeque::new(),
            current: Token::default(),
            next_position: 0,
        }
    }
}

pub(crate) struct CjkBigramStream<T> {
    tail: T,
    pending: VecDeque<Token>,
    current: Token,
    next_position: usize,
}

impl<T: TokenStream> TokenStream for CjkBigramStream<T> {
    fn advance(&mut self) -> bool {
        loop {
            if let Some(mut token) = self.pending.pop_front() {
                // Positions are renumbered so a token that expands into several
                // bigrams pushes every later token along by the same amount.
                token.position = self.next_position;
                self.next_position += 1;
                self.current = token;
                return true;
            }
            if !self.tail.advance() {
                return false;
            }
            split(self.tail.token(), &mut self.pending);
        }
    }

    fn token(&self) -> &Token {
        &self.current
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.current
    }
}

fn piece(source: &Token, from: usize, to: usize) -> Token {
    Token {
        offset_from: source.offset_from + from,
        offset_to: source.offset_from + to,
        position: 0,
        text: source.text[from..to].to_string(),
        position_length: 1,
    }
}

/// Split `token` into maximal runs of CJK and non-CJK characters, expanding
/// each CJK run into overlapping bigrams.
fn split(token: &Token, out: &mut VecDeque<Token>) {
    if !token.text.chars().any(is_cjk) {
        out.push_back(token.clone());
        return;
    }
    let text = &token.text;
    let mut run_start = 0;
    let mut run_is_cjk = None;
    let mut boundaries: Vec<usize> = Vec::new();
    for (i, c) in text.char_indices() {
        let cjk = is_cjk(c);
        if run_is_cjk != Some(cjk) {
            if let Some(was_cjk) = run_is_cjk {
                flush_run(token, run_start, i, was_cjk, &boundaries, out);
            }
            run_start = i;
            run_is_cjk = Some(cjk);
            boundaries.clear();
        }
        boundaries.push(i);
    }
    if let Some(was_cjk) = run_is_cjk {
        flush_run(token, run_start, text.len(), was_cjk, &boundaries, out);
    }
}

/// `boundaries` holds the byte offset of every character in `start..end`.
fn flush_run(
    token: &Token,
    start: usize,
    end: usize,
    is_cjk_run: bool,
    boundaries: &[usize],
    out: &mut VecDeque<Token>,
) {
    if !is_cjk_run || boundaries.len() == 1 {
        out.push_back(piece(token, start, end));
        return;
    }
    for (k, &from) in boundaries.iter().enumerate() {
        let to = boundaries.get(k + 2).copied().unwrap_or(end);
        if k + 1 < boundaries.len() {
            out.push_back(piece(token, from, to));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::tokenizer::{SimpleTokenizer, TextAnalyzer};

    fn analyze(text: &str) -> Vec<(String, usize)> {
        let mut analyzer = TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(CjkBigramFilter)
            .build();
        let mut stream = analyzer.token_stream(text);
        let mut out = Vec::new();
        stream.process(&mut |t| out.push((t.text.clone(), t.position)));
        out
    }

    fn texts(tokens: &[(String, usize)]) -> Vec<&str> {
        tokens.iter().map(|(t, _)| t.as_str()).collect()
    }

    #[test]
    fn a_cjk_run_becomes_overlapping_bigrams() {
        let tokens = analyze("東京都庁");
        assert_eq!(texts(&tokens), vec!["東京", "京都", "都庁"]);
    }

    #[test]
    fn a_lone_cjk_character_stays_a_unigram() {
        assert_eq!(texts(&analyze("都")), vec!["都"]);
    }

    #[test]
    fn latin_tokens_pass_through_untouched() {
        assert_eq!(texts(&analyze("hello world")), vec!["hello", "world"]);
    }

    #[test]
    fn mixed_runs_split_at_script_boundaries() {
        let tokens = analyze("abc東京def");
        assert_eq!(texts(&tokens), vec!["abc", "東京", "def"]);
    }

    #[test]
    fn positions_are_renumbered_so_later_tokens_shift_by_the_extra_bigrams() {
        let tokens = analyze("abc 東京都庁 def");
        assert_eq!(
            tokens,
            vec![
                ("abc".to_string(), 0),
                ("東京".to_string(), 1),
                ("京都".to_string(), 2),
                ("都庁".to_string(), 3),
                ("def".to_string(), 4),
            ]
        );
    }

    #[test]
    fn offsets_point_at_the_source_bytes() {
        let text = "x東京都";
        let mut analyzer = TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(CjkBigramFilter)
            .build();
        let mut stream = analyzer.token_stream(text);
        let mut slices = Vec::new();
        stream.process(&mut |t| slices.push(text[t.offset_from..t.offset_to].to_string()));
        assert_eq!(slices, vec!["x", "東京", "京都"]);
    }
}
