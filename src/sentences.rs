//! Joins recognized utterances into whole sentences before they are translated.
//!
//! An utterance ends at a short pause or at the 10 s cap, so it often stops mid-clause ("And then there is
//! the"). Translated alone, such a fragment comes back as Japanese that trails off ("そして、そこには…") and the
//! next line starts from nowhere. Here the unfinished end of an utterance waits on screen in the original and
//! is joined with the next utterance, and only the finished sentences go to the translator.

use std::time::{Duration, Instant};

/// An unfinished end is translated as it is after this long without new speech.
const PAUSE_FLUSH: Duration = Duration::from_secs(2);
/// …and after this long in any case (an utterance is cut at 10 s, so a sentence still going by then is not
/// waited for again).
const MAX_WAIT: Duration = Duration::from_secs(15);
/// Text this long without a sentence end is translated as it is (the recognizer left out the punctuation).
const MAX_PENDING_CHARS: usize = 300;

/// Words a sentence does not end on. The recognizer puts a period where the audio was cut, so a "sentence"
/// ending in one of these ("And then there is the.") is really unfinished.
/// Only words that hardly ever end a real sentence: "work there." and "that's what it is." do end one.
const DANGLING: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "of", "to", "for", "with", "from", "into", "like", "if", "because", "than",
    "my", "your", "our", "their", "its",
];

/// Abbreviations whose period does not end a sentence.
const ABBREVIATIONS: &[&str] = &["mr", "mrs", "ms", "dr", "prof", "st", "vs", "etc", "e.g", "i.e", "u.s", "jr", "sr"];

#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// A new subtitle line with this original text.
    Show { id: u64, lang: String, text: String },
    /// The original text of a line already shown is now this.
    Revise { id: u64, text: String },
    /// Translate this line.
    Translate { id: u64, lang: String, text: String },
}

struct Pending {
    id: u64,
    lang: String,
    text: String,
    since: Instant,
}

#[derive(Default)]
pub struct Assembler {
    pending: Option<Pending>,
}

impl Assembler {
    /// Takes one recognized utterance and says what to show and what to translate.
    pub fn hear(&mut self, lang: &str, text: &str, mut next_id: impl FnMut() -> u64, now: Instant) -> Vec<Step> {
        let mut steps = Vec::new();
        let (id, joined, new_line) = match self.pending.take() {
            Some(p) if p.lang == lang => (p.id, format!("{} {text}", without_cut_period(&p.text)), false),
            Some(p) => {
                steps.push(Step::Translate { id: p.id, lang: p.lang, text: p.text });
                (next_id(), text.to_string(), true)
            }
            None => (next_id(), text.to_string(), true),
        };
        let (done, rest) = if joined.chars().count() > MAX_PENDING_CHARS {
            (joined.clone(), String::new())
        } else {
            split_finished(&joined)
        };
        let shown = if done.is_empty() { joined.clone() } else { done.clone() };
        steps.push(if new_line {
            Step::Show { id, lang: lang.to_string(), text: shown }
        } else {
            Step::Revise { id, text: shown }
        });
        if done.is_empty() {
            self.pending = Some(Pending { id, lang: lang.to_string(), text: joined, since: now });
            return steps;
        }
        steps.push(Step::Translate { id, lang: lang.to_string(), text: done });
        if !rest.is_empty() {
            let id = next_id();
            steps.push(Step::Show { id, lang: lang.to_string(), text: rest.clone() });
            self.pending = Some(Pending { id, lang: lang.to_string(), text: rest, since: now });
        }
        steps
    }

    /// Whether the waiting end should be translated as it is: the speaker paused, or it has waited too long.
    pub fn due(&self, now: Instant, speaking: bool) -> bool {
        self.pending.as_ref().is_some_and(|p| {
            let waited = now.duration_since(p.since);
            (!speaking && waited >= PAUSE_FLUSH) || waited >= MAX_WAIT
        })
    }

    /// Translates the waiting end as it is.
    pub fn flush(&mut self) -> Option<Step> {
        self.pending.take().map(|p| Step::Translate { id: p.id, lang: p.lang, text: p.text })
    }
}

/// Drops the period the recognizer put where the audio was cut ("there is the." + "inference …").
fn without_cut_period(text: &str) -> &str {
    match text.strip_suffix('.') {
        Some(head) if !head.ends_with('.') && DANGLING.contains(&last_word(head).as_str()) => head,
        _ => text,
    }
}

fn last_word(text: &str) -> String {
    text.rsplit(char::is_whitespace)
        .next()
        .unwrap_or("")
        .trim_matches(|c: char| !c.is_alphanumeric() && c != '.')
        .to_lowercase()
}

/// Splits `text` after its last sentence end into the finished sentences and the unfinished rest.
fn split_finished(text: &str) -> (String, String) {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    for i in (0..chars.len()).rev() {
        let (at, c) = chars[i];
        let end = at + c.len_utf8();
        if matches!(c, '。' | '？' | '！') || (matches!(c, '.' | '?' | '!') && ends_sentence(text, i, &chars)) {
            return (text[..end].trim().to_string(), text[end..].trim().to_string());
        }
    }
    (String::new(), text.trim().to_string())
}

