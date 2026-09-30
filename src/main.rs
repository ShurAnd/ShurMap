// Скрывает окно консоли в релизной сборке под Windows
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod layers;
mod settings;

use eframe::egui;
use serde_json::{Value, json};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};
use walkers::{Map, MapMemory, PmTiles, Style, lon_lat};

const APP_NAME: &str = "ShurMap";

// ---------------------------------------------------------------------------
// Журнал запуска и сообщения об ошибках
// ---------------------------------------------------------------------------

/// Дописывает строку в %APPDATA%\ShurMap\startup.log, чтобы видеть, докуда дошёл запуск.
fn log_step(text: &str) {
    use std::io::Write;
    if let Some(dir) = settings::config_dir() {
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("startup.log"))
        {
            let _ = writeln!(f, "{text}");
        }
    }
}

fn show_error(text: &str) {
    let _ = rfd::MessageDialog::new()
        .set_title(APP_NAME)
        .set_level(rfd::MessageLevel::Error)
        .set_description(text)
        .show();
}

// ---------------------------------------------------------------------------
// Стиль карты: только русские и английские названия
// ---------------------------------------------------------------------------

/// Каждая подпись, построенная из названия, становится: name:ru, иначе name:en, иначе пусто.
/// Подписи без названий (номера дорог, высоты и т. п.) не меняются.
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
// Заголовок PMTiles
// ---------------------------------------------------------------------------

struct FileInfo {
    center: (f64, f64), // долгота, широта
    /// Границы данных: запад, юг, восток, север
    bounds: [f64; 4],
    min_zoom: f64,
    max_zoom: f64,
}

/// Читает фиксированный заголовок PMTiles v3 (127 байт).
fn read_info(path: &Path) -> Option<FileInfo> {
    let mut file = File::open(path).ok()?;
    let mut b = [0u8; 127];
    file.read_exact(&mut b).ok()?;

    if &b[0..7] != b"PMTiles" || b[7] != 3 {
        return None;
    }

    let coord = |o: usize| i32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as f64 / 1e7;

    let (min_lon, min_lat, max_lon, max_lat) = (coord(102), coord(106), coord(110), coord(114));

    Some(FileInfo {
        center: ((min_lon + max_lon) / 2.0, (min_lat + max_lat) / 2.0),
        bounds: [min_lon, min_lat, max_lon, max_lat],
        min_zoom: b[100] as f64,
        max_zoom: b[101] as f64,
    })
}

/// Сколько пикселей должен занимать весь мир по ширине, чтобы область данных `bounds`
/// (запад, юг, восток, север) закрывала окно `size` без пустых краёв.
fn cover_world_px(bounds: [f64; 4], size: (f32, f32)) -> f64 {
    let merc_y = |lat: f64| {
        let lat = lat.clamp(-85.0, 85.0).to_radians();
        (std::f64::consts::FRAC_PI_4 + lat / 2.0).tan().ln()
    };
    let x_share = ((bounds[2] - bounds[0]) / 360.0).max(1e-6);
    let y_share = ((merc_y(bounds[3]) - merc_y(bounds[1])) / std::f64::consts::TAU).max(1e-6);
    (size.0 as f64 / x_share).max(size.1 as f64 / y_share) * 1.01 // небольшой запас
}

// ---------------------------------------------------------------------------
// Кнопки масштаба (+ / −)
// ---------------------------------------------------------------------------

/// Кнопка полноэкранного режима: четыре уголка наружу (включить) или внутрь (выйти).
fn fullscreen_button(ui: &mut egui::Ui, is_full: bool) -> bool {
    let size = 48.0;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    let response = response.on_hover_text(if is_full {
        "Выйти из полноэкранного режима (F11)"
    } else {
        "Во весь экран (F11)"
    });

    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    let painter = ui.painter();
    painter.rect_filled(
        rect.translate(egui::vec2(0.0, 3.0)).expand(2.0),
        16.0,
        egui::Color32::from_black_alpha(35),
    );
    painter.rect_filled(rect, 14.0, egui::Color32::from_white_alpha(240));

    let color = if response.hovered() {
        egui::Color32::from_rgb(26, 115, 232)
    } else {
        egui::Color32::from_rgb(60, 64, 67)
    };
    let stroke = egui::Stroke::new(
        if response.is_pointer_button_down_on() {
            3.0
        } else {
            2.5
        },
        color,
    );

    // Четыре «уголка». Включить: вершины у краёв, стороны смотрят к центру.
    // Выйти: вершины у центра, стороны смотрят наружу.
    let c = rect.center();
    let (corner, arm_dir) = if is_full {
        (4.0, 1.0)
    } else {
        (11.0, -1.0_f32)
    };
    let arm = 7.0;
    for (sx, sy) in [(-1.0_f32, -1.0_f32), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
        let p = c + egui::vec2(sx * corner, sy * corner);
        painter.line_segment([p, p + egui::vec2(sx * arm * arm_dir, 0.0)], stroke);
        painter.line_segment([p, p + egui::vec2(0.0, sy * arm * arm_dir)], stroke);
    }

    response.clicked()
}

