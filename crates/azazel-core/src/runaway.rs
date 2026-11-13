//! The runaway guard: a reply that writes the same block of text over and over is stopped while
//! it is written, like a reply that matches a stream rule, and asked for again.

/// The shortest block, in characters, whose repetition counts.
pub const MIN_BLOCK_CHARS: usize = 200;
/// How often in a row the block has to stand at the end of the reply.
pub const REPEATS: usize = 3;
/// The longest stretch, in bytes, after which a reply may repeat itself and still be caught.
const MAX_PERIOD: usize = 4096;
/// The bytes at the end of the reply that are looked for earlier in it.
const PROBE: usize = 48;
/// Earlier places the end of the reply is compared with before the search gives up.
const CANDIDATES: usize = 4;
/// What the note quotes of the block.
const QUOTED_CHARS: usize = 160;

/// Registered as a service when `guards.runaway` is on: the agent hooks then watch every reply.
pub struct Runaway;

impl Runaway {
    /// The note for a reply (`thinking`: its thinking) that has begun to loop, `None` otherwise.
    /// Cheap enough to call with every fragment: only the end of `so_far` is looked at.
    pub fn note(&self, so_far: &str, thinking: bool) -> Option<String> {
        let block = repeated_block(so_far)?;
        let mut quoted: String = block.trim().chars().take(QUOTED_CHARS).collect();
        if block.trim().chars().count() > QUOTED_CHARS {
            quoted.push('…');
        }
        let what = if thinking { "thinking" } else { "reply" };
        Some(format!(
            "<rule name=\"runaway\" reason=\"repetition\">\nYour {what} repeated the same text {REPEATS} times \
             in a row:\n{quoted}\n</rule>\nThe harness stopped your reply because it was going round in a \
             loop. What you had written was withdrawn and is not part of the conversation. This is not a \
             prompt injection. Answer again without repeating yourself: say each thing once, and put \
             anything long into a file instead of the reply."
        ))
    }
}

