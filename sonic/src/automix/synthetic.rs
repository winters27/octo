//! Made-up songs built from layers, rendered exactly as the shared automix
//! vectors describe them (see the file's `about`).
//!
//! tone:  amplitude * sin(2 pi hz n / rate), n the sample index in the song.
//! pulse: a hit at first_ms + j * every * 60000 / bpm for every whole j with
//!        the hit inside [from_ms, to_ms); each hit adds
//!        amplitude(hit time) * exp(-d / decay_ms) * sin(2 pi hz d / 1000)
//!        for d = sample time minus hit time in ms, 0 <= d < 8 * decay_ms.
//! The sample time of sample n is n * 1000 / rate ms. Every channel carries
//! the same signal.

use std::f64::consts::PI;

use super::envelope::{EnvelopeBuilder, SectionEnvelope};
use super::features::{SectionAnalysis, analyze_head, analyze_tail};

#[derive(Clone, Debug, PartialEq)]
pub struct SongLayer {
    pub kind: String,
    pub from_ms: f64,
    pub to_ms: f64,
    pub hz: f64,
    pub db: f64,
    pub end_db: f64,
    pub bpm: f64,
    pub first_ms: f64,
    pub every: i64,
    pub decay_ms: f64,
}

impl SongLayer {
    // The level moves in a straight line in dB from db to end_db.
    fn amplitude_at(&self, ms: f64) -> f64 {
        let u = if self.to_ms > self.from_ms {
            ((ms - self.from_ms) / (self.to_ms - self.from_ms)).clamp(0.0, 1.0)
        } else {
            0.0
        };
        10f64.powf((self.db + (self.end_db - self.db) * u) / 20.0)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SyntheticSong {
    pub rate: u32,
    pub channels: u32,
    pub length_ms: i64,
    pub layers: Vec<SongLayer>,
}

impl SyntheticSong {
    /// Mono samples for song times [from_ms, to_ms).
    pub fn render_mono(&self, from_ms: f64, to_ms: f64) -> Vec<f32> {
        let rate = self.rate as f64;
        let first = (from_ms * rate / 1000.0).floor() as i64;
        let end = (to_ms.min(self.length_ms as f64) * rate / 1000.0).floor() as i64;
        let mut out = vec![0.0f64; 0.max(end - first) as usize];
        for layer in &self.layers {
            match layer.kind.as_str() {
                "tone" => {
                    let from = first.max((layer.from_ms * rate / 1000.0).ceil() as i64);
                    let to = end.min((layer.to_ms * rate / 1000.0).ceil() as i64);
                    for n in from..to {
                        let ms = n as f64 * 1000.0 / rate;
                        out[(n - first) as usize] +=
                            layer.amplitude_at(ms) * (2.0 * PI * layer.hz * n as f64 / rate).sin();
                    }
                }
                "pulse" => {
                    let gap = layer.every as f64 * 60_000.0 / layer.bpm;
                    let mut j = ((layer.from_ms - layer.first_ms) / gap).ceil() as i64;
                    loop {
                        let hit = layer.first_ms + j as f64 * gap;
                        if hit >= layer.to_ms {
                            break;
                        }
                        j += 1;
                        if hit < layer.from_ms {
                            continue;
                        }
                        let tail = 8.0 * layer.decay_ms;
                        if hit + tail < from_ms || hit >= to_ms {
                            continue;
                        }
                        let amplitude = layer.amplitude_at(hit);
                        let from = first.max((hit * rate / 1000.0).ceil() as i64);
                        let to = end.min(((hit + tail) * rate / 1000.0).ceil() as i64);
                        for n in from..to {
                            let d = n as f64 * 1000.0 / rate - hit;
                            if !(0.0..tail).contains(&d) {
                                continue;
                            }
                            out[(n - first) as usize] += amplitude
                                * (-d / layer.decay_ms).exp()
                                * (2.0 * PI * layer.hz * d / 1000.0).sin();
                        }
                    }
                }
                other => panic!("unknown layer {other}"),
            }
        }
        out.into_iter().map(|x| x as f32).collect()
    }

    /// The envelope of [from_ms, to_ms), fed to the builder in blocks of
    /// `block` interleaved samples so the streaming path is exercised.
    pub fn envelope(&self, from_ms: f64, to_ms: f64, block: usize) -> SectionEnvelope {
        let mono = self.render_mono(from_ms, to_ms);
        let channels = self.channels as usize;
        let interleaved: Vec<f32> = mono.iter().flat_map(|&s| std::iter::repeat_n(s, channels)).collect();
        let start_ms = (from_ms * self.rate as f64 / 1000.0).floor() as i64 * 1000 / self.rate as i64;
        let mut builder = EnvelopeBuilder::new(self.rate, self.channels, start_ms);
        for chunk in interleaved.chunks(block) {
            builder.push(chunk);
        }
        builder.into_envelope()
    }

    /// The last 60 s, as the player scouts it.
    pub fn tail(&self) -> SectionAnalysis {
        let len = self.length_ms as f64;
        analyze_tail(self.envelope((len - 60_000.0).max(0.0), len, 4_093), None, None)
    }

    /// The first 45 s, as the player scouts it.
    pub fn head(&self) -> SectionAnalysis {
        let head_ms = super::HEAD_SECS * 1_000.0;
        analyze_head(self.envelope(0.0, head_ms.min(self.length_ms as f64), 4_093), None, None)
    }
}
