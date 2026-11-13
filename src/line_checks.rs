// Topoliner для RustGIS - расширение-процесс RustGIS.
// Copyright (C) 2026 ООО «Информ++». Лицензия GPL-3.0-or-later.
// SPDX-License-Identifier: GPL-3.0-or-later

//! 1.02 Проверка топологии линий. Перенос модуля `line_checks` плагина
//! QGIS вместе с тем, что делает поверх ядра диалог `LineAuditAlgorithm`.
//!
//! У линий свой набор нарушений. Щели, перекрытия, вложения и волосяные
//! полигоны к ним не применимы вовсе, зато появляются висячие концы,
//! недоводы, перелёты и псевдоузлы.
//!
//! Разделение то же, что и везде. Висячий конец у гидросети или сети
//! выработок обычно является устьем или тупиком, поэтому он остаётся
//! человеку. Недовод и перелёт короче допуска являются следом оцифровки.

use std::collections::HashMap;

use rustgis_kit::Lang;
use serde_json::{Value as J, json};

use crate::audit::{Finding, Kind, SEVERITY_AUTO, SEVERITY_REVIEW, Text, finding_fields};
use crate::rings::{self, DEFAULT_SPIKE_ANGLE, Pt, point_segment};
use crate::rounding::{fmt, nice};

/// Порог точного совпадения.
const EPS: f64 = 1e-9;

/// Сетка опознания общих вершин.
const GRID: f64 = 1e-7;

/// Ключ вершины на сетке опознания.
fn key_of(p: Pt) -> (i64, i64) {
    (
        (p[0] / GRID).round_ties_even() as i64,
        (p[1] / GRID).round_ties_even() as i64,
    )
}

/// Длина ломаной.
pub fn length_of(coords: &[Pt]) -> f64 {
    coords
        .windows(2)
        .map(|w| (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1]))
        .sum()
}

/// Минимальное расстояние от точки до ломаной.
fn distance_to_line(p: Pt, coords: &[Pt]) -> f64 {
    coords
        .windows(2)
        .map(|w| point_segment(p, w[0], w[1]).0)
        .fold(f64::INFINITY, f64::min)
}

/// Сеточный индекс линий по охватам.
struct Grid {
    cell: f64,
    cells: HashMap<(i64, i64), Vec<usize>>,
}

impl Grid {
    fn new(cell: f64) -> Self {
        Self {
            cell: if cell > 0.0 { cell } else { 1.0 },
            cells: HashMap::new(),
        }
    }

    fn add(&mut self, key: usize, coords: &[Pt]) {
        let at = |v: f64| (v / self.cell).floor() as i64;
        let (mut x0, mut y0) = (f64::INFINITY, f64::INFINITY);
        let (mut x1, mut y1) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
        for p in coords {
            x0 = x0.min(p[0]);
            y0 = y0.min(p[1]);
            x1 = x1.max(p[0]);
            y1 = y1.max(p[1]);
        }
        let (i0, i1, j0, j1) = (at(x0), at(x1), at(y0), at(y1));
        let si = ((i1 - i0) / 400).max(1);
        let sj = ((j1 - j0) / 400).max(1);
        let mut i = i0;
        while i <= i1 {
            let mut j = j0;
            while j <= j1 {
                self.cells.entry((i, j)).or_default().push(key);
                j += sj;
            }
            i += si;
        }
    }

