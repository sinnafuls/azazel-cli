//! WAV in and out, and the conversion to what speech services expect: 16 kHz, mono, 16-bit.
//!
//! The reader accepts what recorders and speech endpoints really send. A program that writes WAV
//! to a pipe cannot go back to fill in the lengths of its header, so they are zero or all ones;
//! the audio then simply runs to the end of the stream.

use std::f64::consts::PI;

/// The sample rate recordings are converted to.
pub const SPEECH_RATE: u32 = 16_000;

/// Decoded audio: interleaved samples between -1 and 1.
#[derive(Debug, Clone, PartialEq)]
pub struct Pcm {
    pub rate: u32,
    pub channels: u16,
    pub samples: Vec<f32>,
}

impl Pcm {
    /// The channels averaged into one.
    pub fn mono(&self) -> Vec<f32> {
        let channels = usize::from(self.channels.max(1));
        if channels == 1 {
            return self.samples.clone();
        }
        self.samples
            .chunks_exact(channels)
            .map(|frame| frame.iter().sum::<f32>() / channels as f32)
            .collect()
    }

    pub fn seconds(&self) -> f32 {
        let frames = self.samples.len() / usize::from(self.channels.max(1));
        frames as f32 / self.rate.max(1) as f32
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Int,
    Float,
}

/// How the samples of a WAV stream are stored, and where they start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    pub rate: u32,
    pub channels: u16,
    encoding: Encoding,
    bits: u16,
    /// Offset of the first sample.
    pub data_at: usize,
    /// Length the header claims for the audio; `None` when it is a streaming placeholder.
    declared: Option<usize>,
}

impl Format {
    /// Headerless audio as recorders write it with their "raw" switch.
    pub fn raw_speech() -> Format {
        Format {
            rate: SPEECH_RATE,
            channels: 1,
            encoding: Encoding::Int,
            bits: 16,
            data_at: 0,
            declared: None,
        }
    }

    /// Bytes of one frame (one sample of every channel).
    pub fn frame_bytes(&self) -> usize {
        usize::from(self.bits / 8) * usize::from(self.channels)
    }

    /// Decodes whole samples from `data`, which must start on a sample boundary.
    pub fn samples<'a>(&self, data: &'a [u8]) -> impl Iterator<Item = f32> + 'a {
        let (encoding, bits) = (self.encoding, self.bits);
        data.chunks_exact(usize::from(bits / 8))
            .map(move |raw| match (encoding, bits) {
                (Encoding::Int, 8) => (f32::from(raw[0]) - 128.0) / 128.0,
                (Encoding::Int, 16) => f32::from(i16::from_le_bytes([raw[0], raw[1]])) / 32_768.0,
                (Encoding::Int, 24) => {
                    // Sign-extended through the top byte of an i32.
                    (i32::from_le_bytes([0, raw[0], raw[1], raw[2]]) >> 8) as f32 / 8_388_608.0
                }
                (Encoding::Int, _) => {
                    i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as f32 / 2_147_483_648.0
                }
                (Encoding::Float, 32) => f32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]),
                (Encoding::Float, _) => {
                    let mut wide = [0u8; 8];
                    wide.copy_from_slice(raw);
                    f64::from_le_bytes(wide) as f32
                }
            })
    }

    /// The loudest sample in `data`, between 0 and 1.
    pub fn peak(&self, data: &[u8]) -> f32 {
        self.samples(data)
            .fold(0.0f32, |peak, s| peak.max(s.abs()))
            .min(1.0)
    }
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Reads the header of a WAV stream up to the start of its audio. Fails on anything that is not
/// WAV, on a header that is not complete yet, and on encodings other than plain PCM.
pub fn format(bytes: &[u8]) -> Result<Format, String> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a WAV stream".to_string());
    }
    let mut at = 12;
    let mut found: Option<(Encoding, u16, u16, u32)> = None;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = u32_at(bytes, at + 4);
        let body = at + 8;
        if id == b"data" {
            let (encoding, bits, channels, rate) =
                found.ok_or_else(|| "the WAV stream has no format chunk".to_string())?;
            // A writer that could not seek back leaves 0 or 0xFFFFFFFF here.
            let declared = (size != 0 && size != u32::MAX).then_some(size as usize);
            return Ok(Format {
                rate,
                channels,
                encoding,
                bits,
                data_at: body,
                declared,
            });
        }
        let end = body.saturating_add(size as usize);
        if id == b"fmt " {
            if size < 16 || body + 16 > bytes.len() {
                return Err("the WAV format chunk is cut short".to_string());
            }
            let mut tag = u16_at(bytes, body);
            let channels = u16_at(bytes, body + 2);
            let rate = u32_at(bytes, body + 4);
            let bits = u16_at(bytes, body + 14);
            // The extensible form names the real encoding in its sub-format.
            if tag == 0xFFFE && size >= 26 && body + 26 <= bytes.len() {
                tag = u16_at(bytes, body + 24);
            }
            let encoding = match (tag, bits) {
                (1, 8 | 16 | 24 | 32) => Encoding::Int,
                (3, 32 | 64) => Encoding::Float,
                _ => return Err(format!("unsupported WAV encoding (format {tag}, {bits} bits)")),
            };
            if channels == 0 || rate == 0 {
                return Err("the WAV format chunk names no channels or no sample rate".to_string());
            }
            found = Some((encoding, bits, channels, rate));
        }
        // Chunks are padded to an even length.
        at = end.saturating_add(end & 1);
    }
    Err("the WAV stream ends before its audio starts".to_string())
}

