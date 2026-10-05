//! Рисование значков в картинки (спрайты). Значки на карте — это одинаковые маленькие
//! картинки, поэтому каждую достаточно нарисовать один раз с четырёхкратным сглаживанием,
//! а на карте выводить двумя треугольниками вместо сотен.
//!
//! Модуль не зависит от egui, поэтому проверяется обычными тестами.

/// Сколько подвыборок на сторону пикселя (сглаживание).
const SS: usize = 4;

/// Что рисуется в клетке спрайта. Размеры — в логических пикселях от центра клетки.
#[derive(Clone, Copy)]
pub enum Prim {
    /// Закрашенный круг этого радиуса
    Disk(f32),
    /// Кольцо между двумя радиусами
    Ring(f32, f32),
    /// Треугольники рисунка в единичном квадрате [-1, 1] (по три точки, y вниз),
    /// увеличенные в указанное число логических пикселей на единицу
    Tris(&'static [[f32; 2]], f32),
}

/// Размер клетки в физических пикселях при данном масштабе экрана.
pub fn cell_pixels(cell_logical: f32, ppp: f32) -> usize {
    ((cell_logical * ppp).round() as usize).clamp(8, 256)
}

struct Canvas {
    side: usize,
    cov: Vec<bool>,
    /// Подвыборок на один логический пиксель
    scale: f32,
}

impl Canvas {
    fn new(n: usize, cell_logical: f32) -> Canvas {
        let side = n * SS;
        Canvas {
            side,
            cov: vec![false; side * side],
            scale: side as f32 / cell_logical,
        }
    }

    fn centre(&self) -> f32 {
        self.side as f32 / 2.0
    }

    fn ring(&mut self, r_in: f32, r_out: f32) {
        let c = self.centre();
        let (lo, hi) = (r_in * self.scale, r_out * self.scale);
        let a = ((c - hi).floor().max(0.0)) as usize;
        let b = ((c + hi).ceil() as usize).min(self.side - 1);
        for y in a..=b {
            for x in a..=b {
                let (dx, dy) = (x as f32 + 0.5 - c, y as f32 + 0.5 - c);
                let d2 = dx * dx + dy * dy;
                if d2 >= lo * lo && d2 <= hi * hi {
                    self.cov[y * self.side + x] = true;
                }
            }
        }
    }

    fn tris(&mut self, tris: &[[f32; 2]], unit: f32) {
        let c = self.centre();
        let k = unit * self.scale;
        let pt = |v: [f32; 2]| (c + v[0] * k, c + v[1] * k);
        let max = (self.side - 1) as f32;
        for t in tris.chunks_exact(3) {
            let ((ax, ay), (bx, by), (cx, cy)) = (pt(t[0]), pt(t[1]), pt(t[2]));
            let area = (bx - ax) * (cy - ay) - (by - ay) * (cx - ax);
            if area.abs() < 1e-6 {
                continue;
            }
            let x0 = ax.min(bx).min(cx).floor().clamp(0.0, max) as usize;
            let x1 = ax.max(bx).max(cx).ceil().clamp(0.0, max) as usize;
            let y0 = ay.min(by).min(cy).floor().clamp(0.0, max) as usize;
            let y1 = ay.max(by).max(cy).ceil().clamp(0.0, max) as usize;
            for y in y0..=y1 {
                for x in x0..=x1 {
                    let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                    let w0 = (bx - ax) * (py - ay) - (by - ay) * (px - ax);
                    let w1 = (cx - bx) * (py - by) - (cy - by) * (px - bx);
                    let w2 = (ax - cx) * (py - cy) - (ay - cy) * (px - cx);
                    let inside = if area > 0.0 {
                        w0 >= 0.0 && w1 >= 0.0 && w2 >= 0.0
                    } else {
                        w0 <= 0.0 && w1 <= 0.0 && w2 <= 0.0
                    };
                    if inside {
                        self.cov[y * self.side + x] = true;
                    }
                }
            }
        }
    }

