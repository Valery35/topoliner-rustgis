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

/// Настройки очистки линий.
pub struct FixSettings {
    pub tolerance: f64,
    pub min_length: f64,
    pub trim_overshoots: bool,
    pub close_undershoots: bool,
    pub snap: bool,
    pub drop_short: bool,
    pub spike_angle: f64,
}

/// Счётчики очистки линий.
#[derive(Default)]
pub struct FixStats {
    pub dup_vertices: usize,
    pub spikes: usize,
    pub vertices_moved: usize,
    pub nodes_inserted: usize,
    pub overshoots_trimmed: usize,
    pub undershoots_closed: usize,
    pub zero_dropped: usize,
    pub short_dropped: usize,
    pub max_move: f64,
    pub length_before: f64,
    pub length_after: f64,
}

/// Итог очистки линий.
pub struct Fixed {
    /// Линии по объектам входа, `None` - линия удалена.
    pub lines: Vec<Option<Vec<Pt>>>,
    pub stats: FixStats,
    pub left: Vec<Finding>,
}

/// Обрезает хвост линии до ближайшего пересечения с чужой линией.
fn trim_tail(
    lines: &[Vec<Pt>],
    index: &Grid,
    i: usize,
    side: usize,
    tolerance: f64,
) -> Option<Vec<Pt>> {
    let work: Vec<Pt> = if side == 1 {
        lines[i].clone()
    } else {
        lines[i].iter().rev().copied().collect()
    };
    let mut walked = 0.0;
    for k in (1..work.len()).rev() {
        let (a, b) = (work[k], work[k - 1]);
        let seg_len = (b[0] - a[0]).hypot(b[1] - a[1]);
        let mut best: Option<(Pt, f64)> = None;
        for j in index.near(a, tolerance + seg_len) {
            if j == i {
                continue;
            }
            for w in lines[j].windows(2) {
                let Some(hit) = segment_intersection((a, b), (w[0], w[1])) else {
                    continue;
                };
                let d = (hit[0] - a[0]).hypot(hit[1] - a[1]);
                if best.is_none_or(|(_p, bd)| d < bd) {
                    best = Some((hit, d));
                }
            }
        }
        if let Some((hit, _d)) = best {
            let mut cut: Vec<Pt> = work[..k].to_vec();
            cut.push(hit);
            if side == 0 {
                cut.reverse();
            }
            return Some(cut);
        }
        walked += seg_len;
        if walked > tolerance {
            return None;
        }
    }
    None
}

/// Ближайшая точка на чужой линии в пределах допуска.
fn nearest_point_on_lines(
    p: Pt,
    lines: &[Vec<Pt>],
    index: &Grid,
    skip: usize,
    tolerance: f64,
) -> Option<Pt> {
    let mut best: Option<Pt> = None;
    let mut best_d = tolerance;
    for j in index.near(p, tolerance) {
        if j == skip {
            continue;
        }
        for w in lines[j].windows(2) {
            let (d, t) = point_segment(p, w[0], w[1]);
            if d <= best_d {
                best_d = d;
                best = Some([
                    w[0][0] + t * (w[1][0] - w[0][0]),
                    w[0][1] + t * (w[1][1] - w[0][1]),
                ]);
            }
        }
    }
    best
}

/// Индекс линий и количество концов в каждой точке.
fn index_and_ends(lines: &[Vec<Pt>], cell: f64) -> (Grid, HashMap<(i64, i64), usize>) {
    let mut index = Grid::new(cell);
    for (i, coords) in lines.iter().enumerate() {
        if coords.len() >= 2 {
            index.add(i, coords);
        }
    }
    let mut ends: HashMap<(i64, i64), usize> = HashMap::new();
    for coords in lines.iter().filter(|c| c.len() >= 2) {
        for p in [coords[0], coords[coords.len() - 1]] {
            *ends.entry(key_of(p)).or_insert(0) += 1;
        }
    }
    (index, ends)
}

