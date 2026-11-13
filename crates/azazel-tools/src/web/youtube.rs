//! YouTube addresses in `web_fetch` and `read`: the captions of the video, with the time each
//! part is said at.
//!
//! The page of a video is built by scripts and holds no text, so the captions are asked for the
//! way the YouTube app asks: the player API names the caption tracks of a video, and each track
//! is a document of timed text.

use quick_xml::events::Event;
use reqwest::Url;
use serde_json::{Value, json};

use super::fetch::{self, Fetcher, MAX_BODY_BYTES};
use crate::xml;

/// The app the player API is asked as. The caption addresses it hands a browser are only good
/// together with a token the page computes; the ones it hands the app work as they are.
const CLIENT_NAME: &str = "ANDROID";
const CLIENT_VERSION: &str = "20.10.38";
/// A line of the transcript ends once it holds this many characters,
const LINE_CHARS: usize = 200;
/// or covers this much time.
const LINE_MILLIS: u64 = 30_000;

/// Where YouTube is reached.
pub struct YouTube {
    site: String,
}

impl Default for YouTube {
    fn default() -> Self {
        YouTube::at("https://www.youtube.com")
    }
}

/// The id of the video `url` names, when it is the address of a video on YouTube.
pub(super) fn video(url: &Url) -> Option<String> {
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let mut segments = url.path_segments()?.filter(|segment| !segment.is_empty());
    let id = match host {
        "youtu.be" => segments.next()?.to_string(),
        "youtube.com" | "m.youtube.com" | "music.youtube.com" | "youtube-nocookie.com" => {
            match segments.next()? {
                "watch" => url.query_pairs().find(|(name, _)| name == "v")?.1.into_owned(),
                "shorts" | "embed" | "live" | "v" => segments.next()?.to_string(),
                _ => return None,
            }
        }
        _ => return None,
    };
    let is_id = id.len() == 11
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    is_id.then_some(id)
}

/// A part of the captions: when it starts, in milliseconds, and what is said.
#[derive(Debug, PartialEq, Eq)]
struct Cue {
    start: u64,
    text: String,
}

/// The cues of a timed text document. YouTube writes two kinds: `<p t="1200">` with the time in
/// milliseconds, and the older `<text start="1.2">` in seconds, whose text is escaped twice.
fn cues(timed_text: &str) -> Vec<Cue> {
    let mut reader = xml::reader(timed_text);
    let mut cues = Vec::new();
    // The start of the cue being read, and whether its text is escaped twice.
    let mut open: Option<(u64, bool)> = None;
    let mut text = String::new();
    while let Some(event) = xml::next(&mut reader) {
        match &event {
            Event::Start(start) => match start.name().0 {
                b"p" => {
                    let at = xml::attr(start, "t").and_then(|t| t.parse::<u64>().ok());
                    open = Some((at.unwrap_or(0), false));
                    text.clear();
                }
                b"text" => {
                    let at = xml::attr(start, "start").and_then(|t| t.parse::<f64>().ok());
                    open = Some(((at.unwrap_or(0.0) * 1000.0).round() as u64, true));
                    text.clear();
                }
                _ => {}
            },
            Event::End(end) if matches!(end.name().0, b"p" | b"text") => {
                let Some((start, twice)) = open.take() else {
                    continue;
                };
                let once = match twice {
                    true => quick_xml::escape::unescape(&text).ok(),
                    false => None,
                };
                let said = once.as_deref().unwrap_or(&text);
                let said = said.split_whitespace().collect::<Vec<_>>().join(" ");
                if !said.is_empty() {
                    cues.push(Cue { start, text: said });
                }
            }
            other if open.is_some() => xml::push_text(other, &mut text),
            _ => {}
        }
    }
    cues
}

/// `millis` as `m:ss`, or `h:mm:ss` from an hour on.
fn clock(millis: u64) -> String {
    let seconds = millis / 1000;
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    match hours {
        0 => format!("{minutes}:{seconds:02}"),
        _ => format!("{hours}:{minutes:02}:{seconds:02}"),
    }
}

/// The cues as lines of `[m:ss] text`. Captions come in pieces of a few words; a line gathers
/// them until it is [`LINE_CHARS`] long or covers [`LINE_MILLIS`].
fn transcript_lines(cues: &[Cue]) -> String {
    let mut out = String::new();
    // Where the line being written starts: in time, and in `out` after its stamp.
    let mut line: Option<(u64, usize)> = None;
    for cue in cues {
        match line {
            Some((start, from))
                if cue.start.saturating_sub(start) < LINE_MILLIS
                    && out[from..].chars().count() < LINE_CHARS =>
            {
                out.push(' ');
            }
            _ => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&format!("[{}] ", clock(cue.start)));
                line = Some((cue.start, out.len()));
            }
        }
        out.push_str(&cue.text);
    }
    out
}

