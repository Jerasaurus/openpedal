// Guitar pedal: any input -> effect chain -> any output. Tauri UI lives in ui/index.html.
// cpal's macOS backend is CoreAudio. ponytail: cpal over raw coreaudio-rs, same API underneath, 1/5 the code.
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::{traits::*, HeapRb};
use std::f32::consts::PI;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::collections::VecDeque;
use std::sync::{mpsc, Arc, Mutex};

trait Effect: Send {
    fn process(&mut self, x: f32) -> f32;
    /// Internal memory, copied across when a knob change rebuilds the chain (no clicks).
    fn state(&mut self) -> &mut [f32] {
        &mut []
    }
}

// ---------- effects ----------

struct Gain(f32);
impl Effect for Gain {
    fn process(&mut self, x: f32) -> f32 {
        x * self.0
    }
}

/// tanh waveshaper with a small bias for tube-ish even harmonics.
struct Drive {
    gain: f32,
    bias: f32,
}
impl Effect for Drive {
    fn process(&mut self, x: f32) -> f32 {
        (x * self.gain + self.bias).tanh() - self.bias.tanh()
    }
}

/// Noise gate: kills hum/hiss between palm mutes. Essential at high gain.
struct Gate {
    thresh: f32,
    st: [f32; 2], // envelope, gain
    release: f32,
    smooth: f32,
}
impl Gate {
    fn new(thresh_db: f32, sr: f32) -> Self {
        Gate {
            thresh: 10f32.powf(thresh_db / 20.0),
            st: [0.0; 2],
            release: (-1.0 / (0.08 * sr)).exp(),
            smooth: 1.0 - (-1.0 / (0.003 * sr)).exp(),
        }
    }
}
impl Effect for Gate {
    fn process(&mut self, x: f32) -> f32 {
        let [env, gain] = &mut self.st;
        *env = x.abs().max(*env * self.release);
        let target = if *env > self.thresh { 1.0 } else { 0.0 };
        *gain += (target - *gain) * self.smooth;
        x * *gain
    }
    fn state(&mut self) -> &mut [f32] {
        &mut self.st
    }
}

