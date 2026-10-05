//! Экранная клавиатура для строки поиска: раскладки и правила редактирования текста.
//! Рисуется она в `main.rs`; здесь нет egui, поэтому модуль проверяется обычными тестами.
//!
//! Нужна для сенсорного экрана: когда программа развёрнута на весь экран, системная
//! клавиатура Windows закрыта окном и до неё не дотянуться.

/// Клавиша клавиатуры.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Key {
    /// Буква, цифра или знак (всегда строчные: поиск не различает регистр)
    Char(char),
    Space,
    Backspace,
    Clear,
    /// Переключение русской и английской раскладки
    Lang,
    /// Скрыть клавиатуру
    Done,
}

/// Клавиша в ряду и её ширина в «обычных» клавишах.
#[derive(Clone, Copy, Debug)]
pub struct Cell {
    pub key: Key,
    pub width: f32,
}

/// Что надо сделать после нажатия.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Effect {
    /// Текст не изменился
    Nothing,
    /// Текст изменился
    Edited,
    /// Сменить раскладку
    ToggleLang,
    /// Закрыть клавиатуру
    Close,
}

const RU: [&str; 3] = ["йцукенгшщзхъ", "фывапролджэ", "ячсмитьбюё"];
const EN: [&str; 3] = ["qwertyuiop", "asdfghjkl", "zxcvbnm"];

/// Ширина самого широкого ряда в обычных клавишах (по ней считается размер клавиш).
pub const UNITS: f32 = 12.0;

fn chars(s: &str) -> impl Iterator<Item = Cell> + '_ {
    s.chars().map(|c| Cell {
        key: Key::Char(c),
        width: 1.0,
    })
}

fn cell(key: Key, width: f32) -> Cell {
    Cell { key, width }
}

/// Ряды клавиш: цифры, три ряда букв и нижний ряд с пробелом.
pub fn rows(latin: bool) -> Vec<Vec<Cell>> {
    let letters = if latin { &EN } else { &RU };
    let mut rows = vec![chars("1234567890").collect::<Vec<_>>()];
    rows.push(chars(letters[0]).collect());
    rows.push(chars(letters[1]).collect());
    let mut third: Vec<Cell> = chars(letters[2]).collect();
    third.push(cell(Key::Backspace, 2.0));
    rows.push(third);
    rows.push(vec![
        cell(Key::Lang, 2.0),
        cell(Key::Char('-'), 1.0),
        cell(Key::Space, 5.0),
        cell(Key::Clear, 2.0),
        cell(Key::Done, 2.0),
    ]);
    rows
}

/// Подпись на клавише.
pub fn label(key: Key, latin: bool) -> String {
    match key {
        Key::Char(c) => c.to_uppercase().collect(),
        Key::Space => "пробел".to_string(),
        Key::Backspace => "Стереть".to_string(),
        Key::Clear => "Очистить".to_string(),
        Key::Lang => if latin { "EN → РУ" } else { "РУ → EN" }.to_string(),
        Key::Done => "Готово".to_string(),
    }
}

/// Применяет нажатие к тексту.
pub fn press(text: &mut String, key: Key) -> Effect {
    match key {
        Key::Char(c) => {
            text.push(c);
            Effect::Edited
        }
        Key::Space => {
            // Пробел в начале и двойной пробел поиску не нужны
            if text.is_empty() || text.ends_with(' ') {
                Effect::Nothing
            } else {
                text.push(' ');
                Effect::Edited
            }
        }
        Key::Backspace => {
            if text.pop().is_some() {
                Effect::Edited
            } else {
                Effect::Nothing
            }
        }
        Key::Clear => {
            if text.is_empty() {
                Effect::Nothing
            } else {
                text.clear();
                Effect::Edited
            }
        }
        Key::Lang => Effect::ToggleLang,
        Key::Done => Effect::Close,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn letters(latin: bool) -> Vec<char> {
        rows(latin)
            .iter()
            .flatten()
            .filter_map(|c| match c.key {
                Key::Char(ch) if ch.is_alphabetic() => Some(ch),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn russian_layout_has_every_letter_once() {
        let mut got = letters(false);
        got.sort();
        let mut want: Vec<char> = ('а'..='я').chain(['ё']).collect();
        want.sort();
        assert_eq!(got, want);
    }

    #[test]
    fn latin_layout_has_every_letter_once() {
        let mut got = letters(true);
        got.sort();
        let want: Vec<char> = ('a'..='z').collect();
        assert_eq!(got, want);
    }

    #[test]
    fn rows_fit_the_width() {
        for latin in [false, true] {
            let r = rows(latin);
            assert_eq!(r.len(), 5);
            for row in r {
                let w: f32 = row.iter().map(|c| c.width).sum();
                assert!(w <= UNITS + 1e-6, "ряд шире {UNITS}: {w}");
            }
        }
    }

    #[test]
    fn typing_and_erasing() {
        let mut t = String::new();
        assert_eq!(press(&mut t, Key::Backspace), Effect::Nothing);
        press(&mut t, Key::Char('р'));
        press(&mut t, Key::Char('о'));
        assert_eq!(t, "ро");
        assert_eq!(press(&mut t, Key::Backspace), Effect::Edited);
        assert_eq!(t, "р"); // стирается буква целиком, а не байт
        assert_eq!(press(&mut t, Key::Clear), Effect::Edited);
        assert!(t.is_empty());
        assert_eq!(press(&mut t, Key::Clear), Effect::Nothing);
    }

    #[test]
    fn spaces() {
        let mut t = String::new();
        assert_eq!(press(&mut t, Key::Space), Effect::Nothing);
        press(&mut t, Key::Char('н'));
        assert_eq!(press(&mut t, Key::Space), Effect::Edited);
        assert_eq!(press(&mut t, Key::Space), Effect::Nothing);
        assert_eq!(t, "н ");
    }

    #[test]
    fn service_keys() {
        let mut t = "а".to_string();
        assert_eq!(press(&mut t, Key::Lang), Effect::ToggleLang);
        assert_eq!(press(&mut t, Key::Done), Effect::Close);
        assert_eq!(t, "а");
    }

    #[test]
    fn labels() {
        assert_eq!(label(Key::Char('ё'), false), "Ё");
        assert_eq!(label(Key::Char('1'), false), "1");
    }
}
