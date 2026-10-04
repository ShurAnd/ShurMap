// Скрывает окно консоли в релизной сборке под Windows
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod categories;
mod countries;
mod icons;
mod layers;
mod settings;

use eframe::egui;
use serde_json::{Value, json};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};
use walkers::{Map, MapMemory, PmTiles, Style, lon_lat};

const APP_NAME: &str = "ShurMap";
/// Нейтральный светло-серый фон вокруг карты (виден, когда карта отдалена до размера меньше окна)
const MAP_BACKGROUND: egui::Color32 = egui::Color32::from_rgb(222, 225, 230);
/// Версия берётся из Cargo.toml (поле version), чтобы её не нужно было менять в двух местах
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

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

/// Запущено ли приложение как дочерний процесс с выбранным графическим бэкендом (см. `supervise`).
/// Найден ли жирный шрифт (см. `setup_fonts`).
static BOLD_FONT: AtomicBool = AtomicBool::new(false);

/// Подключает жирный шрифт из системных (стандартный шрифт egui жирного начертания не имеет).
/// Если подходящего файла нет, заголовки выделяются цветом (`bold` вернёт `strong`).
fn setup_fonts(ctx: &egui::Context) {
    let candidates = [
        r"C:\Windows\Fonts\segoeuib.ttf",
        r"C:\Windows\Fonts\arialbd.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    ];
    let Some(bytes) = candidates.iter().find_map(|p| std::fs::read(p).ok()) else {
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "bold".to_string(),
        std::sync::Arc::new(egui::FontData::from_owned(bytes)),
    );
    // Недостающие символы берутся из обычного шрифта
    let mut chain = vec!["bold".to_string()];
    if let Some(base) = fonts.families.get(&egui::FontFamily::Proportional) {
        chain.extend(base.iter().cloned());
    }
    fonts
        .families
        .insert(egui::FontFamily::Name("bold".into()), chain);
    ctx.set_fonts(fonts);
    BOLD_FONT.store(true, Ordering::Relaxed);
}

/// Жирный текст (для заголовков разделов).
fn bold(text: impl Into<String>) -> egui::RichText {
    let text = egui::RichText::new(text);
    if BOLD_FONT.load(Ordering::Relaxed) {
        text.family(egui::FontFamily::Name("bold".into()))
    } else {
        text.strong()
    }
}

static CHILD: AtomicBool = AtomicBool::new(false);
/// Нарисован ли хотя бы один кадр: после этого сбой уже не связан с выбором бэкенда.
static READY: AtomicBool = AtomicBool::new(false);

/// Отмечает первый успешно нарисованный кадр: пишет в журнал и создаёт файл-флаг,
/// по которому родительский процесс понимает, что графика заработала.
fn mark_ready() {
    if !READY.swap(true, Ordering::Relaxed) {
        log_step("first frame drawn");
        if let Some(dir) = settings::config_dir() {
            let _ = std::fs::write(dir.join("ready.flag"), "1");
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

/// `hide_boundaries` убирает из стиля слои границ (их заменяет слой границ стран).
fn localized_style(hide_boundaries: bool) -> Style {
    let mut style_json: Value =
        serde_json::from_str(include_str!("../assets/protomaps-light.json"))
            .expect("bad style json");
    localize(&mut style_json);
    // Названия стран ярче и контрастнее, чем в стандартном стиле
    if let Some(list) = style_json["layers"].as_array_mut() {
        for l in list.iter_mut() {
            if l["id"].as_str() == Some("places_country") {
                l["paint"]["text-color"] = json!("#3b2f7a");
                l["paint"]["text-halo-color"] = json!("#ffffff");
            }
        }
    }
    if hide_boundaries && let Some(list) = style_json["layers"].as_array_mut() {
        list.retain(|l| {
            let source = l["source-layer"].as_str().unwrap_or("");
            let id = l["id"].as_str().unwrap_or("");
            source != "boundaries" && !id.starts_with("boundaries")
        });
    }
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

    // В повреждённом заголовке минимальный масштаб может оказаться больше максимального;
    // `clamp(min, max)` при этом вызвал бы панику
    let (z_a, z_b) = (b[100], b[101]);
    Some(FileInfo {
        center: ((min_lon + max_lon) / 2.0, (min_lat + max_lat) / 2.0),
        bounds: [min_lon, min_lat, max_lon, max_lat],
        min_zoom: z_a.min(z_b) as f64,
        max_zoom: z_a.max(z_b) as f64,
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

/// Кнопка фильтра по странам: воронка. Когда фильтр включён, воронка синяя, с числом стран.
/// Возвращает (нажата ли, область кнопки).
fn filter_button(ui: &mut egui::Ui, active: usize) -> (bool, egui::Rect) {
    let size = 48.0;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    let response = response.on_hover_text(if active > 0 {
        "Фильтр по странам (включён)"
    } else {
        "Фильтр по странам"
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

    let blue = egui::Color32::from_rgb(26, 115, 232);
    let color = if active > 0 || response.hovered() {
        blue
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

    // Воронка: широкий верх, сужение и короткая ножка
    let c = rect.center() + egui::vec2(0.0, 1.0);
    let p = |x: f32, y: f32| c + egui::vec2(x, y);
    if active > 0 {
        let fill = egui::Color32::from_rgba_unmultiplied(26, 115, 232, 70);
        painter.add(egui::Shape::convex_polygon(
            vec![p(-11.0, -9.0), p(11.0, -9.0), p(2.5, 1.0), p(-2.5, 1.0)],
            fill,
            egui::Stroke::NONE,
        ));
        painter.rect_filled(
            egui::Rect::from_two_pos(p(-2.5, 1.0), p(2.5, 8.0)),
            0.0,
            fill,
        );
    }
    painter.add(egui::Shape::closed_line(
        vec![
            p(-11.0, -9.0),
            p(11.0, -9.0),
            p(2.5, 1.0),
            p(2.5, 9.0),
            p(-2.5, 6.0),
            p(-2.5, 1.0),
        ],
        stroke,
    ));

    // Число отмеченных стран в кружке справа вверху
    if active > 0 {
        let at = rect.right_top() + egui::vec2(-4.0, 4.0);
        painter.circle_filled(at, 9.0, blue);
        painter.text(
            at,
            egui::Align2::CENTER_CENTER,
            active.min(99).to_string(),
            egui::FontId::proportional(11.0),
            egui::Color32::WHITE,
        );
    }

    (response.clicked(), rect)
}

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

/// Карточка с названием и сведениями об объекте рядом с точкой `at`.
fn show_tooltip(
    ctx: &egui::Context,
    id: &str,
    hit: &layers::Hit,
    at: egui::Pos2,
    map_rect: egui::Rect,
    pinned: bool,
) {
    let right = at.x > map_rect.center().x;
    let below = at.y > map_rect.center().y;
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
    egui::Area::new(egui::Id::new(id))
        .order(egui::Order::Tooltip)
        .interactable(false)
        .pivot(pivot)
        .fixed_pos(at + offset)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_max_width(360.0);
                ui.horizontal(|ui| {
                    let (r, _) =
                        ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                    ui.painter().rect_filled(r, 2.0, hit.color);
                    ui.strong(&hit.title);
                });
                for line in &hit.details {
                    ui.label(line);
                }
                if pinned {
                    ui.small("Нажмите ещё раз, чтобы закрыть");
                }
            });
        });
}

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

/// Кнопка-корзина, нарисованная линиями (в шрифте нет надёжного значка корзины).
fn trash_button(ui: &mut egui::Ui) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::click());
    let hovered = response.hovered();
    if hovered {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        ui.painter()
            .rect_filled(rect, 4.0, egui::Color32::from_rgb(253, 232, 232));
    }
    let color = if hovered {
        egui::Color32::from_rgb(200, 40, 40)
    } else {
        egui::Color32::from_gray(110)
    };
    let stroke = egui::Stroke::new(1.5, color);
    let c = rect.center();
    let p = |dx: f32, dy: f32| egui::pos2(c.x + dx, c.y + dy);
    let painter = ui.painter();
    // Крышка и ручка
    painter.line_segment([p(-6.0, -4.0), p(6.0, -4.0)], stroke);
    painter.add(egui::Shape::line(
        vec![p(-2.0, -4.0), p(-2.0, -6.5), p(2.0, -6.5), p(2.0, -4.0)],
        stroke,
    ));
    // Корпус
    painter.add(egui::Shape::line(
        vec![p(-4.5, -2.0), p(-3.8, 6.5), p(3.8, 6.5), p(4.5, -2.0)],
        stroke,
    ));
    // Две вертикальные риски
    painter.line_segment([p(-1.5, 0.0), p(-1.5, 4.5)], stroke);
    painter.line_segment([p(1.5, 0.0), p(1.5, 4.5)], stroke);
    response
}

/// Окошко выбора цвета и значка слоя. Возвращает выбранный номер цвета, выбранный значок
/// и занятую окошком область.
fn color_picker(
    ctx: &egui::Context,
    anchor: egui::Pos2,
    layers: &[layers::Layer],
    current: usize,
) -> (
    Option<usize>,
    Option<icons::Icon>,
    Option<categories::Category>,
    egui::Rect,
) {
    let n = layers::palette_len();
    let mut chosen = None;
    let mut chosen_icon = None;
    let mut chosen_category = None;
    let dark = egui::Color32::from_rgb(40, 40, 40);
    let shown = egui::Area::new(egui::Id::new("color_picker"))
        .order(egui::Order::Foreground)
        .fixed_pos(anchor)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.small("Цвет слоя");
                egui::Grid::new("color_grid")
                    .spacing(egui::vec2(6.0, 6.0))
                    .show(ui, |ui| {
                        for idx in 0..n {
                            let (rect, r) = ui
                                .allocate_exact_size(egui::vec2(24.0, 24.0), egui::Sense::click());
                            // Текущий цвет этого слоя обведён тёмной рамкой
                            if idx == layers[current].color % n {
                                ui.painter().rect_filled(rect.expand(3.0), 5.0, dark);
                            }
                            ui.painter()
                                .rect_filled(rect, 4.0, layers::palette_color(idx));

                            // Цвет уже занят другим слоем: помечаем точкой
                            let owner = layers.iter().enumerate().find(|(j, l)| {
                                *j != current && !l.is_borders() && l.color % n == idx
                            });
                            let r = if let Some((_, l)) = owner {
                                ui.painter().circle_filled(
                                    rect.center(),
                                    4.5,
                                    egui::Color32::WHITE,
                                );
                                ui.painter().circle_stroke(
                                    rect.center(),
                                    4.5,
                                    egui::Stroke::new(1.0, dark),
                                );
                                r.on_hover_text(format!(
                                    "Занят слоем «{}». Он получит другой цвет.",
                                    l.name()
                                ))
                            } else {
                                r
                            };
                            if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                chosen = Some(idx);
                            }
                            if idx % 5 == 4 {
                                ui.end_row();
                            }
                        }
                    });

                // Значки для точек слоя (у слоёв из одних линий их нет)
                if layers[current].has_dots() {
                    ui.add_space(4.0);
                    ui.small("Значок");
                    let color = layers[current].display_color();
                    egui::Grid::new("icon_grid")
                        .spacing(egui::vec2(6.0, 6.0))
                        .show(ui, |ui| {
                            for (k, icon) in icons::ALL.iter().copied().enumerate() {
                                let (rect, r) = ui.allocate_exact_size(
                                    egui::vec2(28.0, 28.0),
                                    egui::Sense::click(),
                                );
                                if icon == layers[current].icon {
                                    ui.painter().circle_filled(rect.center(), 15.5, dark);
                                }
                                layers::paint_icon(ui.painter(), rect.center(), 12.0, icon, color);
                                let r = r
                                    .on_hover_text(icon.title())
                                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                                if r.clicked() {
                                    chosen_icon = Some(icon);
                                }
                                if k % 5 == 4 {
                                    ui.end_row();
                                }
                            }
                        });
                }

                // Раздел панели слоёв
                ui.add_space(4.0);
                ui.small("Раздел");
                for cat in categories::ALL {
                    if ui
                        .selectable_label(layers[current].category == cat, cat.title())
                        .clicked()
                    {
                        chosen_category = Some(cat);
                    }
                }
            });
        });
    (chosen, chosen_icon, chosen_category, shown.response.rect)
}

