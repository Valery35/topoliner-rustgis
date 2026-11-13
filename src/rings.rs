// Topoliner для RustGIS - расширение-процесс RustGIS.
// Copyright (C) 2026 ООО «Информ++». Лицензия GPL-3.0-or-later.
// SPDX-License-Identifier: GPL-3.0-or-later

//! Геометрия кольца без булевых операций. Перенос того, что в модуле
//! QGIS лежит в `topo_core`: площадь, периметр, эффективная ширина,
//! повторяющиеся вершины, иглы и самокасания.
//!
//! Кольцо здесь - список вершин без повтора первой в конце.

/// Вершина кольца.
pub type Pt = [f64; 2];

/// Порог точного совпадения координат. Меньше любого разумного допуска.
pub const EPS: f64 = 1e-9;

/// Угол иглы по умолчанию, градусы.
pub const DEFAULT_SPIKE_ANGLE: f64 = 1.0;

/// Площадь замкнутого кольца со знаком (формула шнурков).
pub fn ring_area(coords: &[Pt]) -> f64 {
    let n = coords.len();
    if n < 3 {
        return 0.0;
    }
    let mut s = 0.0;
    for i in 0..n {
        let a = coords[i];
        let b = coords[(i + 1) % n];
        s += a[0] * b[1] - b[0] * a[1];
    }
    0.5 * s
}

/// Длина замкнутого кольца.
pub fn ring_perimeter(coords: &[Pt]) -> f64 {
    let n = coords.len();
    if n < 2 {
        return 0.0;
    }
    let mut s = 0.0;
    for i in 0..n {
        let a = coords[i];
        let b = coords[(i + 1) % n];
        s += (b[0] - a[0]).hypot(b[1] - a[1]);
    }
    s
}

/// Эффективная ширина кольца: удвоенная площадь, делённая на периметр.
///
/// Для длинной узкой полосы даёт её ширину, для круга радиус. Малое
/// значение при заметной площади это признак волосяного полигона.
pub fn ring_width(coords: &[Pt]) -> f64 {
    let p = ring_perimeter(coords);
    if p <= 0.0 {
        return 0.0;
    }
    2.0 * ring_area(coords).abs() / p
}

/// Удаляет вершины, отстоящие от предыдущей не дальше допуска.
/// Ответ - новое кольцо и количество снятых вершин.
pub fn drop_repeated_vertices(coords: &[Pt], closed: bool, tolerance: f64) -> (Vec<Pt>, usize) {
    if coords.is_empty() {
        return (Vec::new(), 0);
    }
    let mut out = vec![coords[0]];
    let mut removed = 0;
    for &p in &coords[1..] {
        let q = out[out.len() - 1];
        if (p[0] - q[0]).hypot(p[1] - q[1]) > tolerance {
            out.push(p);
        } else {
            removed += 1;
        }
    }
    if closed {
        while out.len() > 1 {
            let (a, b) = (out[0], out[out.len() - 1]);
            if (a[0] - b[0]).hypot(a[1] - b[1]) <= tolerance {
                out.pop();
                removed += 1;
            } else {
                break;
            }
        }
    }
    (out, removed)
}

fn is_spike(a: Pt, b: Pt, c: Pt, cos_limit: f64) -> bool {
    let (ux, uy) = (a[0] - b[0], a[1] - b[1]);
    let (vx, vy) = (c[0] - b[0], c[1] - b[1]);
    let lu = ux.hypot(uy);
    let lv = vx.hypot(vy);
    if lu <= EPS || lv <= EPS {
        return true;
    }
    (ux * vx + uy * vy) / (lu * lv) >= cos_limit
}

/// Снимает иглы: вершины, в которых линия разворачивается почти назад.
/// Ответ - новое кольцо и количество снятых игл.
pub fn remove_spikes(coords: &[Pt], closed: bool, max_angle_deg: f64) -> (Vec<Pt>, usize) {
    if coords.len() < 3 {
        return (coords.to_vec(), 0);
    }
    let cos_limit = max_angle_deg.to_radians().cos();
    let min_pts = if closed { 3 } else { 2 };
    let mut removed = 0;
    let mut out: Vec<Pt> = Vec::with_capacity(coords.len());
    for &p in coords {
        out.push(p);
        while out.len() >= 3 {
            let n = out.len();
            if is_spike(out[n - 3], out[n - 2], out[n - 1], cos_limit) {
                out.remove(n - 2);
                removed += 1;
            } else {
                break;
            }
        }
    }
    if closed {
        // Стык кольца: игла может сидеть на первой или последней вершине.
        let mut guard = 0;
        while out.len() > min_pts && guard < coords.len() + 8 {
            guard += 1;
            let n = out.len();
            if is_spike(out[n - 2], out[n - 1], out[0], cos_limit) {
                out.pop();
                removed += 1;
                continue;
            }
            if is_spike(out[n - 1], out[0], out[1], cos_limit) {
                out.remove(0);
                removed += 1;
                continue;
            }
            break;
        }
    }
    if out.len() < min_pts {
        return (coords.to_vec(), 0);
    }
    (out, removed)
}

