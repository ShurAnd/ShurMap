//! Слои карты: файлы GeoJSON с линиями (трубопроводы, железные дороги), точками (аэропорты,
//! порты, станции) и контурами. Файл читается в фоновом потоке, маршруты упрощаются для
//! нескольких масштабов, а рисуются слои поверх карты плагином walkers.

use crate::categories::Category;
use crate::countries::{self, Filter};
use crate::icons::Icon;
use crate::raster::{self, Prim};
use crate::strokes;
use eframe::egui::{self, Color32, Pos2, Shape, Stroke};
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, TryRecvError},
};
use walkers::{MapMemory, Plugin, Projector, lon_lat};

// ---------------------------------------------------------------------------
// Геометрия (не зависит от egui, поэтому проверяется отдельно)
// ---------------------------------------------------------------------------
// PURE-BEGIN
use std::f64::consts::PI;

/// Шаги упрощения маршрутов в единицах Web Mercator (весь мир = 1.0 в ширину).
/// Каждый шаг примерно в 4 раза грубее предыдущего; приложение берёт самую грубую копию,
/// которая на текущем масштабе ещё выглядит гладко (ошибка меньше ~0.7 пикселя).
const TOLS: [f64; 5] = [1.0e-6, 4.0e-6, 1.6e-5, 6.4e-5, 2.5e-4];

/// Долгота/широта в градусах -> Web Mercator в диапазоне 0..1 (x вправо, y вниз).
pub fn mercator(lon: f64, lat: f64) -> [f64; 2] {
    let lat = lat.clamp(-85.0511, 85.0511);
    let x = (lon + 180.0) / 360.0;
    let y = 0.5 - ((PI / 4.0 + lat.to_radians() / 2.0).tan().ln()) / (2.0 * PI);
    [x, y]
}

/// Оставляет первую и последнюю точки и каждую точку, которая не ближе `tol`
/// к предыдущей оставленной.
fn decimate(pts: &[[f64; 2]], tol: f64) -> Vec<[f64; 2]> {
    let t2 = tol * tol;
    let mut out = Vec::with_capacity(pts.len() / 2 + 2);
    out.push(pts[0]);
    let mut last = pts[0];
    for p in &pts[1..pts.len() - 1] {
        let (dx, dy) = (p[0] - last[0], p[1] - last[1]);
        if dx * dx + dy * dy >= t2 {
            out.push(*p);
            last = *p;
        }
    }
    out.push(pts[pts.len() - 1]);
    out
}

/// Нужно ли рисовать объект с таким статусом и бледным ли он.
/// Some(false) = обычный, Some(true) = бледный (строится, проект, простаивает),
/// None = не рисовать (отменён или выведен из эксплуатации).
/// Статус есть в файлах Global Energy Monitor; без статуса объект рисуется обычным.
pub fn faded_for_status(status: &str) -> Option<bool> {
    match status.trim().to_lowercase().as_str() {
        "" | "operating" | "underground gas storage" => Some(false),
        "cancelled" | "retired" | "abandoned" => None,
        _ => Some(true),
    }
}

/// Первый цвет палитры, которым ещё не занят ни один слой; когда все заняты — по кругу.
pub fn pick_color(used: &[usize], palette_len: usize, total_layers: usize) -> usize {
    (0..palette_len)
        .find(|i| !used.contains(i))
        .unwrap_or(total_layers % palette_len)
}

/// Назначает слою `i` цвет `new`. Если этот цвет уже был у другого слоя, тот получает
/// первый свободный цвет (а если свободных нет — прежний цвет слоя `i`).
/// `skip[j]` = слой не участвует в раскраске (например, слой границ).
pub fn recolor(colors: &mut [usize], skip: &[bool], i: usize, new: usize, n: usize) {
    let old = colors[i] % n;
    colors[i] = new % n;
    let conflicts: Vec<usize> = (0..colors.len())
        .filter(|&j| j != i && !skip[j] && colors[j] % n == new % n)
        .collect();
    for j in conflicts {
        let used: Vec<usize> = (0..colors.len())
            .filter(|&k| !skip[k] && k != j)
            .map(|k| colors[k] % n)
            .collect();
        colors[j] = (0..n).find(|c| !used.contains(c)).unwrap_or(old);
    }
}

