//! Spoken replies: what of a reply is read aloud, the OpenAI-style `audio/speech` request, and
//! playback through the system's audio API or a player program.

use std::process::Stdio;

use azazel_ai::Credentials;
use azazel_core::Config;
use azazel_core::config::VoiceConfig;
use serde_json::json;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

#[cfg(any(windows, target_os = "macos"))]
use super::native;
use super::wav;
use super::{
    CLIENT, DEFAULT_VOICE, Endpoint, endpoint, first_installed, last_line, program, short_name, with_file,
};

/// Most characters of a reply that are read aloud.
pub const MAX_CHARS: usize = 1_200;

/// What is said in place of a code block.
const CODE: &str = "Code omitted.";

/// Player programs tried in order when `voice.play_command` is empty and the system's audio API
/// is not used. Each plays the WAV file given as its last argument and then exits.
const PLAYERS: &[&str] = &[
    "ffplay -nodisp -autoexit -loglevel quiet",
    "afplay",
    "paplay",
    "aplay -q",
];

/// One line of Markdown as it should sound: no markers, link targets or addresses.
fn plain(line: &str) -> String {
    let mut rest = line.trim();
    // Quote bars, heading marks and list bullets lead a line.
    rest = rest.trim_start_matches(['>', ' ']);
    rest = rest.trim_start_matches('#').trim_start();
    if let Some(item) = ["- ", "* ", "+ "]
        .iter()
        .find_map(|bullet| rest.strip_prefix(bullet))
    {
        rest = item.trim_start_matches("[ ] ").trim_start_matches("[x] ");
    }
    // A table's rule line says nothing.
    if !rest.is_empty() && rest.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ')) {
        return String::new();
    }
    let mut out = String::with_capacity(rest.len());
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '`' | '*' => {}
            '|' => {
                // A table row is read as a list of its cells.
                out.truncate(out.trim_end().len());
                out.push_str(", ");
            }
            // An image is announced by its description only.
            '!' if chars.peek() == Some(&'[') => {}
            '[' => {
                let label: String = chars.by_ref().take_while(|c| *c != ']').collect();
                if chars.peek() == Some(&'(') {
                    // `[text](target)` is read as its text.
                    chars.by_ref().take_while(|c| *c != ')').for_each(drop);
                    out.push_str(&label);
                } else {
                    out.push_str(&format!("[{label}]"));
                }
            }
            _ => out.push(c),
        }
    }
    let words: Vec<&str> = out
        .split_whitespace()
        .map(
            |word| match word.starts_with("http://") || word.starts_with("https://") {
                true => "link",
                false => word,
            },
        )
        .collect();
    words.join(" ").trim_matches([',', ' ']).to_string()
}

/// Cuts `text` to at most `limit` characters, at the end of a sentence when one is near and
/// otherwise between words.
fn cut(text: &str, limit: usize) -> String {
    let Some((end, _)) = text.char_indices().nth(limit) else {
        return text.to_string();
    };
    let head = &text[..end];
    let sentence = head
        .rmatch_indices(['.', '!', '?'])
        .map(|(at, _)| at + 1)
        .find(|at| head[*at..].starts_with(' ') || *at == head.len());
    let kept = match sentence {
        // Not when that would throw most of the text away.
        Some(at) if at * 2 >= head.len() => at,
        _ => head.rfind(' ').unwrap_or(head.len()),
    };
    head[..kept].trim_end().to_string()
}

/// What of a reply is read aloud: its prose, with each code block replaced by a short remark,
/// Markdown markers removed, and no more than [`MAX_CHARS`] characters.
pub fn speakable(reply: &str) -> String {
    let mut out = String::new();
    // The fence that opened the code block being skipped.
    let mut fence: Option<(char, usize)> = None;
    // The next text starts a new block, so the one before it gets a full stop.
    let mut new_block = false;
    let mut say = |text: &str, new_block: &mut bool| {
        if text.is_empty() {
            return;
        }
        if !out.is_empty() {
            if *new_block && !out.ends_with(['.', '!', '?', ':', ';', ',']) {
                out.push('.');
            }
            out.push(' ');
        }
        out.push_str(text);
        *new_block = false;
    };
    for line in reply.lines() {
        let trimmed = line.trim_start();
        let mark = trimmed.chars().next().filter(|c| matches!(c, '`' | '~'));
        let run = mark.map_or(0, |mark| trimmed.chars().take_while(|c| *c == mark).count());
        match (fence, mark) {
            (Some((open, length)), Some(mark)) if mark == open && run >= length => fence = None,
            (Some(_), _) => {}
            (None, Some(mark)) if run >= 3 => {
                fence = Some((mark, run));
                new_block = true;
                say(CODE, &mut new_block);
                new_block = true;
            }
            (None, _) if trimmed.is_empty() => new_block = true,
            (None, _) => {
                // Headings and list items are blocks of their own.
                if trimmed.starts_with(['#', '-', '*', '+', '|']) {
                    new_block = true;
                }
                let heading = trimmed.starts_with('#');
                say(&plain(trimmed), &mut new_block);
                new_block |= heading;
            }
        }
    }
    cut(&out, MAX_CHARS)
}

/// Asks `{base_url}/audio/speech` to read `text` and returns the WAV audio.
pub async fn synthesize(
    client: &reqwest::Client,
    endpoint: &Endpoint,
    voice: &str,
    text: &str,
) -> Result<Vec<u8>, String> {
    let voice = match voice.trim() {
        "" => DEFAULT_VOICE,
        voice => voice,
    };
    let body = json!({
        "model": endpoint.model,
        "voice": voice,
        "input": text,
        "response_format": "wav",
    });
    let request = endpoint.post(client, "audio/speech").json(&body);
    endpoint.send("speech", request).await
}

