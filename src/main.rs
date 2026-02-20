// warp-grid — keyboard-driven mouse teleporter for Wayland / Niri
//
// Usage
// -----
//   warp-grid          → open overlay, move cursor on selection
//   warp-grid --click  → same, but left-click after final selection
//
// Key layout (two levels, 3×3 each)
// ------------------------------------
//   Level 1 – coarse grid (divides full screen into 9 zones)
//   Level 2 – fine grid  (divides the chosen zone into 9 sub-zones)
//
//   Physical keys      Grid position
//   ─────────────────────────────────
//   U  I  O           [0,0] [0,1] [0,2]   (top row)
//   J  K  L           [1,0] [1,1] [1,2]   (mid row)
//   M  ,  .           [2,0] [2,1] [2,2]   (bot row)
//
//   Escape / Q / Ctrl+C  → cancel
//   Space / Enter → confirm current centre (after level 1 only)

use std::env;

use calloop::EventLoop;
use calloop_wayland_source::WaylandSource;

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_registry,
    delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RepeatInfo},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{
        slot::{Buffer, SlotPool},
        Shm, ShmHandler,
    },
};

use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer::ButtonState, wl_seat, wl_shm, wl_surface},
    Connection, QueueHandle,
};

use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

// ─── grid constants ──────────────────────────────────────────────────────────

const COLS: u32 = 3;
const ROWS: u32 = 3;

/// UIO / JKL / N M ,   →  row-major order (top-left first)
const GRID_KEYS: [Keysym; 9] = [
    Keysym::u, Keysym::i, Keysym::o,
    Keysym::j, Keysym::k, Keysym::l,
    Keysym::m, Keysym::comma, Keysym::period,
];

// ─── rendering helpers ───────────────────────────────────────────────────────

fn cell_label(idx: usize) -> &'static str {
    ["U", "I", "O", "J", "K", "L", "M", ",", "."][idx]
}

/// ARGB8888 colour constants
const OVERLAY_BG:   u32 = 0x55_00_00_00; // semi-transparent black
const GRID_LINE:    u32 = 0xAA_FF_FF_FF; // white grid lines
const CELL_HOVER:   u32 = 0x22_FF_80_00; // subtle blue tint on active zone
const THIRD_STAGE:  u32 = 0x22_00_00_FF;
const TEXT_COLOUR:  u32 = 0xFF_FF_FF_FF; // white labels

fn argb_to_skia(argb: u32) -> tiny_skia::Color {
    let a = ((argb >> 24) & 0xFF) as f32 / 255.0;
    let r = ((argb >> 16) & 0xFF) as f32 / 255.0;
    let g = ((argb >>  8) & 0xFF) as f32 / 255.0;
    let b = ( argb        & 0xFF) as f32 / 255.0;
    tiny_skia::Color::from_rgba(r, g, b, a).unwrap()
}

// ─── app state ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
enum WarpLevel {
    /// Waiting for coarse (level-1) key press
    Coarse,
    /// Coarse zone chosen; waiting for fine (level-2) key press
    Fine { zone_row: u32, zone_col: u32 },
    Finer { zone_row: u32, zone_col: u32, sub_row: u32, sub_col: u32 },
}

struct App {
    // --- wl infrastructure ---
    registry_state: RegistryState,
    seat_state:     SeatState,
    output_state:   OutputState,
    compositor:     CompositorState,
    layer_shell:    LayerShell,
    shm:            Shm,

    // --- virtual pointer (optional – only present when compositor supports it) ---
    vp_manager: Option<ZwlrVirtualPointerManagerV1>,
    vp:         Option<ZwlrVirtualPointerV1>,

    // --- overlay surface ---
    layer:          Option<LayerSurface>,
    pool:           Option<SlotPool>,
    buffer:         Option<Buffer>,
    width:          u32,
    height:         u32,

    // --- keyboard ---
    keyboard:       Option<wl_keyboard::WlKeyboard>,
    modifiers:      Modifiers,