/// Decodes a whole WAV stream. Audio runs to the end of the stream when the header's length is a
/// placeholder or claims more than there is.
pub fn decode(bytes: &[u8]) -> Result<Pcm, String> {
    let format = format(bytes)?;
    Ok(decode_as(&format, bytes))
}

/// Decodes the audio of `bytes` as `format` describes it.
pub fn decode_as(format: &Format, bytes: &[u8]) -> Pcm {
    let available = bytes.len().saturating_sub(format.data_at);
    let length = format
        .declared
        .map_or(available, |declared| declared.min(available));
    let length = length - length % format.frame_bytes().max(1);
    let data = &bytes[format.data_at.min(bytes.len())..][..length];
    Pcm {
        rate: format.rate,
        channels: format.channels,
        samples: format.samples(data).collect(),
    }
}

/// A 16-bit PCM WAV file with a correct header.
pub fn encode(samples: &[i16], rate: u32, channels: u16) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let block = channels * 2;
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * u32::from(block)).to_le_bytes());
    out.extend_from_slice(&block.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

/// The inverse of decoding 16-bit samples, so that such audio passes through unchanged.
pub fn to_i16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|s| (s * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i16)
        .collect()
}

/// Mono audio at any rate as a 16 kHz, 16-bit WAV file of at most `max_bytes`.
pub fn speech_wav(mono: &[f32], rate: u32, max_bytes: usize) -> Vec<u8> {
    let mut samples = to_i16(&resample(mono, rate, SPEECH_RATE));
    samples.truncate(max_bytes.saturating_sub(44) / 2);
    encode(&samples, SPEECH_RATE, 1)
}

