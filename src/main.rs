// Hides the console window on Windows in release builds
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use serde_json::{json, Value};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};
use walkers::{lon_lat, Map, MapMemory, PmTiles, Style};

const APP_NAME: &str = "ShurMap";

// ---------------------------------------------------------------------------
// Remembering the last opened map
// ---------------------------------------------------------------------------

/// Where the last opened map path is stored,
/// e.g. C:\Users\you\AppData\Roaming\ShurMap\last_file.txt
fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join(APP_NAME).join("last_file.txt"))
}

fn save_last_file(path: &Path) {
    if let Some(cfg) = config_path() {
        if let Some(dir) = cfg.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(cfg, path.to_string_lossy().as_bytes());
    }
}

/// Returns the last opened file, but only if it still exists.
fn load_last_file() -> Option<PathBuf> {
    let text = std::fs::read_to_string(config_path()?).ok()?;
    let path = PathBuf::from(text.trim());
    path.is_file().then_some(path)
}

// ---------------------------------------------------------------------------
// Map style: only Russian and English names
// ---------------------------------------------------------------------------

/// Every label that is built from a name becomes: name:ru, else name:en, else nothing.
/// Labels that don't use names (road numbers, elevations, ...) are left alone.
fn localize(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                if key == "text-field" && val.to_string().contains("name") {
                    *val = json!(["coalesce", ["get", "name:ru"], ["get", "name:en"]]);
                } else {
                    localize(val);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(localize),
        _ => {}
    }
}

fn localized_style() -> Style {
    let mut style_json: Value =
        serde_json::from_str(include_str!("../assets/protomaps-light.json"))
            .expect("bad style json");
    localize(&mut style_json);
    serde_json::from_value(style_json).expect("failed to build style")
}

// ---------------------------------------------------------------------------
// PMTiles header
// ---------------------------------------------------------------------------

struct FileInfo {
    center: (f64, f64), // lon, lat
    zoom: f64,
    max_zoom: f64,
}

/// Reads the fixed 127-byte PMTiles v3 header.
fn read_info(path: &Path) -> Option<FileInfo> {
    let mut file = File::open(path).ok()?;
    let mut b = [0u8; 127];
    file.read_exact(&mut b).ok()?;

    if &b[0..7] != b"PMTiles" || b[7] != 3 {
        return None;
    }

    let coord = |o: usize| i32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as f64 / 1e7;

    let (min_zoom, max_zoom) = (b[100] as f64, b[101] as f64);
    let (min_lon, min_lat, max_lon, max_lat) = (coord(102), coord(106), coord(110), coord(114));

    let lon_span = (max_lon - min_lon).max(0.001);
    let lat_span = (max_lat - min_lat).max(0.001);

    let zx = (900.0 * 360.0 / (1024.0 * lon_span)).log2();
    let zy = (600.0 * 360.0 / (1024.0 * lat_span)).log2();
    let zoom = zx.min(zy).clamp(min_zoom, max_zoom);

    Some(FileInfo {
        center: ((min_lon + max_lon) / 2.0, (min_lat + max_lat) / 2.0),
        zoom,
        max_zoom,
    })
}

// ---------------------------------------------------------------------------
// Zoom control (+ / −)
// ---------------------------------------------------------------------------

