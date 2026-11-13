//! `/loop`: one prompt sent again and again, a number of times, on an interval, or until a
//! condition holds. The loop itself is here; the interactive UI drives it (`tui/loops.rs`).
//!
//! Whether a condition holds is judged by the small model (`smol`), outside the session: it is
//! shown the end of the conversation and answers yes or no.

use std::time::{Duration, Instant};

use azazel_agent::AgentInfo;
use azazel_ai::{Block, Message, ModelSpec, Role};
use azazel_core::SessionStore;
use azazel_core::side::{Side, SideQuestion};
use tokio_util::sync::CancellationToken;

/// The role that judges a condition; the lead's own models follow it.
const JUDGE_ROLE: &str = "smol";
const JUDGE_LIMIT: Duration = Duration::from_secs(90);
/// How much of the conversation the judge is shown, from its end.
const EVIDENCE_CHARS: usize = 20_000;
/// How much of one tool result or one call's input the judge is shown.
const RESULT_CHARS: usize = 2_000;
const INPUT_CHARS: usize = 300;
/// The most rounds a counted loop takes.
const MAX_ROUNDS: u32 = 1000;
/// The shortest interval: a loop is not a way to flood a provider.
const MIN_EVERY: Duration = Duration::from_secs(5);

pub const USAGE: &str = "usage: /loop <count> <prompt>, /loop <interval> <prompt> (30s, 10m, 1h), or /loop until \
                         \"<condition>\" <prompt>; /loop stop ends it, esc too";

/// A whole number of minutes as `90m`, `2h`, `3d`; shorter than a minute, as seconds.
pub fn span_text(span: Duration) -> String {
    let seconds = span.as_secs();
    match seconds / 60 {
        _ if seconds % 60 != 0 => format!("{seconds}s"),
        m if m % 1440 == 0 => format!("{}d", m / 1440),
        m if m % 60 == 0 => format!("{}h", m / 60),
        m => format!("{m}m"),
    }
}

/// A length of time as typed: `90s`, `10m`, `2h`, `1d`, `1h30m`, `10 minutes`, `hour`.
pub fn span(text: &str) -> Result<Duration, String> {
    let text = text.trim().to_ascii_lowercase();
    let bad = || format!("`{text}` is not a length of time: a number and a unit, such as 90s, 10m, 2h or 1d");
    let mut total = 0u64;
    let mut rest = text.as_str();
    let mut parts = 0;
    while !rest.is_empty() {
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        // `every hour` is `every 1 hour`.
        let count: u64 = match digits {
            0 if parts == 0 => 1,
            0 => return Err(bad()),
            _ => rest[..digits].parse().map_err(|_| bad())?,
        };
        rest = rest[digits..].trim_start();
        let letters = rest.chars().take_while(char::is_ascii_alphabetic).count();
        let seconds = match &rest[..letters] {
            "s" | "sec" | "secs" | "second" | "seconds" => 1,
            "m" | "min" | "mins" | "minute" | "minutes" => 60,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3600,
            "d" | "day" | "days" => 86_400,
            "w" | "week" | "weeks" => 7 * 86_400,
            _ => return Err(bad()),
        };
        total = count
            .checked_mul(seconds)
            .and_then(|part| total.checked_add(part))
            .ok_or_else(bad)?;
        rest = rest[letters..].trim_start();
        parts += 1;
    }
    match total {
        0 => Err(bad()),
        total => Ok(Duration::from_secs(total)),
    }
}

/// When a loop ends by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Until {
    /// After this many rounds.
    Rounds(u32),
    /// Never: a round starts this long after the one before it started.
    Every(Duration),
    /// Once the judge says this holds.
    Condition(String),
}

/// A loop in progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loop {
    pub until: Until,
    pub prompt: String,
    /// Rounds sent so far.
    round: u32,
    /// Rounds after which an interval or a condition gives up; 0 is no limit.
    max: u32,
    /// When the round in progress was sent.
    sent: Option<Instant>,
}

/// What `/loop <args>` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    Start(Loop),
    Stop,
    Status,
}