    // --- output tracking (for multi-monitor-accurate warping) ---
    current_output: Option<wl_output::WlOutput>,

    // --- warp logic ---
    level:   WarpLevel,
    click:   bool,   // --click flag
    exit:    bool,
    action:  Option<WarpAction>,
}

#[derive(Debug, Clone, Copy)]
struct WarpAction {
    x: f64,
    y: f64,
    click: bool,
}

impl App {
    fn new(
        registry_state: RegistryState,
        seat_state: SeatState,
        output_state: OutputState,
        compositor: CompositorState,
        layer_shell: LayerShell,
        shm: Shm,
        vp_manager: Option<ZwlrVirtualPointerManagerV1>,
        click: bool,
    ) -> Self {
        Self {
            registry_state,
            seat_state,
            output_state,
            compositor,
            layer_shell,
            shm,
            vp_manager,
            vp: None,
            layer: None,
            pool: None,
            buffer: None,
            width: 1,
            height: 1,
            keyboard: None,
            modifiers: Modifiers::default(),
            current_output: None,
            level: WarpLevel::Coarse,
            click,
            exit: false,
            action: None,
        }
    }

    fn ensure_virtual_pointer(&mut self, qh: &QueueHandle<Self>) {
        if self.vp.is_some() {
            return;
        }

        let Some(mgr) = self.vp_manager.as_ref() else {
            return;
        };

        let vp = if let Some(output) = self.current_output.as_ref() {
            mgr.create_virtual_pointer_with_output(None, Some(output), qh, ())
        } else {
            mgr.create_virtual_pointer(None, qh, ())
        };

        self.vp = Some(vp);
    }

    // Create the fullscreen layer surface
    fn create_overlay(&mut self, qh: &QueueHandle<Self>) {
        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some("warp-grid"),
            None, // current output chosen by compositor
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        // size 0,0 = fill the output
        layer.set_size(0, 0);
        layer.commit();
        self.layer = Some(layer);
    }