/// Укорачивает длинное имя для кнопки: "очень-длинное-имя" -> "очень-длинное…".
pub fn short_name(name: &str, max_chars: usize) -> String {
    if name.chars().count() <= max_chars {
        name.to_string()
    } else {
        let head: String = name.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

/// Один непрерывный маршрут.
pub struct Line {
    faded: bool,
    /// Номер подписи в `Data::labels` (для подсказки при наведении)
    pub label: u32,
    min: [f64; 2], // рамка маршрута в единицах Mercator
    max: [f64; 2],
    /// (допуск упрощения, точки); сначала самые грубые, последняя копия — полный маршрут.
    lods: Vec<(f64, Vec<[f64; 2]>)>,
}

impl Line {
    /// `lonlat` — точки [долгота, широта]. None для маршрутов короче двух точек.
    pub fn build(faded: bool, lonlat: &[[f64; 2]]) -> Option<Line> {
        if lonlat.len() < 2 {
            return None;
        }
        let full: Vec<[f64; 2]> = lonlat.iter().map(|p| mercator(p[0], p[1])).collect();

        let mut min = [f64::MAX; 2];
        let mut max = [f64::MIN; 2];
        for p in &full {
            for i in 0..2 {
                min[i] = min[i].min(p[i]);
                max[i] = max[i].max(p[i]);
            }
        }

        // Каждая копия делается из предыдущей (более точной). Шаги без изменений пропускаются.
        let mut levels: Vec<(f64, Vec<[f64; 2]>)> = Vec::new();
        let mut current = full;
        let mut current_tol = 0.0;
        for &tol in &TOLS {
            let simpler = decimate(&current, tol);
            if simpler.len() < current.len() {
                levels.push((current_tol, std::mem::replace(&mut current, simpler)));
                current_tol = tol;
            }
        }
        levels.push((current_tol, current));
        levels.reverse();

        Some(Line {
            faded,
            label: 0,
            min,
            max,
            lods: levels,
        })
    }

    /// Самая грубая копия, ошибка которой не больше `max_tol`.
    pub fn lod(&self, max_tol: f64) -> &[[f64; 2]] {
        self.lods
            .iter()
            .find(|(tol, _)| *tol <= max_tol)
            .or_else(|| self.lods.last())
            .map(|(_, pts)| pts.as_slice())
            .unwrap_or(&[])
    }

    /// Пересекается ли рамка маршрута с `view` = [min_x, min_y, max_x, max_y]?
    pub fn overlaps(&self, view: [f64; 4]) -> bool {
        self.max[0] >= view[0]
            && self.min[0] <= view[2]
            && self.max[1] >= view[1]
            && self.min[1] <= view[3]
    }
}

/// Позиция на экране = смещение + позиция Mercator * масштаб.
pub struct Affine {
    ox: f64,
    oy: f64,
    pub sx: f64, // пикселей на единицу Mercator = ширина всего мира в пикселях
    sy: f64,
}

impl Affine {
    /// Вычисляет преобразование по функции проекции (долгота, широта -> x, y на экране),
    /// «пробуя» её в четырёх точках. Web Mercator линейна по x и y, двух проб на ось хватает.
    pub fn fit(project: impl Fn(f64, f64) -> (f64, f64)) -> Option<Affine> {
        let (x_a, _) = project(-90.0, 0.0);
        let (x_b, _) = project(90.0, 0.0);
        let (_, y_a) = project(0.0, 60.0);
        let (_, y_b) = project(0.0, -60.0);
        let (m_a, m_b) = (mercator(-90.0, 60.0), mercator(90.0, -60.0));

        let sx = (x_b - x_a) / (m_b[0] - m_a[0]);
        let sy = (y_b - y_a) / (m_b[1] - m_a[1]);
        if !(sx.is_finite() && sy.is_finite() && sx > 0.0 && sy > 0.0) {
            return None;
        }
        Some(Affine {
            ox: x_a - m_a[0] * sx,
            oy: y_a - m_a[1] * sy,
            sx,
            sy,
        })
    }

    /// Прямоугольник на экране [слева, сверху, справа, снизу] (+ поле) в единицах Mercator.
    pub fn visible(&self, rect: [f32; 4], margin_px: f64) -> [f64; 4] {
        [
            (rect[0] as f64 - margin_px - self.ox) / self.sx,
            (rect[1] as f64 - margin_px - self.oy) / self.sy,
            (rect[2] as f64 + margin_px - self.ox) / self.sx,
            (rect[3] as f64 + margin_px - self.oy) / self.sy,
        ]
    }

    pub fn screen(&self, m: &[f64; 2]) -> (f32, f32) {
        (
            (self.ox + m[0] * self.sx) as f32,
            (self.oy + m[1] * self.sy) as f32,
        )
    }
}

/// Переводит `pts` в экранные точки и отдаёт каждую часть маршрута, которая попадает в `clip`
/// = [слева, сверху, справа, снизу]. Части далеко за окном не строятся, поэтому рисование
/// остаётся быстрым, даже если приблизить очень длинный трубопровод.
pub fn visible_runs<P>(
    pts: &[[f64; 2]],
    t: &Affine,
    clip: [f32; 4],
    make_point: impl Fn(f32, f32) -> P,
    mut emit: impl FnMut(&[P]),
) {
    let mut run: Vec<P> = Vec::new();
    let mut prev: Option<(f32, f32)> = None;
    for m in pts {
        let cur = t.screen(m);
        if let Some(a) = prev {
            let hit = a.0.max(cur.0) >= clip[0]
                && a.0.min(cur.0) <= clip[2]
                && a.1.max(cur.1) >= clip[1]
                && a.1.min(cur.1) <= clip[3];
            if hit {
                if run.is_empty() {
                    run.push(make_point(a.0, a.1));
                }
                run.push(make_point(cur.0, cur.1));
            } else if !run.is_empty() {
                emit(&run);
                run.clear();
            }
        }
        prev = Some(cur);
    }
    if run.len() >= 2 {
        emit(&run);
    }
}

/// Расстояние в пикселях от точки `p` до отрезка `a`–`b`.
pub fn seg_distance(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 <= f32::EPSILON {
        0.0
    } else {
        (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0)
    };
    let (cx, cy) = (a.0 + t * dx, a.1 + t * dy);
    ((p.0 - cx).powi(2) + (p.1 - cy).powi(2)).sqrt()
}
// PURE-END

// ---------------------------------------------------------------------------
// Цвета слоёв
// ---------------------------------------------------------------------------

const PALETTE: [Color32; 25] = [
    Color32::from_rgb(214, 84, 0),
    Color32::from_rgb(30, 136, 229),
    Color32::from_rgb(46, 160, 67),
    Color32::from_rgb(142, 36, 170),
    Color32::from_rgb(211, 47, 47),
    Color32::from_rgb(0, 137, 123),
    Color32::from_rgb(121, 85, 72),
    Color32::from_rgb(194, 150, 0),
    Color32::from_rgb(233, 30, 99),
    Color32::from_rgb(63, 81, 181),
    Color32::from_rgb(0, 172, 193),
    Color32::from_rgb(158, 157, 36),
    Color32::from_rgb(96, 125, 139),
    Color32::from_rgb(33, 33, 33),
    Color32::from_rgb(255, 138, 101),
    Color32::from_rgb(124, 179, 66),  // салатовый
    Color32::from_rgb(136, 14, 79),   // бордовый
    Color32::from_rgb(13, 71, 161),   // тёмно-синий
    Color32::from_rgb(27, 94, 32),    // тёмно-зелёный
    Color32::from_rgb(186, 0, 160),   // фуксия
    Color32::from_rgb(149, 117, 205), // сиреневый
    Color32::from_rgb(0, 200, 83),    // изумрудный
    Color32::from_rgb(255, 193, 7),   // янтарный
    Color32::from_rgb(161, 136, 127), // серо-коричневый
    Color32::from_rgb(240, 98, 146),  // розовый светлый
];

pub fn palette_len() -> usize {
    PALETTE.len()
}

/// Назначает слою `i` цвет из палитры; конфликтующий слой получает другой цвет.
pub fn assign_color(layers: &mut [Layer], i: usize, new: usize) {
    let n = PALETTE.len();
    let mut colors: Vec<usize> = layers.iter().map(|l| l.color % n).collect();
    let skip: Vec<bool> = layers.iter().map(|l| l.is_borders()).collect();
    recolor(&mut colors, &skip, i, new, n);
    for (l, c) in layers.iter_mut().zip(colors) {
        l.color = c;
    }
}

/// Цвет жирных границ стран.
pub const BORDER_COLOR: Color32 = Color32::from_rgb(70, 62, 90);

pub fn palette_color(index: usize) -> Color32 {
    PALETTE[index % PALETTE.len()]
}

/// Номер цвета для нового слоя: первый неиспользованный.
pub fn next_color(layers: &[Layer]) -> usize {
    let used: Vec<usize> = layers
        .iter()
        .filter(|l| !l.is_borders())
        .map(|l| l.color % PALETTE.len())
        .collect();
    // Из свободных берём цвет, наименее похожий на уже занятые (иначе рядом с оранжевым
    // слоем новый получал бы красный, который в палитре стоит следующим по порядку)
    let free = (0..PALETTE.len()).filter(|i| !used.contains(i));
    let distance = |a: Color32, b: Color32| {
        // «Взвешенное» расстояние между цветами: глаз сильнее различает зелёный
        let (dr, dg, db) = (
            a.r() as f32 - b.r() as f32,
            a.g() as f32 - b.g() as f32,
            a.b() as f32 - b.b() as f32,
        );
        let rm = (a.r() as f32 + b.r() as f32) / 2.0;
        ((2.0 + rm / 256.0) * dr * dr + 4.0 * dg * dg + (2.0 + (255.0 - rm) / 256.0) * db * db)
            .sqrt()
    };
    let nearest = |i: usize| {
        used.iter()
            .map(|&u| distance(PALETTE[i], PALETTE[u]))
            .fold(f32::MAX, f32::min)
    };
    free.max_by(|&a, &b| {
        nearest(a)
            .partial_cmp(&nearest(b))
            .unwrap_or(std::cmp::Ordering::Equal)
            // при равенстве - цвет с меньшим номером
            .then(b.cmp(&a))
    })
    .unwrap_or_else(|| pick_color(&used, PALETTE.len(), layers.len()))
}

// ---------------------------------------------------------------------------
// Чтение GeoJSON (только нужные поля)
// ---------------------------------------------------------------------------

/// Координата [долгота, широта]; лишние значения (высота) игнорируются.
#[derive(Clone, Copy)]
struct Pt([f64; 2]);

impl<'de> Deserialize<'de> for Pt {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct PtVisitor;

        impl<'de> serde::de::Visitor<'de> for PtVisitor {
            type Value = Pt;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("координата [долгота, широта]")
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Pt, A::Error> {
                let lon: f64 = seq
                    .next_element()?
                    .ok_or_else(|| <A::Error as serde::de::Error>::invalid_length(0, &self))?;
                let lat: f64 = seq
                    .next_element()?
                    .ok_or_else(|| <A::Error as serde::de::Error>::invalid_length(1, &self))?;
                while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                Ok(Pt([lon, lat]))
            }
        }

        deserializer.deserialize_seq(PtVisitor)
    }
}

#[derive(Deserialize)]
struct GeoFile {
    features: Vec<RawFeature>,
}

#[derive(Deserialize)]
struct RawFeature {
    #[serde(default)]
    properties: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default)]
    geometry: Option<RawGeometry>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum RawGeometry {
    Point {
        coordinates: Pt,
    },
    MultiPoint {
        coordinates: Vec<Pt>,
    },
    LineString {
        coordinates: Vec<Pt>,
    },
    MultiLineString {
        coordinates: Vec<Vec<Pt>>,
    },
    Polygon {
        coordinates: Vec<Vec<Pt>>,
    },
    MultiPolygon {
        coordinates: Vec<Vec<Vec<Pt>>>,
    },
    #[serde(other)]
    Other,
}

const RANGE_ERROR: &str =
    "Координаты выходят за пределы долготы/широты. Файл должен быть в системе WGS84 (EPSG:4326).";

fn in_range(p: &[f64; 2]) -> bool {
    p[0].abs() <= 360.0 && p[1].abs() <= 90.0
}

struct Dot {
    pos: [f64; 2], // Mercator
    faded: bool,
    label: u32,
}

fn add_dot(faded: bool, label: u32, p: Pt, dots: &mut Vec<Dot>) -> Result<(), String> {
    if !in_range(&p.0) {
        return Err(RANGE_ERROR.to_string());
    }
    dots.push(Dot {
        pos: mercator(p.0[0], p.0[1]),
        faded,
        label,
    });
    Ok(())
}

fn add_line(faded: bool, label: u32, part: &[Pt], lines: &mut Vec<Line>) -> Result<(), String> {
    let lonlat: Vec<[f64; 2]> = part.iter().map(|p| p.0).collect();
    if !lonlat.iter().all(in_range) {
        return Err(RANGE_ERROR.to_string());
    }
    if let Some(mut line) = Line::build(faded, &lonlat) {
        line.label = label;
        lines.push(line);
    }
    Ok(())
}

