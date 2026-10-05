//! Быстрое рисование линий: ломаная превращается в «ленту» из треугольников, а сглаженные
//! края даёт не геометрия, а маленькая текстура-профиль (см. `raster::line_ramp`).
//! У ленты две вершины на точку линии вместо четырёх и нет отдельной фигуры на каждый
//! отрезок, поэтому сотни тысяч точек железных дорог рисуются в разы быстрее.
//!
//! Модуль не зависит от egui и проверяется обычными тестами.

/// Куда складываются вершины и треугольники (в egui это сетка `Mesh`).
pub trait Sink {
    /// Добавляет вершину: положение на экране, координата поперёк линии (0..1) для текстуры
    /// и расстояние от начала линии вдоль неё в пикселях (по нему рисуется пунктир).
    fn vertex(&mut self, x: f32, y: f32, across: f32, along: f32) -> u32;
    fn triangle(&mut self, a: u32, b: u32, c: u32);
}

/// Во сколько раз угол стыка может «раздвинуть» ленту (острые повороты не дают шипов).
const MITER_LIMIT: f32 = 2.5;

type Pt = (f32, f32);

/// Нормаль и растяжение ленты в вершине, где сходятся отрезки с нормалями `n_in` и `n_out`.
fn join(n_in: Option<Pt>, n_out: Pt) -> (Pt, f32) {
    let Some(n0) = n_in else {
        return (n_out, 1.0);
    };
    let (mx, my) = (n0.0 + n_out.0, n0.1 + n_out.1);
    let ml = (mx * mx + my * my).sqrt();
    if ml < 1e-4 {
        return (n_out, 1.0); // разворот на 180°
    }
    let m = (mx / ml, my / ml);
    let d = m.0 * n_out.0 + m.1 * n_out.1;
    (m, (1.0 / d.max(1e-3)).min(MITER_LIMIT))
}

fn emit(sink: &mut impl Sink, p: Pt, n: Pt, half: f32, along: f32) -> (u32, u32) {
    let (ox, oy) = (n.0 * half, n.1 * half);
    let l = sink.vertex(p.0 - ox, p.1 - oy, 0.0, along);
    let r = sink.vertex(p.0 + ox, p.1 + oy, 1.0, along);
    (l, r)
}