    // Draw the grid overlay into an shm buffer
    fn draw(&mut self, _qh: &QueueHandle<Self>) {
        let w = self.width;
        let h = self.height;
        if w == 0 || h == 0 {
            return;
        }
        let stride   = w * 4;

        let pool = match self.pool.as_mut() {
            Some(p) => p,
            None => return,
        };

        let (buffer, canvas) = pool
            .create_buffer(w as i32, h as i32, stride as i32, wl_shm::Format::Argb8888)
            .expect("create buffer");

        // ------ build a tiny-skia pixmap over the same memory ----------------
        let mut pixmap =
            tiny_skia::PixmapMut::from_bytes(canvas, w, h).expect("pixmap");

        // 1. Semi-transparent background
        pixmap.fill(argb_to_skia(OVERLAY_BG));

        let cell_w = w as f32 / COLS as f32;
        let cell_h = h as f32 / ROWS as f32;

        // 2. Highlight the active zone (levels 2 and 3)
        if let WarpLevel::Coarse = self.level {
            // nothing extra
        } else {
            let (zone_row, zone_col) = match self.level {
                WarpLevel::Fine { zone_row, zone_col } => (zone_row, zone_col),
                WarpLevel::Finer { zone_row, zone_col, .. } => (zone_row, zone_col),
                WarpLevel::Coarse => unreachable!(),
            };

            let rx = zone_col as f32 * cell_w;
            let ry = zone_row as f32 * cell_h;

            // Second level grid and labels: only when we're exactly in Fine level.
            if let WarpLevel::Fine { .. } = self.level {
                // subtle tint over the active coarse cell
                if let Some(rect) = tiny_skia::Rect::from_xywh(rx, ry, cell_w, cell_h) {
                    let mut paint = tiny_skia::Paint::default();
                    paint.set_color(argb_to_skia(CELL_HOVER));
                    pixmap.fill_rect(rect, &paint, tiny_skia::Transform::identity(), None);
                }

                let sub_w = cell_w / COLS as f32;
                let sub_h = cell_h / ROWS as f32;
                let mut stroke_paint = tiny_skia::Paint::default();
                stroke_paint.set_color(argb_to_skia(GRID_LINE));
                let mut stroke = tiny_skia::Stroke::default();
                stroke.width = 1.0;

                for col in 0..=COLS {
                    let x = rx + col as f32 * sub_w;
                    let mut pb = tiny_skia::PathBuilder::new();
                    pb.move_to(x, ry);
                    pb.line_to(x, ry + cell_h);
                    if let Some(p) = pb.finish() {
                        pixmap.stroke_path(&p, &stroke_paint, &stroke,
                            tiny_skia::Transform::identity(), None);
                    }
                }
                for row in 0..=ROWS {
                    let y = ry + row as f32 * sub_h;
                    let mut pb = tiny_skia::PathBuilder::new();
                    pb.move_to(rx, y);
                    pb.line_to(rx + cell_w, y);
                    if let Some(p) = pb.finish() {
                        pixmap.stroke_path(&p, &stroke_paint, &stroke,
                            tiny_skia::Transform::identity(), None);
                    }
                }

                // key labels in sub-cells
                let mut idx = 0usize;
                for r in 0..ROWS {
                    for c in 0..COLS {
                        let lx = rx + c as f32 * sub_w + sub_w * 0.5;
                        let ly = ry + r as f32 * sub_h + sub_h * 0.5;
                        draw_label(&mut pixmap, cell_label(idx), lx, ly, 18.0);
                        idx += 1;
                    }
                }
            }

            // 3rd level: if we're in the finest level, subdivide the chosen sub-cell again
            if let WarpLevel::Finer { zone_row, zone_col, sub_row, sub_col } = self.level {
                let rx = zone_col as f32 * cell_w;
                let ry = zone_row as f32 * cell_h;

                let sub_w = cell_w / COLS as f32;
                let sub_h = cell_h / ROWS as f32;

                let sub_rx = rx + sub_col as f32 * sub_w;
                let sub_ry = ry + sub_row as f32 * sub_h;

                // subtle tint over the active fine cell region
                if let Some(rect) = tiny_skia::Rect::from_xywh(sub_rx, sub_ry, sub_w, sub_h) {
                    let mut paint = tiny_skia::Paint::default();
                    paint.set_color(argb_to_skia(THIRD_STAGE));
                    pixmap.fill_rect(rect, &paint, tiny_skia::Transform::identity(), None);
                }

                let sub2_w = sub_w / COLS as f32;
                let sub2_h = sub_h / ROWS as f32;

                let mut stroke_paint = tiny_skia::Paint::default();
                stroke_paint.set_color(argb_to_skia(GRID_LINE));
                let mut stroke = tiny_skia::Stroke::default();
                stroke.width = 1.0;

                for col in 0..=COLS {
                    let x = sub_rx + col as f32 * sub2_w;
                    let mut pb = tiny_skia::PathBuilder::new();
                    pb.move_to(x, sub_ry);
                    pb.line_to(x, sub_ry + sub_h);
                    if let Some(p) = pb.finish() {
                        pixmap.stroke_path(&p, &stroke_paint, &stroke,
                            tiny_skia::Transform::identity(), None);
                    }
                }
                for row in 0..=ROWS {
                    let y = sub_ry + row as f32 * sub2_h;
                    let mut pb = tiny_skia::PathBuilder::new();
                    pb.move_to(sub_rx, y);
                    pb.line_to(sub_rx + sub_w, y);
                    if let Some(p) = pb.finish() {
                        pixmap.stroke_path(&p, &stroke_paint, &stroke,
                            tiny_skia::Transform::identity(), None);
                    }
                }

                // key labels in deepest sub-cells
                let mut idx = 0usize;
                for r in 0..ROWS {
                    for c in 0..COLS {
                        let lx = sub_rx + c as f32 * sub2_w + sub2_w * 0.5;
                        let ly = sub_ry + r as f32 * sub2_h + sub2_h * 0.5;
                        draw_label(&mut pixmap, cell_label(idx), lx, ly, 18.0);
                        idx += 1;
                    }
                }
            }
        }

        // 3. Coarse grid lines (always)
        if let WarpLevel::Coarse = self.level {
            let mut paint = tiny_skia::Paint::default();
            paint.set_color(argb_to_skia(GRID_LINE));
            let mut stroke = tiny_skia::Stroke::default();
            stroke.width = 2.0;

            for col in 0..=COLS {
                let x = col as f32 * cell_w;
                let mut pb = tiny_skia::PathBuilder::new();
                pb.move_to(x, 0.0);
                pb.line_to(x, h as f32);
                if let Some(p) = pb.finish() {
                    pixmap.stroke_path(&p, &paint, &stroke,
                        tiny_skia::Transform::identity(), None);
                }
            }
            for row in 0..=ROWS {
                let y = row as f32 * cell_h;
                let mut pb = tiny_skia::PathBuilder::new();
                pb.move_to(0.0, y);
                pb.line_to(w as f32, y);
                if let Some(p) = pb.finish() {
                    pixmap.stroke_path(&p, &paint, &stroke,
                        tiny_skia::Transform::identity(), None);
                }
            }
        }

        // 4. Key labels in coarse cells (level 1 mode)
        if self.level == WarpLevel::Coarse {
            let mut idx = 0usize;
            for r in 0..ROWS {
                for c in 0..COLS {
                    let lx = (c as f32 + 0.5) * cell_w;
                    let ly = (r as f32 + 0.5) * cell_h;
                    draw_label(&mut pixmap, cell_label(idx), lx, ly, 28.0);
                    idx += 1;
                }
            }
        }

        drop(pixmap); // release borrow of canvas

        // attach buffer
        if let Some(layer) = &self.layer {
            layer.wl_surface().attach(Some(buffer.wl_buffer()), 0, 0);
            layer.wl_surface().damage_buffer(0, 0, w as i32, h as i32);
            layer.wl_surface().commit();
        }
        self.buffer = Some(buffer);
    }

