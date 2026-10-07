# Rust-ScreenRec

Fast screenshots and screen recording, written in Rust. On Linux (X11) use it from the command line or from a launcher modelled on GNOME 42's screenshot UI; Windows and macOS builds have the command line.

## Features

- Capture a selection, the whole screen or a single window (a window keeps recording even while covered or moved)
- H.264 video, encoded on the GPU (NVENC, else VAAPI, Media Foundation or VideoToolbox through ffmpeg) or on the CPU with x264
- Audio: system output, the recorded app only, and/or the microphone (Opus)
- MKV or MP4 output, PNG or JPG screenshots
- Only changed screen regions are processed, so CPU usage stays low

## Requirements

- Rust (edition 2024)
- Linux with X11. In a Wayland session only X11 apps' windows can be recorded (`rec --window`); screenshots and screen recordings say so instead of coming out black.
- Optional, depending on what you use:
  - NVIDIA driver for NVENC; otherwise `ffmpeg` with VAAPI (Intel, AMD) for the GPU, or the CPU
  - `ffmpeg` for CPU encoding and MP4 output
  - PulseAudio/PipeWire tools (`parec`, `pactl`) and `libopus` for audio
  - GNOME for the keyboard shortcut

## Other systems

- Windows 10/11: `shot` and `rec` (GDI capture of every monitor, `--window` with an HWND), NVENC or `ffmpeg` (Media Foundation, x264). No launcher or sound yet.
- macOS 11+: `shot` and `rec` of the main display (no pointer yet), `ffmpeg` (VideoToolbox, x264). Allow your terminal in System Settings › Privacy & Security › Screen Recording. No launcher, window recording or sound yet.

Neither has been tried on a real Windows PC or Mac yet: treat them as previews.

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

In the launcher, pick Area, Screen or Window, then screenshot or recording. Enter or Space captures and Esc closes. Tab and the arrow keys move a focus ring over the controls; with the ring showing, Enter or Space activates the focused one. While you drag an area, its size is shown below it; in Window mode it shows the window's name and size.

The interface is in English, Spanish or Japanese. Pick one in the launcher's ⚙ settings; until you do, it follows your locale (`LANG`).