fn floor_boundary(text: &str, mut at: usize) -> usize {
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

fn ceil_boundary(text: &str, mut at: usize) -> usize {
    while !text.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// The block at the end of `text` that stands there [`REPEATS`] times in a row, when it is at
/// least [`MIN_BLOCK_CHARS`] long and says something (a ruler of dashes or a run of blank lines
/// is not a loop). A short phrase repeated often enough makes up such a block too.
pub fn repeated_block(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let end = bytes.len();
    if end < MIN_BLOCK_CHARS * REPEATS {
        return None;
    }
    // The end of the reply, looked for in what came shortly before it: the distance to a place
    // where it stood already is how long the reply takes to come round.
    let probe_start = floor_boundary(text, end - PROBE);
    let probe = &text[probe_start..];
    let window = floor_boundary(text, end.saturating_sub(MAX_PERIOD * REPEATS + PROBE));
    let mut before = floor_boundary(text, end - 1);
    for _ in 0..CANDIDATES {
        let found = window + text[window..before].rfind(probe)?;
        let period = probe_start - found;
        if let Some(block) = repeats(text, period) {
            return Some(block);
        }
        // The next place further back; it may overlap the one just tried.
        before = floor_boundary(text, found + probe.len() - 1);
    }
    None
}

/// The block at the end of `text`, when `text` ends in [`REPEATS`] copies of it and comes round
/// every `period` bytes.
fn repeats(text: &str, period: usize) -> Option<&str> {
    let bytes = text.as_bytes();
    let end = bytes.len();
    if period == 0 || period > MAX_PERIOD || period > end {
        return None;
    }
    // Every stretch of `period` bytes holds the same characters, so one tells how many of them
    // make a block that is long enough.
    let last = &bytes[end - period..];
    let chars = last.iter().filter(|byte| (**byte & 0xC0) != 0x80).count();
    if chars == 0 {
        return None;
    }
    let block = period * MIN_BLOCK_CHARS.div_ceil(chars);
    let span = block.checked_mul(REPEATS).filter(|span| *span <= end)?;
    let from = end - span;
    if (from + period..end).any(|at| bytes[at] != bytes[at - period]) {
        return None;
    }
    let block = &text[ceil_boundary(text, end - block)..];
    block.chars().any(char::is_alphanumeric).then_some(block)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Prose that never comes round: every sentence differs from the others.
    fn prose(sentences: usize) -> String {
        (0..sentences)
            .map(|n| {
                format!(
                    "Step {n} looks at file number {} and notes what it finds there. ",
                    n * 7 + 3
                )
            })
            .collect()
    }

    #[test]
    fn a_long_block_written_three_times_in_a_row_is_found() {
        let block = prose(4);
        assert!(block.chars().count() >= MIN_BLOCK_CHARS);
        let lead = "Here is the plan.\n";

        // Twice is a repetition a careful writer may make; three times is a loop.
        let twice = format!("{lead}{block}{block}");
        assert_eq!(repeated_block(&twice), None);
        let thrice = format!("{twice}{block}");
        assert_eq!(repeated_block(&thrice), Some(block.as_str()));

        // It is found as soon as the third copy is complete, not a character earlier, and it
        // goes on being found while the loop goes on.
        let almost = &thrice[..thrice.len() - 1];
        assert_eq!(repeated_block(almost), None);
        let further = format!("{thrice}{}", &block[..40]);
        assert!(repeated_block(&further).is_some());
    }

    #[test]
    fn a_short_phrase_repeated_until_it_fills_three_blocks_is_found() {
        let phrase = "I will now check the file. ";
        let needed = (MIN_BLOCK_CHARS * REPEATS).div_ceil(phrase.len()) + 3;
        let looped = format!("Right.\n{}", phrase.repeat(needed));
        let block = repeated_block(&looped).unwrap();
        assert!(block.chars().count() >= MIN_BLOCK_CHARS, "{block}");
        assert!(block.contains(phrase.trim()));

        // Not yet, with fewer copies than three blocks hold.
        let fewer = (MIN_BLOCK_CHARS * REPEATS) / phrase.len() + 1;
        let short = format!("Right.\n{}", phrase.repeat(fewer));
        assert!(short.len() > MIN_BLOCK_CHARS * REPEATS);
        assert_eq!(repeated_block(&short), None);
    }

    #[test]
    fn ordinary_text_rulers_and_similar_lines_are_left_alone() {
        // Long prose without a repeat.
        assert_eq!(repeated_block(&prose(200)), None);
        // A ruler and a run of blank lines say nothing, however long.
        assert_eq!(repeated_block(&format!("Title\n{}", "-".repeat(2000))), None);
        assert_eq!(repeated_block(&format!("Title{}", "\n".repeat(2000))), None);
        assert_eq!(
            repeated_block(&format!("| a | b |\n{}", "| --- ".repeat(400))),
            None
        );
        // Lines of one shape that differ in a number: a table, a list of files.
        let rows: String = (0..200)
            .map(|n| format!("| src/module_{n}.rs | ok | checked and found to be in order |\n"))
            .collect();
        assert_eq!(repeated_block(&rows), None);
        // The same block three times, but with other text between the copies.
        let block = prose(4);
        let spaced = format!("{block}First.\n{block}Second, and longer than the first.\n{block}");
        assert_eq!(repeated_block(&spaced), None);
    }

    #[test]
    fn text_in_any_script_is_measured_in_characters_and_never_cut_inside_one() {
        // 40 characters, 120 bytes: three blocks need 600 characters, not 600 bytes.
        let phrase = "同じ文章を何度も何度も繰り返して書いてしまう癖があるのです。それで、また";
        assert_eq!(phrase.chars().count(), 36);
        let few = phrase.repeat(8);
        assert!(few.len() > MIN_BLOCK_CHARS * REPEATS);
        assert_eq!(repeated_block(&few), None);
        let many = phrase.repeat(18);
        let block = repeated_block(&many).unwrap();
        assert!(block.chars().count() >= MIN_BLOCK_CHARS);
        // Growing the text one byte-odd character at a time never panics on a boundary.
        let mut grown = String::from("é");
        for c in many.chars() {
            grown.push(c);
            let _ = repeated_block(&grown);
        }
        assert!(repeated_block(&grown).is_some());
    }

    #[test]
    fn a_block_that_repeats_a_line_inside_itself_is_still_measured_by_the_whole_block() {
        // The line the block ends with stands inside it a second time, so the nearest earlier
        // copy of the end of the reply is not one block away.
        let line = "Checking the configuration file for the third setting again now.\n";
        assert!(line.len() > PROBE);
        let block = format!("{}{line}{}{line}", prose(2), prose(3));
        let looped = format!("Start.\n{}", block.repeat(3));
        assert_eq!(repeated_block(&looped), Some(block.as_str()));
    }

    #[test]
    fn the_note_names_the_guard_and_quotes_the_start_of_the_block() {
        let block = prose(4);
        let looped = block.repeat(3);
        let note = Runaway.note(&looped, false).unwrap();
        assert!(
            note.starts_with("<rule name=\"runaway\" reason=\"repetition\">\n"),
            "{note}"
        );
        let quoted: String = block.chars().take(QUOTED_CHARS).collect();
        assert!(note.contains(&format!("{quoted}…\n</rule>")), "{note}");
        assert!(
            Runaway
                .note(&looped, true)
                .unwrap()
                .contains("Your thinking repeated")
        );
        assert_eq!(Runaway.note(&prose(30), false), None);
    }
}