    /// Handle a grid key press, return whether we should exit
    fn handle_grid_key(&mut self, sym: Keysym) -> bool {
        let idx = match GRID_KEYS.iter().position(|&k| k == sym) {
            Some(i) => i,
            None    => return false,
        };
        let row = (idx / COLS as usize) as u32;
        let col = (idx % COLS as usize) as u32;

        let w = self.width  as f64;
        let h = self.height as f64;
        let cell_w = w / COLS as f64;
        let cell_h = h / ROWS as f64;

        match self.level {
            WarpLevel::Coarse => {
                self.level = WarpLevel::Fine { zone_row: row, zone_col: col };
                false // stay open for fine selection
            }
            WarpLevel::Fine { zone_row, zone_col } => {
                self.level = WarpLevel::Finer {
                    zone_row,
                    zone_col,
                    sub_row: row,
                    sub_col: col,
                };
                false // stay open for third-level selection
            }
            WarpLevel::Finer { zone_row, zone_col, sub_row, sub_col } => {
                // Compute final pixel coordinate: centre of third-level sub-cell
                let sub_w = cell_w / COLS as f64;
                let sub_h = cell_h / ROWS as f64;
                let sub2_w = sub_w / COLS as f64;
                let sub2_h = sub_h / ROWS as f64;

                let x = zone_col as f64 * cell_w
                    + sub_col as f64 * sub_w
                    + col as f64 * sub2_w
                    + sub2_w * 0.5;
                let y = zone_row as f64 * cell_h
                    + sub_row as f64 * sub_h
                    + row as f64 * sub2_h
                    + sub2_h * 0.5;

                self.action = Some(WarpAction { x, y, click: self.click });
                true
            }
        }
    }

