// Topoliner для RustGIS - расширение-процесс RustGIS.
// Copyright (C) 2026 ООО «Информ++». Лицензия GPL-3.0-or-later.
// SPDX-License-Identifier: GPL-3.0-or-later

//! 2.01 Топологическое упрощение. Перенос прореживания и сглаживания
//! из `topo_simplify` модуля QGIS вместе с диалогом.
//!
//! Общая граница прореживается один раз. Покрытие режется на дуги, дуга
//! прореживается однократно, концы дуг неподвижны. Обоим соседям
//! достаётся один и тот же результат.

use rustgis_kit::Lang;

use crate::arcs::{self, Parts};
use crate::rings::{EPS, Pt};
use crate::rounding::fmt;

/// Способ прореживания.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// Отклонение от хорды. Держит углы.
    Douglas,
    /// Площадь треугольника. Ровнее ведёт себя на плавной кривой.
    Visvalingam,
}

/// Прореживание Дугласа-Пекера. Первая и последняя точки неподвижны.
pub fn douglas_peucker(points: &[Pt], tolerance: f64) -> Vec<Pt> {
    let n = points.len();
    if n < 3 || tolerance <= 0.0 {
        return points.to_vec();
    }
    let mut keep = vec![false; n];
    keep[0] = true;
    keep[n - 1] = true;
    let mut stack = vec![(0usize, n - 1)];
    while let Some((first, last)) = stack.pop() {
        if last <= first + 1 {
            continue;
        }
        let (a, b) = (points[first], points[last]);
        let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
        let d2 = dx * dx + dy * dy;
        let mut best = -1.0;
        let mut best_i = usize::MAX;
        for (i, p) in points.iter().enumerate().take(last).skip(first + 1) {
            let dist = if d2 <= EPS {
                (p[0] - a[0]).hypot(p[1] - a[1])
            } else {
                let t = (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / d2).clamp(0.0, 1.0);
                (p[0] - (a[0] + t * dx)).hypot(p[1] - (a[1] + t * dy))
            };
            if dist > best {
                best = dist;
                best_i = i;
            }
        }
        if best > tolerance && best_i != usize::MAX {
            keep[best_i] = true;
            stack.push((first, best_i));
            stack.push((best_i, last));
        }
    }
    points
        .iter()
        .enumerate()
        .filter(|(i, _)| keep[*i])
        .map(|(_, p)| *p)
        .collect()
}

/// Прореживание по Висвалингаму: вершины убираются по площади
/// треугольника, образованного вершиной и двумя её соседями. Допуск
/// здесь площадь, пересчёт из длины делается снаружи.
pub fn visvalingam(points: &[Pt], tolerance: f64, min_points: usize) -> Vec<Pt> {
    let n = points.len();
    if n <= min_points || tolerance <= 0.0 {
        return points.to_vec();
    }
    let double_area = |a: Pt, b: Pt, c: Pt| -> f64 {
        ((b[0] - a[0]) * (c[1] - a[1]) - (c[0] - a[0]) * (b[1] - a[1])).abs()
    };
    let mut alive: Vec<usize> = (0..n).collect();
    while alive.len() > min_points {
        let mut worst: Option<usize> = None;
        let mut worst_area = f64::INFINITY;
        for position in 1..alive.len() - 1 {
            let value = double_area(
                points[alive[position - 1]],
                points[alive[position]],
                points[alive[position + 1]],
            );
            if worst.is_none() || value < worst_area {
                worst_area = value;
                worst = Some(position);
            }
        }
        let Some(worst) = worst else { break };
        if worst_area > 2.0 * tolerance {
            break;
        }
        alive.remove(worst);
    }
    alive.into_iter().map(|i| points[i]).collect()
}