    /// Линии около точки, по возрастанию номера.
    fn near(&self, p: Pt, radius: f64) -> Vec<usize> {
        let cx = (p[0] / self.cell).floor() as i64;
        let cy = (p[1] / self.cell).floor() as i64;
        let span = ((radius / self.cell).ceil() as i64).max(1);
        let mut out = Vec::new();
        for i in cx - span..=cx + span {
            for j in cy - span..=cy + span {
                if let Some(b) = self.cells.get(&(i, j)) {
                    out.extend_from_slice(b);
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// Точка пересечения двух отрезков строго внутри обоих.
fn segment_intersection(a: (Pt, Pt), b: (Pt, Pt)) -> Option<Pt> {
    let (ax, ay) = (a.1[0] - a.0[0], a.1[1] - a.0[1]);
    let (bx, by) = (b.1[0] - b.0[0], b.1[1] - b.0[1]);
    let den = ax * by - ay * bx;
    if den.abs() < 1e-15 {
        return None;
    }
    let (dx, dy) = (b.0[0] - a.0[0], b.0[1] - a.0[1]);
    let t = (dx * by - dy * bx) / den;
    let u = (dx * ay - dy * ax) / den;
    if t <= 0.0 || t >= 1.0 || u <= 0.0 || u >= 1.0 {
        return None;
    }
    Some([a.0[0] + t * ax, a.0[1] + t * ay])
}

/// Длина хвоста за ближайшим пересечением с чужой линией.
///
/// Перелёт это короткий хвост, торчащий за узлом: линия пересекла соседа
/// и продолжилась дальше на несколько сантиметров.
fn overshoot_length(
    lines: &[Vec<Pt>],
    index: &Grid,
    i: usize,
    side: usize,
    tolerance: f64,
) -> Option<f64> {
    let coords: Vec<Pt> = if side == 1 {
        lines[i].clone()
    } else {
        lines[i].iter().rev().copied().collect()
    };
    let mut walked = 0.0;
    for k in (1..coords.len()).rev() {
        let (a, b) = (coords[k], coords[k - 1]);
        let seg_len = (b[0] - a[0]).hypot(b[1] - a[1]);
        for j in index.near(a, tolerance + seg_len) {
            if j == i {
                continue;
            }
            for w in lines[j].windows(2) {
                let Some(hit) = segment_intersection((a, b), (w[0], w[1])) else {
                    continue;
                };
                return Some(walked + (hit[0] - a[0]).hypot(hit[1] - a[1]));
            }
        }
        walked += seg_len;
        if walked > tolerance {
            return None;
        }
    }
    None
}

/// Точки, где две линии пересекаются, а вершины там нет ни у одной.
fn crossings(lines: &[Vec<Pt>], index: &Grid) -> Vec<(Pt, usize, usize)> {
    let mut out = Vec::new();
    let mut checked: HashMap<(usize, usize), ()> = HashMap::new();
    for (i, a) in lines.iter().enumerate() {
        if a.len() < 2 {
            continue;
        }
        'segments: for k in 0..a.len() - 1 {
            for j in index.near(a[k], 0.0) {
                if j <= i || checked.contains_key(&(i, j)) {
                    continue;
                }
                let b = &lines[j];
                for m in 0..b.len().saturating_sub(1) {
                    let Some(hit) = segment_intersection((a[k], a[k + 1]), (b[m], b[m + 1]))
                    else {
                        continue;
                    };
                    let key = key_of(hit);
                    let has_vertex = [a[k], a[k + 1], b[m], b[m + 1]]
                        .iter()
                        .any(|p| key_of(*p) == key);
                    if !has_vertex {
                        out.push(([round9(hit[0]), round9(hit[1])], i, j));
                        checked.insert((i, j), ());
                        continue 'segments;
                    }
                }
            }
        }
    }
    out
}

fn round9(v: f64) -> f64 {
    format!("{v:.9}").parse().unwrap_or(v)
}

/// Настройки проверки линий.
pub struct Settings {
    pub tolerance: f64,
    pub min_length: f64,
    pub do_dangles: bool,
    pub do_crossings: bool,
    pub do_pseudo: bool,
}

/// Проверка линейного покрытия. `items` - номер объекта и его вершины.
pub fn check_lines(items: &[(i64, Vec<Pt>)], s: &Settings) -> Vec<Finding> {
    let mut findings: Vec<Finding> = Vec::new();
    let fids: Vec<i64> = items.iter().map(|(f, _)| *f).collect();
    let lines: Vec<Vec<Pt>> = items.iter().map(|(_, c)| c.clone()).collect();

    // ── Пообъектные проверки ────────────────────────────────────────────
    for (i, coords) in lines.iter().enumerate() {
        let fid = fids[i];
        if coords.len() < 2 {
            findings.push(
                Finding::new(
                    Kind::Invalid,
                    SEVERITY_REVIEW,
                    Text::of("в линии меньше двух вершин", "the line has fewer than two vertices"),
                )
                .of(fid),
            );
            continue;
        }
        let length = length_of(coords);
        if length <= EPS {
            findings.push(
                Finding::new(
                    Kind::ZeroLength,
                    SEVERITY_AUTO,
                    Text::of("все вершины в одной точке", "all vertices are at the same point"),
                )
                .of(fid)
                .value(length)
                .at(coords.first().copied()),
            );
            continue;
        }
        if s.min_length > 0.0 && length < s.min_length {
            findings.push(
                Finding::new(
                    Kind::ShortLine,
                    SEVERITY_REVIEW,
                    Text::of(
                        &format!("длина {} при пороге {}", fmt(length), fmt(s.min_length)),
                        &format!(
                            "length {} against the threshold {}",
                            fmt(length),
                            fmt(s.min_length)
                        ),
                    ),
                )
                .of(fid)
                .value(length)
                .at(coords.first().copied()),
            );
        }
        let (_r, dups) = rings::drop_repeated_vertices(coords, false, EPS);
        if dups > 0 {
            findings.push(
                Finding::new(
                    Kind::DupVertex,
                    SEVERITY_AUTO,
                    Text::of(
                        &format!("вершин подряд в одной точке: {dups}"),
                        &format!("vertices in a row at the same point: {dups}"),
                    ),
                )
                .of(fid)
                .value(dups as f64)
                .at(coords.first().copied()),
            );
        }
        let (_r, spikes) = rings::remove_spikes(coords, false, DEFAULT_SPIKE_ANGLE);
        if spikes > 0 {
            findings.push(
                Finding::new(
                    Kind::Spike,
                    SEVERITY_AUTO,
                    Text::of(
                        &format!("разворотов линии назад: {spikes}"),
                        &format!("line turns back on itself: {spikes}"),
                    ),
                )
                .of(fid)
                .value(spikes as f64)
                .at(coords.first().copied()),
            );
        }
    }

    // ── Индексы концов и линий ──────────────────────────────────────────
    let mut end_order: Vec<(i64, i64)> = Vec::new();
    let mut ends: HashMap<(i64, i64), Vec<(usize, usize)>> = HashMap::new();
    for (i, coords) in lines.iter().enumerate() {
        if coords.len() < 2 {
            continue;
        }
        for (side, p) in [(0usize, coords[0]), (1, coords[coords.len() - 1])] {
            let key = key_of(p);
            let slot = ends.entry(key).or_default();
            if slot.is_empty() {
                end_order.push(key);
            }
            slot.push((i, side));
        }
    }
    let mut index = Grid::new((s.tolerance * 4.0).max(1e-6));
    for (i, coords) in lines.iter().enumerate() {
        if coords.len() >= 2 {
            index.add(i, coords);
        }
    }

    // ── Дубликаты ───────────────────────────────────────────────────────
    let mut seen: HashMap<Vec<(i64, i64)>, usize> = HashMap::new();
    for (i, coords) in lines.iter().enumerate() {
        if coords.len() < 2 {
            continue;
        }
        let key: Vec<(i64, i64)> = coords.iter().map(|&p| key_of(p)).collect();
        let rev: Vec<(i64, i64)> = key.iter().rev().copied().collect();
        let signature = key.min(rev);
        match seen.get(&signature) {
            Some(&first) => findings.push(
                Finding::new(
                    Kind::Duplicate,
                    SEVERITY_REVIEW,
                    Text::of(
                        "вершины совпадают, атрибуты могут различаться",
                        "the vertices coincide, the attributes may differ",
                    ),
                )
                .pair(fids[first], fids[i])
                .value(length_of(coords))
                .at(coords.first().copied()),
            ),
            None => {
                seen.insert(signature, i);
            }
        }
    }

    // ── Висячие концы, недоводы, перелёты ───────────────────────────────
    if s.do_dangles {
        for (i, coords) in lines.iter().enumerate() {
            if coords.len() < 2 {
                continue;
            }
            for (side, point) in [(0usize, coords[0]), (1, coords[coords.len() - 1])] {
                // Конец стыкуется с другим концом.
                if ends.get(&key_of(point)).is_some_and(|v| v.len() > 1) {
                    continue;
                }
                let mut on_line = false;
                let mut nearest = f64::INFINITY;
                for j in index.near(point, s.tolerance) {
                    if j == i {
                        continue;
                    }
                    let d = distance_to_line(point, &lines[j]);
                    if d <= EPS {
                        on_line = true;
                        break;
                    }
                    nearest = nearest.min(d);
                }
                if on_line {
                    continue;
                }
                // Перелёт проверяется раньше недовода: у хвоста, торчащего
                // за пересечением, конец тоже отстоит от соседней линии.
                if let Some(tail) = overshoot_length(&lines, &index, i, side, s.tolerance)
                    && tail <= s.tolerance
                {
                    findings.push(
                        Finding::new(
                            Kind::Overshoot,
                            SEVERITY_AUTO,
                            Text::of(
                                &format!("хвост за узлом длиной {}", fmt(tail)),
                                &format!("a tail past a node, length {}", fmt(tail)),
                            ),
                        )
                        .of(fids[i])
                        .value(tail)
                        .at(Some(point)),
                    );
                    continue;
                }
                if nearest <= s.tolerance {
                    findings.push(
                        Finding::new(
                            Kind::Undershoot,
                            SEVERITY_AUTO,
                            Text::of(
                                &format!("не доходит до соседней линии на {}", fmt(nearest)),
                                &format!(
                                    "falls short of the neighbouring line by {}",
                                    fmt(nearest)
                                ),
                            ),
                        )
                        .of(fids[i])
                        .value(nearest)
                        .at(Some(point)),
                    );
                    continue;
                }
                findings.push(
                    Finding::new(
                        Kind::Dangle,
                        SEVERITY_REVIEW,
                        Text::of(
                            "конец ни с чем не соединён, соседей ближе допуска нет",
                            "the end is connected to nothing, no neighbour within the tolerance",
                        ),
                    )
                    .of(fids[i])
                    .value(0.0)
                    .at(Some(point)),
                );
            }
        }
    }

    // ── Пересечения без узла ────────────────────────────────────────────
    if s.do_crossings {
        for (point, i, j) in crossings(&lines, &index) {
            findings.push(
                Finding::new(
                    Kind::Crossing,
                    SEVERITY_AUTO,
                    Text::of(
                        "линии пересекаются, узла в точке нет",
                        "the lines cross, there is no node at the point",
                    ),
                )
                .pair(fids[i], fids[j])
                .value(0.0)
                .at(Some(point)),
            );
        }
    }

    // ── Псевдоузлы ──────────────────────────────────────────────────────
    if s.do_pseudo {
        for key in &end_order {
            let Some(owners) = ends.get(key) else { continue };
            if owners.len() != 2 {
                continue;
            }
            let (a, b) = (owners[0].0, owners[1].0);
            // Кольцо, замкнутое само на себя.
            if a == b {
                continue;
            }
            let point = if owners[0].1 == 0 {
                lines[a][0]
            } else {
                lines[a][lines[a].len() - 1]
            };
            findings.push(
                Finding::new(
                    Kind::PseudoNode,
                    SEVERITY_REVIEW,
                    Text::of(
                        "две линии можно объединить в одну",
                        "the two lines can be merged into one",
                    ),
                )
                .pair(fids[a], fids[b])
                .value(0.0)
                .at(Some(point)),
            );
        }
    }

    findings
}

/// Находки слоем точек, поля те же, что у 1.01.
pub fn findings_layer(findings: &[Finding], lang: Lang) -> J {
    let features: Vec<J> = findings
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let geometry = match f.xy {
                Some(p) => json!({"xyz": [p[0], p[1], J::Null]}),
                None => J::Null,
            };
            json!({
                "values": [
                    i + 1,
                    f.kind.code(),
                    f.kind.label(),
                    f.severity,
                    f.fid.unwrap_or(-1),
                    f.fid_b.unwrap_or(-1),
                    nice(f.value),
                    f.note.get(lang),
                    f.key,
                ],
                "geometry": geometry,
            })
        })
        .collect();
    json!({"geometry": "point", "fields": finding_fields(), "features": features})
}

/// Строки журнала, как их печатает диалог модуля. Названия нарушений
/// в сводке идут на языке показа, не как у 1.01.
pub fn report(items: usize, s: &Settings, findings: &[Finding], lang: Lang) -> Vec<String> {
    let pick = |ru: &str, en: &str| match lang {
        Lang::Ru => ru.to_string(),
        Lang::En => en.to_string(),
    };
    let mut out = vec![
        pick("Линий: %d, допуск %g", "Lines: %d, tolerance %g")
            .replacen("%d", &items.to_string(), 1)
            .replacen("%g", &crate::audit::g_format(s.tolerance), 1),
        String::new(),
        pick("── Топология линий ──", "-- Line topology --"),
    ];
    let summary = crate::audit::summarize(findings);
    if summary.is_empty() {
        out.push(pick("Нарушений не найдено.", "No violations found."));
    } else {
        out.push(format!(
            "{:<40} {:>8} {:>8} {:>12} {:>12}",
            pick("нарушение", "violation"),
            pick("чинится", "auto"),
            pick("решать", "operator"),
            pick("медиана", "median"),
            pick("максимум", "maximum")
        ));
        let mut order: Vec<&crate::audit::Slot> = summary.iter().collect();
        order.sort_by_key(|s| std::cmp::Reverse(s.auto + s.review));
        for slot in order {
            out.push(format!(
                "{:<40} {:>8} {:>8} {:>12.4} {:>12.4}",
                match lang {
                    Lang::Ru => slot.kind.label_ru(),
                    Lang::En => slot.kind.label(),
                },
                slot.auto,
                slot.review,
                slot.value_med,
                slot.value_max
            ));
        }
    }
    out.push(String::new());
    let auto = findings
        .iter()
        .filter(|f| f.severity == SEVERITY_AUTO)
        .count();
    out.push(
        pick(
            "Всего находок: %d, из них чинится автоматически: %d, решать человеку: %d",
            "Findings in total: %d, auto-fixed: %d, left to the operator: %d",
        )
        .replacen("%d", &findings.len().to_string(), 1)
        .replacen("%d", &auto.to_string(), 1)
        .replacen("%d", &(findings.len() - auto).to_string(), 1),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Недовод, перелёт и висячие концы различаются.
    #[test]
    fn three_cases_at_a_line_end() {
        let items = vec![
            (1, vec![[0.0, 0.0], [100.0, 0.0]]),
            (2, vec![[50.0, 20.0], [50.0, 0.5]]),
            (3, vec![[30.0, -20.0], [30.0, 1.5]]),
        ];
        let got = check_lines(
            &items,
            &Settings {
                tolerance: 2.0,
                min_length: 0.0,
                do_dangles: true,
                do_crossings: true,
                do_pseudo: false,
            },
        );
        let kinds: Vec<&str> = got.iter().map(|f| f.kind.code()).collect();
        assert_eq!(
            kinds,
            vec!["dangle", "dangle", "dangle", "undershoot", "dangle", "overshoot"],
            "{kinds:?}"
        );
        assert_eq!(got[3].value, 0.5);
        assert_eq!(got[5].value, 1.5);
    }
}