    /// Move (and optionally click) using zwlr_virtual_pointer
    fn execute_action(&self, action: WarpAction) {
        let vp = match self.vp.as_ref() {
            Some(v) => v,
            None => {
                eprintln!("warp-grid: no virtual pointer available");
                return;
            }
        };

        // motion_absolute(time, x, y, x_extent, y_extent)
        // x,y are in coordinate space [0, extent), so pass pixel coords + screen dims
        vp.motion_absolute(0, action.x as u32, action.y as u32, self.width, self.height);
        vp.frame();

        if action.click {
            // BTN_LEFT = 0x110
            vp.button(0, 0x110, ButtonState::Pressed);
            vp.frame();
            vp.button(0, 0x110, ButtonState::Released);
            vp.frame();
        }
    }
}

// ─── tiny label renderer (bitmapped 5×7 font, no system font dep) ────────────

/// Draw a single ASCII character as a tiny rectangle outline + inner fill.
/// For a real font you'd bring in `fontdue` or `ab_glyph`; for labels of
/// 1–2 chars a hand-rolled approach is fine.
fn draw_label(pixmap: &mut tiny_skia::PixmapMut, label: &str, cx: f32, cy: f32, size: f32) {
    let half = size * 0.5;
    let rect = tiny_skia::Rect::from_xywh(cx - half, cy - half, size, size);
    if rect.is_none() {
        return;
    }
    let rect = rect.unwrap();

    // Filled semi-transparent background box
    let mut bg = tiny_skia::Paint::default();
    bg.set_color(tiny_skia::Color::from_rgba(0.0, 0.0, 0.0, 0.55).unwrap());
    pixmap.fill_rect(rect, &bg, tiny_skia::Transform::identity(), None);

    // Border
    let mut border_paint = tiny_skia::Paint::default();
    border_paint.set_color(argb_to_skia(TEXT_COLOUR));
    let mut stroke = tiny_skia::Stroke::default();
    stroke.width = 1.5;
    let mut pb = tiny_skia::PathBuilder::new();
    pb.push_rect(rect);
    if let Some(path) = pb.finish() {
        pixmap.stroke_path(&path, &border_paint, &stroke,
            tiny_skia::Transform::identity(), None);
    }

    // Draw the letter character using a very small pixel-dot approach:
    // We render each character as a cluster of filled squares on a 5×5 grid.
    let char_bytes: &[u8] = label.as_bytes();
    if char_bytes.is_empty() {
        return;
    }
    let ch = char_bytes[0] as char;
    let dots = char_to_dots(ch);
    if dots.is_empty() {
        return;
    }

    let dot_size = size / 6.0;
    let origin_x = cx - dot_size * 2.5;
    let origin_y = cy - dot_size * 2.5;

    let mut dot_paint = tiny_skia::Paint::default();
    dot_paint.set_color(argb_to_skia(TEXT_COLOUR));

    for &(dr, dc) in dots {
        let dx = origin_x + dc as f32 * dot_size;
        let dy = origin_y + dr as f32 * dot_size;
        if let Some(r) = tiny_skia::Rect::from_xywh(dx + 0.5, dy + 0.5,
                                                      dot_size - 1.0, dot_size - 1.0) {
            pixmap.fill_rect(r, &dot_paint, tiny_skia::Transform::identity(), None);
        }
    }
}

/// Returns a list of (row, col) dot positions (0..5 each) for a given char.
/// We only need U I O J K L N M ,
fn char_to_dots(c: char) -> &'static [(u8, u8)] {
    match c {
        'U' => &[(0,0),(0,4),(1,0),(1,4),(2,0),(2,4),(3,0),(3,4),(4,1),(4,2),(4,3)],
        'I' => &[(0,1),(0,2),(0,3),(1,2),(2,2),(3,2),(4,1),(4,2),(4,3)],
        'O' => &[(0,1),(0,2),(0,3),(1,0),(1,4),(2,0),(2,4),(3,0),(3,4),(4,1),(4,2),(4,3)],
        'J' => &[(0,2),(0,3),(0,4),(1,3),(2,3),(3,0),(3,3),(4,1),(4,2)],
        'K' => &[(0,0),(0,3),(1,0),(1,2),(2,0),(2,1),(3,0),(3,2),(4,0),(4,3)],
        'L' => &[(0,0),(1,0),(2,0),(3,0),(4,0),(4,1),(4,2),(4,3)],
        'N' => &[(0,0),(0,4),(1,0),(1,1),(1,4),(2,0),(2,2),(2,4),(3,0),(3,3),(3,4),(4,0),(4,4)],
        'M' => &[(0,0),(0,4),(1,0),(1,1),(1,3),(1,4),(2,0),(2,2),(2,4),(3,0),(3,4),(4,0),(4,4)],
        ',' => &[(3,2),(4,2),(4,1)],
        '.' => &[(2,2)],
        _   => &[],
    }
}