/// Whether the `.`, `?` or `!` at `chars[i]` ends a sentence.
fn ends_sentence(text: &str, i: usize, chars: &[(usize, char)]) -> bool {
    if chars.get(i + 1).is_some_and(|&(_, next)| !next.is_whitespace()) {
        return false; // "3.5", "...", "U.S."
    }
    if i > 0 && chars[i - 1].1 == '.' {
        return false; // the end of "..."
    }
    let word = last_word(&text[..chars[i].0]);
    let word = word.as_str();
    !(chars[i].1 == '.' && ABBREVIATIONS.contains(&word)) && !DANGLING.contains(&word)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> impl FnMut() -> u64 {
        let mut n = 0;
        move || {
            n += 1;
            n
        }
    }

    fn show(id: u64, text: &str) -> Step {
        Step::Show { id, lang: "en".into(), text: text.into() }
    }

    fn translate(id: u64, text: &str) -> Step {
        Step::Translate { id, lang: "en".into(), text: text.into() }
    }

    #[test]
    fn an_unfinished_end_waits_for_the_next_utterance() {
        let (mut a, mut next, now) = (Assembler::default(), ids(), Instant::now());
        assert_eq!(
            a.hear("en", "This is how it works. And so local hands will be the ability for", &mut next, now),
            vec![
                show(1, "This is how it works."),
                translate(1, "This is how it works."),
                show(2, "And so local hands will be the ability for"),
            ]
        );
        assert_eq!(
            a.hear("en", "that agent to work there. And it can", &mut next, now),
            vec![
                Step::Revise { id: 2, text: "And so local hands will be the ability for that agent to work there.".into() },
                translate(2, "And so local hands will be the ability for that agent to work there."),
                show(3, "And it can"),
            ]
        );
    }

    #[test]
    fn a_period_after_a_dangling_word_is_not_a_sentence_end() {
        let (mut a, mut next, now) = (Assembler::default(), ids(), Instant::now());
        assert_eq!(
            a.hear("en", "Right. And then there is the.", &mut next, now),
            vec![show(1, "Right."), translate(1, "Right."), show(2, "And then there is the.")]
        );
        assert_eq!(
            a.hear("en", "inference intelligence.", &mut next, now),
            vec![
                Step::Revise { id: 2, text: "And then there is the inference intelligence.".into() },
                translate(2, "And then there is the inference intelligence."),
            ]
        );
    }

    #[test]
    fn decimals_ellipses_and_abbreviations_do_not_end_a_sentence() {
        assert_eq!(split_finished("It costs 3.5 dollars"), (String::new(), "It costs 3.5 dollars".into()));
        assert_eq!(split_finished("Done. That's like..."), ("Done.".into(), "That's like...".into()));
        assert_eq!(split_finished("Ask Dr. Smith"), (String::new(), "Ask Dr. Smith".into()));
        assert_eq!(split_finished("Is it? Yes! Fine"), ("Is it? Yes!".into(), "Fine".into()));
        assert_eq!(split_finished("できた。次は"), ("できた。".into(), "次は".into()));
    }

    #[test]
    fn a_change_of_language_translates_the_waiting_end_first() {
        let (mut a, mut next, now) = (Assembler::default(), ids(), Instant::now());
        a.hear("en", "And then", &mut next, now);
        assert_eq!(
            a.hear("fr", "Bonjour.", &mut next, now),
            vec![
                translate(1, "And then"),
                Step::Show { id: 2, lang: "fr".into(), text: "Bonjour.".into() },
                Step::Translate { id: 2, lang: "fr".into(), text: "Bonjour.".into() },
            ]
        );
    }

    #[test]
    fn the_waiting_end_is_translated_after_a_pause_or_a_long_wait() {
        let (mut a, mut next, now) = (Assembler::default(), ids(), Instant::now());
        a.hear("en", "And then there is the", &mut next, now);
        assert!(!a.due(now + Duration::from_secs(1), false));
        assert!(!a.due(now + Duration::from_secs(5), true));
        assert!(a.due(now + PAUSE_FLUSH, false));
        assert!(a.due(now + MAX_WAIT, true));
        assert_eq!(a.flush(), Some(translate(1, "And then there is the")));
        assert!(!a.due(now + MAX_WAIT, false));
    }

    #[test]
    fn long_text_without_a_sentence_end_is_translated_as_it_is() {
        let (mut a, mut next, now) = (Assembler::default(), ids(), Instant::now());
        let long = "word ".repeat(70);
        let steps = a.hear("en", long.trim(), &mut next, now);
        assert_eq!(steps, vec![show(1, long.trim()), translate(1, long.trim())]);
        assert_eq!(a.flush(), None);
    }
}