/// Сглаживание срезанием углов по схеме Чайкина. Концы разомкнутой
/// линии неподвижны: у дуги это узлы ветвления, общие у соседей.
pub fn chaikin(points: &[Pt], iterations: usize, closed: bool) -> Vec<Pt> {
    if iterations == 0 || points.len() < 3 {
        return points.to_vec();
    }
    let mut result = points.to_vec();
    for _ in 0..iterations {
        if result.len() < 3 {
            break;
        }
        let mut out: Vec<Pt> = Vec::new();
        if !closed {
            out.push(result[0]);
        }
        let count = if closed {
            result.len()
        } else {
            result.len() - 1
        };
        for i in 0..count {
            let a = result[i];
            let b = result[(i + 1) % result.len()];
            out.push([a[0] + 0.25 * (b[0] - a[0]), a[1] + 0.25 * (b[1] - a[1])]);
            out.push([a[0] + 0.75 * (b[0] - a[0]), a[1] + 0.75 * (b[1] - a[1])]);
        }
        if !closed {
            out.push(result[result.len() - 1]);
        }
        result = out;
    }
    result
}

/// Счётчики упрощения.
#[derive(Default)]
pub struct Stats {
    pub arcs: usize,
    pub arcs_shared: usize,
    pub vertices_in: usize,
    pub vertices_out: usize,
    pub rings_degenerate: usize,
}

/// Итог упрощения набора колец.
pub struct Simplified {
    pub rings: Vec<Option<Vec<Pt>>>,
    pub stats: Stats,
}

/// Настройки упрощения.
pub struct SimplifySettings {
    pub tolerance: f64,
    pub grid: f64,
    pub min_points: usize,
    pub smooth: usize,
    pub method: Method,
}

/// Упрощает кольца, сохраняя общие границы. `closed` говорит про каждое
/// кольцо, замкнутое оно или разомкнутая линия.
pub fn simplify_topology(rings: &[Vec<Pt>], closed: &[bool], s: &SimplifySettings) -> Simplified {
    let (arc_list, paths) = arcs::build_arcs_open(rings, closed, s.grid);
    let mut stats = Stats {
        arcs: arc_list.len(),
        vertices_in: rings.iter().map(Vec::len).sum(),
        ..Stats::default()
    };

    // Дуга общая, если встречается в путях более чем одного кольца.
    let mut users: Vec<std::collections::BTreeSet<usize>> = vec![Default::default(); arc_list.len()];
    for (ri, path) in paths.iter().enumerate() {
        for &(idx, _rev) in path {
            if let Some(slot) = users.get_mut(idx) {
                slot.insert(ri);
            }
        }
    }
    stats.arcs_shared = users.iter().filter(|v| v.len() > 1).count();

    // ── Прореживание каждой дуги ровно один раз ─────────────────────────
    // Допуск в диалоге всегда в единицах длины. Для Висвалингама он
    // пересчитывается в площадь равностороннего треугольника со стороной,
    // равной допуску.
    let area_tolerance = s.tolerance * s.tolerance * 3.0_f64.sqrt() / 4.0;
    let mut simple: Vec<Vec<Pt>> = arc_list
        .iter()
        .map(|arc| {
            if s.min_points > 0 && arc.len() <= s.min_points {
                return arc.clone();
            }
            match s.method {
                Method::Visvalingam => visvalingam(arc, area_tolerance, 2),
                Method::Douglas => douglas_peucker(arc, s.tolerance),
            }
        })
        .collect();

    // ── Сглаживание, тоже по одному разу на дугу ────────────────────────
    if s.smooth > 0 {
        simple = simple
            .iter()
            .map(|arc| {
                // Замкнутая дуга это целое кольцо без узлов ветвления:
                // у неё нет неподвижных концов, сглаживание идёт по кругу.
                let cycle = arc.len() > 2
                    && (arc[0][0] - arc[arc.len() - 1][0]).abs() <= EPS
                    && (arc[0][1] - arc[arc.len() - 1][1]).abs() <= EPS;
                if cycle {
                    let body = chaikin(&arc[..arc.len() - 1], s.smooth, true);
                    let mut out = body.clone();
                    if let Some(&first) = body.first() {
                        out.push(first);
                    }
                    out
                } else {
                    chaikin(arc, s.smooth, false)
                }
            })
            .collect();
    }

    // ── Сборка колец ────────────────────────────────────────────────────
    let mut out: Vec<Option<Vec<Pt>>> = Vec::with_capacity(paths.len());
    for (ri, path) in paths.iter().enumerate() {
        let mut pts: Vec<Pt> = Vec::new();
        for (k, &(idx, rev)) in path.iter().enumerate() {
            let mut seq = simple[idx].clone();
            if rev {
                seq.reverse();
            }
            // Последняя вершина дуги совпадает с первой вершиной следующей.
            // У разомкнутой линии последнюю дугу берём целиком.
            if !closed[ri] && k == path.len() - 1 {
                pts.extend(seq);
            } else if !seq.is_empty() {
                pts.extend(&seq[..seq.len() - 1]);
            }
        }
        let mut cleaned: Vec<Pt> = Vec::with_capacity(pts.len());
        for p in pts {
            let last = cleaned.last().copied();
            if last.is_none_or(|q| (p[0] - q[0]).abs() > EPS || (p[1] - q[1]).abs() > EPS) {
                cleaned.push(p);
            }
        }
        if closed[ri] {
            while cleaned.len() > 1 {
                let (a, b) = (cleaned[0], cleaned[cleaned.len() - 1]);
                if (a[0] - b[0]).abs() <= EPS && (a[1] - b[1]).abs() <= EPS {
                    cleaned.pop();
                } else {
                    break;
                }
            }
        }
        let minimum = if closed[ri] { 3 } else { 2 };
        if cleaned.len() < minimum {
            stats.rings_degenerate += 1;
            out.push(None);
            continue;
        }
        stats.vertices_out += cleaned.len();
        out.push(Some(cleaned));
    }
    Simplified { rings: out, stats }
}