/// 1.04 Очистка топологии линий. Перенос `line_checks.fix_lines`.
pub fn fix_lines(items: &[(i64, Vec<Pt>)], s: &FixSettings) -> Fixed {
    let fids: Vec<i64> = items.iter().map(|(f, _)| *f).collect();
    let mut lines: Vec<Vec<Pt>> = items.iter().map(|(_, c)| c.clone()).collect();
    let mut stats = FixStats {
        length_before: lines.iter().filter(|c| c.len() >= 2).map(|c| length_of(c)).sum(),
        ..FixStats::default()
    };
    let mut left: Vec<Finding> = Vec::new();

    // ── Шаг 1. Артефакты вершин ─────────────────────────────────────────
    for coords in lines.iter_mut() {
        if coords.len() < 2 {
            continue;
        }
        let (out, dups) = rings::drop_repeated_vertices(coords, false, EPS);
        stats.dup_vertices += dups;
        let (out, spikes) = rings::remove_spikes(&out, false, s.spike_angle);
        stats.spikes += spikes;
        *coords = out;
    }

    // ── Шаг 2. Обрезка перелётов ────────────────────────────────────────
    let cell = (s.tolerance * 4.0).max(1e-6);
    if s.trim_overshoots {
        let (index, ends) = index_and_ends(&lines, cell);
        for i in 0..lines.len() {
            if lines[i].len() < 2 {
                continue;
            }
            for side in [1usize, 0] {
                let coords = &lines[i];
                let point = if side == 1 {
                    coords[coords.len() - 1]
                } else {
                    coords[0]
                };
                if ends.get(&key_of(point)).copied().unwrap_or(0) > 1 {
                    continue;
                }
                let Some(tail) = overshoot_length(&lines, &index, i, side, s.tolerance) else {
                    continue;
                };
                if tail > s.tolerance {
                    continue;
                }
                if let Some(trimmed) = trim_tail(&lines, &index, i, side, s.tolerance)
                    && trimmed.len() >= 2
                {
                    lines[i] = trimmed;
                    stats.overshoots_trimmed += 1;
                }
            }
        }
    }

    // ── Шаг 2а. Дотягивание недоводов ───────────────────────────────────
    // Вставка узлов вершины не двигает, поэтому недовод она не закрывает:
    // конец надо перенести на проекцию, на соседнюю линию.
    if s.close_undershoots {
        let (index, ends) = index_and_ends(&lines, cell);
        for i in 0..lines.len() {
            if lines[i].len() < 2 {
                continue;
            }
            for side in [0usize, 1] {
                let coords = &lines[i];
                let point = if side == 0 {
                    coords[0]
                } else {
                    coords[coords.len() - 1]
                };
                if ends.get(&key_of(point)).copied().unwrap_or(0) > 1 {
                    continue;
                }
                let Some(target) = nearest_point_on_lines(point, &lines, &index, i, s.tolerance)
                else {
                    continue;
                };
                let shift = (target[0] - point[0]).hypot(target[1] - point[1]);
                if shift <= EPS {
                    continue;
                }
                let at = if side == 0 { 0 } else { lines[i].len() - 1 };
                lines[i][at] = target;
                stats.undershoots_closed += 1;
                stats.max_move = stats.max_move.max(shift);
                stats.vertices_moved += 1;
            }
        }
    }

    // ── Шаг 3. Вставка узлов ────────────────────────────────────────────
    if s.snap {
        let usable: Vec<usize> = (0..lines.len()).filter(|&i| lines[i].len() >= 2).collect();
        if !usable.is_empty() {
            let work: Vec<Vec<Pt>> = usable.iter().map(|&i| lines[i].clone()).collect();
            let done = crate::snap::insert_nodes(
                &work,
                crate::snap::Nodes {
                    tolerance: s.tolerance,
                    crossings: true,
                    project_onto_edge: true,
                },
            );
            stats.nodes_inserted = done.stats.nodes_inserted;
            for (pos, &i) in usable.iter().enumerate() {
                if let Some(Some(line)) = done.rings.get(pos) {
                    lines[i] = line.clone();
                }
            }
        }
    }

    // ── Шаг 4. Итог ─────────────────────────────────────────────────────
    let mut out: Vec<Option<Vec<Pt>>> = Vec::with_capacity(lines.len());
    for (i, coords) in lines.iter().enumerate() {
        let length = if coords.len() >= 2 {
            length_of(coords)
        } else {
            0.0
        };
        // Линия нулевой длины удаляется всегда, как в модуле.
        if coords.len() < 2 || length <= EPS {
            stats.zero_dropped += 1;
            out.push(None);
            continue;
        }
        if s.min_length > 0.0 && length < s.min_length {
            if s.drop_short {
                stats.short_dropped += 1;
                out.push(None);
                continue;
            }
            left.push(
                Finding::new(
                    Kind::ShortLine,
                    SEVERITY_REVIEW,
                    Text::of(
                        "линия короче порога, удаление не выполнялось",
                        "the line is shorter than the threshold, no deletion was performed",
                    ),
                )
                .of(fids[i])
                .value(length)
                .at(coords.first().copied()),
            );
        }
        stats.length_after += length;
        out.push(Some(coords.clone()));
    }
    Fixed {
        lines: out,
        stats,
        left,
    }
}

