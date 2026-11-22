// OpenPedal: input -> reorderable chain of pedals (with sidechain keys) -> output.
// UI lives in ui/index.html, factory presets in presets.json.
// cpal's macOS backend is CoreAudio.
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::{traits::*, HeapRb};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::f32::consts::PI;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use tauri::Manager;

// ---------- building blocks ----------

fn db(x: f32) -> f32 {
    10f32.powf(x / 20.0)
}

/// One-pole smoothing coefficient for a time constant in ms.
fn coef(ms: f32, sr: f32) -> f32 {
    1.0 - (-1.0 / (ms.max(0.01) * 0.001 * sr)).exp()
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner()) // never let a poisoned lock kill the audio thread
}

#[derive(Clone, Copy)]
enum Bq {
    Lp,
    Hp,
    Peak(f32),      // dB
    LowShelf(f32),  // dB
    HighShelf(f32), // dB
}

/// RBJ cookbook biquad, transposed direct form II. `set` retunes without touching state (no clicks).
#[derive(Default)]
struct Biquad {
    b: [f32; 3],
    a: [f32; 2],
    z: [f32; 2],
}
impl Biquad {
    fn new(kind: Bq, f: f32, q: f32, sr: f32) -> Self {
        let mut b = Self::default();
        b.set(kind, f, q, sr);
        b
    }
    fn set(&mut self, kind: Bq, f: f32, q: f32, sr: f32) {
        let w = 2.0 * PI * f.min(sr * 0.45) / sr;
        let (c, s) = (w.cos(), w.sin());
        let al = s / (2.0 * q);
        let shelf = |db: f32| {
            let a = 10f32.powf(db / 40.0);
            (a, 2.0 * a.sqrt() * al)
        };
        let (b0, b1, b2, a0, a1, a2) = match kind {
            Bq::Lp => ((1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0, 1.0 + al, -2.0 * c, 1.0 - al),
            Bq::Hp => ((1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0, 1.0 + al, -2.0 * c, 1.0 - al),
            Bq::Peak(g) => {
                let a = 10f32.powf(g / 40.0);
                (1.0 + al * a, -2.0 * c, 1.0 - al * a, 1.0 + al / a, -2.0 * c, 1.0 - al / a)
            }
            Bq::LowShelf(g) => {
                let (a, k) = shelf(g);
                (
                    a * ((a + 1.0) - (a - 1.0) * c + k),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * c),
                    a * ((a + 1.0) - (a - 1.0) * c - k),
                    (a + 1.0) + (a - 1.0) * c + k,
                    -2.0 * ((a - 1.0) + (a + 1.0) * c),
                    (a + 1.0) + (a - 1.0) * c - k,
                )
            }
            Bq::HighShelf(g) => {
                let (a, k) = shelf(g);
                (
                    a * ((a + 1.0) + (a - 1.0) * c + k),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * c),
                    a * ((a + 1.0) + (a - 1.0) * c - k),
                    (a + 1.0) - (a - 1.0) * c + k,
                    2.0 * ((a - 1.0) - (a + 1.0) * c),
                    (a + 1.0) - (a - 1.0) * c - k,
                )
            }
        };
        self.b = [b0 / a0, b1 / a0, b2 / a0];
        self.a = [a1 / a0, a2 / a0];
    }
    fn run(&mut self, x: f32) -> f32 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

/// Peak envelope follower.
#[derive(Default)]
struct Env(f32);
impl Env {
    fn run(&mut self, x: f32, att: f32, rel: f32) -> f32 {
        let x = x.abs();
        self.0 += (x - self.0) * if x > self.0 { att } else { rel };
        self.0
    }
}

/// Circular delay line with fractional (linear-interpolated) reads.
struct Line {
    buf: Vec<f32>,
    w: usize,
}
impl Line {
    fn new(n: usize) -> Self {
        Line { buf: vec![0.0; n.max(4)], w: 0 }
    }
    fn push(&mut self, x: f32) {
        self.buf[self.w] = x;
        self.w = (self.w + 1) % self.buf.len();
    }
    /// Sample from `d` samples ago (d >= 1).
    fn read(&self, d: f32) -> f32 {
        let n = self.buf.len();
        let pos = (self.w as f32 - d.clamp(1.0, n as f32 - 2.0)).rem_euclid(n as f32);
        let i = pos as usize % n;
        let fr = pos - pos.floor();
        let (a, b) = (self.buf[i], self.buf[(i + 1) % n]);
        a + (b - a) * fr
    }
}

/// Sidechain sources available to every pedal, per sample.
struct Keys {
    dry: f32,  // guitar before the chain
    aux: f32,  // second input channel (e.g. a mic or drum machine)
    pump: f32, // tempo-synced kick-like envelope, 0..1
}

/// Pick a sidechain source: 0 = the pedal's own input, 1 = guitar, 2 = aux in, 3 = tempo.
fn key(sel: f32, x: f32, k: &Keys) -> f32 {
    match sel.round() as u8 {
        1 => k.dry,
        2 => k.aux,
        3 => k.pump,
        _ => x,
    }
}

// ---------- pedals ----------

trait Fx: Send {
    /// Params arrive in the order of the pedal's Spec, already clamped.
    fn set(&mut self, p: &[f32]);
    fn process(&mut self, x: f32, k: &Keys) -> f32;
    fn action(&mut self, _a: &str) {}
    fn status(&self) -> Option<String> {
        None
    }
}

struct Gate { sr: f32, th: f32, rel: f32, key: f32, env: Env, g: f32 }
impl Fx for Gate {
    fn set(&mut self, p: &[f32]) {
        (self.th, self.rel, self.key) = (db(p[0]), coef(p[1], self.sr), p[2]);
    }
    fn process(&mut self, x: f32, k: &Keys) -> f32 {
        let e = self.env.run(key(self.key, x, k), 1.0, self.rel);
        let target = if e > self.th { 1.0 } else { 0.0 };
        self.g += (target - self.g) * coef(2.0, self.sr);
        x * self.g
    }
}

struct Comp { sr: f32, th: f32, ratio: f32, att: f32, rel: f32, makeup: f32, key: f32, env: Env }
impl Fx for Comp {
    fn set(&mut self, p: &[f32]) {
        self.th = p[0];
        self.ratio = p[1];
        self.att = coef(p[2], self.sr);
        self.rel = coef(p[3], self.sr);
        self.makeup = p[4];
        self.key = p[5];
    }
    fn process(&mut self, x: f32, k: &Keys) -> f32 {
        let e = self.env.run(key(self.key, x, k), self.att, self.rel);
        let over = 20.0 * e.max(1e-6).log10() - self.th;
        let gr = if over > 0.0 { over * (1.0 - 1.0 / self.ratio) } else { 0.0 };
        x * db(self.makeup - gr)
    }
}

/// Mid-hump soft clipper: highpassed signal into tanh, a little clean low end blended back.
struct Overdrive { sr: f32, hp: Biquad, lp: Biquad, g: f32, level: f32 }
impl Fx for Overdrive {
    fn set(&mut self, p: &[f32]) {
        self.g = 1.0 + 59.0 * p[0];
        self.lp.set(Bq::Lp, 700.0 * 8.5f32.powf(p[1]), 0.7, self.sr);
        self.level = p[2] * 2.0;
    }
    fn process(&mut self, x: f32, _: &Keys) -> f32 {
        let c = (self.hp.run(x) * self.g).tanh();
        self.lp.run(c + 0.4 * x) * self.level
    }
}

/// High-gain amp: tight low end, mid push, two biased tanh stages, tone stack. The original pop-punk tone.
struct Dist { sr: f32, hp: Biquad, push: Biquad, lp: Biquad, low: Biquad, mid: Biquad, high: Biquad, g: f32, level: f32 }
impl Fx for Dist {
    fn set(&mut self, p: &[f32]) {
        let sr = self.sr;
        self.g = 25.0 * p[0];
        self.low.set(Bq::LowShelf(p[1]), 120.0, 0.7, sr);
        self.mid.set(Bq::Peak(p[2]), 450.0, 0.9, sr);
        self.high.set(Bq::Peak(p[3]), 2200.0, 1.0, sr);
        self.level = p[4] * 0.5;
    }
    fn process(&mut self, x: f32, _: &Keys) -> f32 {
        let s = self.push.run(self.hp.run(x));
        let s = (s * self.g + 0.15).tanh() - 0.15f32.tanh(); // bias = tube-ish even harmonics
        let s = self.lp.run(s);
        let s = (s * 4.0 + 0.05).tanh() - 0.05f32.tanh();
        self.high.run(self.mid.run(self.low.run(s))) * self.level
    }
}

/// Asymmetric hard clipper, very high gain.
struct Fuzz { sr: f32, g: f32, lp: Biquad, dc: Biquad, level: f32 }
impl Fx for Fuzz {
    fn set(&mut self, p: &[f32]) {
        self.g = 5.0 * 60f32.powf(p[0]);
        self.lp.set(Bq::Lp, 500.0 * 10f32.powf(p[1]), 0.7, self.sr);
        self.level = p[2] * 0.6;
    }
    fn process(&mut self, x: f32, _: &Keys) -> f32 {
        let s = (x * self.g + 0.2).clamp(-0.5, 1.0);
        self.lp.run(self.dc.run(s)) * self.level
    }
}

/// Closed-back 4x12-ish speaker: low cut, thump resonance, steep top rolloff.
struct Cab { sr: f32, hp: Biquad, thump: Biquad, lp1: Biquad, lp2: Biquad }
impl Fx for Cab {
    fn set(&mut self, p: &[f32]) {
        let f = 3500.0 * (7500.0f32 / 3500.0).powf(p[0]);
        self.thump.set(Bq::Peak(p[1]), 110.0, 1.2, self.sr);
        self.lp1.set(Bq::Lp, f, 0.7, self.sr);
        self.lp2.set(Bq::Lp, f * 1.1, 0.7, self.sr);
    }
    fn process(&mut self, x: f32, _: &Keys) -> f32 {
        self.lp2.run(self.lp1.run(self.thump.run(self.hp.run(x))))
    }
}

struct Eq { sr: f32, low: Biquad, mid: Biquad, high: Biquad, level: f32 }
impl Fx for Eq {
    fn set(&mut self, p: &[f32]) {
        self.low.set(Bq::LowShelf(p[0]), 100.0, 0.7, self.sr);
        self.mid.set(Bq::Peak(p[1]), p[2], 0.9, self.sr);
        self.high.set(Bq::HighShelf(p[3]), 3000.0, 0.7, self.sr);
        self.level = db(p[4]);
    }
    fn process(&mut self, x: f32, _: &Keys) -> f32 {
        self.high.run(self.mid.run(self.low.run(x))) * self.level
    }
}

struct Chorus { sr: f32, line: Line, ph: f32, rate: f32, depth: f32, mix: f32 }
impl Fx for Chorus {
    fn set(&mut self, p: &[f32]) {
        (self.rate, self.depth, self.mix) = (p[0], p[1], p[2]);
    }
    fn process(&mut self, x: f32, _: &Keys) -> f32 {
        self.line.push(x);
        self.ph = (self.ph + self.rate / self.sr).fract();
        let d = (0.007 + 0.006 * self.depth * (0.5 + 0.5 * (2.0 * PI * self.ph).sin())) * self.sr;
        x * (1.0 - self.mix) + self.line.read(d) * self.mix
    }
}

/// Four swept first-order allpasses with feedback.
struct Phaser { sr: f32, ph: f32, rate: f32, depth: f32, fb: f32, mix: f32, z: [f32; 4], last: f32 }
impl Fx for Phaser {
    fn set(&mut self, p: &[f32]) {
        (self.rate, self.depth, self.fb, self.mix) = (p[0], p[1], p[2], p[3]);
    }
    fn process(&mut self, x: f32, _: &Keys) -> f32 {
        self.ph = (self.ph + self.rate / self.sr).fract();
        let l = 0.5 + 0.5 * (2.0 * PI * self.ph).sin();
        let t = (PI * 200.0 * 10f32.powf(l * self.depth) / self.sr).tan();
        let a = (t - 1.0) / (t + 1.0);
        let mut s = x + self.last * self.fb;
        for z in &mut self.z {
            let y = a * s + *z;
            *z = s - a * y;
            s = y;
        }
        self.last = s;
        x * (1.0 - self.mix) + s * self.mix
    }
}

struct Tremolo { sr: f32, ph: f32, rate: f32, depth: f32, shape: f32 }
impl Fx for Tremolo {
    fn set(&mut self, p: &[f32]) {
        (self.rate, self.depth, self.shape) = (p[0], p[1], 0.01 + p[2] * 8.0);
    }
    fn process(&mut self, x: f32, _: &Keys) -> f32 {
        self.ph = (self.ph + self.rate / self.sr).fract();
        let l = ((2.0 * PI * self.ph).sin() * self.shape).tanh() / self.shape.tanh(); // sine -> soft square
        x * (1.0 - self.depth * (0.5 - 0.5 * l))
    }
}

/// Tape-ish delay: darkening repeats, glides when time changes, optional sidechain ducking of the echoes.
struct Delay { sr: f32, line: Line, time: f32, target: f32, fb: f32, mix: f32, lp: Biquad, duck: f32, key: f32, env: Env }
impl Fx for Delay {
    fn set(&mut self, p: &[f32]) {
        self.target = p[0] * 0.001 * self.sr;
        if self.time == 0.0 {
            self.time = self.target;
        }
        (self.fb, self.mix) = (p[1], p[2]);
        self.lp.set(Bq::Lp, 1000.0 * 12f32.powf(p[3]), 0.7, self.sr);
        (self.duck, self.key) = (p[4], p[5]);
    }
    fn process(&mut self, x: f32, k: &Keys) -> f32 {
        self.time += (self.target - self.time) * coef(60.0, self.sr);
        let wet = self.line.read(self.time);
        self.line.push(x + self.lp.run(wet) * self.fb);
        let e = self.env.run(key(self.key, x, k), coef(5.0, self.sr), coef(250.0, self.sr));
        x + wet * self.mix * (1.0 - self.duck * (e * 10.0).min(1.0))
    }
}

/// Freeverb-style: 8 damped combs into 4 allpasses.
struct Reverb { sr: f32, combs: Vec<(Vec<f32>, usize, f32)>, aps: Vec<(Vec<f32>, usize)>, fb: f32, damp: f32, mix: f32, duck: f32, key: f32, env: Env }
impl Reverb {
    fn new(sr: f32) -> Self {
        let n = |l: usize| vec![0.0; (l as f32 * sr / 44100.0) as usize + 1];
        Reverb {
            sr,
            combs: [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617].map(|l| (n(l), 0, 0.0)).into(),
            aps: [556, 441, 341, 225].map(|l| (n(l), 0)).into(),
            fb: 0.84, damp: 0.2, mix: 0.3, duck: 0.0, key: 1.0, env: Env::default(),
        }
    }
}
impl Fx for Reverb {
    fn set(&mut self, p: &[f32]) {
        (self.fb, self.damp, self.mix) = (0.7 + 0.28 * p[0], p[1] * 0.4, p[2]);
        (self.duck, self.key) = (p[3], p[4]);
    }
    fn process(&mut self, x: f32, k: &Keys) -> f32 {
        let inp = x * 0.015;
        let mut s = 0.0;
        for (buf, i, f) in &mut self.combs {
            let y = buf[*i];
            *f = y * (1.0 - self.damp) + *f * self.damp;
            buf[*i] = inp + *f * self.fb;
            *i = (*i + 1) % buf.len();
            s += y;
        }
        for (buf, i) in &mut self.aps {
            let b = buf[*i];
            buf[*i] = s + b * 0.5;
            *i = (*i + 1) % buf.len();
            s = b - s;
        }
        let e = self.env.run(key(self.key, x, k), coef(5.0, self.sr), coef(300.0, self.sr));
        x * (1.0 - self.mix * 0.5) + s * 3.0 * self.mix * (1.0 - self.duck * (e * 10.0).min(1.0))
    }
}

/// Sidechain ducker: turns the signal down whenever the key is loud. Key = Tempo gives the EDM pump.
struct Ducker { sr: f32, amount: f32, sens: f32, att: f32, rel: f32, key: f32, env: Env }
impl Fx for Ducker {
    fn set(&mut self, p: &[f32]) {
        self.amount = p[0];
        self.sens = db(p[1]);
        self.att = coef(p[2], self.sr);
        self.rel = coef(p[3], self.sr);
        self.key = p[4];
    }
    fn process(&mut self, x: f32, k: &Keys) -> f32 {
        let e = self.env.run(key(self.key, x, k), self.att, self.rel);
        x * (1.0 - self.amount * (e * self.sens).min(1.0))
    }
}

struct Boost(f32);
impl Fx for Boost {
    fn set(&mut self, p: &[f32]) {
        self.0 = db(p[0]);
    }
    fn process(&mut self, x: f32, _: &Keys) -> f32 {
        x * self.0
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Loop { Empty, Rec, Play, Dub, Stop }

/// 60 s looper. Footswitch: record -> play -> overdub -> play ...
struct Looper { sr: f32, buf: Vec<f32>, len: usize, pos: usize, st: Loop, level: f32 }
impl Fx for Looper {
    fn set(&mut self, p: &[f32]) {
        self.level = p[0];
    }
    fn process(&mut self, x: f32, _: &Keys) -> f32 {
        match self.st {
            Loop::Rec => {
                self.buf[self.pos] = x;
                self.pos += 1;
                if self.pos == self.buf.len() {
                    (self.len, self.pos, self.st) = (self.pos, 0, Loop::Play);
                }
                x
            }
            Loop::Play | Loop::Dub => {
                let y = self.buf[self.pos];
                if self.st == Loop::Dub {
                    self.buf[self.pos] = y + x;
                }
                self.pos = (self.pos + 1) % self.len;
                x + y * self.level
            }
            _ => x,
        }
    }
    fn action(&mut self, a: &str) {
        self.st = match (a, self.st) {
            ("foot", Loop::Empty) => {
                self.pos = 0;
                Loop::Rec
            }
            ("foot", Loop::Rec) | ("stop", Loop::Rec) => {
                (self.len, self.pos) = (self.pos.max(1), 0);
                if a == "stop" { Loop::Stop } else { Loop::Play }
            }
            ("foot", Loop::Play) => Loop::Dub,
            ("foot", Loop::Dub) => Loop::Play,
            ("foot", Loop::Stop) => {
                self.pos = 0;
                Loop::Play
            }
            ("stop", Loop::Play | Loop::Dub) => Loop::Stop,
            ("clear", _) => Loop::Empty,
            (_, s) => s,
        };
    }
    fn status(&self) -> Option<String> {
        let secs = if self.st == Loop::Rec { self.pos } else { self.len } as f32 / self.sr;
        let prog = if self.len > 0 && self.st != Loop::Rec { self.pos as f32 / self.len as f32 } else { 0.0 };
        Some(format!("{:?}|{secs:.1}|{prog:.3}", self.st))
    }
}

// ---------- catalog ----------

#[derive(Serialize)]
struct Param {
    name: &'static str,
    min: f32,
    max: f32,
    def: f32,
    unit: &'static str,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    choices: &'static [&'static str],
}
const fn p(name: &'static str, min: f32, max: f32, def: f32, unit: &'static str) -> Param {
    Param { name, min, max, def, unit, choices: &[] }
}
const fn key_p(def: f32) -> Param {
    Param { name: "key", min: 0.0, max: 3.0, def, unit: "", choices: &["Self", "Guitar", "Aux in", "Tempo"] }
}

#[derive(Serialize)]
struct Spec {
    kind: &'static str,
    name: &'static str,
    blurb: &'static str,
    color: &'static str,
    params: &'static [Param],
}

const SPECS: &[Spec] = &[
    Spec { kind: "gate", name: "Noise Gate", blurb: "silence between chugs", color: "#3a3f47",
        params: &[p("thresh", -80.0, -20.0, -55.0, "dB"), p("release", 10.0, 500.0, 80.0, "ms"), key_p(0.0)] },
    Spec { kind: "comp", name: "Compressor", blurb: "squash & sustain", color: "#2f6fdb",
        params: &[p("thresh", -50.0, 0.0, -20.0, "dB"), p("ratio", 1.0, 20.0, 4.0, ":1"), p("attack", 0.1, 50.0, 5.0, "ms"),
                  p("release", 10.0, 1000.0, 120.0, "ms"), p("makeup", 0.0, 24.0, 6.0, "dB"), key_p(0.0)] },
    Spec { kind: "overdrive", name: "Overdrive", blurb: "mid-hump grit", color: "#2fa84f",
        params: &[p("drive", 0.0, 1.0, 0.5, ""), p("tone", 0.0, 1.0, 0.5, ""), p("level", 0.0, 1.0, 0.5, "")] },
    Spec { kind: "distortion", name: "Hi-Gain", blurb: "pop-punk wall", color: "#ff2e88",
        params: &[p("gain", 0.1, 4.0, 1.0, "×"), p("bass", -12.0, 12.0, 4.0, "dB"), p("mid", -12.0, 12.0, -5.0, "dB"),
                  p("treble", -12.0, 12.0, 4.0, "dB"), p("level", 0.0, 1.0, 0.5, "")] },
    Spec { kind: "fuzz", name: "Fuzz", blurb: "sputtery & huge", color: "#d94a1e",
        params: &[p("fuzz", 0.0, 1.0, 0.7, ""), p("tone", 0.0, 1.0, 0.5, ""), p("level", 0.0, 1.0, 0.5, "")] },
    Spec { kind: "cab", name: "Cab Sim", blurb: "4x12 in a box", color: "#4b3a2f",
        params: &[p("bright", 0.0, 1.0, 0.5, ""), p("thump", 0.0, 9.0, 3.0, "dB")] },
    Spec { kind: "eq", name: "EQ", blurb: "shape it", color: "#d9d4c7",
        params: &[p("low", -12.0, 12.0, 0.0, "dB"), p("mid", -12.0, 12.0, 0.0, "dB"), p("freq", 200.0, 3000.0, 800.0, "Hz"),
                  p("high", -12.0, 12.0, 0.0, "dB"), p("level", -12.0, 12.0, 0.0, "dB")] },
    Spec { kind: "chorus", name: "Chorus", blurb: "shimmer & width", color: "#5ec8e5",
        params: &[p("rate", 0.1, 5.0, 0.8, "Hz"), p("depth", 0.0, 1.0, 0.5, ""), p("mix", 0.0, 1.0, 0.5, "")] },
    Spec { kind: "phaser", name: "Phaser", blurb: "the swoosh", color: "#ff8a1f",
        params: &[p("rate", 0.05, 5.0, 0.4, "Hz"), p("depth", 0.0, 1.0, 0.8, ""), p("feedback", 0.0, 0.9, 0.5, ""), p("mix", 0.0, 1.0, 0.5, "")] },
    Spec { kind: "tremolo", name: "Tremolo", blurb: "amp-style throb", color: "#8a5cf6",
        params: &[p("rate", 0.5, 15.0, 5.0, "Hz"), p("depth", 0.0, 1.0, 0.6, ""), p("shape", 0.0, 1.0, 0.0, "")] },
    Spec { kind: "delay", name: "Delay", blurb: "tape echoes", color: "#f2c230",
        params: &[p("time", 20.0, 1500.0, 380.0, "ms"), p("repeats", 0.0, 0.95, 0.35, ""), p("mix", 0.0, 1.0, 0.35, ""),
                  p("tone", 0.0, 1.0, 0.5, ""), p("duck", 0.0, 1.0, 0.0, ""), key_p(1.0)] },
    Spec { kind: "reverb", name: "Reverb", blurb: "room to cathedral", color: "#1d3a6e",
        params: &[p("size", 0.0, 1.0, 0.5, ""), p("damp", 0.0, 1.0, 0.4, ""), p("mix", 0.0, 1.0, 0.3, ""),
                  p("duck", 0.0, 1.0, 0.0, ""), key_p(1.0)] },
    Spec { kind: "ducker", name: "Sidechain", blurb: "duck to a key", color: "#16161a",
        params: &[p("amount", 0.0, 1.0, 0.8, ""), p("sens", 0.0, 40.0, 12.0, "dB"), p("attack", 0.5, 50.0, 2.0, "ms"),
                  p("release", 20.0, 800.0, 180.0, "ms"), key_p(3.0)] },
    Spec { kind: "boost", name: "Boost", blurb: "clean push", color: "#c0c4cc", params: &[p("gain", 0.0, 20.0, 6.0, "dB")] },
    Spec { kind: "looper", name: "Looper", blurb: "60 s, overdub", color: "#e23b3b", params: &[p("level", 0.0, 1.5, 1.0, "")] },
];

fn spec(kind: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|s| s.kind == kind)
}

fn make(kind: &str, sr: f32) -> Option<Box<dyn Fx>> {
    let bq = |k, f| Biquad::new(k, f, 0.7, sr);
    Some(match kind {
        "gate" => Box::new(Gate { sr, th: 0.0, rel: 0.0, key: 0.0, env: Env::default(), g: 0.0 }),
        "comp" => Box::new(Comp { sr, th: 0.0, ratio: 1.0, att: 1.0, rel: 1.0, makeup: 0.0, key: 0.0, env: Env::default() }),
        "overdrive" => Box::new(Overdrive { sr, hp: bq(Bq::Hp, 600.0), lp: Biquad::default(), g: 1.0, level: 1.0 }),
        "distortion" => Box::new(Dist {
            sr, hp: bq(Bq::Hp, 130.0), push: Biquad::new(Bq::Peak(7.0), 850.0, 0.8, sr), lp: bq(Bq::Lp, 7000.0),
            low: Biquad::default(), mid: Biquad::default(), high: Biquad::default(), g: 25.0, level: 0.25,
        }),
        "fuzz" => Box::new(Fuzz { sr, g: 1.0, lp: Biquad::default(), dc: bq(Bq::Hp, 20.0), level: 0.5 }),
        "cab" => Box::new(Cab { sr, hp: bq(Bq::Hp, 75.0), thump: Biquad::default(), lp1: Biquad::default(), lp2: Biquad::default() }),
        "eq" => Box::new(Eq { sr, low: Biquad::default(), mid: Biquad::default(), high: Biquad::default(), level: 1.0 }),
        "chorus" => Box::new(Chorus { sr, line: Line::new((0.05 * sr) as usize), ph: 0.0, rate: 1.0, depth: 0.5, mix: 0.5 }),
        "phaser" => Box::new(Phaser { sr, ph: 0.0, rate: 0.5, depth: 0.8, fb: 0.5, mix: 0.5, z: [0.0; 4], last: 0.0 }),
        "tremolo" => Box::new(Tremolo { sr, ph: 0.0, rate: 5.0, depth: 0.5, shape: 0.01 }),
        "delay" => Box::new(Delay {
            sr, line: Line::new((1.6 * sr) as usize), time: 0.0, target: 0.0, fb: 0.0, mix: 0.0,
            lp: Biquad::default(), duck: 0.0, key: 1.0, env: Env::default(),
        }),
        "reverb" => Box::new(Reverb::new(sr)),
        "ducker" => Box::new(Ducker { sr, amount: 0.0, sens: 1.0, att: 1.0, rel: 1.0, key: 3.0, env: Env::default() }),
        "boost" => Box::new(Boost(1.0)),
        // ponytail: 60 s preallocated (~11 MB @48k). Make it a param if you want longer loops.
        "looper" => Box::new(Looper { sr, buf: vec![0.0; (60.0 * sr) as usize], len: 0, pos: 0, st: Loop::Empty, level: 1.0 }),
        _ => return None,
    })
}

// ---------- board + engine ----------

fn yes() -> bool {
    true
}
fn bpm_default() -> f32 {
    120.0
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Pedal {
    #[serde(default)]
    id: u64,
    kind: String,
    #[serde(default = "yes")]
    on: bool,
    #[serde(default)]
    params: HashMap<String, f32>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct Board {
    pedals: Vec<Pedal>,
    #[serde(default = "bpm_default")]
    bpm: f32,
    #[serde(default)]
    mute: bool,
    #[serde(default)]
    aux_ch: usize,
}

#[derive(Serialize, Deserialize, Clone)]
struct Preset {
    name: String,
    #[serde(default)]
    user: bool,
    #[serde(default = "bpm_default")]
    bpm: f32,
    pedals: Vec<Pedal>,
}

fn factory_presets() -> Vec<Preset> {
    serde_json::from_str(include_str!("../presets.json")).expect("presets.json is invalid")
}

struct Slot {
    id: u64,
    on: bool,
    fx: Box<dyn Fx>,
}

struct Engine {
    sr: f32,
    chain: Vec<Slot>,
    bpm: f32,
    mute: bool,
    aux_ch: usize,
    beat: f32, // 0..1 phase within the current beat
}

impl Engine {
    /// Sync the chain to the board, reusing live pedals by id so delay tails and loops survive edits.
    /// Returns removed pedals so the caller can drop them outside the lock.
    fn apply(&mut self, b: &Board) -> Vec<Slot> {
        let mut old: HashMap<u64, Slot> = std::mem::take(&mut self.chain).into_iter().map(|s| (s.id, s)).collect();
        for p in &b.pedals {
            let Some(spec) = spec(&p.kind) else { continue };
            // ponytail: new pedals are allocated while the engine lock is held (a few ms for the looper).
            // Only happens when you add a pedal; pre-build outside the lock if that ever glitches.
            let Some(mut fx) = old.remove(&p.id).map(|s| s.fx).or_else(|| make(&p.kind, self.sr)) else { continue };
            let vals: Vec<f32> = spec.params.iter().map(|q| p.params.get(q.name).copied().unwrap_or(q.def).clamp(q.min, q.max)).collect();
            fx.set(&vals);
            self.chain.push(Slot { id: p.id, on: p.on, fx });
        }
        (self.bpm, self.mute, self.aux_ch) = (b.bpm.clamp(40.0, 300.0), b.mute, b.aux_ch);
        old.into_values().collect()
    }

    fn process(&mut self, x: f32, aux: f32) -> f32 {
        self.beat = (self.beat + self.bpm / 60.0 / self.sr).fract();
        let k = Keys { dry: x, aux, pump: (-self.beat * 8.0).exp() };
        let mut s = x;
        for slot in &mut self.chain {
            if slot.on {
                s = slot.fx.process(s, &k);
            }
        }
        if self.mute || !s.is_finite() { 0.0 } else { s.clamp(-1.0, 1.0) } // clamp: protect ears
    }
}

// ---------- audio I/O ----------

struct Shared {
    engine: Mutex<Engine>,
    board: Mutex<Board>,
    peak: AtomicU32, // input peak as f32 bits; positive f32s order like u32s, so fetch_max works
    tuner: Mutex<Tuner>,
}

struct Tuner {
    rx: Option<ringbuf::HeapCons<f32>>,
    win: VecDeque<f32>,
    sr: f32,
}

const TUNER_WIN: usize = 4096;

fn dev_name(d: &cpal::Device) -> String {
    d.description().map(|x| x.name().to_string()).unwrap_or_default()
}

/// Named input device (channel `ch`, 0-based, clamped) -> engine -> named output device.
fn start(in_name: &str, ch: usize, out_name: &str, buf: u32, shared: Arc<Shared>) -> Result<(cpal::Stream, cpal::Stream, String), Box<dyn std::error::Error>> {
    let host = cpal::default_host();
    let in_dev = host.input_devices()?.find(|d| dev_name(d) == in_name).ok_or("input device gone")?;
    let out_dev = host.output_devices()?.find(|d| dev_name(d) == out_name).ok_or("output device gone")?;

    // Both ends must share a sample rate (no resampler): run the input at the output's rate.
    let out_cfg = out_dev.default_output_config()?;
    let rate = out_cfg.sample_rate();
    let in_cfg = in_dev
        .supported_input_configs()?
        .find_map(|c| c.try_with_sample_rate(rate))
        .ok_or_else(|| format!("{in_name} can't run at {rate} Hz"))?;
    let sr = rate as f32;
    let (in_ch, out_ch) = (in_cfg.channels() as usize, out_cfg.channels() as usize);
    // Small fixed hardware buffers: the default (~512 frames) is ~10ms per side.
    let small = |c: cpal::SupportedStreamConfig| {
        let range = *c.buffer_size();
        let mut sc = c.config();
        if let cpal::SupportedBufferSize::Range { min, max } = range {
            sc.buffer_size = cpal::BufferSize::Fixed(buf.clamp(min, max));
        }
        sc
    };
    let (in_cfg, out_cfg) = (small(in_cfg), small(out_cfg));
    let target = 2 * buf as usize; // ring fill we aim for: enough to absorb callback jitter, no more

    {
        // fresh pedal instances at the new sample rate
        let board = lock(&shared.board).clone();
        let mut eng = lock(&shared.engine);
        eng.sr = sr;
        eng.chain.clear();
        eng.apply(&board);
    }

    let (mut tx, mut rx) = HeapRb::<f32>::new(8192).split();
    for _ in 0..target {
        tx.try_push(0.0).ok();
    }
    // tuner tap: raw input, decimated to <=48k so pitch detection stays cheap at 192k.
    // ponytail: decimation without anti-alias filter; fine for pitch, add a lowpass if the tuner jitters at 96k+.
    let decim = (rate / 48000).max(1) as usize;
    let (mut ttx, trx) = HeapRb::<f32>::new(4 * TUNER_WIN).split();
    *lock(&shared.tuner) = Tuner { rx: Some(trx), win: VecDeque::new(), sr: sr / decim as f32 };
    let mut tick = 0usize;

    let err = |e| eprintln!("stream error: {e}");
    let input = in_dev.build_input_stream(
        in_cfg,
        move |data: &[f32], _: &_| {
            // ponytail: the audio thread takes a mutex; the UI only holds it for quick swaps.
            // Move to a lock-free command queue if you ever hear dropouts while editing.
            let mut eng = lock(&shared.engine);
            let (xi, ai) = (ch.min(in_ch - 1), eng.aux_ch.min(in_ch - 1));
            let mut peak = 0f32;
            for frame in data.chunks(in_ch) {
                let x = frame[xi];
                peak = peak.max(x.abs());
                tick += 1;
                if tick % decim == 0 {
                    let _ = ttx.try_push(x);
                }
                // ponytail: drift control by dropping a sample when the ring runs long (two devices = two clocks).
                // Inaudible-ish; swap for adaptive resampling if you hear ticks.
                if tx.occupied_len() > 2 * target {
                    continue;
                }
                let _ = tx.try_push(eng.process(x, frame[ai]));
            }
            shared.peak.fetch_max(peak.to_bits(), Relaxed);
        },
        err,
        None,
    )?;
    let mut last = 0.0;
    let output = out_dev.build_output_stream(
        out_cfg,
        move |data: &mut [f32], _: &_| {
            for frame in data.chunks_mut(out_ch) {
                last = rx.try_pop().unwrap_or(last); // underrun: hold last sample, softer than a 0 click
                frame.fill(last);
            }
        },
        err,
        None,
    )?;
    input.play()?;
    output.play()?;
    Ok((input, output, format!("{rate} Hz · ~{:.1} ms", (3 * buf) as f32 / sr * 1000.0)))
}

// ---------- tuner ----------

/// YIN pitch detection. Returns Hz, or None if quiet/unpitched. Range ~60–1400 Hz.
fn detect_pitch(x: &[f32], sr: f32) -> Option<f32> {
    let max_lag = (sr / 60.0) as usize;
    let min_lag = ((sr / 1400.0) as usize).max(2);
    let w = x.len().checked_sub(max_lag + 1)?;
    let rms = (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt();
    if w < max_lag || rms < 0.003 {
        return None;
    }
    let mut cmnd = vec![1f32; max_lag + 1];
    let mut run = 0f32;
    for tau in 1..=max_lag {
        let d: f32 = (0..w).map(|i| (x[i] - x[i + tau]).powi(2)).sum();
        run += d;
        cmnd[tau] = d * tau as f32 / run.max(1e-12);
    }
    let mut tau = (min_lag..max_lag).find(|&t| cmnd[t] < 0.15)?;
    while tau + 1 < max_lag && cmnd[tau + 1] < cmnd[tau] {
        tau += 1;
    }
    let (a, b, c) = (cmnd[tau - 1], cmnd[tau], cmnd[tau + 1]);
    let shift = 0.5 * (a - c) / (a - 2.0 * b + c);
    Some(sr / (tau as f32 + if shift.is_finite() { shift.clamp(-1.0, 1.0) } else { 0.0 }))
}

/// Hz -> ("E2", cents off, -50..50)
fn note_of(f: f32) -> (String, f32) {
    const N: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    let m = 69.0 + 12.0 * (f / 440.0).log2();
    let r = m.round() as i32;
    (format!("{}{}", N[r.rem_euclid(12) as usize], r.div_euclid(12) - 1), (m - r as f32) * 100.0)
}

// ---------- Tauri commands ----------

type Ctl = mpsc::Sender<(String, usize, String, u32, mpsc::Sender<Result<String, String>>)>;

struct AppState {
    shared: Arc<Shared>,
    ctl: Ctl,
}

/// (input names, default input, output names, default output). Defaults prefer an audio interface / headphones.
#[tauri::command]
fn devices() -> (Vec<String>, String, Vec<String>, String) {
    let host = cpal::default_host();
    let ins: Vec<String> = host.input_devices().map(|ds| ds.map(|d| dev_name(&d)).collect()).unwrap_or_default();
    let outs: Vec<String> = host.output_devices().map(|ds| ds.map(|d| dev_name(&d)).collect()).unwrap_or_default();
    let pick = |v: &[String], pats: &[&str]| v.iter().find(|n| pats.iter().any(|p| n.to_lowercase().contains(p))).cloned();
    let sys_in = host.default_input_device().map(|d| dev_name(&d));
    let din = pick(&ins, &["scarlett", "focusrite", "irig", "interface", "usb audio"]).or(sys_in).or(ins.first().cloned()).unwrap_or_default();
    let dout = pick(&outs, &["earpods", "headphone", "airpods"]).or(outs.first().cloned()).unwrap_or_default();
    (ins, din, outs, dout)
}

#[tauri::command]
fn connect(input: String, ch: usize, out: String, buf: u32, st: tauri::State<AppState>) -> Result<String, String> {
    let (reply, rx) = mpsc::channel();
    st.ctl.send((input, ch, out, buf, reply)).map_err(|e| e.to_string())?;
    rx.recv().map_err(|e| e.to_string())?
}

#[tauri::command]
fn catalog() -> &'static [Spec] {
    SPECS
}

#[tauri::command]
fn get_board(st: tauri::State<AppState>) -> Board {
    lock(&st.shared.board).clone()
}

#[tauri::command]
fn set_board(board: Board, st: tauri::State<AppState>) {
    let garbage = lock(&st.shared.engine).apply(&board);
    drop(garbage); // freed here, outside the engine lock
    *lock(&st.shared.board) = board;
}

#[tauri::command]
fn pedal_action(id: u64, action: String, st: tauri::State<AppState>) {
    if let Some(s) = lock(&st.shared.engine).chain.iter_mut().find(|s| s.id == id) {
        s.fx.action(&action);
    }
}

fn user_presets_path(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("presets.json"))
}

fn user_presets(app: &tauri::AppHandle) -> Vec<Preset> {
    user_presets_path(app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

#[tauri::command]
fn presets(app: tauri::AppHandle) -> Vec<Preset> {
    let mut v = factory_presets();
    v.extend(user_presets(&app).into_iter().map(|p| Preset { user: true, ..p }));
    v
}

fn write_user_presets(app: &tauri::AppHandle, v: &[Preset]) -> Result<(), String> {
    let json = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    std::fs::write(user_presets_path(app)?, json).map_err(|e| e.to_string())
}

#[tauri::command]
fn save_preset(preset: Preset, app: tauri::AppHandle) -> Result<(), String> {
    let mut v = user_presets(&app);
    v.retain(|p| p.name != preset.name);
    v.push(preset);
    write_user_presets(&app, &v)
}

#[tauri::command]
fn delete_preset(name: String, app: tauri::AppHandle) -> Result<(), String> {
    let mut v = user_presets(&app);
    v.retain(|p| p.name != name);
    write_user_presets(&app, &v)
}

#[derive(Serialize)]
struct Poll {
    peak: f32,
    freq: Option<f32>,
    note: Option<String>,
    cents: f32,
    loops: Vec<(u64, String)>,
}

#[tauri::command]
fn poll(st: tauri::State<AppState>) -> Poll {
    let peak = f32::from_bits(st.shared.peak.swap(0, Relaxed));
    let loops = lock(&st.shared.engine).chain.iter().filter_map(|s| Some((s.id, s.fx.status()?))).collect();
    let mut t = lock(&st.shared.tuner);
    let Tuner { rx, win, sr } = &mut *t;
    if let Some(rx) = rx {
        win.extend(rx.pop_iter());
    }
    while win.len() > TUNER_WIN {
        win.pop_front();
    }
    let freq = detect_pitch(win.make_contiguous(), *sr);
    let (note, cents) = freq.map(note_of).map_or((None, 0.0), |(n, c)| (Some(n), c));
    Poll { peak, freq, note, cents, loops }
}

fn initial_board() -> Board {
    let p = factory_presets().into_iter().next().expect("no factory presets");
    let pedals = p.pedals.into_iter().enumerate().map(|(i, x)| Pedal { id: i as u64 + 1, ..x }).collect();
    Board { pedals, bpm: p.bpm, mute: false, aux_ch: 0 }
}

fn main() {
    let shared = Arc::new(Shared {
        engine: Mutex::new(Engine { sr: 48000.0, chain: Vec::new(), bpm: 120.0, mute: false, aux_ch: 0, beat: 0.0 }),
        board: Mutex::new(initial_board()),
        peak: AtomicU32::new(0),
        tuner: Mutex::new(Tuner { rx: None, win: VecDeque::new(), sr: 48000.0 }),
    });
    // cpal streams aren't Send everywhere, so one thread owns them and takes (re)connect requests.
    let (ctl, ctl_rx): (Ctl, _) = mpsc::channel();
    let sh = shared.clone();
    std::thread::spawn(move || {
        let mut _streams = None;
        for (input, ch, out, buf, reply) in ctl_rx {
            _streams = None; // stop old streams before opening the device again
            let r = start(&input, ch, &out, buf, sh.clone()).map(|(i, o, s)| {
                _streams = Some((i, o));
                s
            });
            let _ = reply.send(r.map_err(|e| e.to_string()));
        }
    });
    tauri::Builder::default()
        .manage(AppState { shared, ctl })
        .invoke_handler(tauri::generate_handler![
            devices, connect, catalog, get_board, set_board, pedal_action, presets, save_preset, delete_preset, poll
        ])
        .run(tauri::generate_context!())
        .expect("tauri failed to start");
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48000.0;
    fn sine(i: usize, f: f32, a: f32) -> f32 {
        a * (2.0 * PI * f * i as f32 / SR).sin()
    }
    fn board(kinds: &[(u64, &str)]) -> Board {
        let pedals = kinds.iter().map(|&(id, k)| Pedal { id, kind: k.into(), on: true, params: HashMap::new() }).collect();
        Board { pedals, bpm: 120.0, mute: false, aux_ch: 0 }
    }

    #[test]
    fn biquads() {
        let settle = |mut f: Biquad| (0..5000).map(|_| f.run(1.0)).last().unwrap();
        assert!((settle(Biquad::new(Bq::Lp, 1000.0, 0.7, SR)) - 1.0).abs() < 1e-3); // LP passes DC
        assert!(settle(Biquad::new(Bq::Hp, 1000.0, 0.7, SR)).abs() < 1e-3); // HP kills DC
        assert!((settle(Biquad::new(Bq::LowShelf(6.0), 200.0, 0.7, SR)) - db(6.0)).abs() < 1e-2); // shelf gain at DC
        assert!((settle(Biquad::new(Bq::HighShelf(6.0), 3000.0, 0.7, SR)) - 1.0).abs() < 1e-2); // high shelf leaves DC
    }

    #[test]
    fn every_pedal_is_stable_at_defaults() {
        for s in SPECS {
            let mut e = Engine { sr: SR, chain: vec![], bpm: 120.0, mute: false, aux_ch: 0, beat: 0.0 };
            e.apply(&board(&[(1, s.kind)]));
            assert_eq!(e.chain.len(), 1, "{} not built", s.kind);
            let out: Vec<f32> = (0..SR as usize).map(|i| e.process(sine(i, 110.0, 0.5), 0.0)).collect();
            let peak = out[24000..].iter().fold(0f32, |m, x| m.max(x.abs()));
            assert!(peak.is_finite() && peak > 1e-3, "{}: peak {peak}", s.kind);
        }
    }

    #[test]
    fn factory_presets_are_valid() {
        let ps = factory_presets();
        assert!(ps.len() >= 8);
        for p in &ps {
            for pd in &p.pedals {
                let s = spec(&pd.kind).unwrap_or_else(|| panic!("{}: unknown pedal {}", p.name, pd.kind));
                for name in pd.params.keys() {
                    assert!(s.params.iter().any(|q| q.name == name), "{}: {} has no param {name}", p.name, pd.kind);
                }
            }
            // and they play: loud input stays bounded and audible
            let mut b = board(&[]);
            b.pedals = p.pedals.iter().enumerate().map(|(i, x)| Pedal { id: i as u64 + 1, ..x.clone() }).collect();
            let mut e = Engine { sr: SR, chain: vec![], bpm: p.bpm, mute: false, aux_ch: 0, beat: 0.0 };
            e.apply(&b);
            let peak = (0..SR as usize).map(|i| e.process(sine(i, 110.0, 0.3), 0.0).abs()).skip(24000).fold(0f32, f32::max);
            assert!(peak > 0.02 && peak <= 1.0, "{}: peak {peak}", p.name);
        }
    }

    #[test]
    fn reorder_keeps_pedal_state() {
        let mut e = Engine { sr: SR, chain: vec![], bpm: 120.0, mute: false, aux_ch: 0, beat: 0.0 };
        e.apply(&board(&[(1, "delay"), (2, "boost")]));
        for i in 0..4800 {
            e.process(sine(i, 220.0, 0.5), 0.0); // fill the delay line
        }
        let gone = e.apply(&board(&[(2, "boost"), (1, "delay")]));
        assert!(gone.is_empty());
        assert_eq!(e.chain.iter().map(|s| s.id).collect::<Vec<_>>(), vec![2, 1]);
        // echoes still ring out after the reorder, with silent input
        let tail = (0..SR as usize / 2).map(|_| e.process(0.0, 0.0).abs()).fold(0f32, f32::max);
        assert!(tail > 0.01, "delay tail lost: {tail}");
        assert_eq!(e.apply(&board(&[(2, "boost")])).len(), 1); // removed pedal handed back
    }

    #[test]
    fn sidechain_ducks_on_key() {
        let mut d = make("ducker", SR).unwrap();
        d.set(&[1.0, 20.0, 1.0, 100.0, 2.0]); // full duck, keyed from aux
        let run = |d: &mut Box<dyn Fx>, aux: f32| (0..4800).map(|_| d.process(0.5, &Keys { dry: 0.5, aux, pump: 0.0 })).last().unwrap();
        assert!((run(&mut d, 0.0) - 0.5).abs() < 1e-3, "no key: untouched");
        assert!(run(&mut d, 0.5).abs() < 0.05, "loud key: ducked");
    }

    #[test]
    fn looper_records_and_plays() {
        let mut l = make("looper", SR).unwrap();
        l.set(&[1.0]);
        let k = Keys { dry: 0.0, aux: 0.0, pump: 0.0 };
        l.action("foot"); // rec
        for i in 0..100 {
            l.process(i as f32 / 100.0, &k);
        }
        l.action("foot"); // play
        let back: Vec<f32> = (0..100).map(|_| l.process(0.0, &k)).collect();
        assert!((back[50] - 0.5).abs() < 1e-6 && back.len() == 100);
        assert!(l.status().unwrap().starts_with("Play"));
        l.action("clear");
        assert_eq!(l.process(0.0, &k), 0.0);
    }

    #[test]
    fn tuner_finds_fundamental() {
        // harmonic-rich "string": must lock onto the fundamental, not an overtone
        let tone = |f: f32| -> Vec<f32> {
            (0..TUNER_WIN).map(|i| (1..6).map(|h| sine(i, f * h as f32, 0.2 / h as f32)).sum()).collect()
        };
        for (f, name) in [(82.41, "E2"), (110.0, "A2"), (196.0, "G3"), (329.63, "E4")] {
            let got = detect_pitch(&tone(f), SR).expect("no pitch");
            let (n, cents) = note_of(got);
            assert!((got - f).abs() < 0.5 && n == name && cents.abs() < 5.0, "{f}: got {got} {n} {cents}");
        }
        assert_eq!(detect_pitch(&vec![0.0; TUNER_WIN], SR), None); // silence
        assert!(note_of(440.0 * 2f32.powf(0.2 / 12.0)).1 > 15.0); // 20 cents sharp reads sharp
    }
}