/// Панель фильтра по странам. Возвращает true, если отметки изменились.
fn filter_panel(ui: &mut egui::Ui, filter: &mut countries::Filter, search: &mut String) -> bool {
    let list = &countries::get().list;
    let mut changed = false;
    ui.set_width(310.0);
    ui.horizontal(|ui| {
        ui.strong("Фильтр по странам");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let can_reset = filter.is_active() || !search.is_empty();
            if ui
                .add_enabled(can_reset, egui::Button::new("Сбросить"))
                .on_hover_text("Снять все отметки и очистить поиск")
                .clicked()
            {
                changed = filter.is_active();
                filter.checked.clear();
                search.clear();
            }
        });
    });
    ui.small(if filter.is_active() {
        "Показаны только объекты отмеченных стран"
    } else {
        "Ничего не отмечено — показаны все объекты"
    });
    ui.separator();

    // Континенты: отметка включает или выключает все страны континента
    for (c, name) in countries::CONTINENTS.iter().enumerate() {
        let members: Vec<&countries::Country> = list
            .iter()
            .filter(|k| k.continent == c && !k.hidden)
            .collect();
        if members.is_empty() {
            continue;
        }
        let marked = members
            .iter()
            .filter(|k| filter.checked.contains(&k.code))
            .count();
        let mut all = marked == members.len();
        let text = if marked > 0 && !all {
            format!("{name} ({marked}/{})", members.len())
        } else {
            format!("{name} ({})", members.len())
        };
        if ui.checkbox(&mut all, text).changed() {
            for k in &members {
                if all {
                    filter.checked.insert(k.code.clone());
                } else {
                    filter.checked.remove(&k.code);
                }
            }
            changed = true;
        }
    }
    ui.separator();

    ui.add(
        egui::TextEdit::singleline(search)
            .hint_text("Поиск страны…")
            .desired_width(f32::INFINITY),
    );
    let needle = countries::norm(search);
    egui::ScrollArea::vertical()
        .id_salt("filter_scroll")
        .auto_shrink([false, false])
        .min_scrolled_height(240.0)
        .max_height(240.0)
        .show(ui, |ui| {
            // Общая строка «Россия»: отмечает или снимает обе части сразу
            if needle.is_empty() || "россия".contains(&needle) {
                let mut on = filter.checked.contains(countries::RUS_EU)
                    && filter.checked.contains(countries::RUS_AS);
                if ui.checkbox(&mut on, "Россия").changed() {
                    for code in [countries::RUS_EU, countries::RUS_AS] {
                        if on {
                            filter.checked.insert(code.to_string());
                        } else {
                            filter.checked.remove(code);
                        }
                    }
                    changed = true;
                }
            }
            for k in list.iter().filter(|k| {
                !k.hidden && (needle.is_empty() || countries::norm(&k.ru).contains(&needle))
            }) {
                let mut on = filter.checked.contains(&k.code);
                if ui.checkbox(&mut on, &k.ru).changed() {
                    if on {
                        filter.checked.insert(k.code.clone());
                    } else {
                        filter.checked.remove(&k.code);
                    }
                    changed = true;
                }
            }
            if needle.is_empty() {
                let mut on = filter.checked.contains(countries::UNKNOWN_CODE);
                if ui
                    .checkbox(&mut on, "Страна не определена")
                    .on_hover_text("Объекты без страны в файле и вне границ (например, в море)")
                    .changed()
                {
                    if on {
                        filter.checked.insert(countries::UNKNOWN_CODE.to_string());
                    } else {
                        filter.checked.remove(countries::UNKNOWN_CODE);
                    }
                    changed = true;
                }
            }
        });
    if changed {
        filter.rebuild();
    }
    changed
}