impl RawGeometry {
    /// Первая и последняя точка геометрии (по ним определяется страна, если её нет в свойствах).
    fn ends(&self) -> Vec<[f64; 2]> {
        fn first_last(pts: &[Pt], out: &mut Vec<[f64; 2]>) {
            if let (Some(a), Some(b)) = (pts.first(), pts.last()) {
                out.push(a.0);
                if pts.len() > 1 {
                    out.push(b.0);
                }
            }
        }
        let mut out = Vec::new();
        match self {
            RawGeometry::Point { coordinates } => out.push(coordinates.0),
            RawGeometry::MultiPoint { coordinates } => first_last(coordinates, &mut out),
            RawGeometry::LineString { coordinates } => first_last(coordinates, &mut out),
            RawGeometry::MultiLineString { coordinates } | RawGeometry::Polygon { coordinates } => {
                if let Some(part) = coordinates.first() {
                    first_last(part, &mut out);
                }
            }
            RawGeometry::MultiPolygon { coordinates } => {
                if let Some(ring) = coordinates.first().and_then(|p| p.first()) {
                    first_last(ring, &mut out);
                }
            }
            RawGeometry::Other => {}
        }
        out
    }

    /// Контуры полигонов рисуются линиями.
    fn collect(
        self,
        faded: bool,
        label: u32,
        lines: &mut Vec<Line>,
        dots: &mut Vec<Dot>,
    ) -> Result<(), String> {
        match self {
            RawGeometry::Point { coordinates } => add_dot(faded, label, coordinates, dots)?,
            RawGeometry::MultiPoint { coordinates } => {
                for p in coordinates {
                    add_dot(faded, label, p, dots)?;
                }
            }
            RawGeometry::LineString { coordinates } => add_line(faded, label, &coordinates, lines)?,
            RawGeometry::MultiLineString { coordinates } | RawGeometry::Polygon { coordinates } => {
                for part in &coordinates {
                    add_line(faded, label, part, lines)?;
                }
            }
            RawGeometry::MultiPolygon { coordinates } => {
                for polygon in &coordinates {
                    for ring in polygon {
                        add_line(faded, label, ring, lines)?;
                    }
                }
            }
            RawGeometry::Other => {}
        }
        Ok(())
    }
}

/// Подпись объекта для подсказки при наведении.
pub struct Label {
    pub title: String,
    pub details: Vec<String>,
    /// Страны объекта (номера в таблице стран, см. countries.rs)
    pub countries: Vec<u16>,
}

/// Результат наведения курсора на объект; плагины слоёв кладут его в память egui,
/// а main.rs показывает одну подсказку для ближайшего объекта.
#[derive(Clone, Default)]
pub struct Hit {
    /// Номер слоя в списке и номер объекта в слое: по ним объект подсвечивается
    pub layer: usize,
    pub label: u32,
    pub dist: f32,
    pub title: String,
    pub details: Vec<String>,
    pub color: Color32,
}

/// Свойства, в которых файл может хранить страну объекта.
const COUNTRY_KEYS: [&str; 6] = [
    "CountriesOrAreas",
    "country",
    "Country",
    "COUNTRY",
    "Страна",
    "страна",
];

fn prop_text(props: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    let text = match props.get(key)? {
        serde_json::Value::String(s) => s.trim().to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => return None,
    };
    (!text.is_empty() && text != "--").then_some(text)
}

fn status_ru(status: &str) -> String {
    match status.to_lowercase().as_str() {
        "operating" => "Действует".to_string(),
        "construction" => "Строится".to_string(),
        "proposed" => "Проект".to_string(),
        "announced" => "Объявлен".to_string(),
        "idle" | "mothballed" => "Простаивает".to_string(),
        "shelved" => "Заморожен".to_string(),
        "cancelled" => "Отменён".to_string(),
        "retired" => "Выведен из эксплуатации".to_string(),
        "pre-construction" => "Подготовка к строительству".to_string(),
        "in-development" => "Разрабатывается".to_string(),
        "discovered" => "Открыто, не разрабатывается".to_string(),
        "exploration" => "Разведка".to_string(),
        "decommissioning" => "Выводится из эксплуатации".to_string(),
        "abandoned" => "Заброшено".to_string(),
        "underground gas storage" => "Подземное хранилище газа".to_string(),
        _ => status.to_string(),
    }
}

/// Строит подпись из свойств объекта: название и основные сведения.
fn make_label(props: Option<&serde_json::Map<String, serde_json::Value>>) -> Label {
    let Some(props) = props else {
        return Label {
            title: "Без названия".to_string(),
            details: Vec::new(),
            countries: Vec::new(),
        };
    };

    const TITLE_KEYS: [&str; 11] = [
        "PipelineName",
        "Name",
        "NAME_RU",
        "name:ru",
        "name",
        "NAME",
        "name:en",
        "Title",
        "title",
        "ProjectName",
        "PlantName",
    ];
    let title = TITLE_KEYS
        .iter()
        .find_map(|k| prop_text(props, k))
        .or_else(|| {
            // Любое свойство со словом name в названии, кроме служебных
            props.keys().find_map(|k| {
                let low = k.to_lowercase();
                let skip = ["owner", "other", "segment", "parent"];
                (low.contains("name") && !skip.iter().any(|w| low.contains(w)))
                    .then(|| prop_text(props, k))
                    .flatten()
            })
        })
        .or_else(|| {
            // У объектов Natural Earth без названия (железные дороги и т. п.) берём класс
            prop_text(props, "featurecla").map(|class| match class.as_str() {
                "Railroad" => "Железная дорога".to_string(),
                "Road" => "Дорога".to_string(),
                _ => class,
            })
        })
        .unwrap_or_else(|| "Без названия".to_string());

    let mut details = Vec::new();
    let mut add = |name: &str, value: Option<String>| {
        if let Some(v) = value {
            details.push(format!("{name}: {v}"));
        }
    };
    let with_units = |value: Option<String>, units: Option<String>| {
        value.map(|v| match units {
            Some(u) => format!("{v} {u}"),
            None => v,
        })
    };

    add(
        "Статус",
        prop_text(props, "Status")
            .or_else(|| prop_text(props, "status"))
            .map(|s| status_ru(&s)),
    );
    add("Ресурс", prop_text(props, "Ресурс"));
    // У добывающих объектов (рудники, месторождения) старое поле «Fuel» — это ресурс, а не
    // топливо; «Топливо» остаётся у электростанций
    let fuel_name = match content_hint(props) {
        Some((_, Category::Mining)) => "Ресурс",
        _ => "Топливо",
    };
    add(fuel_name, prop_text(props, "Fuel"));
    add(
        "Владелец",
        prop_text(props, "Owner").or_else(|| prop_text(props, "owner")),
    );
    // Поля OpenStreetMap
    add("Оператор", prop_text(props, "operator"));
    add(
        "Тип",
        prop_text(props, "industrial").map(|v| match v.as_str() {
            "refinery" => "НПЗ".to_string(),
            "oil_storage" => "Нефтебаза / терминал".to_string(),
            "gas_storage" => "Хранилище газа".to_string(),
            _ => v,
        }),
    );
    add(
        "Продукция",
        prop_text(props, "product").map(|v| match v.as_str() {
            "petroleum" => "Нефтепродукты".to_string(),
            _ => v,
        }),
    );
    add("Мощность", prop_text(props, "capacity"));
    add("Дата ввода", prop_text(props, "start_date"));
    add("Год открытия", prop_text(props, "DiscoveryYear"));
    add("Тип реактора", prop_text(props, "Reactor"));
    add("Технология", prop_text(props, "Technology"));
    add("Примечание", prop_text(props, "Note"));
    add("Река", prop_text(props, "River"));
    add("Турбин", prop_text(props, "Turbines"));
    add("Блоки", prop_text(props, "Units"));
    add("Добыча", prop_text(props, "ProductionText"));
    add("Тип добычи", prop_text(props, "ProductionType"));
    add("Размещение", prop_text(props, "Placement"));
    add("Бассейн", prop_text(props, "Basin"));
    add("Регион", prop_text(props, "Subnational"));
    add("Площадь, км²", prop_text(props, "area_km2"));
    add("Страна", prop_text(props, "CountriesOrAreas"));
    add("Сегмент", prop_text(props, "SegmentName"));
    add(
        "Диаметр",
        with_units(
            prop_text(props, "Diameter"),
            prop_text(props, "DiameterUnits"),
        ),
    );
    add(
        "Мощность",
        with_units(
            prop_text(props, "Capacity"),
            prop_text(props, "CapacityUnits"),
        ),
    );
    add("Длина, км", prop_text(props, "LengthMergedKm"));
    match (
        prop_text(props, "StartLocation"),
        prop_text(props, "EndLocation"),
    ) {
        (Some(a), Some(b)) => details.push(format!("Маршрут: {a} — {b}")),
        (Some(a), None) | (None, Some(a)) => details.push(format!("Место: {a}")),
        (None, None) => {}
    }
    // Natural Earth (железные дороги и т. п.): показываем только три поля
    if details.is_empty()
        && props.contains_key("featurecla")
        && props.contains_key("continent")
        && props.contains_key("disp_scale")
    {
        for (name, key) in [
            ("Континент", "continent"),
            ("Масштаб показа", "disp_scale"),
            ("Класс", "featurecla"),
        ] {
            if let Some(value) = prop_text(props, key) {
                details.push(format!("{name}: {value}"));
            }
        }
    }

    // Файл не из Global Energy Monitor: показываем первые простые свойства как есть
    if details.is_empty() {
        for k in props.keys() {
            if details.len() >= 6 {
                break;
            }
            if k.starts_with("name") || k == "wikidata" || k == "website" {
                continue;
            }
            if let Some(text) = prop_text(props, k) {
                if text != title && text.chars().count() <= 80 && !text.starts_with("http") {
                    details.push(format!("{k}: {text}"));
                }
            }
        }
    }

    Label {
        title,
        details,
        countries: Vec::new(),
    }
}

