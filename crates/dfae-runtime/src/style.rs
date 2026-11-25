//! Answer style: compact, no filler. A small model told to be brief still pads its replies with
//! pleasantries and offers; this removes them deterministically so the user gets only the information.

use regex::Regex;
use std::sync::LazyLock;

/// Sentences that only offer more help, ask for permission or sign off.
static FILLER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:let me know|would you like|do you want|shall i|should i (?:proceed|continue|go ahead)|want me to|feel free|if you(?:'d| would| need| want| have)|happy to help|hope this helps|is there anything|anything else|don't hesitate)\b",
    )
    .unwrap()
});
static OPENER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^\s*(?:sure|certainly|of course|absolutely|great question|okay|ok|alright)[!,.]*\s+").unwrap());
static BOUNDARY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[.!?]\s+").unwrap());

/// Drop a leading "Sure!" and trailing offers/questions ("Let me know if you need anything else.").
/// Never empties the text.
pub fn compact(text: &str) -> String {
    let mut t = text.trim().to_string();
    if let Some(m) = OPENER.find(&t)
        && m.end() < t.len()
    {
        t = t[m.end()..].to_string();
        if let Some(c) = t.chars().next() {
            t.replace_range(..c.len_utf8(), &c.to_uppercase().to_string());
        }
    }
    for _ in 0..4 {
        let trimmed = t.trim_end().to_string();
        // the last line, if there is more than one
        if let Some((head, last)) = trimmed.rsplit_once('\n')
            && FILLER.is_match(last)
            && !head.trim().is_empty()
        {
            t = head.trim_end().to_string();
            continue;
        }
        // the last sentence of the last line
        let start = BOUNDARY.find_iter(&trimmed).last().map(|m| m.end());
        match start {
            Some(s) if FILLER.is_match(&trimmed[s..]) && !trimmed[..s].trim().is_empty() => t = trimmed[..s].trim_end().to_string(),
            None if FILLER.is_match(&trimmed) => break, // a single sentence: keep it rather than answer with nothing
            _ => {
                t = trimmed;
                break;
            }
        }
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_openers_and_offers_but_keeps_the_information() {
        assert_eq!(compact("Sure! The file has 3 lines. Let me know if you need anything else."), "The file has 3 lines.");
        assert_eq!(compact("It is 42.\n\nWould you like me to explain how?"), "It is 42.");
        assert_eq!(compact("Done: wrote 14 bytes to test.txt. Feel free to ask for more! Hope this helps."), "Done: wrote 14 bytes to test.txt.");
        assert_eq!(compact("Certainly, 6 * 7 = 42"), "6 * 7 = 42");
    }

    #[test]
    fn never_returns_nothing_and_leaves_normal_answers_alone() {
        assert_eq!(compact("Would you like to proceed?"), "Would you like to proceed?");
        let ok = "A mutex is a lock. It lets one thread in at a time.";
        assert_eq!(compact(ok), ok);
        assert_eq!(compact("  hello  "), "hello");
        assert_eq!(compact("Of course"), "Of course");
    }
}