fn pick(lang: Lang, ru: &str, en: &str) -> String {
    match lang {
        Lang::Ru => ru.to_string(),
        Lang::En => en.to_string(),
    }
}

/// Что получилось у прогона: количество объектов и мера до и после.
pub struct Outcome {
    pub items: usize,
    pub written: usize,
    pub is_polygon: bool,
    /// Площадь для полигонов, длина для линий.
    pub before: f64,
    pub after: f64,
}

/// Строки журнала упрощения.
pub fn report(o: &Outcome, s: &SimplifySettings, stats: &Stats, lang: Lang) -> Vec<String> {
    let (items, written, is_polygon) = (o.items, o.written, o.is_polygon);
    let (before, after) = (o.before, o.after);
    let mut out = vec![
        pick(lang, "Чтение слоя...", "Reading the layer..."),
        String::new(),
        pick(lang, "── Результат ──", "-- Result --"),
        match s.method {
            Method::Visvalingam => pick(lang, "Метод: Висвалингам", "Method: Visvalingam"),
            Method::Douglas => pick(lang, "Метод: Дуглас-Пекер", "Method: Douglas-Peucker"),
        },
    ];
    if s.smooth > 0 {
        out.push(
            pick(lang, "Сглаживание: проходов %d", "Smoothing: %d passes")
                .replacen("%d", &s.smooth.to_string(), 1),
        );
    }
    out.push(
        pick(
            lang,
            "Дуг: %d, из них общих для соседей: %d",
            "Arcs: %d, of which shared by neighbours: %d",
        )
        .replacen("%d", &stats.arcs.to_string(), 1)
        .replacen("%d", &stats.arcs_shared.to_string(), 1),
    );
    let share = if stats.vertices_in > 0 {
        100.0 * stats.vertices_out as f64 / stats.vertices_in as f64
    } else {
        0.0
    };
    out.push(
        pick(
            lang,
            "Вершин было/стало: %d / %d (%.1f %%)",
            "Vertices before/after: %d / %d (%.1f %%)",
        )
        .replacen("%d", &stats.vertices_in.to_string(), 1)
        .replacen("%d", &stats.vertices_out.to_string(), 1)
        .replace("%.1f", &format!("{share:.1}"))
        .replace("%%", "%"),
    );
    let rel = if before != 0.0 {
        100.0 * (after - before) / before
    } else {
        0.0
    };
    out.push(
        if is_polygon {
            pick(
                lang,
                "Площадь до/после: %s / %s (%+.3f %%)",
                "Area before/after: %s / %s (%+.3f %%)",
            )
        } else {
            pick(
                lang,
                "Длина до/после: %s / %s (%+.3f %%)",
                "Length before/after: %s / %s (%+.3f %%)",
            )
        }
        .replacen("%s", &fmt(before), 1)
        .replacen("%s", &fmt(after), 1)
        .replace("%+.3f", &format!("{rel:+.3}"))
        .replace("%%", "%"),
    );
    out.push(
        pick(lang, "Объектов на входе/выходе: %d / %d", "Objects in/out: %d / %d")
            .replacen("%d", &items.to_string(), 1)
            .replacen("%d", &written.to_string(), 1),
    );
    if stats.arcs_shared == 0 && stats.arcs > 0 {
        out.push(pick(
            lang,
            "Общих границ не найдено. Если объекты соприкасаются, увеличьте точность опознания общих вершин.",
            "No shared borders were found. If the objects do touch, increase the precision for matching shared vertices.",
        ));
    }
    out
}

