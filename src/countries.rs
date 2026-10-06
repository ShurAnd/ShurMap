//! Страны и континенты: определение страны объекта и общий фильтр по странам.
//!
//! Границы лежат в `assets/countries_lite.json` (упрощённые, делает tools/make_countries.py)
//! и вшиты в exe. Страна объекта берётся так: сначала из свойства файла
//! (`CountriesOrAreas`, `country`, `Country`...), а если его нет или название не
//! распознано — по координатам (точка в полигоне). Список «исправлений» (overrides)
//! проверяется раньше обычных границ: так Крым и четыре области попадают в Россию.

use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

static RAW: &[u8] = include_bytes!("../assets/countries_lite.json");

type Ring = Vec<[f64; 2]>;
type Polygon = Vec<Ring>; // первое кольцо — внешнее, остальные — дыры

#[derive(Deserialize)]
struct RawCountry {
    code: String,
    ru: String,
    en: String,
    cont: String,
    polys: Vec<Polygon>,
}

#[derive(Deserialize)]
struct RawOverride {
    code: String,
    polys: Vec<Polygon>,
}

#[derive(Deserialize)]
struct RawFile {
    countries: Vec<RawCountry>,
    overrides: Vec<RawOverride>,
}

struct Shape {
    bbox: [f64; 4], // min_x, min_y, max_x, max_y
    rings: Polygon,
}

pub struct Country {
    pub code: String,
    pub ru: String,
    pub continent: usize, // индекс в CONTINENTS
    /// Не показывается в списке: вместо неё две части России
    pub hidden: bool,
    shapes: Vec<Shape>,
}

/// Континенты в том порядке, как они показываются в панели.
pub const CONTINENTS: [&str; 8] = [
    "Европа",
    "Азия",
    "Африка",
    "Северная Америка",
    "Южная Америка",
    "Австралия и Океания",
    "Антарктида",
    "Прочее",
];

fn continent_index(name: &str) -> usize {
    match name {
        "Europe" => 0,
        "Asia" => 1,
        "Africa" => 2,
        "North America" => 3,
        "South America" => 4,
        "Oceania" => 5,
        "Antarctica" => 6,
        _ => 7,
    }
}

pub struct Countries {
    pub list: Vec<Country>, // по алфавиту (русскому)
    overrides: Vec<(usize, Vec<Shape>)>,
    by_name: HashMap<String, usize>,
    /// Россия целиком и две её части (по Уралу)
    rus: Option<[usize; 3]>,
}

/// Граница Европы и Азии внутри России: долгота Уральских гор (приблизительно).
const URAL_LON: f64 = 60.0;

pub const RUS_EU: &str = "RUS-EU";
pub const RUS_AS: &str = "RUS-AS";

/// Номер «страна не определена» — для объектов без свойства и вне всех границ (море).
pub const UNKNOWN: u16 = u16::MAX;

fn shapes_of(polys: Vec<Polygon>) -> Vec<Shape> {
    polys
        .into_iter()
        .filter(|p| !p.is_empty())
        .map(|rings| {
            let mut bbox = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
            for p in &rings[0] {
                bbox[0] = bbox[0].min(p[0]);
                bbox[1] = bbox[1].min(p[1]);
                bbox[2] = bbox[2].max(p[0]);
                bbox[3] = bbox[3].max(p[1]);
            }
            Shape { bbox, rings }
        })
        .collect()
}

fn in_ring(ring: &[[f64; 2]], x: f64, y: f64) -> bool {
    let mut inside = false;
    let mut j = ring.len().wrapping_sub(1);
    for i in 0..ring.len() {
        let (a, b) = (ring[i], ring[j]);
        if (a[1] > y) != (b[1] > y) && x < (b[0] - a[0]) * (y - a[1]) / (b[1] - a[1]) + a[0] {
            inside = !inside;
        }
        j = i;
    }
    inside
}

fn in_shape(s: &Shape, x: f64, y: f64) -> bool {
    if x < s.bbox[0] || x > s.bbox[2] || y < s.bbox[1] || y > s.bbox[3] {
        return false;
    }
    in_ring(&s.rings[0], x, y) && !s.rings[1..].iter().any(|h| in_ring(h, x, y))
}

