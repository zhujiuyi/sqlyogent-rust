use std::ops::Range;

use regex::{Regex, RegexBuilder};

#[derive(Clone, Copy, Default, Eq, PartialEq)]
pub(super) struct SearchOptions {
    pub match_case: bool,
    pub whole_word: bool,
}

pub(super) struct TextSearch(Regex);

impl TextSearch {
    pub fn new(needle: &str, options: SearchOptions) -> Option<Self> {
        if needle.is_empty() {
            return None;
        }
        let mut pattern = regex::escape(needle);
        if options.whole_word {
            pattern = format!(r"\b(?:{pattern})\b");
        }
        RegexBuilder::new(&pattern)
            .case_insensitive(!options.match_case)
            .build()
            .ok()
            .map(Self)
    }

    pub fn matches<'a>(&'a self, text: &'a str) -> impl Iterator<Item = Range<usize>> + 'a {
        self.0.find_iter(text).map(|matched| matched.range())
    }

    pub fn replace_all(&self, text: &str, replacement: &str) -> Option<(String, usize)> {
        let mut matches = self.matches(text);
        let first = matches.next()?;
        let mut changed = String::with_capacity(text.len());
        let mut count = 0;
        let mut offset = 0;
        for hit in std::iter::once(first).chain(matches) {
            changed.push_str(&text[offset..hit.start]);
            changed.push_str(replacement);
            offset = hit.end;
            count += 1;
        }
        changed.push_str(&text[offset..]);
        Some((changed, count))
    }

    pub fn find_in(&self, text: &str, range: Range<usize>, forward: bool) -> Option<Range<usize>> {
        text.get(range.clone())?;
        if forward {
            self.0
                .find_at(text, range.start)
                .filter(|matched| matched.end() <= range.end)
                .map(|matched| matched.range())
        } else {
            let mut cursor = range.start;
            let mut found = None;
            while let Some(matched) = self.0.find_at(text, cursor)
                && matched.end() <= range.end
            {
                found = Some(matched.range());
                // Another backward candidate may overlap this match.
                cursor = matched.start() + text[matched.start()..].chars().next()?.len_utf8();
            }
            found
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_matches_use_utf8_ranges_and_whole_word_context() {
        let text = "SELECT K, k, K_id, 档案.k FROM records";
        let options = SearchOptions {
            whole_word: true,
            ..Default::default()
        };
        let search = TextSearch::new("k", options).unwrap();
        let found: Vec<_> = search.matches(text).map(|range| &text[range]).collect();
        assert_eq!(found, ["K", "k", "k"]);
        let last = search.find_in(text, 0..text.len(), false).unwrap();
        assert_eq!(&text[last], "k");
        let partial_word = text.find("K_id").unwrap();
        assert!(
            search
                .find_in(text, partial_word..partial_word + 1, true)
                .is_none()
        );

        let exact = TextSearch::new(
            "K",
            SearchOptions {
                match_case: true,
                ..options
            },
        )
        .unwrap();
        assert!(exact.find_in(text, 0..text.len(), true).is_none());
        let literal = TextSearch::new(".k", SearchOptions::default()).unwrap();
        assert_eq!(
            literal
                .matches(text)
                .map(|range| &text[range])
                .collect::<Vec<_>>(),
            [".k"]
        );
        let overlapping = TextSearch::new("aa", SearchOptions::default()).unwrap();
        assert_eq!(overlapping.find_in("aaaa", 0..3, false), Some(1..3));
    }
}
