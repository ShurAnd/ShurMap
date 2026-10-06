//! Категории слоёв: разделы панели слоёв («Добыча ископаемых», «Трубопроводы» и т. д.).
//! Категория слоя подбирается по содержимому файла (см. `content_hint` в layers.rs), её можно
//! сменить вручную в окошке цвета слоя, выбор сохраняется в настройках.

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

    #[allow(dead_code)]
    pub fn from_key(key: &str) -> Option<Category> {
        ALL.iter().copied().find(|c| c.key() == key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip() {
        for c in ALL {
            assert_eq!(Category::from_key(c.key()), Some(c));
        }
    }
}