/// Скруглённый блок «+ / −». Возвращает +1.0 (приблизить), -1.0 (отдалить) или 0.0.
fn zoom_control(ui: &mut egui::Ui) -> f64 {
    let btn = 48.0; // размер одной кнопки
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

    // Мягкая тень, затем белая «таблетка»
    painter.rect_filled(
        rect.translate(egui::vec2(0.0, 3.0)).expand(2.0),
        radius + 2.0,
        egui::Color32::from_black_alpha(35),
    );
    painter.rect_filled(rect, radius, egui::Color32::from_white_alpha(240));

    // Разделитель между кнопками
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
            if r.is_pointer_button_down_on() {
                3.5
            } else {
                2.5
            },
            color_of(r),
        )
    };

    let arm = 9.0; // половина длины линий + и −

    // «+» точно по центру своей кнопки
    let c = plus_rect.center();
    painter.line_segment(
        [c - egui::vec2(arm, 0.0), c + egui::vec2(arm, 0.0)],
        stroke_of(&plus),
    );
    painter.line_segment(
        [c - egui::vec2(0.0, arm), c + egui::vec2(0.0, arm)],
        stroke_of(&plus),
    );

    // «−» точно по центру своей кнопки
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
// Ряд кнопок по центру и панель слоёв
// ---------------------------------------------------------------------------

/// Ставит ряд виджетов по центру доступной ширины. Ширина ряда запоминается с прошлого
/// кадра, поэтому в самый первый кадр ряд на мгновение стоит слева.
fn centered_row(ui: &mut egui::Ui, id: &str, add_contents: impl FnOnce(&mut egui::Ui)) {
    let key = ui.id().with(id);
    let known_width: f32 = ui.ctx().data(|d| d.get_temp::<f32>(key)).unwrap_or(0.0);
    let pad = ((ui.available_width() - known_width) / 2.0).max(0.0);

    ui.horizontal(|ui| {
        ui.add_space(pad);
        let start = ui.min_rect().right();
        add_contents(ui);
        let width = ui.min_rect().right() - start;
        if (width - known_width).abs() > 0.5 {
            ui.ctx().data_mut(|d| d.insert_temp(key, width));
            ui.ctx().request_repaint();
        }
    });
}

/// Что произошло в панели слоёв за кадр.
#[derive(Default)]
struct PanelResult {
    changed: bool,
    remove: Option<usize>,
}

fn layers_panel(ui: &mut egui::Ui, layers: &mut [layers::Layer]) -> PanelResult {
    let mut result = PanelResult::default();
    ui.set_max_width(340.0);
    ui.strong("Слои").on_hover_text(
        "Бледным цветом рисуются объекты со статусом «строится», «проект» или «простаивает».\n\
         Отменённые и выведенные из эксплуатации объекты не показываются.",
    );

    for (i, layer) in layers.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            // Цвет слоя
            let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
            ui.painter()
                .rect_filled(rect, 3.0, layers::palette_color(layer.color));

            // Кнопка слоя: нажата = слой включён
            let hover = format!(
                "{}\n{}",
                layer.path.display(),
                layer
                    .data()
                    .map(|d| format!("Объектов на карте: {}", d.objects))
                    .unwrap_or_default()
            );
            let mut visible = layer.visible;
            ui.toggle_value(&mut visible, layers::short_name(&layer.name(), 28))
                .on_hover_text(hover);
            if visible != layer.visible {
                layer.visible = visible;
                if visible {
                    layer.retry_if_failed();
                }
                result.changed = true;
            }

            if layer.is_loading() {
                ui.spinner();
            }

            if ui
                .small_button("×")
                .on_hover_text("Убрать слой из списка")
                .clicked()
            {
                result.remove = Some(i);
            }
        });

        if layer.visible {
            if let Some(message) = layer.error() {
                ui.colored_label(egui::Color32::from_rgb(200, 50, 50), message);
            } else if layer.data().is_some_and(|d| d.objects == 0) {
                ui.small("В файле нет объектов для показа (нужны линии, точки или контуры).");
            }
        }
    }

    // Лицензия данных Global Energy Monitor требует указывать источник
    if layers
        .iter()
        .any(|l| l.visible && l.name().to_lowercase().starts_with("gem"))
    {
        ui.small("Данные: Global Energy Monitor (CC BY 4.0)");
    }
    result
}