/// Приводит название к виду для сравнения: нижний регистр, без «ё», кавычек и лишних пробелов.
pub fn norm(s: &str) -> String {
    s.trim()
        .to_lowercase()
        .replace('ё', "е")
        .replace(['’', '`'], "'")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Написания, которых нет в таблице Natural Earth: (название, код страны).
/// Тайвань и Косово в нашем файле границ входят в Китай и Сербию.
const ALIASES: &[(&str, &str)] = &[
    ("türkiye", "TUR"),
    ("turkiye", "TUR"),
    ("turkey", "TUR"),
    ("китай", "CHN"),
    ("china", "CHN"),
    ("taiwan", "CHN"),
    ("тайвань", "CHN"),
    ("китайская республика (тайвань)", "CHN"),
    ("китайская республика", "CHN"),
    ("dr congo", "COD"),
    ("congo, dem. rep.", "COD"),
    ("democratic republic of the congo", "COD"),
    ("др конго", "COD"),
    ("congo", "COG"),
    ("republic of the congo", "COG"),
    ("kosovo", "SRB"),
    ("косово", "SRB"),
    ("оаэ", "ARE"),
    ("uae", "ARE"),
    ("united arab emirates", "ARE"),
    ("réunion", "FRA"),
    ("reunion", "FRA"),
    ("martinique", "FRA"),
    ("guadeloupe", "FRA"),
    ("french guiana", "FRA"),
    ("кот-д'ивуар", "CIV"),
    ("ivory coast", "CIV"),
    ("south korea", "KOR"),
    ("korea, south", "KOR"),
    ("republic of korea", "KOR"),
    ("north korea", "PRK"),
    ("korea, north", "PRK"),
    ("czech republic", "CZE"),
    ("russian federation", "RUS"),
    ("russia", "RUS"),
    ("usa", "USA"),
    ("united states", "USA"),
    ("united states of america", "USA"),
    ("сша", "USA"),
    ("uk", "GBR"),
    ("united kingdom", "GBR"),
    ("great britain", "GBR"),
    ("великобритания", "GBR"),
    ("бирма", "MMR"),
    ("burma", "MMR"),
    ("myanmar", "MMR"),
    ("swaziland", "SWZ"),
    ("eswatini", "SWZ"),
    ("macedonia", "MKD"),
    ("north macedonia", "MKD"),
    ("bosnia and herzegovina", "BIH"),
    ("bosnia & herzegovina", "BIH"),
    ("dominican republic", "DOM"),
    ("south sudan", "SDS"),
    ("equatorial guinea", "GNQ"),
    ("central african republic", "CAF"),
    ("vietnam", "VNM"),
    ("viet nam", "VNM"),
    ("laos", "LAO"),
    ("syria", "SYR"),
    ("iran", "IRN"),
    ("moldova", "MDA"),
    ("tanzania", "TZA"),
    ("bolivia", "BOL"),
    ("venezuela", "VEN"),
    ("brunei", "BRN"),
    ("cape verde", "CPV"),
    ("east timor", "TLS"),
    ("timor-leste", "TLS"),
    ("gambia", "GMB"),
    ("the gambia", "GMB"),
    ("bahamas", "BHS"),
    ("the bahamas", "BHS"),
];

static COUNTRIES: OnceLock<Countries> = OnceLock::new();

/// Таблица стран; читается из вшитого файла при первом обращении (около 1,3 МБ JSON).
pub fn get() -> &'static Countries {
    COUNTRIES.get_or_init(|| Countries::from_bytes(RAW).expect("countries_lite.json повреждён"))
}