/// Значок и раздел по содержимому объекта. Название файла не учитывается: смотрим только
/// на поля. Признаки (по убыванию надёжности):
///  * `PipelineName` (трубопроводы GEM) и `featurecla` «Railroad» (железные дороги Natural
///    Earth) определяют раздел линейных слоёв;
///  * `industrial` — тип объекта в слоях, подготовленных скриптами ShurMap (и «refinery» у НПЗ
///    из OpenStreetMap);
///  * `TerminalName` — терминалы GEM; `Reactor` — атомные станции GEM;
///  * `Ресурс` или `Fuel` у объектов без `industrial` — месторождения нефти и газа.
fn content_hint(props: &serde_json::Map<String, serde_json::Value>) -> Option<(Icon, Category)> {
    use Category::{Industry, Mining, Pipelines, Power, Transport};
    let text = |k: &str| prop_text(props, k).map(|t| t.to_lowercase());
    let has_key = |k: &str| props.get(k).is_some_and(|v| !v.is_null());
    let has = |t: &Option<String>, keys: &[&str]| {
        t.as_deref()
            .is_some_and(|t| keys.iter().any(|k| t.contains(k)))
    };

    if has_key("PipelineName") {
        return Some((Icon::Dot, Pipelines));
    }
    if has_key("rwdb_rr_id") || has(&text("featurecla"), &["railroad"]) {
        return Some((Icon::Dot, Transport));
    }

    let kind = text("industrial");
    if kind.is_some() {
        let k = &kind;
        let hit = if has(k, &["золотой рудник"]) {
            (Icon::GoldMine, Mining)
        } else if has(k, &["медный рудник"]) {
            (Icon::CopperMine, Mining)
        } else if has(k, &["железорудн"]) {
            (Icon::IronMine, Mining)
        } else if has(k, &["угольная шахта", "угольный разрез"]) {
            (Icon::CoalMine, Mining)
        } else if has(k, &["угольный терминал"]) {
            (Icon::CoalTerminal, Industry)
        } else if has(k, &["аэропорт", "аэродром", "космодром"]) {
            (Icon::Airport, Transport)
        } else if has(k, &["порт"]) {
            (Icon::Port, Transport)
        } else if has(k, &["цемент", "помольный"]) {
            (Icon::Cement, Industry)
        } else if has(k, &["аммиак", "метанол"]) {
            (Icon::ChemN, Industry)
        } else if has(k, &["нефтехими"]) {
            (Icon::ChemPet, Industry)
        } else if has(k, &["электрометаллург", "прямого восстановления"])
        {
            (Icon::SteelEaf, Industry)
        } else if has(k, &["интегрированный комбинат", "металлургический"])
        {
            (Icon::SteelBf, Industry)
        } else if has(k, &["солнечная"]) {
            (Icon::Solar, Power)
        } else if has(k, &["ветровая"]) {
            (Icon::Wind, Power)
        } else if has(k, &["плотинная", "русловая", "гидроаккумул"]) {
            (Icon::Hydro, Power)
        } else if has(k, &["угольная"]) {
            (Icon::Coal, Power)
        } else if has(k, &["тэс", "тэц"]) {
            (Icon::Thermal, Power)
        } else if has(k, &["refinery"]) {
            (Icon::Refinery, Industry)
        } else if has(k, &["oil_storage", "gas_storage"]) {
            (Icon::Tank, Industry)
        } else {
            return None;
        };
        return Some(hit);
    }

    if has_key("TerminalName") {
        return Some((Icon::Tank, Industry));
    }
    if has_key("Reactor") {
        return Some((Icon::Nuclear, Power));
    }

    // Месторождения: «Нефть», «Газ», «Нефть и газ (преимущественно газ)» и т. п.
    let resource = text("Ресурс").or_else(|| text("Fuel"));
    if has(&resource, &["преимущественно газ"])
        || resource.as_deref().is_some_and(|t| t.starts_with("газ"))
    {
        return Some((Icon::GasField, Mining));
    }
    if resource.as_deref().is_some_and(|t| t.starts_with("нефть")) {
        return Some((Icon::OilField, Mining));
    }
    None
}

/// Сколько первых объектов слоя просматривается при выборе значка по содержимому.
const HINT_SAMPLE: usize = 200;

pub struct Data {
    lines: Vec<Line>,
    dots: Vec<Dot>,
    labels: Vec<Label>,
    /// Сколько объектов файла попало на карту
    pub objects: usize,
    /// Значок и раздел, подобранные по содержимому (если признаки нашлись)
    pub hint: Option<(Icon, Category)>,
}

fn load(path: &Path) -> Result<Data, String> {
    if !path.is_file() {
        return Err(format!("Файл не найден: {}", path.display()));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("Не удалось прочитать файл: {e}"))?;
    parse(&bytes)
}

fn parse(bytes: &[u8]) -> Result<Data, String> {
    let json = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let file: GeoFile = serde_json::from_slice(json)
        .map_err(|e| format!("Файл не похож на GeoJSON (нужен тип FeatureCollection): {e}"))?;

    let mut lines: Vec<Line> = Vec::new();
    let mut dots: Vec<Dot> = Vec::new();
    let mut labels: Vec<Label> = Vec::new();
    let mut objects = 0;
    let mut tally: Vec<((Icon, Category), usize)> = Vec::new();
    let mut sampled = 0;

    for feature in file.features {
        if sampled < HINT_SAMPLE {
            sampled += 1;
            if let Some(h) = feature.properties.as_ref().and_then(content_hint) {
                match tally.iter_mut().find(|(k, _)| *k == h) {
                    Some(entry) => entry.1 += 1,
                    None => tally.push((h, 1)),
                }
            }
        }
        let status = feature
            .properties
            .as_ref()
            .and_then(|p| p.get("Status").or_else(|| p.get("status")))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let Some(mut faded) = faded_for_status(status) else {
            continue;
        };
        // Приблизительное местоположение рисуем бледнее, как строящиеся объекты
        let approximate = feature
            .properties
            .as_ref()
            .and_then(|p| p.get("Note"))
            .and_then(|v| v.as_str())
            .is_some_and(|n| n.starts_with("Приблизительное"));
        faded |= approximate;
        let Some(geometry) = feature.geometry else {
            continue; // объект без геометрии (например, трубопровод без известного маршрута)
        };

        let ends = geometry.ends();
        let (lines_before, dots_before) = (lines.len(), dots.len());
        let label = labels.len() as u32;
        geometry.collect(faded, label, &mut lines, &mut dots)?;
        if lines.len() > lines_before || dots.len() > dots_before {
            objects += 1;
            let mut item = make_label(feature.properties.as_ref());
            let property = feature
                .properties
                .as_ref()
                .and_then(|p| COUNTRY_KEYS.iter().find_map(|k| prop_text(p, k)));
            item.countries = countries::get().resolve(property.as_deref(), &ends);
            labels.push(item);
        }
    }

    // Бледные рисуются первыми, обычные — сверху
    lines.sort_by_key(|l| !l.faded);
    dots.sort_by_key(|d| !d.faded);
    // Значок по содержимому берём, если он у большинства просмотренных объектов
    let hint = tally
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .filter(|(_, n)| *n * 2 > sampled)
        .map(|(k, _)| k);
    Ok(Data {
        lines,
        dots,
        labels,
        objects,
        hint,
    })
}

// ---------------------------------------------------------------------------
// Слой: файл + состояние загрузки
// ---------------------------------------------------------------------------

enum State {
    Idle,
    Loading(Receiver<Result<Data, String>>),
    Ready(Data),
    Failed(String),
}

/// Оставляет по одной точке на клетку `cell` пикселей: на мелком масштабе тысячи точек
/// сливаются в пятно, и рисовать каждую незачем. Точки в конце списка (обычные, не бледные)
/// имеют приоритет. Точки за пределами `area` остаются как есть.
fn thin(dots: Vec<(f32, f32, bool, u32)>, cell: f32, area: [f32; 4]) -> Vec<(f32, f32, bool, u32)> {
    let cols = (((area[2] - area[0]) / cell).ceil() as usize).max(1) + 1;
    let rows = (((area[3] - area[1]) / cell).ceil() as usize).max(1) + 1;
    let mut taken = vec![false; cols * rows];
    let mut kept: Vec<(f32, f32, bool, u32)> = Vec::with_capacity(dots.len().min(cols * rows));
    for d in dots.into_iter().rev() {
        let (cx, cy) = (
            ((d.0 - area[0]) / cell).floor(),
            ((d.1 - area[1]) / cell).floor(),
        );
        if cx >= 0.0 && cy >= 0.0 && (cx as usize) < cols && (cy as usize) < rows {
            let slot = cy as usize * cols + cx as usize;
            if taken[slot] {
                continue;
            }
            taken[slot] = true;
        }
        kept.push(d);
    }
    kept.reverse();
    kept
}

