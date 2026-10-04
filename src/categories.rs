//! Категории слоёв: разделы панели слоёв («Добыча ископаемых», «Трубопроводы» и т. д.).
//! Категория слоя по умолчанию подбирается по имени файла; её можно сменить вручную
//! в окошке цвета слоя, выбор сохраняется в настройках.

use std::path::Path;

/// Порядок вариантов = порядок разделов в панели.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Category {
    Mining,
    Industry,
    Pipelines,
    Power,
    Transport,
    Other,
}

pub const ALL: [Category; 6] = [
    Category::Mining,
    Category::Industry,
    Category::Pipelines,
    Category::Power,
    Category::Transport,
    Category::Other,
];

impl Category {
    pub fn title(self) -> &'static str {
        match self {
            Category::Mining => "Добыча ископаемых",
            Category::Industry => "Промышленность и хранение",
            Category::Pipelines => "Трубопроводы",
            Category::Power => "Электроэнергетика",
            Category::Transport => "Транспорт",
            Category::Other => "Прочее",
        }
    }

    /// Ключ для файла настроек.
    pub fn key(self) -> &'static str {
        match self {
            Category::Mining => "mining",
            Category::Industry => "industry",
            Category::Pipelines => "pipelines",
            Category::Power => "power",
            Category::Transport => "transport",
            Category::Other => "other",
        }
    }

    pub fn from_key(key: &str) -> Option<Category> {
        ALL.iter().copied().find(|c| c.key() == key)
    }
}

/// Категория по имени файла. Порядок проверок важен: например, «coal_plants» — это
/// станции (электроэнергетика), а «coal_mines» — добыча.
pub fn guess(path: &Path) -> Category {
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let words: Vec<&str> = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let has = |keys: &[&str]| words.iter().any(|w| keys.iter().any(|k| w.contains(k)));
    let is = |keys: &[&str]| words.iter().any(|w| keys.contains(w));

    if has(&[
        "pipeline",
        "трубопровод",
        "газопровод",
        "нефтепровод",
        "аммиакопровод",
    ]) {
        Category::Pipelines
    } else if has(&[
        "airport",
        "аэропорт",
        "railway",
        "railroad",
        "железн",
        "вокзал",
    ]) || is(&[
        "port", "ports", "seaport", "seaports", "rail", "station", "stations",
    ]) || has(&["порт"])
    {
        Category::Transport
    } else if has(&[
        "refiner",
        "нпз",
        "гпз",
        "нефтеперер",
        "terminal",
        "lng",
        "storage",
        "терминал",
        "спг",
        "хранилищ",
        "нефтебаз",
        "steel",
        "iron_steel",
        "cement",
        "chemical",
        "smelter",
        "metallurg",
        "стал",
        "chem",
        "хим",
        "азот",
        "processing",
        "завод",
        "комбинат",
        "цемент",
        "металлург",
        "химич",
    ]) {
        Category::Industry
    } else if has(&[
        "plant",
        "power",
        "thermal",
        "hydro",
        "nuclear",
        "solar",
        "wind",
        "geothermal",
        "тэс",
        "гэс",
        "аэс",
        "электро",
        "станц",
        "солнеч",
        "ветр",
    ]) {
        Category::Power
    } else if has(&[
        "field",
        "mine",
        "mines",
        "mining",
        "ore",
        "oil",
        "gas",
        "coal",
        "месторожд",
        "шахт",
        "рудник",
        "добыч",
        "нефт",
        "газ",
        "уголь",
        "угол",
        "руд",
    ]) {
        Category::Mining
    } else {
        Category::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guess_by_name() {
        let g = |n: &str| guess(Path::new(n));
        assert_eq!(g("oil_fields.geojson"), Category::Mining);
        assert_eq!(g("gas_fields.geojson"), Category::Mining);
        assert_eq!(g("coal_mines.geojson"), Category::Mining);
        assert_eq!(g("coal_terminals.geojson"), Category::Industry);
        assert_eq!(g("coal_plants.geojson"), Category::Power);
        assert_eq!(g("solar_plants.geojson"), Category::Power);
        assert_eq!(g("wind_plants.geojson"), Category::Power);
        assert_eq!(g("Солнечные электростанции.geojson"), Category::Power);
        assert_eq!(g("Ветровые электростанции.geojson"), Category::Power);
        assert_eq!(g("thermal_plants.geojson"), Category::Power);
        assert_eq!(g("hydro_plants.geojson"), Category::Power);
        assert_eq!(g("nuclear_plants.geojson"), Category::Power);
        assert_eq!(g("refineries_final.geojson"), Category::Industry);
        assert_eq!(g("gas_terminals.geojson"), Category::Industry);
        assert_eq!(g("iron_steel_plants.geojson"), Category::Industry);
        assert_eq!(g("steel_integrated.geojson"), Category::Industry);
        assert_eq!(g("chemicals_ammonia.geojson"), Category::Industry);
        assert_eq!(g("Нефтехимия.geojson"), Category::Industry);
        assert_eq!(g("Химия - аммиак и метанол.geojson"), Category::Industry);
        assert_eq!(g("gold_mines.geojson"), Category::Mining);
        assert_eq!(g("Золотые рудники.geojson"), Category::Mining);
        assert_eq!(g("Медные рудники.geojson"), Category::Mining);
        assert_eq!(g("iron_ore_mines.geojson"), Category::Mining);
        assert_eq!(g("Железорудные шахты.geojson"), Category::Mining);
        assert_eq!(g("steel_electric.geojson"), Category::Industry);
        assert_eq!(g("Сталелитейные комбинаты.geojson"), Category::Industry);
        assert_eq!(
            g("Электросталеплавильные заводы.geojson"),
            Category::Industry
        );
        assert_eq!(g("gas_processing_plants.geojson"), Category::Industry);
        assert_eq!(g("oil_gas_pipelines.geojson"), Category::Pipelines);
        assert_eq!(g("GEM-GGIT-Gas-Pipelines.geojson"), Category::Pipelines);
        assert_eq!(g("airports.geojson"), Category::Transport);
        assert_eq!(g("seaports.geojson"), Category::Transport);
        assert_eq!(g("transport.geojson"), Category::Other);
        assert_eq!(g("something.geojson"), Category::Other);
    }

    #[test]
    fn keys_round_trip() {
        for c in ALL {
            assert_eq!(Category::from_key(c.key()), Some(c));
        }
    }
}