/// RBJ cookbook biquad, transposed direct form II.
enum Kind {
    Lp,
    Hp,
    Peak(f32),     // dB
    LowShelf(f32), // dB
}
struct Biquad {
    b: [f32; 3],
    a: [f32; 2],
    z: [f32; 2],
}
impl Biquad {
    fn new(kind: Kind, f: f32, q: f32, sr: f32) -> Self {
        let w = 2.0 * PI * f / sr;
        let (c, s) = (w.cos(), w.sin());
        let al = s / (2.0 * q);
        let (b0, b1, b2, a0, a1, a2) = match kind {
            Kind::Lp => ((1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0, 1.0 + al, -2.0 * c, 1.0 - al),
            Kind::Hp => ((1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0, 1.0 + al, -2.0 * c, 1.0 - al),
            Kind::Peak(db) => {
                let a = 10f32.powf(db / 40.0);
                (1.0 + al * a, -2.0 * c, 1.0 - al * a, 1.0 + al / a, -2.0 * c, 1.0 - al / a)
            }
            Kind::LowShelf(db) => {
                let a = 10f32.powf(db / 40.0);
                let k = 2.0 * a.sqrt() * al;
                (
                    a * ((a + 1.0) - (a - 1.0) * c + k),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * c),
                    a * ((a + 1.0) - (a - 1.0) * c - k),
                    (a + 1.0) + (a - 1.0) * c + k,
                    -2.0 * ((a - 1.0) + (a + 1.0) * c),
                    (a + 1.0) + (a - 1.0) * c - k,
                )
            }
        };
        Biquad { b: [b0 / a0, b1 / a0, b2 / a0], a: [a1 / a0, a2 / a0], z: [0.0; 2] }
    }
}
impl Effect for Biquad {
    fn process(&mut self, x: f32) -> f32 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
    fn state(&mut self) -> &mut [f32] {
        &mut self.z
    }
}

// ---------- presets ----------

/// Pop-punk high gain: tight low end, mid-pushed drive, cut-through presence,
/// closed-back 4x12-ish cab. Think Rectifier/JCM800 cranked with a boost in front.
#[derive(Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
struct Knobs {
    drive: f32,
    level: f32,
    gate_db: f32,
    bass: f32,
    mid: f32,
    treble: f32,
    bypass: bool,
    mute: bool, // silent tuning
}
impl Default for Knobs {
    fn default() -> Self {
        Knobs { drive: 1.0, level: 0.25, gate_db: -50.0, bass: 4.0, mid: -5.0, treble: 4.0, bypass: false, mute: false }
    }
}

fn blink(sr: f32, k: &Knobs) -> Vec<Box<dyn Effect>> {
    use Kind::*;
    vec![
        Box::new(Gate::new(k.gate_db, sr)),
        Box::new(Biquad::new(Hp, 130.0, 0.7, sr)),       // tighten before clipping: no flub on palm mutes
        Box::new(Biquad::new(Peak(7.0), 850.0, 0.8, sr)), // "boost pedal" mid hump
        Box::new(Drive { gain: 25.0 * k.drive, bias: 0.15 }),
        Box::new(Biquad::new(Lp, 7000.0, 0.7, sr)),       // tame fizz between stages
        Box::new(Drive { gain: 4.0, bias: 0.05 }),
        // tone stack
        Box::new(Biquad::new(LowShelf(k.bass), 120.0, 0.7, sr)), // chug
        Box::new(Biquad::new(Peak(k.mid), 450.0, 0.9, sr)),   // scoop the boxiness
        Box::new(Biquad::new(Peak(k.treble), 2200.0, 1.0, sr)),   // bite
        // cab sim
        Box::new(Biquad::new(Hp, 75.0, 0.7, sr)),
        Box::new(Biquad::new(Peak(3.0), 110.0, 1.2, sr)), // cab thump resonance
        Box::new(Biquad::new(Lp, 5000.0, 0.7, sr)),
        Box::new(Biquad::new(Lp, 5500.0, 0.7, sr)), // stacked = steep speaker rolloff
        Box::new(Gain(k.level)),
    ]
}

fn run_chain(chain: &mut [Box<dyn Effect>], x: f32) -> f32 {
    chain.iter_mut().fold(x, |s, fx| fx.process(s)).clamp(-1.0, 1.0) // clamp: protect ears
}

// ---------- audio I/O ----------

struct Shared {
    knobs: Mutex<Knobs>,
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

/// Named input device (channel `ch`, 0-based, clamped) -> chain -> named output device.
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

    let mut cur = *shared.knobs.lock().unwrap();
    let mut chain = blink(sr, &cur);
    let (mut tx, mut rx) = HeapRb::<f32>::new(8192).split();
    for _ in 0..target {
        tx.try_push(0.0).ok();
    }
    // tuner tap: raw input, decimated to <=48k so pitch detection stays cheap at 192k.
    // ponytail: decimation without anti-alias filter; fine for pitch, add a lowpass if the tuner jitters at 96k+.
    let decim = (rate / 48000).max(1) as usize;
    let (mut ttx, trx) = HeapRb::<f32>::new(4 * TUNER_WIN).split();
    *shared.tuner.lock().unwrap() = Tuner { rx: Some(trx), win: VecDeque::new(), sr: sr / decim as f32 };
    let mut tick = 0usize;