/// Zero crossings of the filter on each side of its centre. Twelve keeps a tone in the pass band
/// within a fraction of a percent and what lies above the new rate's limit below one percent.
const LOBES: f64 = 12.0;

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// Changes the sample rate of mono audio with a windowed-sinc filter. Going down, the filter also
/// removes what the lower rate cannot carry, which would otherwise fold back as noise.
pub fn resample(input: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || from == 0 || to == 0 || input.is_empty() {
        return input.to_vec();
    }
    let (from, to) = (u64::from(from), u64::from(to));
    // Fraction of the input's bandwidth that is kept.
    let cutoff = if to < from {
        0.95 * to as f64 / from as f64
    } else {
        1.0
    };
    let half = (LOBES / cutoff).ceil() as usize;
    let taps = 2 * half;
    // An output sample sits between two input samples at one of a few repeating fractions; the
    // filter is worked out once for each.
    let step = gcd(from, to);
    let phases = (to / step) as usize;
    let mut table = vec![0.0f32; phases * taps];
    let mut row = vec![0.0f64; taps];
    for (phase, weights) in table.chunks_exact_mut(taps).enumerate() {
        let fraction = (phase as u64 * step) as f64 / to as f64;
        let mut sum = 0.0;
        for (tap, weight) in row.iter_mut().enumerate() {
            // Distance, in input samples, from the output sample to this input sample.
            let x = tap as f64 - (half as f64 - 1.0) - fraction;
            let t = PI * x * cutoff;
            let sinc = if t.abs() < 1e-9 { 1.0 } else { t.sin() / t };
            let window = 0.5 + 0.5 * (PI * x / half as f64).cos();
            *weight = sinc * window;
            sum += *weight;
        }
        // Scaled so that a constant input stays exactly where it is.
        for (weight, value) in weights.iter_mut().zip(&row) {
            *weight = (value / sum) as f32;
        }
    }
    let length = (input.len() as u64 * to / from) as usize;
    let mut out = Vec::with_capacity(length);
    for n in 0..length as u64 {
        let position = n * from;
        let base = (position / to) as usize;
        let phase = ((position % to) / step) as usize;
        let weights = &table[phase * taps..][..taps];
        // The first tap belongs to input sample `base - half + 1`.
        let first = base as isize - half as isize + 1;
        let mut acc = 0.0f32;
        for (tap, weight) in weights.iter().enumerate() {
            if let Some(sample) = usize::try_from(first + tap as isize)
                .ok()
                .and_then(|index| input.get(index))
            {
                acc += sample * weight;
            }
        }
        out.push(acc);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f64, rate: u32, seconds: f64) -> Vec<f32> {
        (0..(f64::from(rate) * seconds) as usize)
            .map(|n| (2.0 * PI * hz * n as f64 / f64::from(rate)).sin() as f32)
            .collect()
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    #[test]
    fn resampling_keeps_a_tone_and_its_length() {
        for from in [48_000, 44_100, 8_000, 22_050] {
            let out = resample(&tone(440.0, from, 0.5), from, SPEECH_RATE);
            assert_eq!(out.len(), 8_000, "from {from}");
            let expected = tone(440.0, SPEECH_RATE, 0.5);
            // The ends lack the neighbours the filter needs.
            let worst = out[200..7_800]
                .iter()
                .zip(&expected[200..7_800])
                .map(|(a, b)| (a - b).abs())
                .fold(0.0, f32::max);
            assert!(worst < 0.01, "from {from}: off by {worst}");
        }
    }

    #[test]
    fn resampling_down_removes_what_the_lower_rate_cannot_carry() {
        // 12 kHz is above the 8 kHz limit of a 16 kHz stream; unfiltered it would come out as a
        // loud 4 kHz tone.
        let out = resample(&tone(12_000.0, 48_000, 0.5), 48_000, SPEECH_RATE);
        assert!(rms(&out[200..7_800]) < 0.01, "rms {}", rms(&out));
        // A constant level is kept exactly.
        let flat = resample(&[0.5; 4_800], 48_000, SPEECH_RATE);
        assert!(flat[100..1_500].iter().all(|s| (s - 0.5).abs() < 1e-4));
    }

    #[test]
    fn encoded_wav_has_a_canonical_header_and_length() {
        let wav = encode(&[0, 1, -1, i16::MAX], 16_000, 1);
        assert_eq!(wav.len(), 44 + 8);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(u32_at(&wav, 4) as usize, wav.len() - 8);
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(u32_at(&wav, 16), 16);
        assert_eq!(u16_at(&wav, 20), 1, "PCM");
        assert_eq!(u16_at(&wav, 22), 1, "mono");
        assert_eq!(u32_at(&wav, 24), 16_000);
        assert_eq!(u32_at(&wav, 28), 32_000, "bytes per second");
        assert_eq!(u16_at(&wav, 32), 2, "bytes per frame");
        assert_eq!(u16_at(&wav, 34), 16, "bits");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(u32_at(&wav, 40), 8);
        assert_eq!(&wav[44..], &[0, 0, 1, 0, 0xFF, 0xFF, 0xFF, 0x7F]);

        let pcm = decode(&wav).unwrap();
        assert_eq!((pcm.rate, pcm.channels, pcm.samples.len()), (16_000, 1, 4));
        assert_eq!(to_i16(&pcm.samples), [0, 1, -1, i16::MAX]);
    }

    #[test]
    fn speech_wav_is_16_khz_mono_and_capped() {
        let wav = speech_wav(&tone(440.0, 48_000, 1.0), 48_000, usize::MAX);
        let pcm = decode(&wav).unwrap();
        assert_eq!((pcm.rate, pcm.channels), (16_000, 1));
        assert_eq!(pcm.samples.len(), 16_000);
        assert!((pcm.seconds() - 1.0).abs() < 1e-6);
        assert_eq!(wav.len(), 44 + 32_000);

        let capped = speech_wav(&tone(440.0, 48_000, 1.0), 48_000, 1_044);
        assert_eq!(capped.len(), 1_044);
        assert_eq!(u32_at(&capped, 40), 1_000);
    }

    /// A stereo 44.1 kHz stream as a recorder writes it to a pipe: an extra chunk before the
    /// audio and placeholder lengths.
    fn streamed(placeholder: u32) -> Vec<u8> {
        let mut wav = encode(&[1000, -1000, 2000, -2000, 3000, -3000], 44_100, 2);
        wav[4..8].copy_from_slice(&placeholder.to_le_bytes());
        wav[40..44].copy_from_slice(&placeholder.to_le_bytes());
        // A `LIST` chunk of odd length, padded, between `fmt ` and `data`.
        let mut list = b"LIST".to_vec();
        list.extend_from_slice(&3u32.to_le_bytes());
        list.extend_from_slice(b"abc\0");
        wav.splice(36..36, list);
        wav
    }

    #[test]
    fn a_streamed_wav_runs_to_the_end_of_the_stream() {
        for placeholder in [0, u32::MAX] {
            let pcm = decode(&streamed(placeholder)).unwrap();
            assert_eq!((pcm.rate, pcm.channels), (44_100, 2));
            assert_eq!(pcm.samples.len(), 6);
            assert_eq!(pcm.mono(), [0.0, 0.0, 0.0]);
        }
        // A trailing half frame is dropped.
        let mut cut = streamed(0);
        cut.truncate(cut.len() - 1);
        assert_eq!(decode(&cut).unwrap().samples.len(), 4);
    }

    #[test]
    fn other_sample_types_decode() {
        let with = |tag: u16, bits: u16, data: &[u8]| {
            let mut wav = encode(&[], 8_000, 1);
            wav[20..22].copy_from_slice(&tag.to_le_bytes());
            wav[34..36].copy_from_slice(&bits.to_le_bytes());
            wav[40..44].copy_from_slice(&(data.len() as u32).to_le_bytes());
            wav.extend_from_slice(data);
            decode(&wav).map(|pcm| pcm.samples)
        };
        assert_eq!(with(1, 8, &[128, 255, 0]).unwrap(), [0.0, 127.0 / 128.0, -1.0]);
        assert_eq!(with(1, 24, &[0, 0, 0x40, 0, 0, 0x80]).unwrap(), [0.5, -1.0]);
        assert_eq!(with(1, 32, &[0, 0, 0, 0x40]).unwrap(), [0.5]);
        assert_eq!(with(3, 32, &0.25f32.to_le_bytes()).unwrap(), [0.25]);
        assert_eq!(with(3, 64, &(-0.5f64).to_le_bytes()).unwrap(), [-0.5]);
        assert!(with(2, 4, &[]).unwrap_err().contains("unsupported"));
    }
}
