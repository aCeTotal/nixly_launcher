use nucleo::{Config, Matcher, Utf32Str};

pub struct Index {
    matcher: Matcher,
}

#[derive(Clone, Copy, Debug)]
pub struct Hit {
    pub idx: usize,
    #[allow(dead_code)] // surfaced for future ranking inspection / debug logs
    pub score: u32,
}

impl Index {
    pub fn new() -> Self {
        let mut config = Config::DEFAULT;
        config.ignore_case = true;
        Self { matcher: Matcher::new(config) }
    }

    // Empty query → all items in input order.
    // Non-empty → tier-based ranking so a literal best match always wins:
    //   tier 0: case-insensitive exact equality
    //   tier 1: case-insensitive prefix
    //   tier 2: nucleo fuzzy
    // Within a tier: nucleo score desc, then shorter label, then alpha.
    pub fn search<T, F>(&mut self, items: &[T], get_label: F, query: &str) -> Vec<Hit>
    where
        F: Fn(&T) -> &str,
    {
        if query.is_empty() {
            return items
                .iter()
                .enumerate()
                .map(|(idx, _)| Hit { idx, score: 0 })
                .collect();
        }
        // ignore_case=true requires a lowercase needle — nucleo lowercases
        // the haystack per-char internally but trusts the caller to have
        // pre-lowercased the needle. Passing uppercase characters in panics
        // inside fuzzy_optimal.rs ("should have been caught by prefilter").
        let query_lower = query.to_lowercase();
        let mut needle_buf = Vec::new();
        let needle = Utf32Str::new(&query_lower, &mut needle_buf);
        let mut hay_buf = Vec::new();

        struct Scored {
            idx: usize,
            score: u32,
            tier: u8,
            len: usize,
            label_lower: String,
        }

        let mut scored: Vec<Scored> = Vec::with_capacity(items.len());
        for (idx, item) in items.iter().enumerate() {
            let label = get_label(item);
            hay_buf.clear();
            let hay = Utf32Str::new(label, &mut hay_buf);
            let Some(score) = self.matcher.fuzzy_match(hay, needle) else { continue };
            let label_lower = label.to_lowercase();
            let tier = if label_lower == query_lower {
                0
            } else if label_lower.starts_with(&query_lower) {
                1
            } else {
                2
            };
            scored.push(Scored {
                idx,
                score: score as u32,
                tier,
                len: label.chars().count(),
                label_lower,
            });
        }
        scored.sort_by(|a, b| {
            a.tier
                .cmp(&b.tier)
                .then_with(|| b.score.cmp(&a.score))
                .then_with(|| a.len.cmp(&b.len))
                .then_with(|| a.label_lower.cmp(&b.label_lower))
        });
        scored
            .into_iter()
            .map(|s| Hit { idx: s.idx, score: s.score })
            .collect()
    }
}
