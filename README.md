> **Disclaimer:** this is a vibecoded project. I'm learning guitar and didn't want to spend money on pedals, so I had an AI build me one. Expect rough edges.

# OpenPedal

A modular desktop pedalboard for guitar (Rust + Tauri). Plug into any audio interface, build a chain from 15 pedals, reorder it by dragging, and route sidechains between them.

![screenshot](docs/screenshot.png)

## Pedals

Noise Gate · Compressor · Overdrive · Hi-Gain · Fuzz · Cab Sim · EQ · Chorus · Phaser · Tremolo · Delay · Reverb · Sidechain · Boost · Looper, plus a tuner (stomp it to mute while tuning) at the head of the chain.

- **Chain:** click or drag pedals in from the bottom palette; drag a pedal's name to reorder, ◀ ▶ ✕ on hover. Delay tails and loops keep going while you rearrange.
- **Sidechain:** Gate, Compressor, Delay, Reverb and Sidechain pedals have a **key** selector:
  - *Self* — the signal at that point in the chain
  - *Guitar* — your dry input (ducked delay/reverb: echoes get out of the way while you play)
  - *Aux in* — another input channel (pick it in the top bar), e.g. a mic or drum machine on input 1
  - *Tempo* — a kick-style pump at the BPM (tap tempo in the top bar)
- **Presets:** 11 factory presets (Pop-Punk Power, Metal Tight, Blues Crunch, Fuzz Wall, 80s Lead, Ducked Delay Lead, Ambient Swells, Sidechain Pump, Funk Clean, Surf Spring, Looper Practice). Save your own; they live in `~/Library/Application Support/io.github.jerasaurus.openpedal/presets.json`. Factory presets are plain JSON in [`presets.json`](presets.json).
- **Knobs:** drag up/down, scroll, or arrow keys. Shift = fine. Double-click = default.

## Run

```sh
cargo run --release
```

Needs Rust. Use headphones, and keep the buffer small for low latency. Or grab a `.dmg` from [Releases](../../releases).

## Releases

Push a tag and CI builds a universal (Apple silicon + Intel) `.dmg` into a draft GitHub release:

```sh
git tag v0.2.0 && git push origin v0.2.0
```

Without Apple secrets the build is **unsigned** (macOS will warn on first open: right-click → Open). To get a **signed + notarized** build, add these repo secrets (Settings → Secrets and variables → Actions):

| Secret | What |
|---|---|
| `APPLE_CERTIFICATE` | Your **Developer ID Application** certificate exported as `.p12`, base64: `base64 -i cert.p12 \| pbcopy` |
| `APPLE_CERTIFICATE_PASSWORD` | Password you set when exporting the `.p12` |
| `APPLE_SIGNING_IDENTITY` | e.g. `Developer ID Application: Your Name (TEAMID)` — see `security find-identity -v -p codesigning` |
| `APPLE_ID` | Your Apple ID email |
| `APPLE_PASSWORD` | An [app-specific password](https://account.apple.com) for that Apple ID |
| `APPLE_TEAM_ID` | Your 10-character team ID |

A Developer ID certificate needs the paid Apple Developer Program; an "Apple Development" certificate can't be notarized.