/// Вершины, встречающиеся в кольце более одного раза, в порядке первого
/// появления. Кольцо, касающееся само себя в точке, формально бывает
/// корректным, но при дальнейшей обработке даёт непредсказуемый результат.
pub fn self_touch_points(coords: &[Pt]) -> Vec<Pt> {
    // Ключ - координаты, округлённые до девяти знаков, как в модуле.
    let key = |c: Pt| {
        let r = |v: f64| format!("{v:.9}");
        (r(c[0]), r(c[1]))
    };
    let mut order: Vec<(String, String)> = Vec::new();
    let mut counts: std::collections::HashMap<(String, String), usize> =
        std::collections::HashMap::new();
    let mut first: std::collections::HashMap<(String, String), Pt> =
        std::collections::HashMap::new();
    for &c in coords {
        let k = key(c);
        let slot = counts.entry(k.clone()).or_insert(0);
        if *slot == 0 {
            order.push(k.clone());
            // точка показывается округлённой, как ключ словаря в модуле
            first.insert(k.clone(), [round9(c[0]), round9(c[1])]);
        }
        *slot += 1;
    }
    order
        .into_iter()
        .filter(|k| counts.get(k).copied().unwrap_or(0) > 1)
        .filter_map(|k| first.get(&k).copied())
        .collect()
}

fn round9(v: f64) -> f64 {
    format!("{v:.9}").parse().unwrap_or(v)
}

/// Расстояние от точки до отрезка и параметр проекции в диапазоне [0, 1].
pub fn point_segment(p: Pt, a: Pt, b: Pt) -> (f64, f64) {
    let dx = b[0] - a[0];
    let dy = b[1] - a[1];
    let d2 = dx * dx + dy * dy;
    if d2 <= 0.0 {
        return ((p[0] - a[0]).hypot(p[1] - a[1]), 0.0);
    }
    let mut t = ((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / d2;
    t = t.clamp(0.0, 1.0);
    let qx = a[0] + t * dx;
    let qy = a[1] + t * dy;
    ((p[0] - qx).hypot(p[1] - qy), t)
}

/// Статистика длин рёбер: (медиана, пятый процентиль, всего рёбер).
/// Кольца приходят замкнутыми, с повтором первой вершины в конце.
pub fn segment_length_stats(rings: &[Vec<Pt>]) -> (f64, f64, usize) {
    let mut lengths: Vec<f64> = Vec::new();
    for coords in rings {
        if coords.len() < 2 {
            continue;
        }
        let closed = (coords[0][0] - coords[coords.len() - 1][0]).abs() <= EPS
            && (coords[0][1] - coords[coords.len() - 1][1]).abs() <= EPS;
        let body = if closed {
            &coords[..coords.len() - 1]
        } else {
            &coords[..]
        };
        let n = body.len();
        let last = if closed { n } else { n.saturating_sub(1) };
        for i in 0..last {
            let a = body[i];
            let b = body[(i + 1) % n];
            let d = (b[0] - a[0]).hypot(b[1] - a[1]);
            if d > EPS {
                lengths.push(d);
            }
        }
    }
    if lengths.is_empty() {
        return (0.0, 0.0, 0);
    }
    lengths.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = lengths[lengths.len() / 2];
    let i = ((0.05 * lengths.len() as f64) as usize).saturating_sub(1);
    (median, lengths[i], lengths.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_of_a_narrow_strip() {
        let ring = [[0.0, 0.0], [200.0, 0.0], [200.0, 0.5], [0.0, 0.5]];
        assert!((ring_area(&ring).abs() - 100.0).abs() < 1e-9);
        assert!((ring_width(&ring) - 0.49875311720698257).abs() < 1e-12);
    }

    #[test]
    fn a_spike_is_a_turn_back() {
        // уголок с разворотом назад в вершине 305.0001 25
        let ring = [
            [300.0, 0.0],
            [310.0, 0.0],
            [310.0, 10.0],
            [305.0, 10.0],
            [305.0001, 25.0],
            [305.0, 10.0],
            [300.0, 10.0],
        ];
        let (_out, spikes) = remove_spikes(&ring, true, DEFAULT_SPIKE_ANGLE);
        assert_eq!(spikes, 2);
        let touch = self_touch_points(&ring);
        assert_eq!(touch, vec![[305.0, 10.0]]);
    }
}