/// Кольца объектов и адрес каждого кольца, с признаком замкнутости.
pub struct Flat {
    pub rings: Vec<Vec<Pt>>,
    pub closed: Vec<bool>,
    pub owner: Vec<(usize, usize)>,
}

/// Разбирает объекты в плоский список колец. Кольцо полигона приходит
/// замкнутым, повтор первой вершины ядру не нужен. Линия остаётся как есть.
pub fn flatten(items: &[arcs::Shape], is_polygon: bool) -> Flat {
    let mut flat = Flat {
        rings: Vec::new(),
        closed: Vec::new(),
        owner: Vec::new(),
    };
    for (k, (_fid, parts)) in items.iter().enumerate() {
        for (p, part) in parts.iter().enumerate() {
            for ring in part {
                let mut coords = ring.clone();
                if is_polygon && coords.len() > 1 && coords[0] == coords[coords.len() - 1] {
                    coords.pop();
                }
                flat.rings.push(coords);
                flat.closed.push(is_polygon);
                flat.owner.push((k, p));
            }
        }
    }
    flat
}

/// Собирает кольца обратно по объектам.
pub fn rebuild(items: &[arcs::Shape], flat: &Flat, rings: &[Option<Vec<Pt>>]) -> Vec<Parts> {
    let mut out: Vec<Parts> = items
        .iter()
        .map(|(_fid, parts)| vec![Vec::new(); parts.len()])
        .collect();
    for (i, &(k, p)) in flat.owner.iter().enumerate() {
        if let Some(ring) = rings.get(i).and_then(Option::as_ref)
            && let Some(part) = out.get_mut(k).and_then(|o| o.get_mut(p))
        {
            part.push(ring.clone());
        }
    }
    for shape in out.iter_mut() {
        shape.retain(|part| !part.is_empty());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Пила вдоль прямой срезается Дугласом-Пекером целиком.
    #[test]
    fn a_saw_is_thinned_away() {
        let saw: Vec<Pt> = vec![
            [10.0, 0.0],
            [10.4, 1.25],
            [9.6, 2.5],
            [10.4, 3.75],
            [9.6, 5.0],
            [10.0, 10.0],
        ];
        let got = douglas_peucker(&saw, 1.0);
        assert_eq!(got, vec![[10.0, 0.0], [10.0, 10.0]]);
        // Висвалингам на той же пиле держит вершины
        let kept = visvalingam(&saw, 1.0 * 1.0 * 3.0_f64.sqrt() / 4.0, 2);
        assert_eq!(kept.len(), saw.len());
    }

    /// Сглаживание срезает углы и концы линии не двигает.
    #[test]
    fn chaikin_keeps_the_ends() {
        let line = vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0]];
        let got = chaikin(&line, 1, false);
        assert_eq!(got.first(), Some(&[0.0, 0.0]));
        assert_eq!(got.last(), Some(&[10.0, 10.0]));
        assert!(got.len() > line.len());
    }
}
