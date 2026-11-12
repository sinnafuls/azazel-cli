//! Voice in the UI: dictation into the input and replies read aloud.

use std::time::{Duration, Instant};

use azazel_ai::{Message, StopReason};
use azazel_tui::{Role, Spinner};
use crossterm::event::{KeyEvent, KeyEventKind};
use tokio::task::AbortHandle;
use tokio_util::sync::CancellationToken;

use super::{Internal, Tui};
use crate::voice::record::{self, Recording};
use crate::voice::{dictate, insert, speak, stt};

/// Cells of the level meter.
const METER_CELLS: usize = 8;

/// The quietest level the meter shows, in decibels below full scale. Speech at arm's length
/// sits around -25 dB, so a linear meter would hardly move.
const METER_FLOOR_DB: f32 = -48.0;

/// A `voice` key this soon after the last one comes from a key that is held down. Not every
/// terminal marks such repeats, and the first follows the press by up to half a second.
const HELD_KEY_GAP: Duration = Duration::from_millis(700);

/// What a voice task running in the background came back with.
pub(super) enum Done {
    /// The transcript of a recording, or why there is none.
    Heard(Result<String, String>),
    /// The utterance with this number has ended, or could not be spoken.
    Spoken(u64, Result<(), String>),
}

#[derive(Default)]
enum Dictation {
    #[default]
    Idle,
    Recording(Box<Recording>),
    Transcribing(AbortHandle),
}

#[derive(Default)]
pub(super) struct Voice {
    dictation: Dictation,
    /// `/speak on`: the last reply of each run is read aloud.
    speak: bool,
    /// The lead's newest message of the run in progress.
    reply: String,
    /// The utterance being fetched or played, with what stops it.
    speaking: Option<(u64, CancellationToken)>,
    utterances: u64,
    /// When the `voice` key last arrived.
    pressed: Option<Instant>,
}

impl Drop for Voice {
    /// A reply is not read on after the UI is gone.
    fn drop(&mut self) {
        if let Some((_, cancel)) = &self.speaking {
            cancel.cancel();
        }
    }
}

/// A bar that fills with the loudness of what the microphone hears.
fn meter(level: f32) -> String {
    let decibels = 20.0 * level.max(f32::MIN_POSITIVE).log10();
    let filled = ((1.0 - decibels / METER_FLOOR_DB) * METER_CELLS as f32)
        .round()
        .clamp(0.0, METER_CELLS as f32) as usize;
    format!("{}{}", "▮".repeat(filled), "▯".repeat(METER_CELLS - filled))
}

impl Tui {
    /// The `voice` key. A key that is held down would otherwise start a recording and end it
    /// straight away.
    pub(super) async fn voice_pressed(&mut self, key: &KeyEvent) {
        let now = Instant::now();
        let held = key.kind == KeyEventKind::Repeat
            || self
                .voice
                .pressed
                .is_some_and(|last| now.duration_since(last) < HELD_KEY_GAP);
        self.voice.pressed = Some(now);
        if !held {
            self.voice_toggle().await;
        }
    }

    /// `/voice`, and the `voice` key: starts recording, or stops it and transcribes.
    pub(super) async fn voice_toggle(&mut self) {
        match self.voice.dictation {
            Dictation::Recording(_) => self.transcribe_recording(),
            Dictation::Transcribing(_) => {
                self.notice = Some("still transcribing; esc cancels".to_string());
            }
            Dictation::Idle => {
                if !stt::configured(&self.app.config.voice) {
                    return self.fail(stt::NOT_SET);
                }
                // The microphone would pick the speech up.
                self.hush();
                match record::start(&self.app.config.voice).await {
                    Ok(recording) => self.voice.dictation = Dictation::Recording(Box::new(recording)),
                    Err(why) => self.fail(&format!("cannot record: {why}")),
                }
            }
        }
    }

    fn transcribe_recording(&mut self) {
        let Dictation::Recording(recording) = std::mem::take(&mut self.voice.dictation) else {
            return;
        };
        let (app, done) = (self.app.clone(), self.internal_tx.clone());
        let task = tokio::spawn(async move {
            let heard = dictate(*recording, &app.config, &app.credentials).await;
            let _ = done.send(Internal::Voice(Done::Heard(heard)));
        });
        self.voice.dictation = Dictation::Transcribing(task.abort_handle());
    }

    /// Ends a recording that reached a limit or whose recorder stopped by itself.
    pub(super) fn voice_tick(&mut self) {
        let Dictation::Recording(recording) = &mut self.voice.dictation else {
            return;
        };
        let limit = self.app.config.voice.max_secs;
        let timed_out = limit > 0 && recording.elapsed() >= Duration::from_secs(limit);
        if timed_out {
            self.notice = Some(format!("recording stopped at the limit of {limit} s"));
        }
        if timed_out || recording.ended() {
            self.transcribe_recording();
        }
    }

    /// Whether the voice indicator moves and its limits need watching.
    pub(super) fn voice_animates(&self) -> bool {
        !matches!(self.voice.dictation, Dictation::Idle)
    }