/// Добавляет в сетку белый рисунок значка.
fn add_glyph(mesh: &mut egui::Mesh, center: Pos2, radius: f32, icon: Icon) {
    let tris = icon.tris();
    let scale = radius * 0.66;
    let base = mesh.vertices.len() as u32;
    for v in tris {
        mesh.colored_vertex(
            Pos2::new(center.x + v[0] * scale, center.y + v[1] * scale),
            Color32::WHITE,
        );
    }
    for k in 0..(tris.len() / 3) as u32 {
        mesh.add_triangle(base + 3 * k, base + 3 * k + 1, base + 3 * k + 2);
    }
}

pub struct Layer {
    pub path: PathBuf,
    pub visible: bool,
    /// Номер цвета в палитре (сохраняется в настройках)
    pub color: usize,
    /// Значок точек слоя (сохраняется в настройках)
    pub icon: Icon,
    /// Раздел панели слоёв (сохраняется в настройках)
    pub category: Category,
    /// Раздел выбран вручную: по содержимому слоя его больше не подбираем
    pub category_manual: bool,
    /// Данные, вшитые в программу (тогда файл не читается)
    builtin: Option<&'static [u8]>,
    /// Значок и раздел по содержимому уже подбирались
    hinted: bool,
    state: State,
}

impl Layer {
    pub fn new(path: PathBuf, visible: bool, color: usize) -> Self {
        // Значок и раздел подбираются по содержимому, когда слой загрузится (название
        // файла не учитывается); до этого и для слоёв без признаков — точка и «Прочее»
        Self {
            path,
            visible,
            color,
            icon: Icon::Dot,
            category: Category::Other,
            category_manual: false,
            builtin: None,
            hinted: false,
            state: State::Idle,
        }
    }

    /// Слой из данных, вшитых в exe. В списке слоёв и в настройках его нет.
    pub fn builtin_borders(bytes: &'static [u8]) -> Self {
        Self {
            path: PathBuf::from("admin_0_builtin"),
            visible: true,
            color: 0,
            icon: Icon::Dot,
            category: Category::Other,
            category_manual: true,
            builtin: Some(bytes),
            hinted: true,
            state: State::Idle,
        }
    }

    /// Слой границ стран (файл Natural Earth `..._admin_0_...`): рисуется жирной тёмной
    /// линией, а границы из самой карты на это время скрываются.
    pub fn is_borders(&self) -> bool {
        self.name().to_lowercase().contains("admin_0")
    }

    /// Рисовать ли в панели значок слоя вместо цветного квадрата: у слоёв из точек — да,
    /// у линейных (трубопроводы, железные дороги) остаётся квадрат цвета.
    pub fn shows_badge(&self) -> bool {
        match self.data() {
            Some(d) => !d.dots.is_empty(),
            None => self.icon != Icon::Dot, // ещё не загружен: судим по запомненному значку
        }
    }

    /// Цвет слоя на карте и в панели.
    pub fn display_color(&self) -> Color32 {
        if self.is_borders() {
            BORDER_COLOR
        } else {
            palette_color(self.color)
        }
    }

    /// Название для кнопки: имя файла без расширения.
    pub fn name(&self) -> String {
        self.path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Запускает чтение файла в фоновом потоке (только если оно ещё не начиналось).
    pub fn start_loading(&mut self, ctx: &egui::Context) {
        if !matches!(self.state, State::Idle) {
            return;
        }
        let ctx = ctx.clone();
        let path = self.path.clone();
        let builtin = self.builtin;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(match builtin {
                Some(bytes) => parse(bytes),
                None => load(&path),
            });
            ctx.request_repaint(); // разбудить окно, чтобы показать результат
        });
        self.state = State::Loading(rx);
    }

    /// После ошибки позволяет попробовать снова (вызывается при повторном включении слоя).
    pub fn retry_if_failed(&mut self) {
        if matches!(self.state, State::Failed(_)) {
            self.state = State::Idle;
        }
    }

    /// Вызывать каждый кадр. Возвращает true, если у слоя сменились значок или раздел
    /// (тогда нужно сохранить настройки).
    pub fn poll(&mut self) -> bool {
        if let State::Loading(rx) = &self.state {
            match rx.try_recv() {
                Ok(Ok(data)) => self.state = State::Ready(data),
                Ok(Err(message)) => self.state = State::Failed(message),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.state = State::Failed("Загрузка слоя неожиданно прервалась".to_string())
                }
            }
        }
        self.apply_hint()
    }

    /// Когда данные загрузились, подбирает значок и раздел по содержимому. Раздел, выбранный
    /// вручную, не трогается; значок всегда по содержимому.
    fn apply_hint(&mut self) -> bool {
        if self.hinted {
            return false;
        }
        let Some(hint) = self.data().map(|d| d.hint) else {
            return false;
        };
        self.hinted = true;
        let Some((icon, category)) = hint else {
            return false;
        };
        let before = (self.icon, self.category);
        self.icon = icon;
        if !self.category_manual {
            self.category = category;
        }
        before != (self.icon, self.category)
    }

    /// Раздел, выбранный пользователем.
    pub fn set_category(&mut self, category: Category) {
        self.category = category;
        self.category_manual = true;
    }

    pub fn is_loading(&self) -> bool {
        matches!(self.state, State::Loading(_))
    }

    pub fn error(&self) -> Option<&str> {
        match &self.state {
            State::Failed(message) => Some(message),
            _ => None,
        }
    }

    pub fn data(&self) -> Option<&Data> {
        match &self.state {
            State::Ready(data) => Some(data),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Рисование
// ---------------------------------------------------------------------------

/// Ключ в памяти egui для результата наведения (см. `Hit`).
pub const HIT_ID: &str = "layer_hover_hit";
/// Ключ в памяти egui: положение клика или касания в этом кадре (если оно было).
pub const TAP_ID: &str = "layer_tap_pos";
/// Насколько близко (в пикселях) нужно навести курсор на линию или точку.
const LINE_HIT_PX: f32 = 7.0;
const TAP_HIT_PX: f32 = 14.0;
const DOT_BONUS_PX: f32 = 3.0;

/// Плагин карты, который рисует один слой.
pub struct LayerPlugin<'a> {
    pub data: &'a Data,
    pub color: Color32,
    /// Границы стран: жирная линия, без подсказок
    pub borders: bool,
    /// Номер слоя в списке (для подсветки выбранного объекта)
    pub layer_id: usize,
    /// Объект этого слоя, по которому нажали: рисуется выделенным
    pub selected: Option<u32>,
    /// Общий фильтр по странам (у слоя границ не применяется)
    pub filter: &'a Filter,
    /// Значок точек слоя
    pub icon: Icon,
    /// Рисовать линии пунктиром (транспортные слои: железные дороги и т. п.)
    pub dashed: bool,
}

/// Ключи в памяти egui: время подготовки слоёв за кадр (накапливается / последнее значение).
pub const MS_ACC_ID: &str = "layers_ms_acc";
pub const MS_SHOWN_ID: &str = "layers_ms_shown";
/// То же для разбивки по частям: [мс на линии, мс на точки, точек линий, точек на карте].
pub const PARTS_ACC_ID: &str = "layers_parts_acc";
pub const PARTS_SHOWN_ID: &str = "layers_parts_shown";

/// Если точек на экране больше, рисуются простые кружки (значки были бы слишком тяжёлыми).
const ICON_LIMIT: usize = 1500;
const ICON_RADIUS: f32 = 10.0;
/// Радиус простой точки (когда значки не рисуются).
const DOT_RADIUS: f32 = 4.0;
/// Размер клетки спрайта в логических пикселях: радиус, белое кольцо вокруг и запас.
const ICON_CELL: f32 = 24.0;
const DOT_CELL: f32 = 12.0;
const ATLAS_COLS: usize = 8;

/// Готовые картинки значков и точек. Каждый значок один раз рисуется в текстуру
/// (с учётом плотности пикселей экрана), а на карте выводится двумя прямоугольниками:
/// круг цвета слоя и белые кольцо с рисунком. Это в десятки раз легче, чем собирать
/// значок из сотен треугольников для каждой точки в каждом кадре.
#[derive(Clone)]
struct Sprites {
    ppp: f32,
    icon_tex: egui::TextureHandle,
    icon_n: usize,
    dot_tex: egui::TextureHandle,
    dot_n: usize,
    /// Профиль сглаживания поперёк линии: строка 0 для линий толщиной `LINE_THICK`, строка 1
    /// для `LINE_THIN`
    line_tex: egui::TextureHandle,
    /// Пунктирные профили для толстых и тонких линий (повторяются вдоль линии)
    dash_thick_tex: egui::TextureHandle,
    dash_thin_tex: egui::TextureHandle,
    /// Половина полной ширины ленты (с краем сглаживания) для толстых и тонких линий
    half_thick: f32,
    half_thin: f32,
}

/// Толщина обычных линий и бледных (и границ стран) в логических пикселях.
const LINE_THICK: f32 = 2.0;
const LINE_THIN: f32 = 1.5;
const LINE_TEXELS: usize = 64;
/// Пунктир: штрих и период в логических пикселях.
const DASH_ON: f32 = 8.0;
const DASH_PERIOD: f32 = 12.0;

/// Сетка, в которую `strokes::strip` складывает ленты линий.
struct MeshSink<'a> {
    mesh: &'a mut egui::Mesh,
    color: Color32,
    /// Строка текстуры-профиля сплошной линии (0.25 — толстая, 0.75 — тонкая)
    row: f32,
    /// Период пунктира в пикселях; 0 — линия сплошная
    period: f32,
}