// ---------------------------------------------------------------------------
// Приложение
// ---------------------------------------------------------------------------

struct ViewerApp {
    tiles: Option<PmTiles>,
    file: Option<PathBuf>,
    map_memory: MapMemory,
    center: (f64, f64), // долгота, широта
    min_zoom: f64,
    max_zoom: f64,
    /// Границы новой карты, по которым при первом показе подбирается масштаб
    pending_fit: Option<[f64; 4]>,
    fit_tries: u32,
    layers: Vec<layers::Layer>,
}

impl ViewerApp {
    fn new() -> Self {
        Self {
            tiles: None,
            file: None,
            map_memory: MapMemory::default(),
            center: (0.0, 0.0),
            min_zoom: 0.0,
            max_zoom: 12.0,
            pending_fit: None,
            fit_tries: 0,
            layers: Vec::new(),
        }
    }

    /// Записывает в настройки открытую карту и список слоёв.
    fn save_settings(&self) {
        settings::save(&settings::Settings {
            map: self.file.as_ref().map(|p| p.to_string_lossy().into_owned()),
            layers: self
                .layers
                .iter()
                .map(|l| settings::LayerEntry {
                    path: l.path.to_string_lossy().into_owned(),
                    visible: l.visible,
                    color: l.color,
                })
                .collect(),
        });
    }

    fn open(&mut self, path: PathBuf, ctx: egui::Context) {
        let info = read_info(&path).unwrap_or(FileInfo {
            center: (0.0, 0.0),
            bounds: [-180.0, -85.0, 180.0, 85.0],
            min_zoom: 0.0,
            max_zoom: 12.0,
        });

        self.center = info.center;
        self.min_zoom = info.min_zoom;
        self.max_zoom = info.max_zoom;
        self.map_memory = MapMemory::default();
        // Масштаб подбирается в ui(), когда известен размер окна
        self.pending_fit = Some(info.bounds);
        self.fit_tries = 0;

        self.tiles = Some(PmTiles::with_style(&path, localized_style(), ctx));
        self.file = Some(path);
        self.save_settings();
    }

    fn add_layer(&mut self, path: PathBuf) {
        if let Some(existing) = self.layers.iter_mut().find(|l| l.path == path) {
            // Этот файл уже в списке: просто включаем
            existing.visible = true;
            existing.retry_if_failed();
        } else {
            let color = layers::next_color(&self.layers);
            self.layers.push(layers::Layer::new(path, true, color));
        }
        self.save_settings();
    }

    fn change_zoom(&mut self, delta: f64) {
        let z = (self.map_memory.zoom() + delta).clamp(0.0, self.max_zoom);
        let _ = self.map_memory.set_zoom(z);
    }
}

