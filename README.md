# warp-grid

Keyboard-driven mouse teleporter for Wayland (Niri, Sway, Hyprland, etc.).

Splits the screen into a 3×3 grid. Press one key to jump to a coarse zone,
press another key to land precisely within it. Two keystrokes → any pixel.

```
┌─────┬─────┬─────┐
│  U  │  I  │  O  │
├─────┼─────┼─────┤
│  J  │  K  │  L  │
├─────┼─────┼─────┤
│  N  │  M  │  ,  │
└─────┴─────┴─────┘
  Level 1: full screen
  Level 2: inside chosen zone (same keys)
```

**Controls**
| Key | Action |
|-----|--------|
| `U I O / J K L / N M ,` | Select grid cell |
| `Space` or `Enter` | Confirm current zone centre (skip level 2) |
| `Escape` or `Q` | Cancel |

---

## Requirements

- Rust toolchain (`rustup` — https://rustup.rs)
- A Wayland compositor that supports:
  - `wlr-layer-shell-v1`   (Niri ✓, Sway ✓, Hyprland ✓)
  - `zwlr-virtual-pointer-v1`  (Niri ✓, Sway ✓, Hyprland ✓)
- `libxkbcommon-dev` (usually already installed)

```
# Debian/Ubuntu
sudo apt install libxkbcommon-dev

# Arch
sudo pacman -S libxkbcommon

# Fedora
sudo dnf install libxkbcommon-devel
```

---

## Build

```bash
git clone <this repo>
cd warp-grid
cargo build --release
# Binary: ./target/release/warp-grid
```

Install to PATH:
```bash
sudo install -m755 target/release/warp-grid /usr/local/bin/
# or without sudo:
cp target/release/warp-grid ~/.local/bin/
```

---

## Niri config (`~/.config/niri/config.kdl`)

```kdl
// Bind Super+Space to open warp-grid (move only)
binds {
    Super+Space { spawn "warp-grid"; }
    Super+Shift+Space { spawn "warp-grid" "--click"; }
}
```

Reload: `niri msg action reload-config`

---

## Usage flow

1. Press `Super+Space`  → overlay appears, full screen divided into 9 zones
2. Press e.g. `I`       → top-centre zone selected; overlay zooms into it
3. Press e.g. `M`       → cursor jumps to the centre-bottom of that zone
4. Overlay closes automatically

For `--click` mode the cursor moves **and** left-clicks in one go.

---

## Troubleshooting

**"layer shell not available"** — your compositor doesn't support
`wlr-layer-shell-v1`. All mainstream wlroots-based compositors do; KDE/GNOME
may not.

**"virtual pointer not available"** — your compositor doesn't support
`zwlr-virtual-pointer-v1`. The overlay will still open and close but the
cursor won't move. File a bug with your compositor.

**Overlay opens but no keys work** — make sure no other tool (e.g. your
keyboard mouse mode) is grabbing the keyboard at the same time. Disable it
before triggering warp-grid.

---

## How it works

- Opens a **fullscreen `wlr-layer-shell` overlay** at the `Overlay` layer
  with `KeyboardInteractivity::Exclusive` — this grabs all key input while
  it is open, so no keys accidentally fire in the app behind it.
- Draws the grid using **`tiny-skia`** into a Wayland shared-memory buffer
  (no GPU required, no system graphics libraries beyond libxkbcommon).
- On final selection, destroys the surface and emits a
  **`zwlr_virtual_pointer_v1.motion_absolute`** request with the target
  coordinates normalised to `u32::MAX`. The compositor maps this to the
  correct pixel on whatever output the pointer is on.
- For `--click` it follows up with `button` press+release for `BTN_LEFT`.