/// Что произошло в панели слоёв за кадр.
#[derive(Default)]
struct PanelResult {
    changed: bool,
    remove: Option<usize>,
    /// Нажали на кружок цвета: номер слоя и его положение на экране
    color_click: Option<(usize, egui::Rect)>,
}

fn layers_panel(
    ui: &mut egui::Ui,
    layers: &mut [layers::Layer],
    collapsed: &mut bool,
    closed: &mut [bool; 6],
) -> PanelResult {
    let mut result = PanelResult::default();
    ui.set_max_width(340.0);
    ui.horizontal(|ui| {
        let title = if *collapsed {
            format!("Слои ({})", layers.len())
        } else {
            "Слои".to_string()
        };
        let ms = ui
            .ctx()
            .data(|d| d.get_temp::<f32>(egui::Id::new(layers::MS_SHOWN_ID)))
            .unwrap_or(0.0);
        ui.strong(title).on_hover_text(format!(
            "Бледным цветом рисуются объекты со статусом «строится», «проект» или «простаивает».\n\
             Отменённые и выведенные из эксплуатации объекты не показываются.\n\n\
             Подготовка слоёв в последнем кадре: {ms:.1} мс"
        ));
        let label = if *collapsed {
            "Развернуть"
        } else {
            "Свернуть"
        };
        if ui.small_button(label).clicked() {
            *collapsed = !*collapsed;
        }
    });
    if *collapsed {
        return result;
    }
    ui.horizontal(|ui| {
        let any_on = layers.iter().any(|l| l.visible);
        if ui
            .add_enabled(any_on, egui::Button::new("Снять все"))
            .on_hover_text("Выключить все слои (из списка они не удаляются)")
            .clicked()
        {
            for layer in layers.iter_mut() {
                layer.visible = false;
            }
            result.changed = true;
        }
    });

    // Строки слоёв. Короткий список рисуется как есть: высота подгоняется под содержимое.
    // Длинный (больше MAX_ROWS) прокручивается в окне фиксированной высоты. Автоподгонка
    // ScrollArea под содержимое здесь не используется: после сворачивания и разворачивания
    // панели она «запоминала» высоту в три строки.
    const MAX_ROWS: usize = 15;
    let many = layers.len() > MAX_ROWS;
    let mut rows = |ui: &mut egui::Ui| {
        for cat in categories::ALL {
            let members: Vec<usize> = (0..layers.len())
                .filter(|&i| layers[i].category == cat)
                .collect();
            if members.is_empty() {
                continue;
            }
            let c = cat as usize;
            let on = members.iter().filter(|&&i| layers[i].visible).count();
            ui.horizontal(|ui| {
                let arrow = if closed[c] { "+" } else { "–" };
                if ui.small_button(arrow).clicked() {
                    closed[c] = !closed[c];
                    result.changed = true;
                }
                // Галочка раздела включает и выключает все его слои сразу
                let mut all = on == members.len();
                if ui
                    .checkbox(&mut all, "")
                    .on_hover_text("Включить или выключить все слои раздела")
                    .changed()
                {
                    for &i in &members {
                        layers[i].visible = all;
                        if all {
                            layers[i].retry_if_failed();
                        }
                    }
                    result.changed = true;
                }
                let title = bold(format!("{} ({}/{})", cat.title(), on, members.len()));
                if ui
                    .add(egui::Label::new(title).sense(egui::Sense::click()))
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    closed[c] = !closed[c];
                    result.changed = true;
                }
            });
            if closed[c] {
                continue;
            }
            for i in members {
                let layer = &mut layers[i];
                ui.horizontal(|ui| {
                    // Цвет слоя
                    let (rect, swatch) =
                        ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::click());
                    ui.painter().rect_filled(rect, 3.0, layer.display_color());
                    if !layer.is_borders() {
                        let swatch = swatch
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .on_hover_text("Изменить цвет слоя");
                        if swatch.clicked() {
                            result.color_click = Some((i, rect));
                        }
                    }

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

                    if trash_button(ui)
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
                        ui.small(
                            "В файле нет объектов для показа (нужны линии, точки или контуры).",
                        );
                    }
                }
            }
        }
    };
    if many {
        egui::ScrollArea::vertical()
            .id_salt("layers_scroll")
            .auto_shrink([false, false])
            .min_scrolled_height(420.0)
            .max_height(420.0)
            .show(ui, |ui| rows(ui));
    } else {
        rows(ui);
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
    /// Скрыты ли сейчас границы из самой карты
    boundaries_hidden: bool,
    file: Option<PathBuf>,
    map_memory: MapMemory,
    center: (f64, f64), // долгота, широта
    min_zoom: f64,
    max_zoom: f64,
    /// Границы новой карты, по которым при первом показе подбирается масштаб
    pending_fit: Option<[f64; 4]>,
    fit_tries: u32,
    /// Где началось нажатие (чтобы отличить касание от перетаскивания)
    press_pos: Option<egui::Pos2>,
    /// Закреплённая подсказка: объект и место, где по нему нажали
    pinned: Option<(layers::Hit, egui::Pos2)>,
    layers: Vec<layers::Layer>,
    /// Открытое окошко выбора цвета: номер слоя и положение его кружка
    color_picker: Option<(usize, egui::Rect)>,
    /// Панель слоёв свёрнута
    layers_collapsed: bool,
    /// Общий фильтр по странам, окошко фильтра и строка поиска в нём
    filter: countries::Filter,
    filter_open: bool,
    filter_search: String,
    /// Рисовать точки значками (флаг в панели слоёв)
    /// Свёрнутые разделы панели слоёв (по номеру категории)
    closed: [bool; 6],
    /// Вшитые в exe границы стран (российская версия, только суша); в списке слоёв не видны
    borders: layers::Layer,
}

