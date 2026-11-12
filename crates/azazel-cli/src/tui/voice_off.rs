//! What the UI's voice hooks do in a build without the `voice` feature: nothing, apart from
//! saying so when voice is asked for.

use azazel_ai::{Message, StopReason};
use crossterm::event::KeyEvent;

use super::Tui;

const MISSING: &str = "this build of azazel has no voice support";

impl Tui {
    pub(super) async fn voice_pressed(&mut self, _key: &KeyEvent) {
        self.fail(MISSING);
    }

    pub(super) async fn voice_toggle(&mut self) {
        self.fail(MISSING);
    }

    pub(super) fn voice_tick(&mut self) {}

    pub(super) fn voice_animates(&self) -> bool {
        false
    }

    pub(super) fn voice_status(&self) -> Option<String> {
        None
    }

    pub(super) fn voice_key(&mut self, _key: &KeyEvent) -> bool {
        false
    }

    pub(super) fn voice_reply(&mut self, _message: &Message) {}

    pub(super) fn voice_run_ended(&mut self, _stop: &StopReason) {}

    pub(super) fn speak_command(&mut self, _args: &str) {
        self.fail(MISSING);
    }
}
