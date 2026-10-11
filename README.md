# GloorpFlorp

Private screen sharing and voice chat for two Windows PCs.

You and one friend can watch each other's screens in high quality (up to
1440p at 60 fps) with low delay, hear each other's game sound, and talk with
push-to-talk or voice activation. It was made for playing games like Escape
from Tarkov together.

GloorpFlorp connects your two PCs directly through
[Tailscale](https://tailscale.com). There is nothing to sign up for.

**On this page:** [Quick start](#quick-start) ·
[Privacy](#privacy) ·
[What you need](#what-you-need) ·
[Getting the app](#getting-the-app) ·
[How to use it](#how-to-use-it) ·
[Tips](#recommended-settings-and-tips) ·
[Troubleshooting](#troubleshooting) ·
[Limitations](#limitations) ·
[For developers](#for-developers)

## Quick start

1. Both of you: install [Tailscale](https://tailscale.com/download) and sign in.
2. Both of you: extract the whole GloorpFlorp zip and double-click
   `GloorpFlorp.exe`.
3. One of you clicks **Host** and sends the code to the other.
4. The other pastes the code and clicks **Join**. You can now talk.
5. Click **Start sharing** to show your screen. Your friend's screen opens in
   its own window.

The rest of this guide explains each step.

## Privacy

- **No servers, no accounts, no analytics.** GloorpFlorp has no website to
  log in to, sends no usage data and doesn't check for updates. It only ever
  connects to your friend's PC. (You do need a free Tailscale account, but
  that's Tailscale's, not GloorpFlorp's.)
- **Direct and encrypted.** Your video, game sound and voice go straight from
  one PC to the other inside your Tailscale network, which encrypts
  everything. If Tailscale can't link your PCs directly, it passes the
  still-encrypted data through one of its relay servers, which can't read it.
- **Only these files, all on your own PC:**
  - your settings: `%APPDATA%\GloorpFlorp\settings.json`
  - a log for troubleshooting: `%APPDATA%\GloorpFlorp\log.txt`
  - the log from the time before: `%APPDATA%\GloorpFlorp\log.old.txt`
- To remove GloorpFlorp completely, delete its folder and the
  `%APPDATA%\GloorpFlorp` folder.

## What you need

On **both** PCs:

- **Windows 10 version 2004 (May 2020) or newer, or Windows 11.** To check:
  press **Windows + R**, type `winver` and press Enter.
- **An NVIDIA graphics card with an up-to-date driver.** Update it with the
  NVIDIA App (or GeForce Experience), or from
  [nvidia.com/drivers](https://www.nvidia.com/drivers). An old driver is the
  most likely reason screen sharing won't start.
- **Tailscale, installed and signed in**
  ([download](https://tailscale.com/download)). When it works, the Tailscale
  icon in the taskbar tray shows you as connected and lists your friend's PC.
- **A headset** (strongly recommended, see [tips](#recommended-settings-and-tips)).
- **Enough upload speed** for whoever is sharing: about 25–30 Mbps for the
  default quality. Less works with lower settings.

### If you use two different Tailscale accounts

Your PCs must be able to reach each other in Tailscale. If you each have your
own Tailscale account, one of you shares their PC with the other:

1. Open the [Tailscale admin console](https://login.tailscale.com/admin/machines)
   and sign in.
2. Find your PC in the list, click the **...** menu next to it and choose
   **Share**.
3. Send the invite link to your friend. They open it and accept.

**The person whose PC is shared must be the one who clicks Host.** Sharing
lets your friend's PC reach yours, but not the other way round, so the
friend always joins.

## Getting the app

### Download

The GitHub repository is private, so only its owner (signed in to GitHub) can
download builds:

- **From a release** (when there is one): on the repository's GitHub page,
  click **Releases** on the right, open the newest one and click
  `GloorpFlorp-windows-x64.zip` under **Assets**.
- **From the latest automatic build:** open the **Actions** tab, click the
  newest run with a green check mark, scroll down to **Artifacts** and click
  **GloorpFlorp-windows-x64**. These downloads are deleted after 14 days;
  every new change to the code makes a new one.

**Your friend needs the same zip.** They can't download it from GitHub, so
send it to them yourself, for example as a Google Drive, OneDrive or Dropbox
link, or with WeTransfer. It's over 100 MB, too big for most chat apps. You
must both use the same version: `VERSIONS.txt` in the folder shows which one
you have.

### Extract it

1. Right-click the zip and choose **Extract All...**.
2. Pick a normal folder you can find again, such as `Documents` or the
   Desktop, and click **Extract**.
3. Open the extracted `GloorpFlorp` folder. Keep everything in it together:
   `GloorpFlorp.exe` needs the `ffmpeg` and `mpv` folders next to it.

> Don't double-click `GloorpFlorp.exe` while looking inside the zip. Windows
> would run it without its `ffmpeg` and `mpv` folders, and screen sharing
> wouldn't work.

### First launch

1. Double-click `GloorpFlorp.exe`.
2. If you see **"Windows protected your PC"**, click **More info**, then
   **Run anyway**. This appears because GloorpFlorp isn't code-signed
   (signing costs money and is meant for public apps), not because something
   is wrong.
3. When **Windows Defender Firewall** asks about GloorpFlorp, tick **both
   Private networks and Public networks**, then click **Allow access**.
   Windows often counts the Tailscale network as "Public", so Private alone
   may not be enough. This usually appears the first time you click Host
   (sometimes also when joining). If you clicked Cancel by mistake, see
   [Troubleshooting](#troubleshooting).

## How to use it

### Connect

**The host:**

1. Click **Host**.
2. GloorpFlorp shows a code like `K7QM-2XPA-9TD`. Copy it and send it to your
   friend (Discord, text message, anything).
3. Keep GloorpFlorp open. It shows when your friend has joined.

**The friend:**

1. Paste the code into the code box. Upper or lower case, spaces and dashes
   don't matter.
2. Click **Join**.

As soon as you're connected, you can talk (see [Voice chat](#voice-chat)).

Good to know:

- The code contains the host's Tailscale address and a random secret. Only
  devices in your Tailscale network can reach the host at all, and the secret
  stops anyone joining by accident.
- If the connection drops, the friend's PC tries to reconnect by itself for
  30 seconds. The host keeps waiting, so the friend can rejoin with the same
  code for as long as the host leaves GloorpFlorp open. The host's screen
  sharing and game audio keep going, so the friend sees and hears them again
  as soon as they're back.
- **Leave** disconnects you. It also stops your screen sharing and game
  audio. **New code** (while waiting for your friend) closes the room and
  opens a new one with a different code.

### Share your screen

1. Choose the **monitor** to share.
2. Choose the **quality** (resolution, frame rate and bitrate). The defaults
   are a good start; see [tips](#recommended-settings-and-tips).
3. Click **Start sharing**. Click **Stop sharing** when you're done.

- GloorpFlorp shares your **whole monitor**, including everything else on it
  (chat windows, notifications).
- You can both share at the same time.
- Sharing uses your graphics card's separate video encoder (NVENC), so it
  barely affects your game's frame rate.

### Watch your friend's screen

- When your friend starts sharing, their screen opens in its **own window**
  by itself. To stop that, untick **Open the viewer by itself when my friend
  starts sharing** under "Friend's screen".
- Move and resize it like any other window. A second monitor is ideal.
- Press **f** or **double-click** the picture to switch fullscreen on or off.
- Close the window whenever you like. Your friend's PC then stops sending
  video, which saves bandwidth. Click **Open viewer** to bring it back.
- A newly opened window can stay black for a few seconds, until the next
  full picture arrives.

### Game audio

Game audio is separate from video: you can send your PC's sound with or
without sharing your screen. Under "Game audio", choose what to send, then
click **Start sending game audio** (and **Stop sending game audio** when
you're done). It always starts off when you open GloorpFlorp.

- **Everything except GloorpFlorp**: all sound your PC plays (game, music,
  videos, other voice chat apps) except GloorpFlorp's own sound, so your
  friend never hears their own voice coming back.
- **Only one program**: pick it from the list, for example
  `EscapeFromTarkov.exe`. Only that program's sound is sent. Start the game
  first so it shows up in the list (click **Refresh** if it's missing). If
  you close the game, game audio stops by itself: after restarting the game,
  click **Start sending game audio** again.

You keep hearing your game as normal. Your friend can set the volume of your
game separately from your voice.

> If you're also in a Discord (or other) call with each other, use **Only one
> program**. With "Everything except GloorpFlorp", the Discord call would be
> sent to your friend as well, and they'd hear themselves echo.

### Voice chat

Choose how your microphone sends:

- **Push-to-talk**: your voice is sent only while you hold a key. Click the
  button to set the key, then press any keyboard key or mouse button. The
  side mouse buttons (Mouse 4 and Mouse 5) work well. It works while the game
  is in focus (unless the game runs as administrator, see
  [Troubleshooting](#troubleshooting)). The game still receives the key too,
  so pick one you don't use in the game.
- **Voice activation**: sends when you speak. If the start of your words
  gets cut off, raise the sensitivity. If keyboard clicks get through, lower
  it.
- **Always on**: sends all the time. Noise suppression still removes steady
  background noise.

Also:

- **Mute**: your microphone sends nothing.
- **Deafen**: you hear nothing from your friend (no voice, no game audio).
- **Volume**: separate sliders for your friend's voice and your friend's game
  audio.
- **Microphone and speakers**: choose them under "Voice". "System default"
  follows Windows' sound settings. If the one you chose is unplugged,
  GloorpFlorp uses the system default until it's back.
- **Noise suppression, echo cancellation and automatic mic volume** are on by
  default. Leave them on unless you have a reason not to.

Push-to-talk works by asking Windows whether the key is being held.
GloorpFlorp doesn't hook into your keyboard or touch the game in any way.

### Connection details

Click **Connection details** at the bottom of the window to see how things
are going:

- **Ping**: the round trip time between the PCs. Under about 50 ms is great.
- **Bitrates: video** and **Bitrates: audio**: how much video and audio is
  being sent and received.
- **Dropped frames**: video frames skipped because the network couldn't keep
  up. If this keeps climbing, lower the bitrate.
- **Audio loss**: missing audio packets. A few percent is fine; small gaps
  are filled in.

## Recommended settings and tips

- **Codec: leave it on Auto.** An RTX 40 or 50 series card (like the RTX 5080)
  sends AV1, which gives the best picture for the bandwidth. The RTX 3070 Ti
  can't encode AV1, so it uses HEVC automatically. Both cards can play all of
  them.
- **Quality:** 1440p at 60 fps with about **20–25 Mbps** looks great.
  Fast-moving games look better with more bitrate, if your upload allows it.
- **Frame rate: stay at 60.** 120 and 144 fps are marked "experimental": in
  tests the screen capture only reached about 75–90 pictures per second.
- **Upload speed matters most.** The sharer's upload speed must be well above
  the bitrate. Test it at [speedtest.net](https://www.speedtest.net). If your
  upload is 30 Mbps, use 20 Mbps or less. With slower upload, choose 1080p
  and a lower bitrate.
- **Run games in borderless windowed mode** (in Tarkov: Settings, Graphics,
  Screen mode: Borderless). Exclusive fullscreen can make the shared picture
  freeze or go black, especially when alt-tabbing.
- **Use a second monitor for watching**, so the viewer window doesn't sit on
  top of your game.
- **Use headsets, especially when sending game audio.** Echo cancellation
  only removes your friend's voice and game audio from what your microphone
  picks up, not your own game's sound. On speakers, your game leaks into
  your microphone, so your friend hears it twice (once as game audio, once
  slightly later through your voice).
- **Pause big downloads** (game updates, cloud backups, streaming on other
  devices) while you play.

## Troubleshooting

| Problem | What to try |
|---|---|
| GloorpFlorp says Tailscale isn't running | Start Tailscale from the Start menu and sign in. The tray icon should show you as connected. Then click **Host** again. |
| The friend can't connect | Is GloorpFlorp still open on the host's PC? Are both PCs connected in Tailscale, and can each see the other in Tailscale's device list? With two Tailscale accounts, the person whose PC is *shared* must host. Check the host's firewall (next row). In Command Prompt on the friend's PC, run `tailscale ping <host's PC name>`: if that fails, the problem is in Tailscale, not GloorpFlorp. |
| Firewall: you clicked Cancel, or only ticked Private | Press Start, type **Allow an app through Windows Firewall** and open it. Click **Change settings**, find **GloorpFlorp**, tick both **Private** and **Public**, then **OK**. If it isn't listed, click **Allow another app...** and pick `GloorpFlorp.exe`. Windows asks again if you move the GloorpFlorp folder. |
| "Wrong room code" or "typo" message | Copy and paste the code instead of typing it. If the host restarted GloorpFlorp or clicked **New code**, the code has changed: ask for the current one. |
| A message about different versions | You're using different builds. Both of you use the same zip (compare `VERSIONS.txt`). |
| Everything is slow or laggy | Open Command Prompt and run `tailscale status`. If the line for your friend's PC says **relay** (instead of **direct**), your data goes through Tailscale's relay servers, which are much slower. Restart Tailscale on both PCs, turn off any other VPN and try again. Some home routers need UPnP turned on for a direct connection. Until then, use a low bitrate. |
| The viewer window stays black | Wait a few seconds. Check that your friend is still sharing and picked the right monitor. Games in exclusive fullscreen can't always be captured: switch the game to borderless windowed. On a laptop, the shared monitor must be connected to the NVIDIA graphics card. Close the viewer and click **Open viewer**. |
| Video is choppy or freezes now and then | Lower the bitrate (try 15 Mbps) or choose 1080p. Check `tailscale status` for **relay** (see above). Pause downloads. If **dropped frames** in the connection details keeps climbing, the network can't keep up. |
| An error about NVENC, the encoder or the driver when you start sharing | Update your NVIDIA driver and restart the PC; this fixes most of these. Then choose another codec (H.264) under **Codec** in "Your screen". Close other programs that record with the graphics card (OBS, NVIDIA Instant Replay) and try again. |
| "Couldn't find FFmpeg", or the viewer never opens | GloorpFlorp can't find its `ffmpeg` or `mpv` folder. Extract the whole zip again and run `GloorpFlorp.exe` from the extracted folder. If your antivirus removed a file, restore it, then extract again. |
| Your friend hears no game audio | Check that GloorpFlorp says "Sending game audio." (click **Start sending game audio**) and that the right program is chosen. Some games play sound from a different program than the one with the window: try **Everything except GloorpFlorp**. If you restarted the game, click **Start sending game audio** again. Ask your friend to check their game-audio volume slider and that they're not deafened. |
| Your friend hears an echo of their own voice | You're probably on speakers: use a headset, or turn your speakers down and keep echo cancellation on. If you're also in a Discord call together, send only one program's game audio (see [Game audio](#game-audio)). |
| Your friend can't hear you | Check that you're not muted, the right microphone is chosen, and the mic meter moves when you talk. With push-to-talk, hold the key while talking. In Windows Settings, under Privacy, Microphone, make sure desktop apps may use the microphone. |
| Push-to-talk doesn't work while the game is running | If the game (or its launcher) runs as administrator, Windows hides its key presses from normal programs. Close GloorpFlorp, right-click `GloorpFlorp.exe` and choose **Run as administrator**. Or use **Voice activation** instead. |

### The log file

GloorpFlorp writes what it's doing to a log file. When something goes wrong,
it's the most useful thing to send to whoever is helping you.

1. Press **Windows + R**, paste `%APPDATA%\GloorpFlorp` and press Enter.
2. Send `log.txt` from that folder (from both PCs, if you can), and say
   roughly when the problem happened. If you have restarted GloorpFlorp
   since, send `log.old.txt` too: it's the log from the time before.

The log contains technical details such as your PC's name, Tailscale
addresses and audio device names. It doesn't contain any of your audio or
video.

## Limitations

To be honest about what GloorpFlorp is and isn't:

- **Two people only.** No groups.
- **Windows and NVIDIA only.** Screen sharing needs an NVIDIA graphics card's
  video encoder.
- **Low delay, but not zero.** Fine for watching and calling things out, but
  you wouldn't want to play a game through it.
- **Video can stutter on a bad connection.** Video is sent over TCP, so it
  never arrives garbled, but when the network hiccups the picture briefly
  freezes and then skips ahead to catch up. Voice and game audio travel
  separately and keep going.
- **Whole monitors only**, not single windows. Everything on the shared
  monitor is visible.
- **No recording, no remote control.**
- **Not code-signed**, so Windows shows the "Windows protected your PC"
  warning the first time.
- **Not fully tested yet.** Most parts have been tested on one PC (the RTX
  5080), but sharing a real game screen and a real session between the two
  PCs haven't been tried yet, so the first real session may uncover problems.
  When something doesn't work, send the log files from both PCs.

## Updating

Download the new zip, extract it and use the new folder (you can delete the
old one). Your settings are kept, because they're stored in
`%APPDATA%\GloorpFlorp`. Both of you need to update.

## For developers

GloorpFlorp is a Rust workspace. [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)
explains the design, the data flow and the reasons behind the big decisions.

| Path | What's in it |
|---|---|
| `crates/gf-proto` | Wire format, room codes, video bitstream handling, audio jitter buffer (pure Rust, no I/O) |
| `crates/gf-sys` | Windows-specific parts: monitors, game audio capture, push-to-talk keys, Tailscale address |
| `crates/gf-net` | The TCP + UDP connection between the two PCs |
| `crates/gf-video` | Runs FFmpeg (screen capture + NVENC) and mpv (viewer window) |
| `crates/gf-audio` | Microphone processing, Opus, jitter buffers, mixing, playback |
| `app` | The engine that wires everything together, the window (egui), settings, logging |
| `packaging` | Assembles the Windows zip (used by CI) |

**Build from source** on Windows: install [Rust](https://rustup.rs), Visual
Studio Build Tools with "Desktop development with C++", and
[CMake](https://cmake.org/download/) (libopus is compiled from source). Then:

```
cargo build --release
```

The app is `target\release\GloorpFlorp.exe`. To run it, copy the `ffmpeg`
and `mpv` folders from a release zip next to it (or have `ffmpeg` and `mpv`
on your PATH). The viewer needs mpv 0.41 or newer.

**Tests:** `cargo test --workspace`. Most tests also run on Linux (install
`libasound2-dev`, `cmake` and `ffmpeg`). There, the Windows-specific parts
are stubs and video uses FFmpeg's test pattern with software encoders. Tests
that need FFmpeg skip their video part (with a note) when it isn't found.

**Environment variables for development and tests:**

| Variable | Effect |
|---|---|
| `GLOORP_FAKE_AUDIO=1` | No sound devices: the microphone is a test tone, playback goes nowhere, game audio is a 440 Hz tone. |
| `GLOORP_FAKE_VIDEO=1` | On Windows too: FFmpeg's test pattern with software encoders instead of the screen and NVENC; without mpv the viewer is a hidden FFmpeg process. |
| `GLOORP_DEV_IP=127.0.0.1` | Host and join on this address instead of Tailscale, so two copies can talk on one PC. |
| `GLOORP_DEV_PORT=47801` | Host on and join this port instead of 47800. |

For two copies on one PC, start both with `GLOORP_DEV_IP=127.0.0.1`; one
clicks Host, the other pastes the code and clicks Join. (If the host used
`GLOORP_DEV_PORT`, give the joining copy the same port.)

**CI:** [.github/workflows/build.yml](.github/workflows/build.yml) runs tests
and clippy on Linux, and tests, the release build and packaging
([packaging/package-windows.ps1](packaging/package-windows.ps1)) on Windows.
Pushing a tag that starts with `v` (for example
`git tag v0.1.0 && git push origin v0.1.0`) publishes a GitHub Release with
the zip.