impl ViewerApp {
    fn new() -> Self {
        Self {
            tiles: None,
            boundaries_hidden: false,
            file: None,
            map_memory: MapMemory::default(),
            center: (0.0, 0.0),
            min_zoom: 0.0,
            max_zoom: 12.0,
            pending_fit: None,
            fit_tries: 0,
            press_pos: None,
            pinned: None,
            layers: Vec::new(),
            color_picker: None,
            layers_collapsed: false,
            filter: countries::Filter::default(),
            filter_open: false,
            filter_search: String::new(),
            closed: [false; 6],
            borders: layers::Layer::builtin_borders(include_bytes!(
                "../assets/borders_rus.geojson"
            )),
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
                    icon: Some(l.icon.key().to_string()),
                    category: Some(l.category.key().to_string()),
                })
                .collect(),
            filter: self.filter.codes(),
            closed_categories: categories::ALL
                .iter()
                .filter(|c| self.closed[**c as usize])
                .map(|c| c.key().to_string())
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
        self.pinned = None;

        self.boundaries_hidden = false;
        self.tiles = Some(PmTiles::with_style(&path, localized_style(false), ctx));
        self.file = Some(path);
        self.save_settings();
    }

    /// Добавляет в список слой из файла (настройки не сохраняет: это делает вызывающий код).
    fn add_layer(&mut self, path: PathBuf) {
        // Границы стран вшиты в программу и всегда включены: отдельным слоем их не добавляем
        if layers::Layer::new(path.clone(), true, 0).is_borders() {
            return;
        }
        if let Some(existing) = self.layers.iter_mut().find(|l| l.path == path) {
            // Этот файл уже в списке: просто включаем
            existing.visible = true;
            existing.retry_if_failed();
        } else {
            let color = layers::next_color(&self.layers);
            self.layers.push(layers::Layer::new(path, true, color));
        }
    }

    fn change_zoom(&mut self, delta: f64) {
        let z = (self.map_memory.zoom() + delta).clamp(0.0, self.max_zoom);
        let _ = self.map_memory.set_zoom(z);
    }
}

