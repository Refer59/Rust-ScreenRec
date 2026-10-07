# Rust-ScreenRec

Fast screenshots and screen recording for X11, written in Rust. Use it from the command line or from a launcher modelled on GNOME 42's screenshot UI.

## Features

- Capture a selection, the whole screen or a single window (a window keeps recording even while covered or moved)
- H.264 video, encoded on the GPU with NVENC or on the CPU with x264
- Audio: system output, the recorded app only, and/or the microphone (Opus)
- MKV or MP4 output, PNG or JPG screenshots
- Only changed screen regions are processed, so CPU usage stays low

## Requirements

- Linux with X11 (Wayland is not supported)
- Rust (edition 2024)
- Optional, depending on what you use:
  - NVIDIA driver for GPU encoding (falls back to the CPU if missing)
  - `ffmpeg` for CPU encoding and MP4 output
  - PulseAudio/PipeWire tools (`parec`, `pactl`) and `libopus` for audio
  - GNOME for the keyboard shortcut

## Install

```sh
./install.sh
```

This builds and installs `screenrec` to `~/.cargo/bin` and binds the launcher to `-` in GNOME. You can change the shortcut from the launcher's ⚙ settings.

## Usage

```sh
screenrec                                  # launcher: screenshot or recording
screenrec shot [file.png|.jpg]             # full-screen screenshot
screenrec rec [file.mkv|.mp4] [-r FPS] [--window ID] [--cpu]
screenrec install                          # set up the GNOME shortcut
```

`rec` records until Ctrl+C or SIGTERM. Max FPS is 60 on the GPU and 30 on the CPU; `--cpu` forces CPU encoding. Files go to your Pictures and Videos folders by default.

In the launcher, press Enter or Space to capture and Esc to cancel.