impl eframe::App for ViewerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Фоновая загрузка слоёв: забираем готовое и запускаем чтение включённых слоёв
        for layer in &mut self.layers {
            layer.poll();
            if layer.visible {
                layer.start_loading(ui.ctx());
            }
        }

        // Верхняя панель: две кнопки по центру
        ui.vertical_centered(|ui| {
            centered_row(ui, "toolbar", |ui| {
                if ui.button("Выбрать карту…").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .set_title("Выберите файл карты")
                        .add_filter("Карты PMTiles", &["pmtiles"])
                        .pick_file()
                    {
                        self.open(path, ui.ctx().clone());
                    }
                }

                let add = ui
                    .add_enabled(self.tiles.is_some(), egui::Button::new("Добавить слой…"))
                    .on_disabled_hover_text("Сначала выберите карту");
                if add.clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .set_title("Выберите файл слоя")
                        .add_filter("Слои (GeoJSON)", &["geojson", "json"])
                        .pick_file()
                    {
                        self.add_layer(path);
                    }
                }
            });

            if let Some(file) = &self.file {
                ui.small(
                    file.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                );
            }
        });
        ui.separator();

        // Область под панелью, которую занимает карта
        let map_rect = ui.available_rect_before_wrap();

        // F11 — полноэкранный режим
        if ui.input(|i| i.key_pressed(egui::Key::F11)) {
            let full = ui.input(|i| i.viewport().fullscreen).unwrap_or(false);
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Fullscreen(!full));
        }

        match self.tiles.as_mut() {
            Some(tiles) => {
                let mut map = Map::new(
                    Some(tiles),
                    &mut self.map_memory,
                    lon_lat(self.center.0, self.center.1),
                )
                // Колесо мыши масштабирует без Ctrl
                .zoom_with_ctrl(false)
                // Левая кнопка мыши двигает карту
                .drag_pan_buttons(egui::DragPanButtons::PRIMARY);

                // Слои рисуются поверх карты в порядке списка
                for layer in &self.layers {
                    if !layer.visible {
                        continue;
                    }
                    if let Some(data) = layer.data() {
                        map = map.with_plugin(layers::LayerPlugin {
                            data,
                            color: layers::palette_color(layer.color),
                        });
                    }
                }
                // Пока подбирается масштаб, плагин измеряет размер мира в пикселях
                let fitting = self.pending_fit.is_some();
                if fitting {
                    map = map.with_plugin(layers::FitProbe);
                }
                let hit_id = egui::Id::new(layers::HIT_ID);
                ui.ctx().data_mut(|d| d.remove_temp::<layers::Hit>(hit_id));
                ui.add(map);
                let hit = ui.ctx().data(|d| d.get_temp::<layers::Hit>(hit_id));

                // Подбор стартового масштаба: измеряем, сколько пикселей занимает мир,
                // и поправляем масштаб (обычно за 1-3 кадра), пока карта не закроет окно
                if let Some(bounds) = self.pending_fit
                    && map_rect.width() > 100.0
                    && map_rect.height() > 100.0
                {
                    let measured = ui
                        .ctx()
                        .data(|d| d.get_temp::<f32>(egui::Id::new(layers::WORLD_PX_ID)));
                    if let Some(current) = measured.filter(|w| *w > 1.0) {
                        let needed = cover_world_px(bounds, (map_rect.width(), map_rect.height()));
                        let dz = (needed / current as f64).log2();
                        let z0 = self.map_memory.zoom();
                        let z1 = (z0 + dz).clamp(self.min_zoom, self.max_zoom);
                        self.fit_tries += 1;
                        if dz.abs() < 0.005
                            || (z1 - z0).abs() < 1e-4
                            || self.fit_tries > 8
                            || self.map_memory.set_zoom(z1).is_err()
                        {
                            self.pending_fit = None;
                        }
                        ui.ctx().request_repaint();
                    } else {
                        ui.ctx().request_repaint();
                    }
                }

                // В файле нет тайлов глубже максимального масштаба
                if self.map_memory.zoom() > self.max_zoom {
                    let _ = self.map_memory.set_zoom(self.max_zoom);
                }

                // Подсказка с названием объекта под курсором
                if let Some(hit) = hit
                    && let Some(pointer) = ui.input(|i| i.pointer.hover_pos())
                {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    let right = pointer.x > map_rect.center().x;
                    let below = pointer.y > map_rect.center().y;
                    let pivot = match (right, below) {
                        (false, false) => egui::Align2::LEFT_TOP,
                        (true, false) => egui::Align2::RIGHT_TOP,
                        (false, true) => egui::Align2::LEFT_BOTTOM,
                        (true, true) => egui::Align2::RIGHT_BOTTOM,
                    };
                    let offset = egui::vec2(
                        if right { -14.0 } else { 14.0 },
                        if below { -14.0 } else { 14.0 },
                    );
                    egui::Area::new(egui::Id::new("hover_tooltip"))
                        .order(egui::Order::Tooltip)
                        .interactable(false)
                        .pivot(pivot)
                        .fixed_pos(pointer + offset)
                        .show(ui.ctx(), |ui| {
                            egui::Frame::popup(ui.style()).show(ui, |ui| {
                                ui.set_max_width(360.0);
                                ui.horizontal(|ui| {
                                    let (r, _) = ui.allocate_exact_size(
                                        egui::vec2(10.0, 10.0),
                                        egui::Sense::hover(),
                                    );
                                    ui.painter().rect_filled(r, 2.0, hit.color);
                                    ui.strong(&hit.title);
                                });
                                for line in &hit.details {
                                    ui.label(line);
                                }
                            });
                        });
                }

                // Кнопка полноэкранного режима над кнопками + / −
                let is_full = ui.input(|i| i.viewport().fullscreen).unwrap_or(false);
                let mut toggle_full = false;
                egui::Area::new(egui::Id::new("fullscreen_button"))
                    .pivot(egui::Align2::RIGHT_BOTTOM)
                    .fixed_pos(egui::pos2(
                        map_rect.right() - 16.0,
                        map_rect.center().y - 48.0 - 14.0,
                    ))
                    .show(ui.ctx(), |ui| {
                        toggle_full = fullscreen_button(ui, is_full);
                    });
                if toggle_full {
                    ui.ctx()
                        .send_viewport_cmd(egui::ViewportCommand::Fullscreen(!is_full));
                }

                // Кнопки + / − по центру правого края карты
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

                // Панель слоёв в левом верхнем углу карты
                if !self.layers.is_empty() {
                    let mut panel = PanelResult::default();
                    egui::Area::new(egui::Id::new("layers_panel"))
                        .fixed_pos(map_rect.left_top() + egui::vec2(12.0, 12.0))
                        .show(ui.ctx(), |ui| {
                            egui::Frame::popup(ui.style()).show(ui, |ui| {
                                panel = layers_panel(ui, &mut self.layers);
                            });
                        });

                    if let Some(i) = panel.remove {
                        self.layers.remove(i);
                        self.save_settings();
                    } else if panel.changed {
                        self.save_settings();
                    }
                }
            }
            None => {
                ui.centered_and_justified(|ui| {
                    ui.label("Нажмите «Выбрать карту…» и выберите файл .pmtiles");
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Запуск
// ---------------------------------------------------------------------------

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

fn main() {
    // DirectX 12 по умолчанию, если бэкенд не выбран вручную (на некоторых ноутбуках
    // бэкенд по умолчанию вызывает сбой)
    #[cfg(windows)]
    {
        if std::env::var_os("WGPU_BACKEND").is_none() {
            // SAFETY: вызывается в самом начале, пока других потоков нет
            unsafe { std::env::set_var("WGPU_BACKEND", "dx12") };
        }
    }

    log_step("--- start ---");
    env_logger::init();

    // Паники показываем окном (в релизной сборке нет консоли)
    std::panic::set_hook(Box::new(|info| {
        log_step(&format!("PANIC: {info}"));
        show_error(&format!("ShurMap аварийно завершился:\n\n{info}"));
    }));

    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_icon(load_icon())
            .with_inner_size([1200.0, 800.0]),
        ..Default::default()
    };

    log_step("calling run_native");
    let result = eframe::run_native(
        APP_NAME,
        options,
        Box::new(|cc| {
            log_step("graphics initialised, window created");
            // Нужно для векторных тайлов: запускает GPU-рендерер walkers
            walkers::install_renderer(cc.wgpu_render_state.as_ref());
            log_step("walkers renderer installed");

            let saved = settings::load();
            let mut app = ViewerApp::new();

            // Сначала слои, потом карта: open() записывает настройки целиком
            app.layers = saved
                .layers
                .iter()
                .map(|e| layers::Layer::new(PathBuf::from(&e.path), e.visible, e.color))
                .collect();
            if let Some(map) = saved.map.map(PathBuf::from).filter(|p| p.is_file()) {
                app.open(map, cc.egui_ctx.clone());
            }
            log_step("app ready");
            Ok(Box::new(app))
        }),
    );

    if let Err(e) = result {
        log_step(&format!("ERROR: {e}"));
        show_error(&format!("ShurMap не смог запуститься:\n\n{e}"));
    }
    log_step("exited normally");
}
