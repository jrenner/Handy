# Handy on Linux / Wayland (GNOME) — Setup Guide

This guide sets up Handy push-to-talk speech-to-text on a Linux machine running
a **Wayland** session, with notes specific to **GNOME** (Mutter). It is the
process used to get the `wayland-ptt-gnome` branch working end to end.

> Tested on Ubuntu 25.10, GNOME Shell 49, Wayland, AMD GPU (Vulkan).
> KDE/wlroots compositors are noted where behavior differs.

## Why a special guide?

Three Wayland realities require setup that the upstream app doesn't handle out
of the box on GNOME:

1. **Global hotkeys** can't be grabbed directly on Wayland. Handy uses the **XDG
   Desktop Portal** `GlobalShortcuts` backend instead. This branch fixes the
   portal path (accepts portal v1, registers the app id so GNOME accepts the
   bind, tolerates more key bindings).
2. **Typing the transcribed text** can't use `wtype` on GNOME — Mutter doesn't
   implement the virtual-keyboard protocol (`zwp_virtual_keyboard_manager_v1`),
   same as KDE's KWin. This branch routes injection through **`ydotool`**, which
   needs the **`ydotoold`** daemon and `/dev/uinput` access.
3. **The recording overlay** would otherwise steal keyboard focus (no
   layer-shell on GNOME). On Linux the overlay defaults to **off**, which avoids
   this.

---

## 1. Prerequisites

Install build tooling:

- [Rust](https://rustup.rs/) (latest stable)
- [Bun](https://bun.sh/)

Install system build dependencies (from `BUILD.md`):

```bash
# Ubuntu/Debian
sudo apt update
sudo apt install build-essential libasound2-dev pkg-config libssl-dev \
  libvulkan-dev vulkan-tools glslc libgtk-3-dev libwebkit2gtk-4.1-dev \
  libayatana-appindicator3-dev librsvg2-dev libgtk-layer-shell0 \
  libgtk-layer-shell-dev patchelf cmake
```

(See `BUILD.md` for Fedora/Arch package lists.)

Install the text-injection tools (the part specific to GNOME Wayland):

```bash
sudo apt install -y ydotool ydotoold
```

---

## 2. Clone the fork and check out the branch

```bash
git clone git@github.com:jrenner/Handy.git
cd Handy
git checkout wayland-ptt-gnome
bun install
```

## 3. Download the VAD model (required)

```bash
mkdir -p src-tauri/resources/models
curl -o src-tauri/resources/models/silero_vad_v4.onnx \
  https://blob.handy.computer/silero_vad_v4.onnx
```

## 4. Build a release binary

```bash
bun run tauri build --no-bundle
```

This produces a self-contained binary plus its resources:

- binary: `src-tauri/target/release/handy`
- resources (VAD model, icons, sounds): `src-tauri/target/release/resources/`

> Keep the binary and its `resources/` directory together. Running the binary
> **in place** from `target/release/` is the simplest option — Tauri finds its
> resources there. Do not move just the binary elsewhere (its resource path
> resolution expects an installed `…/lib/handy` layout). If you want a real
> system install instead, build the `.deb` (`bun run tauri build`) and install
> it per `BUILD.md`.

---

## 5. Set up `ydotoold` (text injection daemon)

`ydotool` injects keystrokes through the kernel's `/dev/uinput`. It needs the
`ydotoold` daemon running and write access to `/dev/uinput`.

### 5a. Give your user access to `/dev/uinput`

On Ubuntu, `/dev/uinput` is owned `root:input` and group-writable, so just add
yourself to the `input` group, then **log out and back in**:

```bash
sudo usermod -aG input "$USER"
# log out / log back in, then verify:
id -nG | tr ' ' '\n' | grep -x input
```

> If `/dev/uinput` is not group-writable on your distro, add a udev rule:
> `echo 'KERNEL=="uinput", GROUP="input", MODE="0660"' | sudo tee /etc/udev/rules.d/99-uinput.rules`
> then `sudo udevadm control --reload && sudo udevadm trigger`.

### 5b. Run `ydotoold` as a systemd user service

```bash
mkdir -p ~/.config/systemd/user
cat > ~/.config/systemd/user/ydotoold.service <<'EOF'
[Unit]
Description=ydotoold virtual input daemon (for Handy text injection on Wayland)
After=graphical-session.target

[Service]
ExecStart=/usr/bin/ydotoold
Restart=on-failure
RestartSec=2

[Install]
WantedBy=default.target
EOF

systemctl --user daemon-reload
systemctl --user enable --now ydotoold.service
```

Verify it's up and `ydotool` can reach it (should print
`Using ydotoold backend`, not `backend unavailable`):

```bash
systemctl --user is-active ydotoold     # -> active
ydotool type ""                          # -> ydotool: notice: Using ydotoold backend
```

The daemon listens on `/tmp/.ydotool_socket`, which is the default path
`ydotool` looks for — no environment variables needed.

---

## 6. Desktop launcher (Super → search → launch)

Create an application entry so the app appears in the GNOME app grid (press
**Super**, type "Handy"). Point it at the release binary and give it an icon.
This entry is **also** what the GlobalShortcuts portal uses to identify the app
(its filename must match the app id `com.pais.handy`).

```bash
REPO="$HOME/Handy"     # adjust to your clone path
cat > ~/.local/share/applications/com.pais.handy.desktop <<EOF
[Desktop Entry]
Type=Application
Name=Handy
Comment=Offline speech-to-text (push-to-talk)
Exec=$REPO/src-tauri/target/release/handy
Icon=$REPO/src-tauri/icons/128x128.png
Terminal=false
Categories=Utility;AudioVideo;Audio;
StartupWMClass=com.pais.handy
Keywords=speech;dictation;transcribe;voice;
EOF
update-desktop-database ~/.local/share/applications
```

> If you previously installed an AppImage/deb, remove its stale `.desktop` entry
> (e.g. `~/.local/share/applications/handy.desktop`) so searching "Handy"
> doesn't launch the old binary.

The app even auto-creates a minimal `com.pais.handy.desktop` on first run if one
doesn't exist (so the portal can identify it), but the version above is nicer
(visible in search, has an icon).

---

## 7. Autostart on login (optional)

Handy manages its own autostart entry when you enable autostart in its settings,
or you can create one explicitly:

```bash
cat > ~/.config/autostart/Handy.desktop <<EOF
[Desktop Entry]
Type=Application
Name=Handy
Exec=$HOME/Handy/src-tauri/target/release/handy
Terminal=false
StartupNotify=false
EOF
```

`ydotoold` autostarts via its systemd user service (step 5b).

---

## 8. First run & settings

```bash
./src-tauri/target/release/handy
```

On Linux the relevant defaults are already correct:

| Setting                   | Default on Linux | Why                                            |
| ------------------------- | ---------------- | ---------------------------------------------- |
| `keyboard_implementation` | `portal` (Wayland) | Only backend that can grab global keys on Wayland |
| `push_to_talk`            | `true`           | Hold the hotkey to talk                        |
| `overlay_position`        | `none`           | Avoids the focus-stealing overlay on GNOME     |

In the app's settings you can change the **Transcribe** hotkey (default
`Ctrl+Space`; modifier+key combos like `Ctrl+Alt+]` work). Pick a model in the
model selector (e.g. Parakeet) on first run.

Settings persist in `~/.local/share/com.pais.handy/settings_store.json`.

---

## 9. Verify end to end

1. Click into any text field.
2. Hold the hotkey, speak, release.
3. The transcribed text types into the field.

Watch logs while testing:

```bash
RUST_LOG=info ./src-tauri/target/release/handy
```

You should see, in order:

```
Registered app id 'com.pais.handy' with the desktop portal
XDG GlobalShortcuts bound 'transcribe' as 'Press <Control>...'
... Transcription result: <your words>
ydotool type stderr: ydotool: notice: Using ydotoold backend
```

---

## Troubleshooting

### Fixing the settings file

Settings live in `~/.local/share/com.pais.handy/settings_store.json` (keys are
nested under a top-level `"settings"` object). The three that matter on
GNOME/Wayland and their required values:

| Key                       | Must be  |
| ------------------------- | -------- |
| `keyboard_implementation` | `portal` |
| `overlay_position`        | `none`   |
| `push_to_talk`            | `true`   |

To reset them safely, **quit Handy first** (the app overwrites the file on
exit, so edits made while it's running are lost), then:

```bash
pkill -x handy
python3 - <<'PY'
import json
p = __import__("os").path.expanduser("~/.local/share/com.pais.handy/settings_store.json")
d = json.load(open(p))
d["settings"]["keyboard_implementation"] = "portal"
d["settings"]["overlay_position"] = "none"
d["settings"]["push_to_talk"] = True
json.dump(d, open(p, "w"), indent=1)
print("fixed:", {k: d["settings"][k] for k in
      ("keyboard_implementation", "overlay_position", "push_to_talk")})
PY
```

Then start Handy again. You can also change `keyboard_implementation` from the
app: enable debug mode (`Ctrl+Shift+D`) and pick **XDG Desktop Portal (Wayland)**
in the Keyboard Implementation selector.

> **Why `keyboard_implementation` reverts to `tauri`:** if the portal shortcut
> fails to initialize, Handy falls back to the (Wayland-incapable) `tauri`
> backend **and persists that choice** to settings — so it stays broken on the
> next launch until you set it back to `portal`. The most common trigger is
> **two instances running at once** (e.g. launching a second copy while one is
> already up) contending for the portal session.

> **Tip — avoid the revert:** launch Handy **only one way**. Let autostart (or
> the Super → "Handy" launcher) run a single instance, and don't start a second
> copy from a terminal while one is already running. The single-instance guard
> normally routes a second launch to the running instance, but starting several
> at once (as can happen during development/testing) can race the portal session
> and trip the fallback. If the hotkey suddenly stops working, suspect this
> first and re-check `keyboard_implementation` (above).

### Other issues

- **Hotkey does nothing / log says "Falling back to Tauri ... implementation"**
  The portal bind was rejected. First check `keyboard_implementation` is
  `portal` (see above). Then ensure `com.pais.handy.desktop` exists in
  `~/.local/share/applications/` (so GNOME can identify the app), and restart
  Handy. Check `journalctl --user -b | grep -i shortcut` for
  `invalid app_id` messages. A healthy startup logs (look in the journal for a
  GNOME-launched app, or run from a terminal with `RUST_LOG=info`):
  `Registered app id 'com.pais.handy' …` → `XDG GlobalShortcuts bound …` →
  `XDG GlobalShortcuts portal initialized`.

- **Words transcribe but no text appears**
  Text injection isn't reaching the focused window:
  - Confirm `ydotoold` is running: `systemctl --user is-active ydotoold`.
  - Confirm `ydotool type "x"` prints `Using ydotoold backend` (not
    `backend unavailable`). If unavailable, the daemon isn't running or you lack
    `/dev/uinput` access (step 5a).
  - Make sure the recording **overlay** is off (`overlay_position = none`). A
    visible overlay steals focus on GNOME and the text lands nowhere.

- **`ydotool: backend unavailable (may have latency+delay issues)`**
  The daemon isn't reachable. Daemonless `ydotool` does **not** work on GNOME
  (the virtual device is created/destroyed too fast). Start `ydotoold`.

- **KDE / wlroots note**
  On KDE, `kwtype` is preferred for typing; on wlroots compositors (Sway,
  Hyprland) `wtype` works and `ydotoold` isn't needed. Handy auto-selects.