/// The name a caption track goes by, such as `English (auto-generated)`.
fn track_name(track: &Value) -> &str {
    let name = &track["name"];
    name["simpleText"]
        .as_str()
        .or_else(|| name["runs"][0]["text"].as_str())
        .or_else(|| track["languageCode"].as_str())
        .unwrap_or("unnamed")
}

/// The track to read: captions a person wrote before those the site generated from the sound,
/// and English before other languages.
fn pick(tracks: &[Value]) -> Option<&Value> {
    let rank = |track: &Value| {
        let generated = track["kind"].as_str() == Some("asr");
        let english = track["languageCode"]
            .as_str()
            .is_some_and(|code| code == "en" || code.starts_with("en-"));
        (generated, !english)
    };
    tracks.iter().min_by_key(|track| rank(track))
}

impl YouTube {
    /// YouTube at `site` in place of `https://www.youtube.com`.
    pub fn at(site: &str) -> Self {
        YouTube {
            site: site.trim_end_matches('/').to_string(),
        }
    }

    /// The transcript of `video` under its title, its channel and its length.
    pub(super) async fn transcript(&self, video: &str, http: &Fetcher) -> Result<String, String> {
        let page = format!("https://www.youtube.com/watch?v={video}");
        let api = format!("{}/youtubei/v1/player?prettyPrint=false", self.site);
        let api = Url::parse(&api).map_err(|e| format!("{api}: {e}"))?;
        let ask = json!({
            "context": {"client": {"clientName": CLIENT_NAME, "clientVersion": CLIENT_VERSION}},
            "videoId": video,
        });
        let (api, response) = http.post(api, &ask).await?;
        if !response.status().is_success() {
            return Err(format!("YouTube returned HTTP {} for {page}", response.status()));
        }
        let player = fetch::body(response, &api, MAX_BODY_BYTES).await?;
        let player: Value = serde_json::from_slice(&player)
            .map_err(|e| format!("YouTube's answer for {page} could not be read: {e}"))?;
        let playable = &player["playabilityStatus"];
        if playable["status"].as_str() != Some("OK") {
            let why = playable["reason"].as_str().unwrap_or("it cannot be played");
            return Err(format!("{page} has no transcript to read: {why}"));
        }
        let tracks = &player["captions"]["playerCaptionsTracklistRenderer"]["captionTracks"];
        let tracks = tracks.as_array().map_or(&[][..], Vec::as_slice);
        let track =
            pick(tracks).ok_or_else(|| format!("{page} has no captions, so there is no transcript"))?;
        let address = track["baseUrl"].as_str().unwrap_or_default();
        let address = Url::parse(address)
            .or_else(|_| api.join(address))
            .map_err(|e| format!("YouTube named captions for {page} that cannot be fetched: {e}"))?;
        let (address, response) = http.get(address, &[]).await?;
        if !response.status().is_success() {
            return Err(format!(
                "YouTube returned HTTP {} for the captions of {page}",
                response.status()
            ));
        }
        let timed_text = fetch::body(response, &address, MAX_BODY_BYTES).await?;
        let cues = cues(&String::from_utf8_lossy(&timed_text));
        if cues.is_empty() {
            return Err(format!("YouTube returned no captions for {page}"));
        }

        let details = &player["videoDetails"];
        let mut out = format!("# {}\n", details["title"].as_str().unwrap_or(video));
        if let Some(author) = details["author"].as_str() {
            out.push_str(&format!("{author} · "));
        }
        let seconds = details["lengthSeconds"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok());
        if let Some(seconds) = seconds {
            out.push_str(&format!("{} · ", clock(seconds * 1000)));
        }
        out.push_str(&format!("{page}\nCaptions: {}", track_name(track)));
        let others: Vec<&str> = tracks
            .iter()
            .filter(|other| !std::ptr::eq(*other, track))
            .map(track_name)
            .collect();
        if !others.is_empty() {
            out.push_str(&format!(" (not shown: {})", others.join(", ")));
        }
        out.push_str("\n\n");
        out.push_str(&transcript_lines(&cues));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_addresses_are_known_in_every_form_and_other_pages_are_not_videos() {
        for address in [
            "https://www.youtube.com/watch?v=jNQXAC9IVRw",
            "https://youtube.com/watch?feature=share&v=jNQXAC9IVRw&t=10s",
            "https://m.youtube.com/watch?v=jNQXAC9IVRw",
            "https://music.youtube.com/watch?v=jNQXAC9IVRw",
            "https://youtu.be/jNQXAC9IVRw?si=abc",
            "https://www.youtube.com/shorts/jNQXAC9IVRw",
            "https://www.youtube.com/embed/jNQXAC9IVRw",
            "https://www.youtube.com/live/jNQXAC9IVRw",
            "https://www.youtube-nocookie.com/embed/jNQXAC9IVRw",
        ] {
            let id = video(&Url::parse(address).unwrap());
            assert_eq!(id.as_deref(), Some("jNQXAC9IVRw"), "{address}");
        }
        for address in [
            "https://www.youtube.com/",
            "https://www.youtube.com/watch",
            "https://www.youtube.com/watch?v=short",
            "https://www.youtube.com/watch?v=jNQXAC9IVR%2F",
            "https://www.youtube.com/@jawed",
            "https://www.youtube.com/playlist?list=PL1234567890",
            "https://www.youtube.com/results?search_query=zoo",
            "https://youtu.be/",
            "https://example.com/watch?v=jNQXAC9IVRw",
        ] {
            assert_eq!(video(&Url::parse(address).unwrap()), None, "{address}");
        }
    }

    fn cue(start: u64, text: &str) -> Cue {
        Cue {
            start,
            text: text.into(),
        }
    }

    #[test]
    fn both_kinds_of_timed_text_become_cues() {
        // What the app is given: milliseconds, words of generated captions in `<s>`, and
        // paragraphs that only hold a line break.
        let new = "<?xml version=\"1.0\" encoding=\"utf-8\" ?><timedtext format=\"3\">\n<head>\n\
                   <ws id=\"0\"/>\n</head>\n<body>\n<w t=\"0\" id=\"1\"/>\n\
                   <p t=\"320\" d=\"14260\" w=\"1\">[Music]</p>\n\
                   <p t=\"18790\" w=\"1\" a=\"1\">\n</p>\n\
                   <p t=\"18800\" d=\"7160\" w=\"1\"><s ac=\"0\">We&#39;re</s><s t=\"239\"> no</s>\
                   <s t=\"559\"> strangers</s></p>\n\
                   <p t=\"22640\" d=\"4320\">You know the rules\nand so do I</p>\n</body>\n</timedtext>";
        assert_eq!(
            cues(new),
            [
                cue(320, "[Music]"),
                cue(18800, "We're no strangers"),
                cue(22640, "You know the rules and so do I"),
            ]
        );
        let old = "<?xml version=\"1.0\" encoding=\"utf-8\" ?><transcript>\
                   <text start=\"1.2\" dur=\"2.16\">All right, so here we are</text>\
                   <text start=\"12.616\" dur=\"1.751\">and that&amp;#39;s &amp;quot;cool&amp;quot;</text>\
                   </transcript>";
        assert_eq!(
            cues(old),
            [
                cue(1200, "All right, so here we are"),
                cue(12616, "and that's \"cool\""),
            ]
        );
        assert_eq!(cues("not timed text at all"), []);
    }

    #[test]
    fn cues_gather_into_lines_by_length_and_by_time() {
        let close = [
            cue(1200, "one"),
            cue(3000, "two"),
            cue(31_199, "three"),
            cue(31_200, "four"),
        ];
        assert_eq!(transcript_lines(&close), "[0:01] one two three\n[0:31] four");

        let long = "word ".repeat(45);
        let long = long.trim();
        let wordy = [cue(0, long), cue(2000, "next"), cue(3_723_000, "late")];
        assert_eq!(
            transcript_lines(&wordy),
            format!("[0:00] {long}\n[0:02] next\n[1:02:03] late")
        );
        assert_eq!(transcript_lines(&[]), "");
    }

    #[test]
    fn written_captions_win_over_generated_ones_and_english_over_other_languages() {
        let track = |code: &str, kind: Option<&str>, name: &str| {
            let mut track = json!({"languageCode": code, "name": {"runs": [{"text": name}]}});
            if let Some(kind) = kind {
                track["kind"] = json!(kind);
            }
            track
        };
        let all = [
            track("de", None, "German"),
            track("en", Some("asr"), "English (auto-generated)"),
            track("en-GB", None, "English (United Kingdom)"),
        ];
        assert_eq!(pick(&all).map(track_name), Some("English (United Kingdom)"));
        assert_eq!(pick(&all[..2]).map(track_name), Some("German"));
        assert_eq!(pick(&all[1..2]).map(track_name), Some("English (auto-generated)"));
        assert_eq!(pick(&[]), None);
        assert_eq!(track_name(&json!({"name": {"simpleText": "Dutch"}})), "Dutch");
        assert_eq!(track_name(&json!({"languageCode": "nl"})), "nl");
    }
}