// ─── SCTK trait impls ────────────────────────────────────────────────────────

impl CompositorHandler for App {
    fn scale_factor_changed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface, _new_factor: i32) {}
    fn transform_changed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface, _new_transform: wl_output::Transform) {}
    fn frame(&mut self, _conn: &Connection, qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface, _time: u32) {
        self.draw(qh);
    }
    fn surface_enter(&mut self, _conn: &Connection, qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface, output: &wl_output::WlOutput) {
        self.current_output = Some(output.clone());

        // Rebind virtual pointer to the output where the overlay currently lives.
        if let Some(vp) = self.vp.take() {
            vp.destroy();
        }
        self.ensure_virtual_pointer(qh);
    }
    fn surface_leave(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface, _output: &wl_output::WlOutput) {}
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState { &mut self.output_state }
    fn new_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput) {}
    fn update_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput) {}
}

impl LayerShellHandler for App {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.exit = true;
    }

    fn configure(&mut self, _conn: &Connection, qh: &QueueHandle<Self>,
        _layer: &LayerSurface, configure: LayerSurfaceConfigure, _serial: u32) {
        let new_w = configure.new_size.0;
        let new_h = configure.new_size.1;

        if new_w == 0 || new_h == 0 {
            // compositor hasn't told us the size yet — use a sane default
            // and wait for the real configure
            return;
        }

        let resized = self.width != new_w || self.height != new_h;
        self.width  = new_w;
        self.height = new_h;

        if resized || self.pool.is_none() {
            let pool_size = (new_w * new_h * 4 * 2) as usize; // double-buffer
            self.pool = Some(
                SlotPool::new(pool_size, &self.shm).expect("create shm pool")
            );
        }

        // Create virtual pointer once we know dimensions/output context.
        self.ensure_virtual_pointer(qh);

        self.draw(qh);
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState { &mut self.seat_state }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}

    fn new_capability(&mut self, _conn: &Connection, qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat, capability: Capability) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            let keyboard = self.seat_state.get_keyboard(qh, &seat, None)
                .expect("get keyboard");
            self.keyboard = Some(keyboard);
        }
    }

    fn remove_capability(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat, capability: Capability) {
        if capability == Capability::Keyboard {
            if let Some(kbd) = self.keyboard.take() {
                kbd.release();
            }
        }
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat) {}
}

impl KeyboardHandler for App {
    fn enter(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard, _surface: &wl_surface::WlSurface,
        _serial: u32, _raw: &[u32], _keysyms: &[Keysym]) {}

    fn leave(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard, _surface: &wl_surface::WlSurface,
        _serial: u32) {}

    fn press_key(&mut self, _conn: &Connection, qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard, _serial: u32, event: KeyEvent) {
        let sym = event.keysym;

        // Cancel keys
        if sym == Keysym::Escape || sym == Keysym::q || sym == Keysym::Q
            || (self.modifiers.ctrl && (sym == Keysym::c || sym == Keysym::C))
        {
            self.exit = true;
            return;
        }

        // Space/Enter = confirm current cursor position (cancel after level 1)
        if sym == Keysym::space || sym == Keysym::Return {
            if let WarpLevel::Fine { zone_row, zone_col } = self.level {
                // Confirm centre of current zone without fine selection
                let w = self.width  as f64;
                let h = self.height as f64;
                let cell_w = w / COLS as f64;
                let cell_h = h / ROWS as f64;
                let x = zone_col as f64 * cell_w + cell_w * 0.5;
                let y = zone_row as f64 * cell_h + cell_h * 0.5;
                self.action = Some(WarpAction { x, y, click: self.click });
                self.exit = true;
            }
            return;
        }

        // Grid navigation
        let done = self.handle_grid_key(sym);
        if done {
            self.exit = true;
        } else {
            // Redraw with updated level/zone highlight
            self.draw(qh);
        }
    }