/// What a loop does once a round has ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// Send the prompt again now.
    Send,
    /// Send it again after this long.
    Wait(Duration),
    /// Ask the judge whether the condition holds, then see [`Loop::after_verdict`].
    Judge,
    /// The loop is over; the line says why.
    Done(String),
}

/// What the judge made of the condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub met: bool,
    pub why: String,
}

/// Reads the arguments of `/loop`. `max` is `[schedule] loop_max`.
pub fn parse(args: &str, max: u32) -> Result<Parsed, String> {
    let args = args.trim();
    match args {
        "" | "status" => return Ok(Parsed::Status),
        "stop" | "off" | "cancel" | "end" => return Ok(Parsed::Stop),
        _ => {}
    }
    let (first, rest) = args.split_once(char::is_whitespace).unwrap_or((args, ""));
    let rest = rest.trim();
    let (until, prompt) = if first.eq_ignore_ascii_case("until") {
        let quote = rest.chars().next().filter(|c| matches!(c, '"' | '\''));
        let quoted = quote.and_then(|quote| rest[1..].split_once(quote));
        let Some((condition, prompt)) = quoted else {
            return Err(format!(
                "put the condition in quotes: /loop until \"the tests pass\" <prompt>\n{USAGE}"
            ));
        };
        if condition.trim().is_empty() {
            return Err(format!("the condition is empty\n{USAGE}"));
        }
        (Until::Condition(condition.trim().to_string()), prompt.trim())
    } else if let Ok(rounds) = first.trim_end_matches(['x', 'X']).parse::<u32>() {
        if !(1..=MAX_ROUNDS).contains(&rounds) {
            return Err(format!("a loop takes between 1 and {MAX_ROUNDS} rounds"));
        }
        (Until::Rounds(rounds), rest)
    } else if let Ok(every) = span(first) {
        if every < MIN_EVERY {
            return Err(format!(
                "the shortest interval of a loop is {}",
                span_text(MIN_EVERY)
            ));
        }
        (Until::Every(every), rest)
    } else {
        return Err(USAGE.to_string());
    };
    if prompt.is_empty() {
        return Err(format!("the loop needs a prompt\n{USAGE}"));
    }
    Ok(Parsed::Start(Loop {
        until,
        prompt: prompt.to_string(),
        round: 0,
        max,
        sent: None,
    }))
}

impl Loop {
    /// Notes that a round's prompt went out at `now`, and returns its number.
    pub fn sent(&mut self, now: Instant) -> u32 {
        self.round += 1;
        self.sent = Some(now);
        self.round
    }

    /// The round in progress as a few words: `round 2 of 5`, `round 3, every 10m`.
    pub fn describe(&self) -> String {
        match &self.until {
            Until::Rounds(rounds) => format!("round {} of {rounds}", self.round),
            Until::Every(every) => format!("round {}, every {}", self.round, span_text(*every)),
            Until::Condition(condition) => format!("round {}, until \"{condition}\"", self.round),
        }
    }

    fn exhausted(&self) -> Option<Next> {
        (self.max != 0 && self.round >= self.max).then(|| {
            Next::Done(format!(
                "loop stopped after {} rounds (`[schedule] loop_max`)",
                self.round
            ))
        })
    }

    /// What follows a round that ended at `now` without failing.
    pub fn after_round(&self, now: Instant) -> Next {
        match &self.until {
            Until::Rounds(rounds) if self.round >= *rounds => {
                Next::Done(format!("loop finished: {} rounds", self.round))
            }
            Until::Rounds(_) => Next::Send,
            Until::Every(every) => self.exhausted().unwrap_or_else(|| {
                let since = self
                    .sent
                    .map_or(Duration::ZERO, |sent| now.saturating_duration_since(sent));
                Next::Wait(every.saturating_sub(since))
            }),
            Until::Condition(_) => Next::Judge,
        }
    }