impl eframe::App for ViewerApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        mark_ready();
        // Время, которое слои потратили в прошлом кадре (для подсказки в панели слоёв)
        ui.ctx().data_mut(|d| {
            let acc = egui::Id::new(layers::MS_ACC_ID);
            let total = d.get_temp::<f32>(acc).unwrap_or(0.0);
            d.insert_temp(egui::Id::new(layers::MS_SHOWN_ID), total);
            d.insert_temp(acc, 0.0f32);
        });
        // Фоновая загрузка слоёв: забираем готовое и запускаем чтение включённых слоёв
        self.borders.poll();
        self.borders.start_loading(ui.ctx());
        for layer in &mut self.layers {
            layer.poll();
            if layer.visible {
                layer.start_loading(ui.ctx());
            }
        }

        // Пока включён и загружен слой границ стран, границы самой карты скрываем
        // (иначе рядом с жирной линией была бы вторая, тонкая и другая)
        let want_hidden = self.borders.data().is_some()
            || self
                .layers
                .iter()
                .any(|l| l.visible && l.is_borders() && l.data().is_some());
        if want_hidden != self.boundaries_hidden
            && let Some(file) = self.file.clone()
        {
            self.tiles = Some(PmTiles::with_style(
                &file,
                localized_style(want_hidden),
                ui.ctx().clone(),
            ));
            self.boundaries_hidden = want_hidden;
        }

        // Верхняя панель: кнопки по центру
        ui.vertical_centered(|ui| {
            centered_row(ui, "toolbar", |ui| {
                if ui.button("Выбрать карту…").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .set_parent(&*frame)
                        .set_title("Выберите файл карты")
                        .add_filter("Карты PMTiles", &["pmtiles"])
                        .pick_file()
                    {
                        self.open(path, ui.ctx().clone());
                    }
                }

                let add = ui
                    .add_enabled(self.tiles.is_some(), egui::Button::new("Добавить слои…"))
                    .on_disabled_hover_text("Сначала выберите карту");
                if add.clicked() {
                    // Можно выбрать сразу несколько файлов (Ctrl или Shift в окне выбора)
                    if let Some(mut paths) = rfd::FileDialog::new()
                        .set_parent(&*frame)
                        .set_title("Выберите файлы слоёв (можно несколько)")
                        .add_filter("Слои (GeoJSON)", &["geojson", "json"])
                        .pick_files()
                    {
                        // Порядок слоёв и цветов не зависит от порядка выбора в окне
                        paths.sort();
                        for path in paths {
                            self.add_layer(path);
                        }
                        self.save_settings();
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

        // Фон за картой: при сильном отдалении карта не занимает всё окно, и без заливки
        // вокруг неё было бы чёрное поле. Заливка рисуется раньше карты, то есть под ней.
        if self.tiles.is_some() {
            ui.painter().rect_filled(map_rect, 0.0, MAP_BACKGROUND);
        }

        // Касание или клик (без перетаскивания) по карте, не по кнопкам и панелям
        let mut tap: Option<egui::Pos2> = None;
        let (pressed, released, pos) = ui.input(|i| {
            (
                i.pointer.primary_pressed(),
                i.pointer.primary_released(),
                i.pointer.interact_pos(),
            )
        });
        if pressed {
            self.press_pos = pos;
        }
        if released
            && let (Some(a), Some(b)) = (self.press_pos.take(), pos)
            && a.distance(b) < 10.0
            && map_rect.contains(b)
            && ui.ctx().layer_id_at(b) == Some(ui.layer_id())
        {
            tap = Some(b);
        }
        ui.ctx().data_mut(|d| match tap {
            Some(p) => {
                d.insert_temp(egui::Id::new(layers::TAP_ID), p);
            }
            None => {
                d.remove_temp::<egui::Pos2>(egui::Id::new(layers::TAP_ID));
            }
        });

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
                if let Some(data) = self.borders.data() {
                    map = map.with_plugin(layers::LayerPlugin {
                        data,
                        color: self.borders.display_color(),
                        borders: true,
                        layer_id: usize::MAX,
                        selected: None,
                        filter: &self.filter,
                        icon: icons::Icon::Dot,
                    });
                }
                for (index, layer) in self.layers.iter().enumerate() {
                    if !layer.visible {
                        continue;
                    }
                    if let Some(data) = layer.data() {
                        let selected = self
                            .pinned
                            .as_ref()
                            .filter(|(h, _)| h.layer == index)
                            .map(|(h, _)| h.label);
                        map = map.with_plugin(layers::LayerPlugin {
                            data,
                            color: layer.display_color(),
                            borders: layer.is_borders(),
                            layer_id: index,
                            selected,
                            filter: &self.filter,
                            icon: layer.icon,
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

                // Нажатие по объекту закрепляет подсказку; повторное нажатие по нему, по другому
                // месту карты или по другому объекту меняет или убирает её
                if let Some(p) = tap {
                    self.pinned = match (hit.clone(), self.pinned.take()) {
                        (Some(h), Some((old, _)))
                            if h.layer == old.layer && h.label == old.label =>
                        {
                            None
                        }
                        (Some(h), _) => Some((h, p)),
                        (None, _) => None,
                    };
                }

                if let Some((pinned, at)) = &self.pinned {
                    show_tooltip(ui.ctx(), "pinned_tooltip", pinned, *at, map_rect, true);
                }

                // Подсказка при наведении курсора (не показываем лишний раз для закреплённого)
                if tap.is_none()
                    && let Some(hit) = &hit
                    && let Some(pointer) = ui.input(|i| i.pointer.hover_pos())
                {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    let is_pinned = self
                        .pinned
                        .as_ref()
                        .is_some_and(|(p, _)| p.layer == hit.layer && p.label == hit.label);
                    if !is_pinned {
                        show_tooltip(ui.ctx(), "hover_tooltip", hit, pointer, map_rect, false);
                    }
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

                // Кнопка фильтра над кнопкой полноэкранного режима
                let mut filter_rect = egui::Rect::NOTHING;
                let mut filter_clicked = false;
                let active = self.filter.checked.len();
                egui::Area::new(egui::Id::new("filter_button"))
                    .pivot(egui::Align2::RIGHT_BOTTOM)
                    .fixed_pos(egui::pos2(
                        map_rect.right() - 16.0,
                        map_rect.center().y - 48.0 - 14.0 - 48.0 - 14.0,
                    ))
                    .show(ui.ctx(), |ui| {
                        (filter_clicked, filter_rect) = filter_button(ui, active);
                    });
                if filter_clicked {
                    self.filter_open = !self.filter_open;
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
                                panel = layers_panel(
                                    ui,
                                    &mut self.layers,
                                    &mut self.layers_collapsed,
                                    &mut self.closed,
                                );
                            });
                        });

                    if let Some(i) = panel.remove {
                        self.pinned = None;
                        self.color_picker = None;
                        self.layers.remove(i);
                        self.save_settings();
                    } else if panel.changed {
                        self.pinned = None;
                        self.save_settings();
                    }

                    // Окошко выбора цвета
                    if self.layers_collapsed {
                        self.color_picker = None;
                    }
                    if let Some((i, rect)) = panel.color_click {
                        self.color_picker = match self.color_picker {
                            Some((k, _)) if k == i => None,
                            _ => Some((i, rect)),
                        };
                    }
                    if let Some((i, swatch)) = self.color_picker {
                        if i >= self.layers.len() {
                            self.color_picker = None;
                        } else {
                            let anchor = swatch.left_bottom() + egui::vec2(0.0, 6.0);
                            let (chosen, chosen_icon, chosen_category, area) =
                                color_picker(ui.ctx(), anchor, &self.layers, i);
                            if let Some(c) = chosen {
                                layers::assign_color(&mut self.layers, i, c);
                                self.color_picker = None;
                                self.save_settings();
                            } else if let Some(icon) = chosen_icon {
                                self.layers[i].icon = icon;
                                self.color_picker = None;
                                self.save_settings();
                            } else if let Some(cat) = chosen_category {
                                self.layers[i].category = cat;
                                self.color_picker = None;
                                self.save_settings();
                            } else if panel.color_click.is_none()
                                && ui.ctx().input(|s| s.pointer.primary_pressed())
                            {
                                let pos = ui.ctx().input(|s| s.pointer.interact_pos());
                                if pos.is_some_and(|p| !area.contains(p) && !swatch.contains(p)) {
                                    self.color_picker = None;
                                }
                            }
                        }
                    }
                }

                // Окошко фильтра слева от кнопки фильтра
                if self.filter_open {
                    {
                        let button = filter_rect;
                        let mut changed = false;
                        let area = egui::Area::new(egui::Id::new("filter_panel"))
                            .order(egui::Order::Foreground)
                            .pivot(egui::Align2::RIGHT_TOP)
                            .fixed_pos(button.left_top() + egui::vec2(-8.0, 0.0))
                            .show(ui.ctx(), |ui| {
                                egui::Frame::popup(ui.style()).show(ui, |ui| {
                                    changed =
                                        filter_panel(ui, &mut self.filter, &mut self.filter_search);
                                });
                            });
                        if changed {
                            self.pinned = None;
                            self.save_settings();
                        }
                        // Нажатие вне окошка и вне кнопки закрывает его
                        if ui.ctx().input(|s| s.pointer.primary_pressed()) {
                            let pos = ui.ctx().input(|s| s.pointer.interact_pos());
                            if pos.is_some_and(|p| {
                                !area.response.rect.contains(p) && !button.contains(p)
                            }) {
                                self.filter_open = false;
                            }
                        }
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

/// Графические бэкенды в порядке попыток: сначала DirectX 12, затем Vulkan, затем OpenGL.
const BACKENDS: [&str; 3] = ["dx12", "vulkan", "gl"];

fn backend_file() -> Option<PathBuf> {
    settings::config_dir().map(|d| d.join("backend.txt"))
}

/// Сколько секунд ждём первого кадра от дочернего процесса, прежде чем считать запуск зависшим.
#[cfg(windows)]
const FIRST_FRAME_TIMEOUT_SECS: u64 = 60;

/// Родительский процесс (только Windows): запускает ShurMap с `--backend=...` по очереди.
/// Как только дочерний процесс нарисовал первый кадр (появился файл-флаг), родитель
/// завершается, а окно живёт само. Если дочерний процесс завершился или завис до первого
/// кадра (ошибка или сбой драйвера), пробуется следующий бэкенд. Сработавший запоминается
/// в backend.txt (файл переписывается только при изменении) и в следующий раз идёт первым.
/// Возвращает код выхода или None, если запустить дочерний процесс нельзя.
#[cfg(windows)]
fn supervise() -> Option<i32> {
    use std::time::{Duration, Instant};

    let exe = std::env::current_exe().ok()?;
    let dir = settings::config_dir()?;
    let flag = dir.join("ready.flag");
    let _ = std::fs::create_dir_all(&dir);

    let saved = backend_file()
        .and_then(|f| std::fs::read_to_string(f).ok())
        .map(|t| t.trim().to_string());
    let mut order: Vec<&str> = Vec::new();
    if let Some(b) = BACKENDS.iter().find(|b| Some(**b) == saved.as_deref()) {
        order.push(b);
    }
    for b in BACKENDS {
        if !order.contains(&b) {
            order.push(b);
        }
    }

    // Остальные аргументы командной строки передаём как есть
    let extra: Vec<_> = std::env::args_os()
        .skip(1)
        .filter(|a| !a.to_string_lossy().starts_with("--backend="))
        .collect();

    let mut last_code = 1;
    for backend in order {
        let _ = std::fs::remove_file(&flag);
        log_step(&format!("launching with graphics backend: {backend}"));
        let mut child = match std::process::Command::new(&exe)
            .arg(format!("--backend={backend}"))
            .args(&extra)
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                log_step(&format!("could not start child process: {e}"));
                return None;
            }
        };

        let started = Instant::now();
        loop {
            if flag.exists() {
                // Окно работает: запоминаем бэкенд (только если он изменился) и выходим
                if saved.as_deref() != Some(backend) {
                    if let Some(f) = backend_file() {
                        let _ = std::fs::write(f, backend);
                    }
                }
                return Some(0);
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    // Мог успеть нарисовать кадр и сразу закрыться
                    if flag.exists() || status.success() {
                        return Some(status.code().unwrap_or(0));
                    }
                    last_code = status.code().unwrap_or(1);
                    log_step(&format!(
                        "backend {backend} failed before the first frame (code {last_code})"
                    ));
                    break;
                }
                Ok(None) => {}
                Err(e) => {
                    log_step(&format!("could not wait for child process: {e}"));
                    return Some(1);
                }
            }
            if started.elapsed() > Duration::from_secs(FIRST_FRAME_TIMEOUT_SECS) {
                log_step(&format!("backend {backend}: no first frame, killing"));
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    show_error(&format!(
        "ShurMap не смог запустить графику ни через DirectX 12, ни через Vulkan, ни через OpenGL.\n\n\
         Обновите драйвер видеокарты. Подробности записаны в файл:\n{}",
        dir.join("startup.log").display()
    ));
    Some(last_code)
}

fn main() {
    let backend_arg =
        std::env::args().find_map(|a| a.strip_prefix("--backend=").map(str::to_owned));

    if let Some(backend) = backend_arg {
        // Дочерний процесс: бэкенд выбран родителем
        CHILD.store(true, Ordering::Relaxed);
        // SAFETY: вызывается в самом начале, пока других потоков нет
        unsafe { std::env::set_var("WGPU_BACKEND", backend) };
    } else if std::env::var_os("WGPU_BACKEND").is_none() {
        // Бэкенд не выбран вручную: на Windows перебираем DX12, Vulkan, OpenGL
        #[cfg(windows)]
        {
            match supervise() {
                Some(code) => std::process::exit(code),
                // Дочерний процесс запустить не удалось: работаем в этом процессе на DX12
                None => unsafe { std::env::set_var("WGPU_BACKEND", "dx12") },
            }
        }
    }

    run_app();
}

fn run_app() {
    log_step(&format!("--- start {APP_VERSION} ---"));
    env_logger::init();

    // Паники пишем в журнал всегда, а окно показываем только для главного потока и только
    // если графика уже заработала. Паника фонового потока (например, чтения слоя) не
    // останавливает программу: слой просто покажет ошибку. Паника до первого кадра в
    // дочернем процессе - повод для родителя попробовать другой бэкенд.
    std::panic::set_hook(Box::new(|info| {
        let on_main = std::thread::current().name() == Some("main");
        log_step(&format!(
            "PANIC ({}): {info}",
            std::thread::current().name().unwrap_or("поток")
        ));
        let silent = CHILD.load(Ordering::Relaxed) && !READY.load(Ordering::Relaxed);
        if on_main && !silent {
            show_error(&format!("ShurMap аварийно завершился:\n\n{info}"));
        }
    }));

    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        viewport: egui::ViewportBuilder::default()
            .with_title(format!("{APP_NAME} {APP_VERSION}"))
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

            setup_fonts(&cc.egui_ctx);
            let saved = settings::load();
            let mut app = ViewerApp::new();

            // Сначала слои, потом карта: open() записывает настройки целиком
            app.layers = saved
                .layers
                .iter()
                .map(|e| {
                    let mut layer = layers::Layer::new(PathBuf::from(&e.path), e.visible, e.color);
                    if let Some(icon) = e.icon.as_deref().and_then(icons::Icon::from_key) {
                        layer.icon = icon;
                    }
                    if let Some(cat) = e
                        .category
                        .as_deref()
                        .and_then(categories::Category::from_key)
                    {
                        layer.category = cat;
                    }
                    layer
                })
                // Границы стран вшиты в программу: такой файл как отдельный слой не нужен
                .filter(|l| !l.is_borders())
                .collect();
            app.filter = countries::Filter::from_codes(&saved.filter);
            for c in categories::ALL {
                app.closed[c as usize] = saved.closed_categories.iter().any(|k| k == c.key());
            }
            if let Some(map) = saved.map.map(PathBuf::from).filter(|p| p.is_file()) {
                app.open(map, cc.egui_ctx.clone());
            }
            log_step("app ready");
            Ok(Box::new(app))
        }),
    );

    if let Err(e) = result {
        log_step(&format!("ERROR: {e}"));
        if CHILD.load(Ordering::Relaxed) {
            // Родитель увидит, что кадр не нарисован, и попробует другой бэкенд
            std::process::exit(3);
        }
        show_error(&format!("ShurMap не смог запуститься:\n\n{e}"));
    }
    log_step("exited normally");
}