impl Countries {
    fn from_bytes(bytes: &[u8]) -> Result<Countries, String> {
        let raw: RawFile = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        let mut list: Vec<Country> = Vec::new();
        let mut names: Vec<(String, String)> = Vec::new(); // (название, код)
        for c in raw.countries {
            names.push((norm(&c.en), c.code.clone()));
            names.push((norm(&c.ru), c.code.clone()));
            list.push(Country {
                code: c.code,
                ru: c.ru,
                continent: continent_index(&c.cont),
                // Антарктида в списке не нужна
                hidden: continent_index(&c.cont) == 6,
                shapes: shapes_of(c.polys),
            });
        }
        for (code, ru, continent) in [
            (RUS_EU, "Россия (европейская часть)", 0),
            (RUS_AS, "Россия (азиатская часть)", 1),
        ] {
            list.push(Country {
                code: code.to_string(),
                ru: ru.to_string(),
                continent,
                hidden: false,
                shapes: Vec::new(),
            });
        }
        if let Some(r) = list.iter_mut().find(|c| c.code == "RUS") {
            r.hidden = true; // сама «Россия» в списке не нужна
        }
        list.sort_by(|a, b| a.ru.to_lowercase().cmp(&b.ru.to_lowercase()));
        let by_code: HashMap<&str, usize> = list
            .iter()
            .enumerate()
            .map(|(i, c)| (c.code.as_str(), i))
            .collect();

        let mut by_name: HashMap<String, usize> = HashMap::new();
        for (n, code) in names {
            if let Some(&i) = by_code.get(code.as_str()) {
                by_name.insert(n, i);
            }
        }
        for (n, code) in ALIASES {
            if let Some(&i) = by_code.get(code) {
                by_name.insert(norm(n), i);
            }
        }
        let overrides = raw
            .overrides
            .into_iter()
            .filter_map(|o| {
                by_code
                    .get(o.code.as_str())
                    .map(|&i| (i, shapes_of(o.polys)))
            })
            .collect();
        let idx = |code: &str| by_code.get(code).copied();
        let rus = match (idx("RUS"), idx(RUS_EU), idx(RUS_AS)) {
            (Some(a), Some(b), Some(c)) => Some([a, b, c]),
            _ => None,
        };
        Ok(Countries {
            list,
            overrides,
            by_name,
            rus,
        })
    }

    /// Страна по названию из свойства файла (русское или английское).
    pub fn by_name(&self, name: &str) -> Option<u16> {
        self.by_name.get(&norm(name)).map(|&i| i as u16)
    }

    /// Страна по координатам: сначала исправления (Россия), потом обычные границы.
    pub fn at(&self, lon: f64, lat: f64) -> Option<u16> {
        for (i, shapes) in &self.overrides {
            if shapes.iter().any(|s| in_shape(s, lon, lat)) {
                return Some(*i as u16);
            }
        }
        self.list
            .iter()
            .position(|c| c.shapes.iter().any(|s| in_shape(s, lon, lat)))
            .map(|i| i as u16)
    }

    /// Страны объекта: из текстового свойства (значения через « / », «;» или «,»),
    /// а если оно пустое или не распознано — по первой и последней точке геометрии.
    pub fn resolve(&self, property: Option<&str>, points: &[[f64; 2]]) -> Vec<u16> {
        let mut found: Vec<u16> = Vec::new();
        if let Some(text) = property {
            for part in text.split(['/', ';', ',']) {
                if let Some(i) = self.by_name(part) {
                    if !found.contains(&i) {
                        found.push(i);
                    }
                }
            }
        }
        if found.is_empty() {
            for p in [points.first(), points.last()].into_iter().flatten() {
                if let Some(i) = self.at(p[0], p[1]) {
                    if !found.contains(&i) {
                        found.push(i);
                    }
                }
            }
        }
        if found.is_empty() {
            found.push(UNKNOWN);
        }
        // Россию делим на европейскую и азиатскую части: по точкам объекта
        if let Some([rus, eu, asia]) = self.rus {
            if let Some(pos) = found.iter().position(|&i| i as usize == rus) {
                found.remove(pos);
                let mut parts: Vec<u16> = Vec::new();
                for p in [points.first(), points.last()].into_iter().flatten() {
                    let i = if p[0] >= URAL_LON { asia } else { eu } as u16;
                    if !parts.contains(&i) {
                        parts.push(i);
                    }
                }
                if parts.is_empty() {
                    parts = vec![eu as u16, asia as u16];
                }
                for i in parts {
                    if !found.contains(&i) {
                        found.push(i);
                    }
                }
            }
        }
        found
    }
}

/// Общий фильтр: пусто = показывать всё; иначе только объекты хотя бы одной отмеченной страны.
#[derive(Default, Clone)]
pub struct Filter {
    /// Коды отмеченных стран (ADM0_A3); "?" — объекты без определённой страны
    pub checked: HashSet<String>,
    /// Какие номера стран сейчас разрешены (считается из `checked`)
    allowed: Vec<bool>,
    allow_unknown: bool,
}

pub const UNKNOWN_CODE: &str = "?";