    /// What voice is doing, for the rule above the input.
    pub(super) fn voice_status(&self) -> Option<String> {
        let theme = &self.theme;
        let muted = |text: &str| theme.paint(Role::Muted, text);
        match &self.voice.dictation {
            Dictation::Recording(recording) => {
                let seconds = recording.elapsed().as_secs();
                let key = match self.keymap.keys_for("voice").first() {
                    Some(key) => key.to_string(),
                    None => "/voice".to_string(),
                };
                Some(format!(
                    "{} {}",
                    theme.paint(
                        Role::Error,
                        &format!("● rec {}:{:02}", seconds / 60, seconds % 60)
                    ),
                    muted(&format!(
                        "{} · {key} transcribes · esc cancels",
                        meter(recording.level())
                    )),
                ))
            }
            Dictation::Transcribing(_) => Some(format!(
                "{} {}",
                theme.paint(Role::Accent, Spinner::now()),
                muted("transcribing · esc cancels"),
            )),
            // The agent's own activity matters more than a reply still being read.
            Dictation::Idle if self.voice.speaking.is_some() && !self.busy => {
                Some(muted("speaking · esc or typing stops"))
            }
            Dictation::Idle => None,
        }
    }

    /// Stops the reply being read aloud. Returns whether there was one.
    fn hush(&mut self) -> bool {
        match self.voice.speaking.take() {
            Some((_, cancel)) => {
                cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// Sees every key before the input does: any key stops speech, and the interrupt key cancels
    /// voice input. Returns whether the key is used up.
    pub(super) fn voice_key(&mut self, key: &KeyEvent) -> bool {
        let escape = self.overlay.is_none() && self.keymap.action_for(key) == Some("abort");
        let mut used = self.hush() && escape;
        if escape && self.voice_animates() {
            // Dropping a recording stops it.
            if let Dictation::Transcribing(task) = std::mem::take(&mut self.voice.dictation) {
                task.abort();
            }
            self.notice = Some("voice input cancelled".to_string());
            used = true;
        }
        used
    }

    /// Remembers the lead's newest message: the last one of a run is the reply.
    pub(super) fn voice_reply(&mut self, message: &Message) {
        if self.voice.speak {
            self.voice.reply = message.text();
        }
    }

    /// Reads the reply of the run that just ended, when spoken replies are on.
    pub(super) fn voice_run_ended(&mut self, stop: &StopReason) {
        let reply = std::mem::take(&mut self.voice.reply);
        if !self.voice.speak || *stop == StopReason::Aborted {
            return;
        }
        let text = speak::speakable(&reply);
        if text.is_empty() {
            return;
        }
        self.hush();
        self.voice.utterances += 1;
        let number = self.voice.utterances;
        let cancel = CancellationToken::new();
        self.voice.speaking = Some((number, cancel.clone()));
        let (app, done) = (self.app.clone(), self.internal_tx.clone());
        tokio::spawn(async move {
            let spoken = speak::say(&app.config, &app.credentials, &text, &cancel).await;
            let _ = done.send(Internal::Voice(Done::Spoken(number, spoken)));
        });
    }

    pub(super) async fn voice_done(&mut self, done: Done) {
        match done {
            Done::Heard(heard) => {
                // A transcript that arrives after voice input was cancelled is dropped.
                if !matches!(self.voice.dictation, Dictation::Transcribing(_)) {
                    return;
                }
                self.voice.dictation = Dictation::Idle;
                match heard {
                    Ok(text) if text.is_empty() => self.notice = Some("heard nothing".to_string()),
                    Ok(text) => {
                        insert(&mut self.editor, &text);
                        self.refresh_completion();
                        if self.app.config.voice.auto_send {
                            self.submit(false).await;
                        }
                    }
                    Err(why) => self.fail(&why),
                }
            }
            Done::Spoken(number, spoken) => {
                if matches!(&self.voice.speaking, Some((current, _)) if *current == number) {
                    self.voice.speaking = None;
                }
                if let Err(why) = spoken {
                    self.warn(&format!("{why} (/speak off turns spoken replies off)"));
                }
            }
        }
    }

    /// `/speak on|off|stop`.
    pub(super) fn speak_command(&mut self, args: &str) {
        match args {
            "on" if self.app.config.voice.tts.trim().is_empty() => self.fail(
                "spoken replies are not set up: set [voice] tts = \"provider/model\" (for example \"openai/tts-1\")",
            ),
            "on" => {
                self.voice.speak = true;
                self.info("spoken replies: on (esc or typing stops one)");
            }
            "off" => {
                self.voice.speak = false;
                self.hush();
                self.info("spoken replies: off");
            }
            "stop" => {
                let stopped = self.hush();
                self.info(if stopped { "stopped" } else { "nothing is being spoken" });
            }
            "" => self.info(&format!(
                "spoken replies: {} (/speak on|off|stop)",
                if self.voice.speak { "on" } else { "off" }
            )),
            _ => self.fail("usage: /speak on|off|stop"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_meter_follows_loudness_in_decibels() {
        assert_eq!(meter(0.0), "▯▯▯▯▯▯▯▯");
        assert_eq!(meter(0.003), "▯▯▯▯▯▯▯▯");
        // -24 dB, a normal speaking level, fills half.
        assert_eq!(meter(0.063), "▮▮▮▮▯▯▯▯");
        assert_eq!(meter(1.0), "▮▮▮▮▮▮▮▮");
        assert_eq!(meter(7.0), "▮▮▮▮▮▮▮▮");
    }
}