    /// What follows the judge's answer. A judge that cannot be asked ends the loop: it would
    /// otherwise go on without anything to stop it.
    pub fn after_verdict(&self, verdict: Result<Verdict, String>) -> Next {
        match verdict {
            Ok(Verdict { met: true, why }) => {
                Next::Done(format!("loop finished after {} rounds: {why}", self.round))
            }
            Ok(Verdict { met: false, .. }) => self.exhausted().unwrap_or(Next::Send),
            Err(why) => Next::Done(format!(
                "loop stopped: the condition could not be checked ({why})"
            )),
        }
    }
}

fn cut(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}

/// One message as lines of plain text.
fn rendered(message: &Message) -> String {
    let mut out = String::new();
    let who = match message.role {
        Role::User => "user",
        Role::Assistant => "agent",
        Role::Tool => "tool",
    };
    for block in &message.content {
        let line = match block {
            Block::Text { text } if !text.trim().is_empty() => format!("{who}: {}", text.trim()),
            Block::ToolCall { name, input, .. } => {
                format!("agent calls {name}: {}", cut(&input.to_string(), INPUT_CHARS))
            }
            Block::ToolResult {
                content, is_error, ..
            } => {
                let text: String = content
                    .iter()
                    .filter_map(|block| match block {
                        Block::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<&str>>()
                    .join("\n");
                let kind = if *is_error { "tool error" } else { "tool result" };
                format!("{kind}: {}", cut(text.trim(), RESULT_CHARS))
            }
            _ => continue,
        };
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// The end of the conversation as text, newest last.
fn evidence(session: &SessionStore) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut size = 0;
    for entry in session.build_context(None).iter().rev() {
        let part = rendered(&entry.message);
        size += part.chars().count();
        if size > EVIDENCE_CHARS && !parts.is_empty() {
            break;
        }
        parts.push(part);
    }
    parts.reverse();
    parts.concat()
}

/// Reads the judge's answer: `YES` or `NO` first, then why.
pub fn verdict(answer: &str) -> Result<Verdict, String> {
    let answer = answer.trim();
    let word: String = answer
        .chars()
        .skip_while(|c| !c.is_ascii_alphabetic())
        .take_while(char::is_ascii_alphabetic)
        .collect();
    let met = match word.to_ascii_lowercase().as_str() {
        "yes" => true,
        "no" => false,
        _ => {
            return Err(format!(
                "the judge answered neither yes nor no: {}",
                cut(answer.lines().next().unwrap_or_default(), 80)
            ));
        }
    };
    let why = answer
        .split_once(&word)
        .map(|(_, rest)| {
            rest.trim_start_matches(|c: char| c.is_whitespace() || "*_.:,;!-–—".contains(c))
                .trim()
        })
        .filter(|why| !why.is_empty())
        .map(|why| why.lines().next().unwrap_or_default().trim().to_string());
    Ok(Verdict {
        met,
        why: why.unwrap_or_else(|| match met {
            true => "the condition holds".to_string(),
            false => "the condition does not hold yet".to_string(),
        }),
    })
}

fn question(condition: &str, prompt: &str, conversation: &str) -> String {
    format!(
        "A coding agent is sent the same prompt again and again until a condition holds. Decide whether it \
         holds now.\n\nThe prompt: {prompt}\nThe condition: {condition}\n\nThe end of the conversation so \
         far:\n<conversation>\n{conversation}</conversation>\n\nGo by what the conversation shows was done \
         and what the tools printed, not by what the agent intends or claims without showing it. Answer \
         with YES or NO as the first word, then one short sentence that says why."
    )
}

/// Asks the small model whether `condition` holds, going by the end of the session.
pub async fn judge(
    side: &Side,
    agent: &AgentInfo,
    lead_chain: Vec<ModelSpec>,
    session: &SessionStore,
    condition: &str,
    prompt: &str,
) -> Result<Verdict, String> {
    let mut chain = vec![ModelSpec::Role(JUDGE_ROLE.to_string())];
    chain.extend(lead_chain);
    let mut ignore = |_: &str| {};
    let asked = side.ask(
        SideQuestion {
            agent,
            context: Vec::new(),
            question: question(condition, prompt, &evidence(session)),
            chain,
            purpose: "loop",
            framed: false,
        },
        &mut ignore,
        CancellationToken::new(),
    );
    match tokio::time::timeout(JUDGE_LIMIT, asked).await {
        Ok(Ok(answer)) => verdict(&answer.text),
        Ok(Err(why)) => Err(why),
        Err(_) => Err("the judge took too long".to_string()),
    }
}

/// For tests, which have no terminal to type `/loop` into: the arguments of `AZAZEL_TEST_LOOP`
/// start a loop whose first round is the prompt of the run.
#[cfg(feature = "test-support")]
pub fn from_env(max: u32) -> Option<Loop> {
    let args = std::env::var("AZAZEL_TEST_LOOP").ok()?;
    match parse(&args, max) {
        Ok(Parsed::Start(mut looped)) => {
            looped.sent(Instant::now());
            Some(looped)
        }
        _ => None,
    }
}

/// For tests: what the loop does after a round, as the UI would go about it. `Ok` is the
/// prompt of the next round, `Err` why the loop is over.
#[cfg(feature = "test-support")]
pub async fn drive(
    looped: &mut Loop,
    side: Option<std::sync::Arc<Side>>,
    lead: &azazel_agent::AgentHandle,
    session: &SessionStore,
) -> Result<String, String> {
    let mut next = looped.after_round(Instant::now());
    loop {
        next = match next {
            Next::Send => {
                looped.sent(Instant::now());
                return Ok(looped.prompt.clone());
            }
            Next::Wait(wait) => {
                tokio::time::sleep(wait).await;
                Next::Send
            }
            Next::Judge => {
                let Until::Condition(condition) = &looped.until else {
                    return Err("only a condition is judged".to_string());
                };
                let verdict = match &side {
                    Some(side) => {
                        let chain = lead.status().chain;
                        judge(side, lead.info(), chain, session, condition, &looped.prompt).await
                    }
                    None => Err("no model is available in this session".to_string()),
                };
                looped.after_verdict(verdict)
            }
            Next::Done(why) => return Err(why),
        };
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn started(args: &str, max: u32) -> Loop {
        match parse(args, max).unwrap() {
            Parsed::Start(looped) => looped,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_arguments_of_loop_say_how_it_ends() {
        assert_eq!(parse("", 50), Ok(Parsed::Status));
        assert_eq!(parse(" stop ", 50), Ok(Parsed::Stop));
        let counted = started("3 fix one failing test", 50);
        assert_eq!(
            (counted.until, counted.prompt.as_str()),
            (Until::Rounds(3), "fix one failing test")
        );
        assert_eq!(started("5x go", 50).until, Until::Rounds(5));
        let timed = started("10m  look at the deploy", 50);
        assert_eq!(
            (timed.until, timed.prompt.as_str()),
            (Until::Every(Duration::from_secs(600)), "look at the deploy")
        );
        let judged = started("until \"cargo test passes\" fix the next failure", 50);
        assert_eq!(
            (judged.until, judged.prompt.as_str()),
            (
                Until::Condition("cargo test passes".to_string()),
                "fix the next failure"
            )
        );
        assert_eq!(
            started("until 'it says \"done\"' go on", 50).until,
            Until::Condition("it says \"done\"".to_string())
        );

        let error = |args: &str| parse(args, 50).unwrap_err();
        assert!(error("until the tests pass fix them").starts_with("put the condition in quotes"));
        assert!(error("until \"\" go").starts_with("the condition is empty"));
        assert!(error("3").starts_with("the loop needs a prompt"));
        assert!(error("0 go").contains("between 1 and 1000"));
        assert!(error("2s go").contains("shortest interval"));
        assert_eq!(error("sometimes go"), USAGE);
    }

    #[test]
    fn a_counted_loop_sends_its_rounds_and_ends() {
        let mut looped = started("2 go", 50);
        let now = Instant::now();
        assert_eq!(looped.sent(now), 1);
        assert_eq!(looped.describe(), "round 1 of 2");
        assert_eq!(looped.after_round(now), Next::Send);
        assert_eq!(looped.sent(now), 2);
        assert_eq!(
            looped.after_round(now),
            Next::Done("loop finished: 2 rounds".to_string())
        );
    }

    #[test]
    fn a_timed_loop_waits_out_what_is_left_of_its_interval() {
        let mut looped = started("10m go", 2);
        let sent = Instant::now();
        looped.sent(sent);
        assert_eq!(looped.describe(), "round 1, every 10m");
        assert_eq!(
            looped.after_round(sent + Duration::from_secs(90)),
            Next::Wait(Duration::from_secs(510))
        );
        // A round that took longer than the interval is followed at once.
        assert_eq!(
            looped.after_round(sent + Duration::from_secs(700)),
            Next::Wait(Duration::ZERO)
        );
        looped.sent(sent + Duration::from_secs(600));
        assert_eq!(
            looped.after_round(sent + Duration::from_secs(660)),
            Next::Done("loop stopped after 2 rounds (`[schedule] loop_max`)".to_string())
        );
        // No limit: it goes on until it is stopped.
        let mut endless = started("10m go", 0);
        for _ in 0..100 {
            endless.sent(sent);
        }
        assert_eq!(endless.after_round(sent), Next::Wait(Duration::from_secs(600)));
    }

    #[test]
    fn a_loop_with_a_condition_goes_by_the_judge() {
        let mut looped = started("until \"the tests pass\" fix one", 2);
        let now = Instant::now();
        looped.sent(now);
        assert_eq!(looped.after_round(now), Next::Judge);
        let no = || {
            Ok(Verdict {
                met: false,
                why: "two tests fail".into(),
            })
        };
        assert_eq!(looped.after_verdict(no()), Next::Send);
        looped.sent(now);
        assert_eq!(
            looped.after_verdict(Ok(Verdict {
                met: true,
                why: "all 12 tests pass".into()
            })),
            Next::Done("loop finished after 2 rounds: all 12 tests pass".to_string())
        );
        // Still not there after the last round it may take.
        assert_eq!(
            looped.after_verdict(no()),
            Next::Done("loop stopped after 2 rounds (`[schedule] loop_max`)".to_string())
        );
        // A check that fails ends the loop.
        assert_eq!(
            looped.after_verdict(Err("no model answers".into())),
            Next::Done("loop stopped: the condition could not be checked (no model answers)".to_string())
        );
    }

    #[test]
    fn the_judges_answer_is_yes_or_no_and_why() {
        assert_eq!(
            verdict("YES. All 12 tests pass in the last run.\nMore words."),
            Ok(Verdict {
                met: true,
                why: "All 12 tests pass in the last run.".into()
            })
        );
        assert_eq!(
            verdict("**No** - `parse_empty` still fails"),
            Ok(Verdict {
                met: false,
                why: "`parse_empty` still fails".into()
            })
        );
        assert_eq!(
            verdict("no"),
            Ok(Verdict {
                met: false,
                why: "the condition does not hold yet".into()
            })
        );
        assert_eq!(
            verdict("yes\nbecause the build is green"),
            Ok(Verdict {
                met: true,
                why: "because the build is green".into()
            })
        );
        assert!(
            verdict("It depends on what you mean.")
                .unwrap_err()
                .starts_with("the judge answered neither yes nor no: It depends")
        );
        assert!(verdict("").is_err());
    }

    #[test]
    fn lengths_of_time_are_read_and_written() {
        assert_eq!(span("90s"), Ok(Duration::from_secs(90)));
        assert_eq!(span("1h30m"), Ok(Duration::from_secs(5400)));
        assert_eq!(span("2 hours"), Ok(Duration::from_secs(7200)));
        assert_eq!(span("day"), Ok(Duration::from_secs(86_400)));
        assert!(span("ten minutes").is_err());
        assert!(span("0m").is_err());
        assert!(span("5").is_err());
        let text = |seconds: u64| span_text(Duration::from_secs(seconds));
        assert_eq!(
            [
                text(45),
                text(90),
                text(600),
                text(7200),
                text(172_800),
                text(5400)
            ],
            ["45s", "90s", "10m", "2h", "2d", "90m"]
        );
    }
}