impl Filter {
    #[allow(dead_code)]
    pub fn from_codes(codes: &[String]) -> Filter {
        let mut checked: HashSet<String> = codes.iter().cloned().collect();
        // Старые настройки хранили «Россию» целиком
        if checked.remove("RUS") {
            checked.insert(RUS_EU.to_string());
            checked.insert(RUS_AS.to_string());
        }
        let mut f = Filter {
            checked,
            ..Default::default()
        };
        f.rebuild();
        f
    }

    /// Пересчитать таблицу после изменения `checked`.
    pub fn rebuild(&mut self) {
        let list = &get().list;
        self.allowed = list
            .iter()
            .map(|c| self.checked.contains(&c.code))
            .collect();
        self.allow_unknown = self.checked.contains(UNKNOWN_CODE);
    }

    pub fn is_active(&self) -> bool {
        !self.checked.is_empty()
    }

    pub fn passes(&self, countries: &[u16]) -> bool {
        if !self.is_active() {
            return true;
        }
        countries.iter().any(|&c| {
            if c == UNKNOWN {
                self.allow_unknown
            } else {
                self.allowed.get(c as usize).copied().unwrap_or(false)
            }
        })
    }

    #[allow(dead_code)]
    pub fn codes(&self) -> Vec<String> {
        let mut v: Vec<String> = self.checked.iter().cloned().collect();
        v.sort();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_points() {
        let c = get();
        assert_eq!(c.list.len(), 251);
        let ru = |i: u16| c.list[i as usize].code.as_str();
        assert_eq!(ru(c.by_name("Türkiye").unwrap()), "TUR");
        assert_eq!(ru(c.by_name("Китай").unwrap()), "CHN");
        assert_eq!(ru(c.by_name("Taiwan").unwrap()), "CHN");
        assert_eq!(ru(c.by_name(" germany ").unwrap()), "DEU");
        assert_eq!(ru(c.at(37.6, 55.75).unwrap()), "RUS"); // Москва
        assert_eq!(ru(c.at(34.1, 44.95).unwrap()), "RUS"); // Симферополь
        assert_eq!(ru(c.at(35.1, 47.85).unwrap()), "RUS"); // Запорожье
        assert_eq!(ru(c.at(30.5, 50.45).unwrap()), "UKR"); // Киев
        assert_eq!(ru(c.at(2.35, 48.85).unwrap()), "FRA"); // Париж
        assert!(c.at(-30.0, 30.0).is_none()); // океан
    }

    #[test]
    fn russia_split() {
        let c = get();
        let code = |v: Vec<u16>| -> Vec<String> {
            v.into_iter()
                .map(|i| c.list[i as usize].code.clone())
                .collect()
        };
        assert_eq!(
            code(c.resolve(Some("Russia"), &[[37.6, 55.75]])),
            ["RUS-EU"]
        );
        assert_eq!(code(c.resolve(Some("Россия"), &[[82.9, 55.0]])), ["RUS-AS"]);
        assert_eq!(
            code(c.resolve(None, &[[37.6, 55.75], [135.0, 48.0]])).len(),
            2
        );
        let f = Filter::from_codes(&["RUS".to_string()]);
        assert!(f.checked.contains(RUS_EU) && f.checked.contains(RUS_AS));
    }

    #[test]
    fn resolve_multi_and_fallback() {
        let c = get();
        let v = c.resolve(Some("Austria / Germany"), &[]);
        assert_eq!(v.len(), 2);
        assert_eq!(c.resolve(None, &[[-30.0, 30.0]]), vec![UNKNOWN]);
        let v = c.resolve(Some("???"), &[[37.6, 55.75]]);
        assert_eq!(c.list[v[0] as usize].code, "RUS");
    }

    #[test]
    fn filter_semantics() {
        let c = get();
        let rus = c.by_name("Russia").unwrap();
        let fra = c.by_name("France").unwrap();
        let none = Filter::default();
        assert!(none.passes(&[rus]) && none.passes(&[UNKNOWN]));
        let f = Filter::from_codes(&["RUS".to_string()]);
        assert!(f.passes(&[rus]) && f.passes(&[fra, rus]));
        assert!(!f.passes(&[fra]) && !f.passes(&[UNKNOWN]));
        let g = Filter::from_codes(&[UNKNOWN_CODE.to_string()]);
        assert!(g.passes(&[UNKNOWN]) && !g.passes(&[rus]));
    }
}
