# Working on GloorpFlorp on your own PC

This sets up your Windows PC so Claude Code can build and test GloorpFlorp
there instead of on a small cloud machine. You do it once. It takes about
20–30 minutes, mostly waiting for downloads.

What you get: much faster builds (your 9800X3D vs. 4 cloud cores), and Claude
can actually run the app on real Windows with your RTX 5080, your microphone
and your screen. That's how the hardware parts (NVENC, game audio capture,
the viewer window) get tested for real.

## 1. Install the tools

Open **Terminal** as administrator (right-click the Start button →
**Terminal (Admin)**), then paste these lines one at a time. Each one
installs one tool. Say yes if Windows asks.

```powershell
winget install --id Git.Git -e
winget install --id Kitware.CMake -e
winget install --id Rustlang.Rustup -e
winget install --id Microsoft.VisualStudio.2022.BuildTools -e --override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
```

What they are:

| Tool | Why |
|---|---|
| Git | Downloads the project from GitHub and uploads changes |
| CMake | Builds the Opus audio library that GloorpFlorp uses |
| Rustup | Installs Rust, the language GloorpFlorp is written in |
| Visual Studio Build Tools | Microsoft's compiler and linker that Rust uses on Windows (no Visual Studio app, just the tools) |

The last one is big (a few GB) and takes a while.

**Close the terminal and open a new one** (normal, not admin) so Windows
picks up the new tools, then check they work:

```powershell
git --version
cargo --version
cmake --version
```

Each should print a version number. If `cargo` says "not recognized",
restart the PC and try again.

## 2. Download the project

In the same (normal) terminal:

```powershell
cd $HOME\Documents
git clone https://github.com/munhyeonsua/GloorpFlorp.git
cd GloorpFlorp
git checkout claude/lucid-fermi-dew58r
```

The repository is public, so cloning needs no sign-in. The first time
something is pushed to GitHub, a browser window asks you to sign in to
GitHub. Sign in and approve it.

## 3. Open it in Claude

1. Install the Claude desktop app from <https://claude.ai/download> and sign in.
2. Go to the **Code** tab and choose the folder
   `Documents\GloorpFlorp` as the working folder.
3. Start a new session and say:

   > Read docs/HANDOFF.md and continue building GloorpFlorp v1.

Claude will ask before running commands on your PC. You can approve each one,
or allow common ones (like `cargo build`) for the whole session.

## Good to know

- Building the first time takes a few minutes (it compiles all the
  libraries). After that it's quick.
- Nothing here changes how your PC or games behave. The tools only matter
  when building GloorpFlorp.
- To remove everything later: uninstall Git, CMake, Rustup and
  "Visual Studio Build Tools 2022" from Settings → Apps, and delete the
  `GloorpFlorp` folder.