impl strokes::Sink for MeshSink<'_> {
    fn vertex(&mut self, x: f32, y: f32, across: f32, along: f32) -> u32 {
        let v = if self.period > 0.0 {
            along / self.period
        } else {
            self.row
        };
        self.mesh.vertices.push(egui::epaint::Vertex {
            pos: Pos2::new(x, y),
            uv: Pos2::new(across, v),
            color: self.color,
        });
        (self.mesh.vertices.len() - 1) as u32
    }

    fn triangle(&mut self, a: u32, b: u32, c: u32) {
        self.mesh.indices.extend_from_slice(&[a, b, c]);
    }
}

fn upload(
    ctx: &egui::Context,
    name: &str,
    cell: f32,
    n: usize,
    cells: &[Vec<Prim>],
) -> egui::TextureHandle {
    let (w, h, rgba) = raster::build_atlas(cell, n, ATLAS_COLS, cells);
    let image = egui::ColorImage::from_rgba_premultiplied([w, h], &rgba);
    ctx.load_texture(name, image, egui::TextureOptions::LINEAR)
}

/// Спрайты для текущего масштаба экрана (создаются при первом обращении и при смене масштаба).
fn sprites(ctx: &egui::Context) -> Sprites {
    let id = egui::Id::new("layer_sprites");
    let ppp = ctx.pixels_per_point();
    if let Some(s) = ctx.data(|d| d.get_temp::<Sprites>(id)) {
        if (s.ppp - ppp).abs() < 0.01 {
            return s;
        }
    }
    // Клетка 0 — круг, остальные — белое кольцо с рисунком значка (по номеру значка)
    let icon_n = raster::cell_pixels(ICON_CELL, ppp);
    let mut icon_cells = vec![vec![Prim::Disk(ICON_RADIUS)]];
    for icon in crate::icons::ALL {
        icon_cells.push(vec![
            Prim::Ring(ICON_RADIUS, ICON_RADIUS + 1.0),
            Prim::Tris(icon.tris(), ICON_RADIUS * 0.66),
        ]);
    }
    let dot_n = raster::cell_pixels(DOT_CELL, ppp);
    let dot_cells = vec![
        vec![Prim::Disk(DOT_RADIUS)],
        vec![Prim::Ring(DOT_RADIUS, DOT_RADIUS + 1.0)],
    ];
    // Сглаживание края линии — один физический пиксель, как у egui
    let feather = 1.0 / ppp;
    let mut ramp = Vec::with_capacity(LINE_TEXELS * 2 * 4);
    for width in [LINE_THICK, LINE_THIN] {
        for a in raster::line_ramp(width, feather, LINE_TEXELS) {
            ramp.extend_from_slice(&[a, a, a, a]);
        }
    }
    let ramp_image = egui::ColorImage::from_rgba_premultiplied([LINE_TEXELS, 2], &ramp);
    // Пунктир: четыре строки на физический пиксель вдоль периода, текстура повторяется
    let dash_rows = ((DASH_PERIOD * ppp * 4.0).round() as usize).max(8);
    let repeat = egui::TextureOptions {
        wrap_mode: egui::TextureWrapMode::Repeat,
        ..egui::TextureOptions::LINEAR
    };
    let dash_tex = |name: &str, width: f32| {
        let alpha = raster::dash_ramp(width, feather, LINE_TEXELS, DASH_ON, DASH_PERIOD, dash_rows);
        let mut px = Vec::with_capacity(alpha.len() * 4);
        for a in alpha {
            px.extend_from_slice(&[a, a, a, a]);
        }
        let image = egui::ColorImage::from_rgba_premultiplied([LINE_TEXELS, dash_rows], &px);
        ctx.load_texture(name, image, repeat)
    };
    let s = Sprites {
        ppp,
        line_tex: ctx.load_texture("layer_lines", ramp_image, egui::TextureOptions::LINEAR),
        dash_thick_tex: dash_tex("layer_dash_thick", LINE_THICK),
        dash_thin_tex: dash_tex("layer_dash_thin", LINE_THIN),
        half_thick: raster::line_half(LINE_THICK, feather),
        half_thin: raster::line_half(LINE_THIN, feather),
        icon_tex: upload(ctx, "layer_icons", ICON_CELL, icon_n, &icon_cells),
        icon_n,
        dot_tex: upload(ctx, "layer_dots", DOT_CELL, dot_n, &dot_cells),
        dot_n,
    };
    ctx.data_mut(|d| d.insert_temp(id, s.clone()));
    s
}

/// Значок слоя в панели: круг цвета слоя с белым кольцом и рисунком. `size` — сторона
/// клетки значка в логических пикселях (у самого значка вокруг есть небольшой запас).
pub fn paint_badge(ui: &egui::Ui, center: Pos2, size: f32, icon: Icon, fill: Color32) {
    let sprites = sprites(ui.ctx());
    let total = 1 + crate::icons::ALL.len();
    let uv = |k: usize| {
        let u = raster::cell_uv(k, sprites.icon_n, ATLAS_COLS, total);
        egui::Rect::from_min_max(Pos2::new(u[0], u[1]), Pos2::new(u[2], u[3]))
    };
    let ppp = sprites.ppp;
    let center = Pos2::new(
        (center.x * ppp).round() / ppp,
        (center.y * ppp).round() / ppp,
    );
    let rect = egui::Rect::from_center_size(center, egui::Vec2::splat(size));
    let mut mesh = egui::Mesh::with_texture(sprites.icon_tex.id());
    mesh.add_rect_with_uv(rect, uv(0), fill);
    mesh.add_rect_with_uv(rect, uv(icon as usize + 1), Color32::WHITE);
    ui.painter().add(Shape::mesh(mesh));
}