    let err = |e| eprintln!("stream error: {e}");
    let input = in_dev.build_input_stream(
        in_cfg,
        move |data: &[f32], _: &_| {
            if let Ok(k) = shared.knobs.try_lock() {
                if *k != cur {
                    cur = *k;
                    // ponytail: rebuilds (allocates) on the audio thread, only while a knob moves. Fine in practice; add in-place coefficient updates if you hear dropouts while turning knobs.
                    let mut new = blink(sr, &cur);
                    for (n, o) in new.iter_mut().zip(chain.iter_mut()) {
                        let (ns, os) = (n.state(), o.state());
                        if ns.len() == os.len() {
                            ns.copy_from_slice(os);
                        }
                    }
                    chain = new;
                }
            }
            let mut peak = 0f32;
            for frame in data.chunks(in_ch) {
                let x = frame[ch.min(in_ch - 1)];
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
                let y = if cur.mute { 0.0 } else if cur.bypass { x } else { run_chain(&mut chain, x) };
                let _ = tx.try_push(y);
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
fn get_knobs(st: tauri::State<AppState>) -> (Knobs, Knobs) {
    (*st.shared.knobs.lock().unwrap(), Knobs::default())
}

#[tauri::command]
fn set_knobs(knobs: Knobs, st: tauri::State<AppState>) {
    *st.shared.knobs.lock().unwrap() = knobs;
}

#[derive(serde::Serialize)]
struct Poll {
    peak: f32,
    freq: Option<f32>,
    note: Option<String>,
    cents: f32,
}

#[tauri::command]
fn poll(st: tauri::State<AppState>) -> Poll {
    let peak = f32::from_bits(st.shared.peak.swap(0, Relaxed));
    let mut t = st.shared.tuner.lock().unwrap();
    let Tuner { rx, win, sr } = &mut *t;
    if let Some(rx) = rx {
        win.extend(rx.pop_iter());
    }
    while win.len() > TUNER_WIN {
        win.pop_front();
    }
    let freq = detect_pitch(win.make_contiguous(), *sr);
    let (note, cents) = freq.map(note_of).map_or((None, 0.0), |(n, c)| (Some(n), c));
    Poll { peak, freq, note, cents }
}

fn main() {
    let shared = Arc::new(Shared {
        knobs: Mutex::new(Knobs::default()),
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
        .invoke_handler(tauri::generate_handler![devices, connect, get_knobs, set_knobs, poll])
        .run(tauri::generate_context!())
        .expect("tauri failed to start");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_and_chain_sane() {
        let sr = 48000.0;
        let settle = |mut f: Biquad| (0..5000).map(|_| f.process(1.0)).last().unwrap();
        assert!((settle(Biquad::new(Kind::Lp, 1000.0, 0.7, sr)) - 1.0).abs() < 1e-3); // LP passes DC
        assert!(settle(Biquad::new(Kind::Hp, 1000.0, 0.7, sr)).abs() < 1e-3); // HP kills DC
        // loud sine through the full chain stays finite, bounded, non-silent
        let mut chain = blink(sr, &Knobs::default());
        let out: Vec<f32> = (0..48000)
            .map(|i| run_chain(&mut chain, 0.3 * (2.0 * PI * 110.0 * i as f32 / sr).sin()))
            .collect();
        let peak = out[24000..].iter().fold(0f32, |m, x| m.max(x.abs()));
        assert!(peak.is_finite() && peak > 0.05 && peak <= 1.0, "peak {peak}");
    }

    #[test]
    fn tuner_finds_fundamental() {
        let sr = 48000.0;
        // harmonic-rich "string": must lock onto the fundamental, not an overtone
        let tone = |f: f32| -> Vec<f32> {
            (0..TUNER_WIN)
                .map(|i| (1..6).map(|h| (2.0 * PI * f * h as f32 * i as f32 / sr).sin() * 0.2 / h as f32).sum())
                .collect()
        };
        for (f, name) in [(82.41, "E2"), (110.0, "A2"), (196.0, "G3"), (329.63, "E4")] {
            let got = detect_pitch(&tone(f), sr).expect("no pitch");
            let (n, cents) = note_of(got);
            assert!((got - f).abs() < 0.5 && n == name && cents.abs() < 5.0, "{f}: got {got} {n} {cents}");
        }
        assert_eq!(detect_pitch(&vec![0.0; TUNER_WIN], sr), None); // silence
        assert!(note_of(440.0 * 2f32.powf(0.2 / 12.0)).1 > 15.0); // 20 cents sharp reads sharp
    }
}