/// Рисует ломаную `run` лентой, у которой от оси до края `half` пикселей (вместе с краем
/// сглаживания). Повторяющиеся точки пропускаются; если различимых точек меньше двух,
/// ничего не рисуется.
pub fn strip<P: Copy>(run: &[P], xy: impl Fn(P) -> Pt, half: f32, sink: &mut impl Sink) {
    let mut it = run.iter().map(|&p| xy(p));
    let Some(mut a) = it.next() else {
        return;
    };
    let mut n_in: Option<Pt> = None; // нормаль отрезка, пришедшего в `a`
    let mut last: Option<(u32, u32)> = None; // вершины предыдущей точки ленты
    let mut along = 0.0f32; // длина пройденной линии
    for b in it {
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let len = (dx * dx + dy * dy).sqrt();
        if len < 1e-3 {
            continue; // та же точка
        }
        let n_out = (-dy / len, dx / len);
        let (n, scale) = join(n_in, n_out);
        let (l, r) = emit(sink, a, n, half * scale, along);
        if let Some((pl, pr)) = last {
            sink.triangle(pl, pr, l);
            sink.triangle(pr, r, l);
        }
        last = Some((l, r));
        n_in = Some(n_out);
        along += len;
        a = b;
    }
    // Последняя точка: стыка нет, лента просто заканчивается
    if let (Some(n), Some((pl, pr))) = (n_in, last) {
        let (l, r) = emit(sink, a, n, half, along);
        sink.triangle(pl, pr, l);
        sink.triangle(pr, r, l);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Collect {
        v: Vec<(f32, f32, f32)>,
        along: Vec<f32>,
        t: Vec<[u32; 3]>,
    }

    impl Sink for Collect {
        fn vertex(&mut self, x: f32, y: f32, across: f32, along: f32) -> u32 {
            self.v.push((x, y, across));
            self.along.push(along);
            (self.v.len() - 1) as u32
        }
        fn triangle(&mut self, a: u32, b: u32, c: u32) {
            self.t.push([a, b, c]);
        }
    }

    fn run(pts: &[(f32, f32)], half: f32) -> Collect {
        let mut c = Collect::default();
        strip(pts, |p| p, half, &mut c);
        c
    }

    fn area(c: &Collect) -> f32 {
        c.t.iter()
            .map(|&[a, b, d]| {
                let (p, q, r) = (c.v[a as usize], c.v[b as usize], c.v[d as usize]);
                ((q.0 - p.0) * (r.1 - p.1) - (q.1 - p.1) * (r.0 - p.0)).abs() / 2.0
            })
            .sum()
    }

    #[test]
    fn straight_line() {
        let c = run(&[(0.0, 0.0), (10.0, 0.0)], 1.5);
        assert_eq!(c.v.len(), 4); // две вершины на точку
        assert_eq!(c.t.len(), 2);
        assert!((area(&c) - 30.0).abs() < 1e-4); // 10 × 3
        // Поперёк линии: одна сторона 0, другая 1
        assert!(c.v.iter().any(|v| v.2 == 0.0) && c.v.iter().any(|v| v.2 == 1.0));
        // Лента симметрична относительно оси
        assert!(c.v.iter().all(|v| (v.1.abs() - 1.5).abs() < 1e-5));
    }

    #[test]
    fn distance_along_the_line() {
        // Длина копится вдоль ломаной: 0, 10, 20; обе вершины точки получают одно значение
        let c = run(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)], 1.0);
        assert_eq!(c.along, vec![0.0, 0.0, 10.0, 10.0, 20.0, 20.0]);
        // Пропущенный дубликат длину не меняет
        let c = run(&[(0.0, 0.0), (0.0, 0.0), (5.0, 0.0)], 1.0);
        assert_eq!(c.along, vec![0.0, 0.0, 5.0, 5.0]);
    }

    #[test]
    fn right_angle_has_miter() {
        // Поворот на 90°: внешний угол ленты — острый «мит», площадь = два прямоугольника
        // без перекрытия по оси плюс угловой квадрат
        let c = run(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)], 1.0);
        assert_eq!(c.v.len(), 6);
        assert_eq!(c.t.len(), 4);
        // Ось 20, ширина 2: 40 минус/плюс угол; на стыке ровно квадрат 1×1 добавлен снаружи
        // и вырезан изнутри, итог = 20 × 2 = 40
        assert!((area(&c) - 40.0).abs() < 1e-3, "площадь {}", area(&c));
    }

    #[test]
    fn sharp_turn_is_limited() {
        // Очень острый поворот не даёт бесконечного шипа
        let c = run(&[(0.0, 0.0), (10.0, 0.0), (0.2, 0.5)], 1.0);
        // Вершины стыка — третья и четвёртая (две вершины на каждую точку)
        let far = c.v[2..4]
            .iter()
            .map(|v| (v.0 - 10.0).hypot(v.1))
            .fold(0.0f32, f32::max);
        assert!(far <= 2.5 * 1.0 + 1e-3, "шип длиной {far}");
    }

    #[test]
    fn duplicates_and_short_runs() {
        assert!(run(&[], 1.0).v.is_empty());
        assert!(run(&[(1.0, 1.0)], 1.0).v.is_empty());
        assert!(run(&[(1.0, 1.0), (1.0, 1.0)], 1.0).v.is_empty());
        let c = run(&[(0.0, 0.0), (0.0, 0.0), (5.0, 0.0), (5.0, 0.0)], 1.0);
        assert_eq!(c.v.len(), 4);
        assert!((area(&c) - 10.0).abs() < 1e-4);
    }

    #[test]
    fn reversal_does_not_blow_up() {
        let c = run(&[(0.0, 0.0), (5.0, 0.0), (0.0, 0.0)], 1.0);
        assert!(c.v.iter().all(|v| v.0.is_finite() && v.1.is_finite()));
    }
}