impl Plugin for LayerPlugin<'_> {
    fn run(
        self: Box<Self>,
        ui: &mut egui::Ui,
        response: &egui::Response,
        projector: &Projector,
        _map_memory: &MapMemory,
    ) {
        let started = std::time::Instant::now();
        let rect = ui.max_rect();
        let Some(t) = Affine::fit(|lon, lat| {
            let v = projector.project(lon_lat(lon, lat));
            (v.x as f64, v.y as f64)
        }) else {
            return;
        };

        let window = [rect.left(), rect.top(), rect.right(), rect.bottom()];
        let view = t.visible(window, 50.0);
        let clip = [
            window[0] - 40.0,
            window[1] - 40.0,
            window[2] + 40.0,
            window[3] + 40.0,
        ];
        let max_tol = 0.7 / t.sx; // 0.7 пикселя в единицах Mercator
        let painter = ui.painter_at(rect);
        let sprites = sprites(ui.ctx());

        // Курсор над картой (но не во время перетаскивания и не над кнопками)
        // Касание или клик (его положение кладёт в память main.rs) ловится с запасом побольше,
        // потому что палец менее точен, чем курсор
        let tap = ui.ctx().data(|d| d.get_temp::<Pos2>(egui::Id::new(TAP_ID)));
        let (pointer, reach) = if self.borders {
            (None, LINE_HIT_PX) // у границ стран подсказок нет
        } else if let Some(tap) = tap {
            (Some(tap), TAP_HIT_PX)
        } else if response.hovered() && !response.dragged() {
            (ui.input(|i| i.pointer.hover_pos()), LINE_HIT_PX)
        } else {
            (None, LINE_HIT_PX)
        };
        let mut best: Option<(f32, u32)> = None; // (расстояние в пикселях, номер подписи)
        let mut line_pts = 0usize; // сколько точек линий отдано на рисование (для подсказки)

        let solid = self.color;
        let pale = Color32::from_rgba_unmultiplied(solid.r(), solid.g(), solid.b(), 110);

        let filtering = !self.borders && self.filter.is_active();
        // Какие объекты проходят фильтр: считается один раз за кадр
        let pass: Vec<bool> = if filtering {
            self.data
                .labels
                .iter()
                .map(|l| self.filter.passes(&l.countries))
                .collect()
        } else {
            Vec::new()
        };
        let shown = |label: u32| !filtering || pass.get(label as usize).copied().unwrap_or(false);

        // Линии рисуются двумя сетками: тонкие (бледные, границы стран) и обычные. Бледные идут
        // первыми, чтобы обычные оказались сверху. Подробнее — в модуле strokes.
        let caps_id = egui::Id::new(("line_caps", self.layer_id));
        let caps = ui
            .ctx()
            .data(|d| d.get_temp::<[usize; 2]>(caps_id))
            .unwrap_or([0, 0]);
        let (thin_tex, thick_tex, period) = if self.dashed {
            (
                sprites.dash_thin_tex.id(),
                sprites.dash_thick_tex.id(),
                DASH_PERIOD,
            )
        } else {
            (sprites.line_tex.id(), sprites.line_tex.id(), 0.0)
        };
        let mut pale_mesh = egui::Mesh::with_texture(thin_tex);
        let mut solid_mesh = egui::Mesh::with_texture(thick_tex);
        for (mesh, cap) in [(&mut pale_mesh, caps[0]), (&mut solid_mesh, caps[1])] {
            mesh.vertices.reserve(cap + cap / 8);
            mesh.indices.reserve(3 * (cap + cap / 8));
        }
        let thin_color = if self.borders { solid } else { pale };

        for line in &self.data.lines {
            if !line.overlaps(view) || !shown(line.label) {
                continue;
            }
            // Маршрут короче полутора пикселей не виден: не тратим на него время
            if (line.max[0] - line.min[0]) * t.sx < 1.5 && (line.max[1] - line.min[1]) * t.sy < 1.5
            {
                continue;
            }
            let is_thin = self.borders || line.faded;
            visible_runs(line.lod(max_tol), &t, clip, Pos2::new, |run| {
                line_pts += run.len();
                if let Some(p) = pointer {
                    for w in run.windows(2) {
                        let d = seg_distance((p.x, p.y), (w[0].x, w[0].y), (w[1].x, w[1].y));
                        if d <= reach && best.is_none_or(|b| d < b.0) {
                            best = Some((d, line.label));
                        }
                    }
                }
                if is_thin {
                    let mut sink = MeshSink {
                        mesh: &mut pale_mesh,
                        color: thin_color,
                        row: 0.75,
                        period,
                    };
                    strokes::strip(run, |p| (p.x, p.y), sprites.half_thin, &mut sink);
                } else {
                    let mut sink = MeshSink {
                        mesh: &mut solid_mesh,
                        color: solid,
                        row: 0.25,
                        period,
                    };
                    strokes::strip(run, |p| (p.x, p.y), sprites.half_thick, &mut sink);
                }
            });
        }
        ui.ctx().data_mut(|d| {
            d.insert_temp(
                caps_id,
                [pale_mesh.vertices.len(), solid_mesh.vertices.len()],
            );
        });
        if !pale_mesh.is_empty() {
            painter.add(Shape::mesh(pale_mesh));
        }
        if !solid_mesh.is_empty() {
            painter.add(Shape::mesh(solid_mesh));
        }

        let lines_ms = started.elapsed().as_secs_f32() * 1000.0;
        let mut visible: Vec<(f32, f32, bool, u32)> = Vec::new();
        for dot in &self.data.dots {
            let [x, y] = dot.pos;
            if x < view[0] || x > view[2] || y < view[1] || y > view[3] || !shown(dot.label) {
                continue;
            }
            let (sx, sy) = t.screen(&dot.pos);
            visible.push((sx, sy, dot.faded, dot.label));
        }
        let want_icons = !self.borders && self.icon != Icon::Dot;
        let glyphs = want_icons && visible.len() <= ICON_LIMIT;
        // Густые точки прореживаем: на экране всё равно видно только пятно
        if glyphs {
            if visible.len() > 250 {
                visible = thin(visible, 2.0 * ICON_RADIUS, clip);
            }
        } else if visible.len() > 600 {
            visible = thin(visible, 7.0, clip);
        }
        let radius = if glyphs { ICON_RADIUS } else { DOT_RADIUS };
        let ppp = sprites.ppp;
        let (tex, n, cell, total, top) = if glyphs {
            (
                sprites.icon_tex.id(),
                sprites.icon_n,
                ICON_CELL,
                1 + crate::icons::ALL.len(),
                self.icon as usize + 1,
            )
        } else {
            (sprites.dot_tex.id(), sprites.dot_n, DOT_CELL, 2, 1)
        };
        let uv = |k: usize| {
            let u = raster::cell_uv(k, n, ATLAS_COLS, total);
            egui::Rect::from_min_max(Pos2::new(u[0], u[1]), Pos2::new(u[2], u[3]))
        };
        let (uv_disk, uv_top) = (uv(0), uv(top));
        let size = egui::Vec2::splat(cell);
        let mut mesh = egui::Mesh::with_texture(tex);
        mesh.reserve_vertices(visible.len() * 8);
        mesh.reserve_triangles(visible.len() * 4);
        for &(sx, sy, faded, label) in &visible {
            let fill = if faded { pale } else { solid };
            // Центр совмещаем с пикселями экрана: картинка получается чёткой
            let center = Pos2::new((sx * ppp).round() / ppp, (sy * ppp).round() / ppp);
            let rect = egui::Rect::from_center_size(center, size);
            mesh.add_rect_with_uv(rect, uv_disk, fill);
            mesh.add_rect_with_uv(rect, uv_top, Color32::WHITE);
            if let Some(p) = pointer {
                // Точки легче поймать, чем линии: вычитаем небольшой бонус
                let d = ((p.x - sx).powi(2) + (p.y - sy).powi(2)).sqrt()
                    - DOT_BONUS_PX
                    - (radius - 4.0);
                if d <= reach && best.is_none_or(|b| d < b.0) {
                    best = Some((d, label));
                }
            }
        }
        let drawn_dots = visible.len();
        if !mesh.is_empty() {
            painter.add(Shape::mesh(mesh));
        }
        let dots_ms = started.elapsed().as_secs_f32() * 1000.0 - lines_ms;

        // Выбранный нажатием объект: белый ореол и более толстая линия поверх остальных
        if let Some(sel) = self.selected {
            let halo = Stroke::new(8.0, Color32::from_white_alpha(235));
            let main = Stroke::new(4.5, solid);
            for line in self.data.lines.iter().filter(|l| l.label == sel) {
                if !line.overlaps(view) {
                    continue;
                }
                visible_runs(line.lod(max_tol), &t, clip, Pos2::new, |run| {
                    painter.add(Shape::line(run.to_vec(), halo));
                    painter.add(Shape::line(run.to_vec(), main));
                });
            }
            for dot in self.data.dots.iter().filter(|d| d.label == sel) {
                let [x, y] = dot.pos;
                if x < view[0] || x > view[2] || y < view[1] || y > view[3] {
                    continue;
                }
                let (sx, sy) = t.screen(&dot.pos);
                painter.circle(
                    Pos2::new(sx, sy),
                    radius + 5.0,
                    solid,
                    Stroke::new(3.0, Color32::WHITE),
                );
                if glyphs {
                    let mut m = egui::Mesh::default();
                    add_glyph(&mut m, Pos2::new(sx, sy), radius + 5.0, self.icon);
                    painter.add(Shape::mesh(m));
                }
            }
        }

        if let Some((dist, index)) = best {
            if let Some(label) = self.data.labels.get(index as usize) {
                let id = egui::Id::new(HIT_ID);
                ui.ctx().data_mut(|data| {
                    let better = data.get_temp::<Hit>(id).is_none_or(|h| dist < h.dist);
                    if better {
                        data.insert_temp(
                            id,
                            Hit {
                                layer: self.layer_id,
                                label: index,
                                dist,
                                title: label.title.clone(),
                                details: label.details.clone(),
                                color: solid,
                            },
                        );
                    }
                });
            }
        }

        // Сколько времени ушло на подготовку слоёв (показывается в подсказке заголовка «Слои»)
        let ms = started.elapsed().as_secs_f32() * 1000.0;
        ui.ctx().data_mut(|d| {
            let id = egui::Id::new(MS_ACC_ID);
            let total = d.get_temp::<f32>(id).unwrap_or(0.0) + ms;
            d.insert_temp(id, total);
            let id = egui::Id::new(PARTS_ACC_ID);
            let mut parts = d.get_temp::<[f32; 4]>(id).unwrap_or([0.0; 4]);
            parts[0] += lines_ms;
            parts[1] += dots_ms;
            parts[2] += line_pts as f32;
            parts[3] += drawn_dots as f32;
            d.insert_temp(id, parts);
        });
    }
}

/// Ключ в памяти egui: ширина всего мира в пикселях при текущем масштабе (см. `FitProbe`).
pub const WORLD_PX_ID: &str = "map_world_px";