/// The player program to run: the configured one, else the first installed default. `None`
/// where the system's audio API plays.
fn player(settings: &VoiceConfig) -> Result<Option<&str>, String> {
    let configured = settings.play_command.trim();
    if !configured.is_empty() {
        return Ok(Some(configured));
    }
    if cfg!(any(windows, target_os = "macos")) {
        return Ok(None);
    }
    first_installed(PLAYERS).map(Some).ok_or_else(|| {
        "no player program found: install ffplay, paplay or aplay, or set [voice] play_command".to_string()
    })
}

/// What plays speech on this machine, for `azazel doctor`.
pub fn describe(settings: &VoiceConfig) -> Result<String, String> {
    match player(settings)? {
        Some(command) => {
            let configured = !settings.play_command.trim().is_empty();
            let (found, _) = program(command).map_err(|why| match configured {
                true => format!("{why} (from [voice] play_command)"),
                false => why,
            })?;
            Ok(format!("the program {}", found.display()))
        }
        #[cfg(any(windows, target_os = "macos"))]
        None => native::output_name().map(|name| format!("the default output device ({name})")),
        #[cfg(not(any(windows, target_os = "macos")))]
        None => Err("no player".to_string()),
    }
}

/// Plays WAV audio until it ends or `cancel` fires.
pub async fn play(settings: &VoiceConfig, audio: &[u8], cancel: &CancellationToken) -> Result<(), String> {
    let pcm = wav::decode(audio).map_err(|e| format!("speech cannot be played: {e}"))?;
    let command = match player(settings)? {
        Some(command) => command,
        #[cfg(any(windows, target_os = "macos"))]
        None => {
            let cancel = cancel.clone();
            return tokio::task::spawn_blocking(move || native::play(&pcm, &cancel))
                .await
                .map_err(|e| format!("speech cannot be played: {e}"))?;
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        None => return Err("no player".to_string()),
    };
    let (program, args) = program(command).map_err(|why| format!("{why} (from [voice] play_command)"))?;
    let name = short_name(&program);
    // Players want a file, and one whose header has the lengths a streamed reply leaves out.
    let dir = tempfile::tempdir().map_err(|e| format!("could not save the speech: {e}"))?;
    let file = dir.path().join("speech.wav");
    let clean = wav::encode(&wav::to_i16(&pcm.samples), pcm.rate, pcm.channels);
    tokio::fs::write(&file, clean)
        .await
        .map_err(|e| format!("could not save the speech: {e}"))?;
    let child = Command::new(&program)
        .args(with_file(args, &file))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("could not start {name}: {e}"))?;
    tokio::select! {
        // Dropping the wait stops the player.
        _ = cancel.cancelled() => Ok(()),
        output = child.wait_with_output() => match output {
            Ok(output) if output.status.success() => Ok(()),
            Ok(output) => {
                let said = last_line(&output.stderr);
                Err(match said.is_empty() {
                    true => format!("{name} failed ({})", output.status),
                    false => format!("{name} failed ({}): {said}", output.status),
                })
            }
            Err(e) => Err(format!("could not run {name}: {e}")),
        },
    }
}

/// Reads `text` aloud with the `[voice] tts` model, until it ends or `cancel` fires.
pub async fn say(
    config: &Config,
    credentials: &Credentials,
    text: &str,
    cancel: &CancellationToken,
) -> Result<(), String> {
    let settings = &config.voice;
    let work = async {
        let endpoint = endpoint(config, credentials, "tts", &settings.tts).await?;
        synthesize(&CLIENT, &endpoint, &settings.voice, text).await
    };
    let audio = tokio::select! {
        _ = cancel.cancelled() => return Ok(()),
        audio = work => audio?,
    };
    play(settings, &audio, cancel).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_blocks_are_replaced_and_markers_removed() {
        let reply = "## Fixed\n\nThe **bug** was in `parse()`:\n\n```rust\nfn parse() {}\n```\n\n\
                     - see [the docs](https://example.com/a) for more\n- run the tests\n\nDone.";
        assert_eq!(
            speakable(reply),
            "Fixed. The bug was in parse(): Code omitted. see the docs for more. run the tests. Done."
        );
    }

    #[test]
    fn tables_images_and_addresses_are_tamed() {
        assert_eq!(speakable("| a | b |\n|---|---|\n| 1 | 2 |"), "a, b. 1, 2");
        assert_eq!(
            speakable("see `items[0]` (the first)"),
            "see items[0] (the first)"
        );
        assert_eq!(
            speakable("![a chart](chart.png) at https://example.com/x now"),
            "a chart at link now"
        );
        assert_eq!(
            speakable("> quoted *text*\n\n1. first\n- [x] done"),
            "quoted text. 1. first. done"
        );
    }

    #[test]
    fn long_replies_are_cut_at_a_sentence_or_a_word() {
        let sentence = "This sentence has exactly forty characters. ";
        let long = sentence.repeat(40);
        let spoken = speakable(&long);
        assert!(spoken.chars().count() <= MAX_CHARS);
        assert!(spoken.ends_with("characters."));
        assert_eq!(spoken.chars().count(), 44 * 27 - 1);

        // No sentence end in reach: between words, and counted in characters, not bytes.
        let words = "żółw ".repeat(400);
        let spoken = speakable(&words);
        assert_eq!(spoken.chars().count(), 1_199);
        assert!(spoken.ends_with("żółw"));

        assert_eq!(cut("short", 10), "short");
        assert_eq!(cut("One. Two three four", 12), "One. Two");
        assert_eq!(cut("One two. Three four", 12), "One two.");
    }
}