/// Строки журнала очистки линий. `lost` - объекты, у которых не осталось
/// ни одной части.
pub fn fix_report(items: usize, written: usize, lost: usize, f: &Fixed, lang: Lang) -> Vec<String> {
    let pick = |ru: &str, en: &str| match lang {
        Lang::Ru => ru.to_string(),
        Lang::En => en.to_string(),
    };
    let s = &f.stats;
    let one = |ru: &str, en: &str, n: usize| pick(ru, en).replacen("%d", &n.to_string(), 1);
    let mut out = vec![
        String::new(),
        pick("── Исправлено молча ──", "-- Fixed silently --"),
        one(
            "Повторяющихся вершин снято:  %d",
            "Repeated vertices removed:   %d",
            s.dup_vertices,
        ),
        one("Игл снято:                   %d", "Spikes removed:              %d", s.spikes),
        one(
            "Перелётов обрезано:          %d",
            "Overshoots trimmed:          %d",
            s.overshoots_trimmed,
        ),
        pick(
            "Недоводов закрыто:           %d (макс. смещение %s)",
            "Undershoots closed:          %d (max shift %s)",
        )
        .replacen("%d", &s.undershoots_closed.to_string(), 1)
        .replacen("%s", &fmt(s.max_move), 1),
        one(
            "Узлов вставлено:             %d",
            "Nodes inserted:              %d",
            s.nodes_inserted,
        ),
    ];
    if s.zero_dropped > 0 {
        out.push(one(
            "Линий нулевой длины удалено: %d",
            "Zero length lines removed:   %d",
            s.zero_dropped,
        ));
    }
    if s.short_dropped > 0 {
        out.push(one(
            "Коротких линий удалено:      %d",
            "Short lines removed:         %d",
            s.short_dropped,
        ));
    }
    let (before, after) = (s.length_before, s.length_after);
    let rel = if before != 0.0 {
        100.0 * (after - before) / before
    } else {
        0.0
    };
    out.push(String::new());
    out.push(
        pick(
            "Длина до/после: %s / %s (%+.3f %%)",
            "Length before/after: %s / %s (%+.3f %%)",
        )
        .replacen("%s", &fmt(before), 1)
        .replacen("%s", &fmt(after), 1)
        .replace("%+.3f", &format!("{rel:+.3}"))
        .replace("%%", "%"),
    );
    out.push(
        pick("Объектов на входе/выходе: %d / %d", "Objects in/out: %d / %d")
            .replacen("%d", &items.to_string(), 1)
            .replacen("%d", &written.to_string(), 1),
    );
    if lost > 0 {
        out.push(one("Объектов потеряно: %d", "Objects lost: %d", lost));
    }
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