    /// Прозрачность каждого пикселя (0..255): доля закрашенных подвыборок.
    fn alpha(&self) -> Vec<u8> {
        let n = self.side / SS;
        let mut out = vec![0u8; n * n];
        for y in 0..n {
            for x in 0..n {
                let mut count = 0usize;
                for sy in 0..SS {
                    for sx in 0..SS {
                        if self.cov[(y * SS + sy) * self.side + x * SS + sx] {
                            count += 1;
                        }
                    }
                }
                out[y * n + x] = (count * 255 / (SS * SS)) as u8;
            }
        }
        out
    }
}

/// Рисует одну клетку: возвращает прозрачность `n * n`.
pub fn render_cell(cell_logical: f32, n: usize, prims: &[Prim]) -> Vec<u8> {
    let mut canvas = Canvas::new(n, cell_logical);
    for p in prims {
        match *p {
            Prim::Disk(r) => canvas.ring(0.0, r),
            Prim::Ring(a, b) => canvas.ring(a, b),
            Prim::Tris(t, unit) => canvas.tris(t, unit),
        }
    }
    canvas.alpha()
}

/// Атлас из клеток `cols` в ряд. Возвращает ширину, высоту и пиксели RGBA (белые, с
/// премультиплицированной прозрачностью, как ждёт egui).
pub fn build_atlas(
    cell_logical: f32,
    n: usize,
    cols: usize,
    cells: &[Vec<Prim>],
) -> (usize, usize, Vec<u8>) {
    let cols = cols.min(cells.len()).max(1);
    let rows = cells.len().div_ceil(cols);
    let (w, h) = (cols * n, rows * n);
    let mut rgba = vec![0u8; w * h * 4];
    for (k, prims) in cells.iter().enumerate() {
        let alpha = render_cell(cell_logical, n, prims);
        let (ox, oy) = ((k % cols) * n, (k / cols) * n);
        for y in 0..n {
            for x in 0..n {
                let a = alpha[y * n + x];
                let i = ((oy + y) * w + ox + x) * 4;
                rgba[i..i + 4].copy_from_slice(&[a, a, a, a]);
            }
        }
    }
    (w, h, rgba)
}

/// Половина полной ширины ленты линии: от оси до внешнего края сглаживания.
pub fn line_half(width: f32, feather: f32) -> f32 {
    width / 2.0 + feather / 2.0
}

/// Профиль поперёк линии: прозрачность в `texels` точках от левого края ленты до правого.
/// Внутри линии 255, к краю плавно спадает до 0 на ширине `feather` (как сглаживание egui).
pub fn line_ramp(width: f32, feather: f32, texels: usize) -> Vec<u8> {
    let half = line_half(width, feather);
    (0..texels)
        .map(|i| {
            let x = ((i as f32 + 0.5) / texels as f32 - 0.5) * 2.0 * half;
            let a = ((width / 2.0 + feather / 2.0 - x.abs()) / feather).clamp(0.0, 1.0);
            (a * 255.0).round() as u8
        })
        .collect()
}

/// Профиль пунктирной линии: `rows` строк по `texels` точек (поперёк линии), строки идут вдоль
/// линии на длину одного периода `period`. В каждом периоде штрих длиной `on` с мягкими
/// концами (шириной `feather`), отступ от начала периода 1 пиксель.
pub fn dash_ramp(
    width: f32,
    feather: f32,
    texels: usize,
    on: f32,
    period: f32,
    rows: usize,
) -> Vec<u8> {
    let across = line_ramp(width, feather, texels);
    let mut out = Vec::with_capacity(texels * rows);
    for j in 0..rows {
        let y = (j as f32 + 0.5) / rows as f32 * period;
        let centre = 1.0 + on / 2.0;
        let along = (((on / 2.0 - (y - centre).abs()) / feather) + 0.5).clamp(0.0, 1.0);
        for &a in &across {
            out.push((a as f32 * along).round() as u8);
        }
    }
    out
}

/// Положение клетки `k` в атласе в долях ширины и высоты: (левый, верхний, правый, нижний).
pub fn cell_uv(k: usize, n: usize, cols: usize, total: usize) -> [f32; 4] {
    let cols = cols.min(total).max(1);
    let rows = total.div_ceil(cols);
    let (w, h) = ((cols * n) as f32, (rows * n) as f32);
    let (x, y) = (((k % cols) * n) as f32, ((k / cols) * n) as f32);
    [x / w, y / h, (x + n as f32) / w, (y + n as f32) / h]
}

#[cfg(test)]
mod tests {
    use super::*;

    static SQUARE: [[f32; 2]; 6] = [
        [-0.5, -0.5],
        [0.5, -0.5],
        [0.5, 0.5],
        [-0.5, -0.5],
        [0.5, 0.5],
        [-0.5, 0.5],
    ];