    fn release_key(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard, _serial: u32, _event: KeyEvent) {}

    fn update_modifiers(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard, _serial: u32,
        modifiers: Modifiers, _layout: u32) {
        self.modifiers = modifiers;
    }

    fn update_repeat_info(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard, _info: RepeatInfo) {}
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm { &mut self.shm }
}

// Delegate macros wire SCTK dispatch boilerplate
delegate_compositor!(App);
delegate_output!(App);
delegate_layer!(App);
delegate_seat!(App);
delegate_keyboard!(App);
delegate_shm!(App);
delegate_registry!(App);

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState { &mut self.registry_state }
    registry_handlers![OutputState, SeatState];
}

// Virtual pointer has no events – empty dispatch impl
impl wayland_client::Dispatch<ZwlrVirtualPointerManagerV1, ()> for App {
    fn event(_state: &mut Self, _proxy: &ZwlrVirtualPointerManagerV1,
        _event: <ZwlrVirtualPointerManagerV1 as wayland_client::Proxy>::Event,
        _data: &(), _conn: &Connection, _qh: &QueueHandle<Self>) {}
}
impl wayland_client::Dispatch<ZwlrVirtualPointerV1, ()> for App {
    fn event(_state: &mut Self, _proxy: &ZwlrVirtualPointerV1,
        _event: <ZwlrVirtualPointerV1 as wayland_client::Proxy>::Event,
        _data: &(), _conn: &Connection, _qh: &QueueHandle<Self>) {}
}

// ─── main ────────────────────────────────────────────────────────────────────

fn main() {
    let click = env::args().any(|a| a == "--click");

    let conn = Connection::connect_to_env().expect("connect to Wayland");
    let (globals, event_queue) = registry_queue_init::<App>(&conn)
        .expect("registry queue init");
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh).expect("compositor");
    let layer_shell = LayerShell::bind(&globals, &qh).expect("layer shell – is compositor wlr-layer-shell-v1 compatible?");
    let shm         = Shm::bind(&globals, &qh).expect("shm");
    let seat_state  = SeatState::new(&globals, &qh);
    let output_state = OutputState::new(&globals, &qh);
    let registry_state = RegistryState::new(&globals);

    // Try to bind virtual pointer manager (optional)
    let vp_manager: Option<ZwlrVirtualPointerManagerV1> =
        globals.bind(&qh, 1..=2, ()).ok();

    if vp_manager.is_none() {
        eprintln!("warp-grid: zwlr_virtual_pointer_manager_v1 not available – cursor won't move");
    }

    let mut app = App::new(
        registry_state, seat_state, output_state,
        compositor, layer_shell, shm, vp_manager, click,
    );

    // Create the overlay surface
    app.create_overlay(&qh);

    // Run event loop
    let mut event_loop: EventLoop<App> = EventLoop::try_new().expect("event loop");
    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .expect("insert wayland source");

    loop {
        event_loop
            .dispatch(None, &mut app)
            .expect("dispatch");

        if app.exit {
            break;
        }
    }

    // Execute the warp action AFTER the overlay is gone (surface destroyed on drop)
    drop(app.layer.take());

    // Flush remaining Wayland requests (surface destruction + pointer motion)
    // We need a final roundtrip via the raw connection. Re-use the loop for one tick.
    if let Some(action) = app.action {
        app.execute_action(action);

        // One more dispatch so the motion_absolute request is flushed
        event_loop
            .dispatch(Some(std::time::Duration::from_millis(16)), &mut app)
            .ok();
    }
}