/// Невидимый плагин: измеряет, сколько пикселей занимает весь мир при текущем масштабе.
/// По этому измерению main.rs подбирает стартовый масштаб, не полагаясь на размер тайлов.
pub struct FitProbe;

impl Plugin for FitProbe {
    fn run(
        self: Box<Self>,
        ui: &mut egui::Ui,
        _response: &egui::Response,
        projector: &Projector,
        _map_memory: &MapMemory,
    ) {
        if let Some(t) = Affine::fit(|lon, lat| {
            let v = projector.project(lon_lat(lon, lat));
            (v.x as f64, v.y as f64)
        }) {
            let world_px = t.sx as f32;
            ui.ctx()
                .data_mut(|d| d.insert_temp(egui::Id::new(WORLD_PX_ID), world_px));
        }
    }
}

#[cfg(test)]
mod icon_tests {
    use super::*;

    fn hint(json: &str) -> Option<(Icon, Category)> {
        parse(json.as_bytes()).unwrap().hint
    }

    fn point(props: &str) -> String {
        format!(
            r#"{{"type":"Feature","properties":{props},"geometry":{{"type":"Point","coordinates":[10.0,50.0]}}}}"#
        )
    }

    fn line(props: &str) -> String {
        format!(
            r#"{{"type":"Feature","properties":{props},"geometry":{{"type":"LineString","coordinates":[[10.0,50.0],[11.0,51.0]]}}}}"#
        )
    }

    fn collection(features: &[String]) -> String {
        format!(
            r#"{{"type":"FeatureCollection","features":[{}]}}"#,
            features.join(",")
        )
    }

    /// Слой из одного объекта с такими свойствами: какой значок и раздел определились.
    fn one(props: &str) -> Option<(Icon, Category)> {
        hint(&collection(&[point(props)]))
    }

    #[test]
    fn by_industrial() {
        use Category::*;
        let t = |kind: &str| {
            one(&format!(
                r#"{{"Name":"A","Status":"operating","industrial":"{kind}"}}"#
            ))
        };
        assert_eq!(t("Золотой рудник"), Some((Icon::GoldMine, Mining)));
        assert_eq!(t("Медный рудник"), Some((Icon::CopperMine, Mining)));
        assert_eq!(
            t("Железорудная шахта или карьер"),
            Some((Icon::IronMine, Mining))
        );
        assert_eq!(t("Угольная шахта"), Some((Icon::CoalMine, Mining)));
        assert_eq!(t("Угольный разрез"), Some((Icon::CoalMine, Mining)));
        assert_eq!(
            t("Угольный терминал (импорт)"),
            Some((Icon::CoalTerminal, Industry))
        );
        assert_eq!(t("Крупный аэропорт"), Some((Icon::Airport, Transport)));
        assert_eq!(t("Космодром"), Some((Icon::Airport, Transport)));
        assert_eq!(t("Морской или речной порт"), Some((Icon::Port, Transport)));
        assert_eq!(
            t("Цементный завод полного цикла (обжиг клинкера и помол)"),
            Some((Icon::Cement, Industry))
        );
        assert_eq!(
            t("Помольный завод (цемент из привозного клинкера)"),
            Some((Icon::Cement, Industry))
        );
        assert_eq!(
            t("Производство аммиака и метанола"),
            Some((Icon::ChemN, Industry))
        );
        assert_eq!(
            t("Нефтехимия: олефины и ароматика"),
            Some((Icon::ChemPet, Industry))
        );
        assert_eq!(
            t("Электрометаллургический завод"),
            Some((Icon::SteelEaf, Industry))
        );
        assert_eq!(
            t("Завод прямого восстановления железа"),
            Some((Icon::SteelEaf, Industry))
        );
        assert_eq!(
            t("Интегрированный комбинат (доменные печи)"),
            Some((Icon::SteelBf, Industry))
        );
        assert_eq!(
            t("Металлургический завод (тип не уточнён)"),
            Some((Icon::SteelBf, Industry))
        );
        assert_eq!(t("Солнечная электростанция"), Some((Icon::Solar, Power)));
        assert_eq!(
            t("Ветровая электростанция (морская)"),
            Some((Icon::Wind, Power))
        );
        assert_eq!(
            t("плотинная (с водохранилищем)"),
            Some((Icon::Hydro, Power))
        );
        assert_eq!(t("гидроаккумулирующая (ГАЭС)"), Some((Icon::Hydro, Power)));
        assert_eq!(t("Угольная ТЭС"), Some((Icon::Coal, Power)));
        assert_eq!(
            t("Угольная ТЭЦ (с выработкой тепла)"),
            Some((Icon::Coal, Power))
        );
        assert_eq!(t("ТЭС"), Some((Icon::Thermal, Power)));
        assert_eq!(t("ТЭЦ (с выработкой тепла)"), Some((Icon::Thermal, Power)));
        assert_eq!(t("refinery"), Some((Icon::Refinery, Industry)));
        assert_eq!(t("oil_storage"), Some((Icon::Tank, Industry)));
        assert_eq!(t("gas_storage"), Some((Icon::Tank, Industry)));
        // Неизвестный тип: подсказки нет
        assert_eq!(t("что-то своё"), None);
    }

    #[test]
    fn by_other_fields() {
        use Category::*;
        assert_eq!(
            one(r#"{"Name":"A","TerminalName":"Abadi LNG Terminal","Fuel":"LNG"}"#),
            Some((Icon::Tank, Industry))
        );
        assert_eq!(
            one(r#"{"Name":"A","Reactor":"PWR","Units":"2"}"#),
            Some((Icon::Nuclear, Power))
        );
        // Месторождения: поле «Ресурс» и прежнее «Fuel»
        let f = |v: &str| one(&format!(r#"{{"Name":"A","Ресурс":"{v}"}}"#));
        assert_eq!(f("Нефть"), Some((Icon::OilField, Mining)));
        assert_eq!(f("Нефть и газ"), Some((Icon::OilField, Mining)));
        assert_eq!(
            f("Нефть и газ (преимущественно нефть)"),
            Some((Icon::OilField, Mining))
        );
        assert_eq!(f("Газ"), Some((Icon::GasField, Mining)));
        assert_eq!(f("Газ и конденсат"), Some((Icon::GasField, Mining)));
        assert_eq!(
            f("Нефть и газ (преимущественно газ)"),
            Some((Icon::GasField, Mining))
        );
        assert_eq!(
            one(r#"{"Name":"A","Fuel":"Газ"}"#),
            Some((Icon::GasField, Mining))
        );
    }

    #[test]
    fn lines() {
        use Category::*;
        let pipe =
            line(r#"{"PipelineName":"Double E Pipeline","Fuel":"Gas","Status":"operating"}"#);
        assert_eq!(hint(&collection(&[pipe])), Some((Icon::Dot, Pipelines)));
        let rail = line(r#"{"featurecla":"Railroad","scalerank":10}"#);
        assert_eq!(hint(&collection(&[rail])), Some((Icon::Dot, Transport)));
    }

    #[test]
    fn unknown_and_mixed() {
        // Чужой файл без признаков: подсказки нет
        assert_eq!(one(r#"{"Name":"A","Status":"operating"}"#), None);
        // Смесь без явного большинства: подсказки нет
        let a = point(r#"{"Name":"A","industrial":"Золотой рудник"}"#);
        let b = point(r#"{"Name":"B","industrial":"Медный рудник"}"#);
        assert_eq!(hint(&collection(&[a, b])), None);
        // Название файла роли не играет: слой «Золото» без содержимого остаётся точкой
        let layer = Layer::new(PathBuf::from("Золото.geojson"), true, 0);
        assert_eq!((layer.icon, layer.category), (Icon::Dot, Category::Other));
    }

    #[test]
    fn fuel_is_resource_for_mining() {
        let details = |json: &str| {
            let v: serde_json::Value = serde_json::from_str(json).unwrap();
            make_label(v.as_object()).details
        };
        let mine = details(r#"{"Name":"A","industrial":"Золотой рудник","Fuel":"золото"}"#);
        assert!(mine.contains(&"Ресурс: золото".to_string()));
        let field = details(r#"{"Name":"A","Fuel":"Нефть"}"#);
        assert!(field.contains(&"Ресурс: Нефть".to_string()));
        let plant = details(r#"{"Name":"A","industrial":"ТЭС","Fuel":"природный газ"}"#);
        assert!(plant.contains(&"Топливо: природный газ".to_string()));
    }

    #[test]
    fn hint_respects_manual_choice() {
        let gold = point(r#"{"Name":"A","industrial":"Золотой рудник"}"#);
        let data = parse(collection(&[gold]).as_bytes()).unwrap();
        let mut layer = Layer::new(PathBuf::from("x.geojson"), true, 0);
        layer.set_category(Category::Industry);
        layer.state = State::Ready(data);
        assert!(layer.apply_hint());
        assert_eq!(layer.icon, Icon::GoldMine);
        assert_eq!(layer.category, Category::Industry); // выбранный вручную раздел сохранён
    }
}
