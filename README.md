<div align="center">
  <img src="renarrator.png" width="180" alt="Renarrator logo" />
  <h1>Renarrator</h1>
  <p>
    <b>A background engine for play-on-typing audio triggers.</b><br/>
    Type a word - Windows plays a sound. Lives in the tray, layout-independent.
  </p>
  <p>
    <img src="https://img.shields.io/badge/platform-Windows%2010%2F11-0078D6?logo=windows&logoColor=white" alt="platform" />
    <img src="https://img.shields.io/badge/built%20with-Tauri%202-24C8DB?logo=tauri&logoColor=white" alt="tauri" />
    <img src="https://img.shields.io/badge/engine-Rust-orange?logo=rust&logoColor=white" alt="rust" />
    <img src="https://img.shields.io/badge/license-MIT-green" alt="license" />
  </p>
</div>

![Renarrator Settings Window](docs/settings-window.png)

## What It Is

Renarrator is a background application for Windows that monitors physical keyboard input across **any** application and instantly plays a sound effect when a typed word matches one of your triggers.

Type `banana` in a chat, a game, or a text editor - get a sound. The keyboard layout (Russian/English) doesn't matter: the engine compares **physical keys** rather than characters, so `banana` and its layout equivalent are treated as the exact same word by the trigger.

## Features

- **Global Keyboard Intercept** - Low-level hook (WinAPI `WH_KEYBOARD_LL` via `rdev`), works on top of any active application without requiring window focus.
- **Layout-Independent Triggers: Words, Numbers, and Phrases** - Maps physical keys to characters, case-insensitive. A trigger can be a single word (`banana`), a numeric code (`123`), or a multi-word phrase (`banana apple` - triggers on the last letter, no trailing space required). `Enter`/`Backspace` and a pause longer than 2 seconds clear the buffer; `Space` acts as a word separator within phrases.
- **Multiple Sounds per Trigger with Weights** - Weighted random selection (`P = weight / Σweights`) to keep sound effects from feeling repetitive.
- **Polyphony / Overlap** - Sounds can overlap or interrupt previous ones (toggled with a single checkbox).
- **Flexible Volume Controls** - Master volume × individual file volume.
- **Supported Audio Formats**: `.mp3`, `.wav`, `.ogg` (`rodio` engine / WASAPI).
- **System Tray Integration** - Left-click the icon to open settings; right-click for a quick menu (pause detection / stop all sounds / exit).
- **Drag & Drop** - Drag audio files directly onto the sound line in settings.
- **Windows Startup** - Optional autorun via `HKCU\...\Run`.
- **Glass UI** - Acrylic blur and rounded corners via DWM, featuring a custom title bar.
- **Soundpad-Style Mic Mixing** - Continuously captures your default microphone and mixes trigger sounds into it in real time, rendering the mix to a virtual audio cable so voice chat apps (Discord, TeamSpeak, in-game voice) hear the sounds as if they came from your mic. Per-trigger toggles: "Play into microphone" (others hear it) and "Play for myself" (you hear it too, on by default). Zero manual device setup: the first time you enable "Play into microphone" on any trigger, Renarrator detects or silently installs a virtual audio cable for you (one Windows admin prompt, see Privacy below) - you only need to pick that cable as your microphone once in Discord/your game's own settings, same as you would for the original Soundpad.

## Installation (User Guide)

1. Go to the [Releases](https://github.com/reteren/renarrator/releases) page.
2. Download `Renarrator_x.x.x_x64-setup.exe` from the latest release.
3. Run the installer (NSIS) - a shortcut will appear in the Start menu.
4. After launching, the app sits in the system tray. Left-click the icon → settings: add a trigger, enter words, drag and drop sounds, and click **Save**.

> Windows SmartScreen may display a warning about an unsigned installer (code is not signed with an EV certificate) - click "More info → Run anyway".

### Configuration Location

`%APPDATA%\KeySoundTrigger\config.json` - Automatically created on the first launch; can be edited manually (the app will apply changes on the next save).

### Privacy

The keystroke buffer exists **solely in RAM**, resets after each word, and is never transmitted anywhere. The one network request the app can make: the first time you check **"Play into microphone"** on a trigger, if no virtual audio cable is already installed, Renarrator downloads the official VB-CABLE installer from `vb-audio.com` and runs it - Windows will show the same administrator/UAC prompt you'd see installing it by hand. This never happens unless you enable that checkbox.

## Building from Source

**Requirements:** Windows 10/11, [Rust stable (MSVC)](https://rustup.rs/), [Node.js 18+](https://nodejs.org/), Visual Studio Build Tools ("Desktop development with C++" workload), WebView2 (built into Windows 11).

```powershell
git clone [https://github.com/reteren/renarrator.git](https://github.com/reteren/renarrator.git)
cd renarrator
npm install

# Dev mode (hot-reload UI)
npm run dev

# Production build → src-tauri\target\release\bundle\nsis\
npm run build

```

## Creating a Release (For Maintainers)

```powershell
git tag v0.2.0
git push origin v0.2.0

```

GitHub Actions (`.github/workflows/release.yml`) will build the NSIS installer and publish a release with the `Renarrator_x64-setup.exe` artifact — users can simply download and run it.

## Project Structure

```
├─ src/                     # Frontend (vanilla JS, no bundler)
│  ├─ index.html            # Settings window
│  ├─ tray-menu.html        # Custom tray menu
│  └─ fonts/                # Manrope (woff2)
├─ src-tauri/
│  ├─ src/
│  │  ├─ lib.rs             # Entry point: windows, tray, Tauri commands, updater
│  │  ├─ keyboard_hook.rs   # Global low-level hook (rdev)
│  │  ├─ layout_map.rs      # Physical keys → characters (RU/EN)
│  │  ├─ buffer_manager.rs  # Input buffer, timeouts, word matching
│  │  ├─ audio_engine.rs    # rodio/cpal: polyphony, weights, volume, mic capture + mixing
│  │  ├─ config.rs          # %APPDATA%\KeySoundTrigger\config.json
│  │  ├─ autostart.rs       # Startup via Windows registry
│  │  ├─ virtual_mic_setup.rs # Downloads/installs VB-CABLE on demand (UAC-elevated)
│  │  └─ win_glass.rs       # DWM acrylic + rounded regions
│  └─ tauri.conf.json
└─ .github/workflows/       # Release CI

```

## License

[MIT](https://www.google.com/search?q=LICENSE) © reteren