/// Rounded "+ / −" zoom control. Returns +1.0 (zoom in), -1.0 (zoom out) or 0.0.
fn zoom_control(ui: &mut egui::Ui) -> f64 {
    let btn = 48.0; // size of each button, change to make bigger/smaller
    let radius = 14.0;

    let (rect, _) = ui.allocate_exact_size(egui::vec2(btn, btn * 2.0), egui::Sense::hover());
    let plus_rect = egui::Rect::from_min_size(rect.min, egui::vec2(btn, btn));
    let minus_rect =
        egui::Rect::from_min_size(rect.min + egui::vec2(0.0, btn), egui::vec2(btn, btn));

    let plus = ui.interact(plus_rect, ui.id().with("zoom_plus"), egui::Sense::click());
    let minus = ui.interact(minus_rect, ui.id().with("zoom_minus"), egui::Sense::click());

    if plus.hovered() || minus.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    let painter = ui.painter();

    // Soft shadow, then the white pill
    painter.rect_filled(
        rect.translate(egui::vec2(0.0, 3.0)).expand(2.0),
        radius + 2.0,
        egui::Color32::from_black_alpha(35),
    );
    painter.rect_filled(rect, radius, egui::Color32::from_white_alpha(240));

    // Divider between the two buttons
    let mid = rect.center().y;
    painter.line_segment(
        [
            egui::pos2(rect.left() + 10.0, mid),
            egui::pos2(rect.right() - 10.0, mid),
        ],
        egui::Stroke::new(1.0, egui::Color32::from_gray(215)),
    );

    let idle = egui::Color32::from_rgb(60, 64, 67);
    let active = egui::Color32::from_rgb(26, 115, 232);
    let color_of = |r: &egui::Response| if r.hovered() { active } else { idle };
    let stroke_of = |r: &egui::Response| {
        egui::Stroke::new(
            if r.is_pointer_button_down_on() { 3.5 } else { 2.5 },
            color_of(r),
        )
    };

    let arm = 9.0; // half the length of the + / − lines

    // "+" exactly in the middle of its button
    let c = plus_rect.center();
    painter.line_segment(
        [c - egui::vec2(arm, 0.0), c + egui::vec2(arm, 0.0)],
        stroke_of(&plus),
    );
    painter.line_segment(
        [c - egui::vec2(0.0, arm), c + egui::vec2(0.0, arm)],
        stroke_of(&plus),
    );

    // "−" exactly in the middle of its button
    let c = minus_rect.center();
    painter.line_segment(
        [c - egui::vec2(arm, 0.0), c + egui::vec2(arm, 0.0)],
        stroke_of(&minus),
    );

    if plus.clicked() {
        1.0
    } else if minus.clicked() {
        -1.0
    } else {
        0.0
    }
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

struct ViewerApp {
    tiles: Option<PmTiles>,
    file: Option<PathBuf>,
    map_memory: MapMemory,
    center: (f64, f64), // lon, lat
    max_zoom: f64,
}

impl ViewerApp {
    fn new() -> Self {
        Self {
            tiles: None,
            file: None,
            map_memory: MapMemory::default(),
            center: (0.0, 0.0),
            max_zoom: 12.0,
        }
    }

    fn open(&mut self, path: PathBuf, ctx: egui::Context) {
        let info = read_info(&path).unwrap_or(FileInfo {
            center: (0.0, 0.0),
            zoom: 2.0,
            max_zoom: 12.0,
        });

        self.center = info.center;
        self.max_zoom = info.max_zoom;
        self.map_memory = MapMemory::default();
        let _ = self.map_memory.set_zoom(info.zoom);

        self.tiles = Some(PmTiles::with_style(&path, localized_style(), ctx));
        save_last_file(&path);
        self.file = Some(path);
    }

    fn change_zoom(&mut self, delta: f64) {
        let z = (self.map_memory.zoom() + delta).clamp(0.0, self.max_zoom);
        let _ = self.map_memory.set_zoom(z);
    }
}

impl eframe::App for ViewerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Toolbar: button centered at the top
        ui.vertical_centered(|ui| {
            if ui.button("Select map file…").clicked() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("PMTiles", &["pmtiles"])
                    .pick_file()
                {
                    self.open(path, ui.ctx().clone());
                }
            }

            if let Some(file) = &self.file {
                ui.small(
                    file.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                );
            }
        });
        ui.separator();

        // The area below the toolbar that the map fills
        let map_rect = ui.available_rect_before_wrap();

        match self.tiles.as_mut() {
            Some(tiles) => {
                ui.add(
                    Map::new(
                        Some(tiles),
                        &mut self.map_memory,
                        lon_lat(self.center.0, self.center.1),
                    )
                    // Mouse wheel zooms on its own (no Ctrl needed)
                    .zoom_with_ctrl(false)
                    // Left mouse button drags the map
                    .drag_pan_buttons(egui::DragPanButtons::PRIMARY),
                );

                // The file has no tiles beyond its max zoom
                if self.map_memory.zoom() > self.max_zoom {
                    let _ = self.map_memory.set_zoom(self.max_zoom);
                }

                // + / - control, vertically centered on the right side of the map
                let mut delta = 0.0;
                egui::Area::new(egui::Id::new("zoom_buttons"))
                    .pivot(egui::Align2::RIGHT_CENTER)
                    .fixed_pos(egui::pos2(map_rect.right() - 16.0, map_rect.center().y))
                    .show(ui.ctx(), |ui| {
                        delta = zoom_control(ui);
                    });
                if delta != 0.0 {
                    self.change_zoom(delta);
                }
            }
            None => {
                ui.centered_and_justified(|ui| {
                    ui.label("Click “Select map file…” and choose a .pmtiles file");
                });
            }
        }
    }
}

fn load_icon() -> egui::IconData {
    let img = image::load_from_memory(include_bytes!("../assets/icon.png"))
        .expect("bad icon")
        .into_rgba8();
    let (width, height) = img.dimensions();
    egui::IconData {
        rgba: img.into_raw(),
        width,
        height,
    }
}

fn main() -> eframe::Result<()> {
    env_logger::init();

    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_icon(load_icon())
            .with_inner_size([1200.0, 800.0]),
        ..Default::default()
    };

    eframe::run_native(
        APP_NAME, // also the window title
        options,
        Box::new(|cc| {
            // Required for vector tiles: sets up walkers' GPU renderer
            walkers::install_renderer(cc.wgpu_render_state.as_ref());

            let mut app = ViewerApp::new();
            if let Some(path) = load_last_file() {
                app.open(path, cc.egui_ctx.clone());
            }
            Ok(Box::new(app))
        }),
    )
}