    #[test]
    fn disk_is_round_and_smooth() {
        let n = cell_pixels(24.0, 1.0);
        assert_eq!(n, 24);
        let a = render_cell(24.0, n, &[Prim::Disk(10.0)]);
        assert_eq!(a[12 * n + 12], 255); // центр закрашен
        assert_eq!(a[0], 0); // угол пуст
        assert_eq!(a[12 * n + 1], 0); // дальше радиуса 10 (клетка ±12) пусто
        // Края сглажены: есть и частично закрашенные пиксели
        assert!(a.iter().any(|&v| v > 0 && v < 255));
        // Площадь близка к πr² (с точностью до сглаживания)
        let area: f32 = a.iter().map(|&v| v as f32 / 255.0).sum();
        let want = std::f32::consts::PI * 100.0;
        assert!((area - want).abs() < 4.0, "площадь {area}, ждали {want}");
    }

    #[test]
    fn ring_has_hole() {
        let n = cell_pixels(24.0, 1.0);
        let a = render_cell(24.0, n, &[Prim::Ring(10.0, 11.0)]);
        assert_eq!(a[12 * n + 12], 0);
        assert!(a[12 * n + 1] > 0 || a[12 * n + 2] > 0);
    }

    #[test]
    fn triangles_fill_exactly() {
        // Квадрат 0.5 × 0.5 единицы при 20 пикселях на единицу = 10 × 10 пикселей
        let n = cell_pixels(24.0, 1.0);
        let a = render_cell(24.0, n, &[Prim::Tris(&SQUARE, 20.0)]);
        let area: f32 = a.iter().map(|&v| v as f32 / 255.0).sum();
        assert!((area - 400.0).abs() < 1.0, "площадь {area}");
        // Шов между двумя треугольниками не оставляет щели
        assert_eq!(a[12 * n + 12], 255);
        assert_eq!(a[11 * n + 11], 255);
    }

    #[test]
    fn scales_with_screen_density() {
        // При масштабе 2 клетка вдвое больше, а площадь в логических пикселях та же
        let n = cell_pixels(24.0, 2.0);
        assert_eq!(n, 48);
        let a = render_cell(24.0, n, &[Prim::Disk(10.0)]);
        let area: f32 = a.iter().map(|&v| v as f32 / 255.0).sum::<f32>() / 4.0;
        assert!((area - std::f32::consts::PI * 100.0).abs() < 4.0);
    }

    #[test]
    fn atlas_layout() {
        let cells = vec![vec![Prim::Disk(10.0)]; 10];
        let n = 24;
        let (w, h, rgba) = build_atlas(24.0, n, 8, &cells);
        assert_eq!((w, h), (8 * n, 2 * n));
        assert_eq!(rgba.len(), w * h * 4);
        // Центр клетки 9 (второй ряд, вторая клетка) закрашен, белым и непрозрачно
        let i = ((n + 12) * w + n + 12) * 4;
        assert_eq!(&rgba[i..i + 4], &[255, 255, 255, 255]);
        let uv = cell_uv(9, n, 8, 10);
        assert_eq!(
            uv,
            [n as f32 / w as f32, 0.5, 2.0 * n as f32 / w as f32, 1.0]
        );
    }

    #[test]
    fn line_ramp_profile() {
        // Линия шириной 2 со сглаживанием 1: край на расстоянии 1.5 от оси, полная
        // непрозрачность в пределах 0.5
        let r = line_ramp(2.0, 1.0, 64);
        assert_eq!(r.len(), 64);
        assert!(r[0] < 20 && r[63] < 20); // у краёв почти прозрачно
        assert_eq!(r[31], 255);
        assert_eq!(r[32], 255);
        // Симметрия
        assert!((0..32).all(|i| r[i] == r[63 - i]));
        // Площадь профиля = ширина линии (в долях: ширина / полная ширина ленты × 255)
        let sum: f32 =
            r.iter().map(|&v| v as f32 / 255.0).sum::<f32>() / 64.0 * 2.0 * line_half(2.0, 1.0);
        assert!((sum - 2.0).abs() < 0.05, "ширина {sum}");
    }

    #[test]
    fn dash_ramp_profile() {
        let (texels, rows) = (16, 48);
        let d = dash_ramp(2.0, 1.0, texels, 8.0, 12.0, rows);
        assert_eq!(d.len(), texels * rows);
        let at = |y: f32| {
            let j = ((y / 12.0 * rows as f32) as usize).min(rows - 1);
            d[j * texels + texels / 2]
        };
        assert_eq!(at(5.0), 255); // середина штриха
        assert_eq!(at(11.5), 0); // отступ
        assert_eq!(at(0.2), 0); // до начала штриха
        // Доля закрашенного вдоль периода близка к 8 / 12
        let share: f32 = (0..rows)
            .map(|j| d[j * texels + texels / 2] as f32 / 255.0)
            .sum::<f32>()
            / rows as f32;
        assert!((share - 8.0 / 12.0).abs() < 0.05, "доля {share}");
    }
}
