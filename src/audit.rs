// Topoliner для RustGIS - расширение-процесс RustGIS.
// Copyright (C) 2026 ООО «Информ++». Лицензия GPL-3.0-or-later.
// SPDX-License-Identifier: GPL-3.0-or-later

//! 1.01 Проверка топологии полигонов. Перенос `topo_checks.check_items`
//! модуля QGIS вместе с тем, что делает поверх ядра диалог
//! `TopologyAuditAlgorithm`: состав полей, округление величин, пояснения
//! и сводка в журнале.
//!
//! Слой не изменяется. Находки делятся по полю `severity`: `auto` -
//! заведомый мусор, `review` - возможный смысл. Границу проводят два
//! порога, допуск в единицах длины и порог площади.

use rustgis_kit::Lang;
use serde_json::{Value as J, json};

use crate::backend::{self, Geom};
use crate::rings::{self, DEFAULT_SPIKE_ANGLE, Pt};
use crate::rounding::{fmt, nice};
use crate::snap;

/// Площадь, ниже которой пересечение считается пустым.
const AREA_EPS: f64 = 1e-9;

pub const SEVERITY_AUTO: &str = "auto";
pub const SEVERITY_REVIEW: &str = "review";

/// Строка на двух языках.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Text {
    pub ru: String,
    pub en: String,
}

impl Text {
    fn of(ru: &str, en: &str) -> Self {
        Self {
            ru: ru.to_string(),
            en: en.to_string(),
        }
    }

    pub fn get(&self, lang: Lang) -> &str {
        match lang {
            Lang::Ru => &self.ru,
            Lang::En => &self.en,
        }
    }
}

/// Вид нарушения: код поля `type` и название на двух языках.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Invalid,
    SelfTouch,
    DupVertex,
    Spike,
    Unsnapped,
    OnEdge,
    TinyHole,
    TinyPart,
    TinyFeature,
    Sliver,
    Overlap,
    Gap,
    Duplicate,
    Nested,
    Lost,
}

impl Kind {
    pub fn code(self) -> &'static str {
        match self {
            Kind::Invalid => "invalid",
            Kind::SelfTouch => "self_touch",
            Kind::DupVertex => "dup_vertex",
            Kind::Spike => "spike",
            Kind::Unsnapped => "unsnapped",
            Kind::OnEdge => "on_edge",
            Kind::TinyHole => "tiny_hole",
            Kind::TinyPart => "tiny_part",
            Kind::TinyFeature => "tiny_feature",
            Kind::Sliver => "sliver",
            Kind::Overlap => "overlap",
            Kind::Gap => "gap",
            Kind::Duplicate => "duplicate",
            Kind::Nested => "nested",
            Kind::Lost => "lost",
        }
    }

    /// Название нарушения. Модуль печатает его по-английски на обоих
    /// языках показа, и прогоны сверяются с этим.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Invalid => "invalid geometry",
            Kind::SelfTouch => "ring self-touch",
            Kind::DupVertex => "repeated vertices",
            Kind::Spike => "spike",
            Kind::Unsnapped => "vertex near a neighbour edge",
            Kind::OnEdge => "vertex lies on a neighbour edge without a node",
            Kind::TinyHole => "tiny hole",
            Kind::TinyPart => "tiny part",
            Kind::TinyFeature => "tiny object",
            Kind::Sliver => "sliver polygon",
            Kind::Overlap => "overlap",
            Kind::Gap => "gap in the coverage",
            Kind::Duplicate => "duplicate object",
            Kind::Nested => "object inside another",
            Kind::Lost => "object lost during repair",
        }
    }
}

/// Находка проверки.
#[derive(Debug, Clone)]
pub struct Finding {
    pub kind: Kind,
    pub severity: &'static str,
    pub fid: Option<i64>,
    pub fid_b: Option<i64>,
    pub value: f64,
    pub xy: Option<Pt>,
    pub note: Text,
    pub key: String,
}

impl Finding {
    fn new(kind: Kind, severity: &'static str, note: Text) -> Self {
        Self {
            kind,
            severity,
            fid: None,
            fid_b: None,
            value: 0.0,
            xy: None,
            note,
            key: String::new(),
        }
    }

    fn at(mut self, xy: Option<Pt>) -> Self {
        self.xy = xy;
        self
    }

    fn of(mut self, fid: i64) -> Self {
        self.fid = Some(fid);
        self
    }

    fn pair(mut self, a: i64, b: i64) -> Self {
        self.fid = Some(a);
        self.fid_b = Some(b);
        self
    }

    fn value(mut self, v: f64) -> Self {
        self.value = v;
        self
    }
}

/// Объект входа: номер, геометрия и её кольца по частям.
pub struct Item {
    pub fid: i64,
    pub geom: Geom,
    /// Части: внешнее кольцо и дыры, вершины без повтора первой.
    pub parts: Vec<Vec<Vec<Pt>>>,
}

/// Настройки проверки.
pub struct Settings {
    pub tolerance: f64,
    pub area_threshold: f64,
    pub do_overlaps: bool,
    pub do_gaps: bool,
    pub do_unsnapped: bool,
    pub cavity_area: f64,
}

/// Мусорное ли перекрытие.
///
/// Площадь здесь не работает: полоса шириной с допуск и длиной в десятки
/// метров наберёт сотню квадратных единиц, оставаясь следствием смещения
/// вершин. Решает эффективная ширина, удвоенная площадь на периметр.
fn overlap_is_debris(inter: &Geom, area_threshold: f64, tolerance: f64) -> bool {
    let area = backend::area(inter);
    if area < area_threshold {
        return true;
    }
    let perimeter = backend::boundary_length(inter);
    if perimeter <= 0.0 {
        return false;
    }
    (2.0 * area / perimeter) < tolerance
}

/// Точка для показа находки. У вырожденной геометрии внутренней точки
/// нет, и берётся центр охвата.
fn safe_point(g: &Geom) -> Option<Pt> {
    if backend::is_empty(g) {
        return None;
    }
    if let Some(p) = backend::point_on_surface(g) {
        return Some(p);
    }
    let (x0, y0, x1, y1) = backend::bounds(g)?;
    Some([0.5 * (x0 + x1), 0.5 * (y0 + y1)])
}

/// Пары объектов, чьи охваты задевают друг друга. Порядок пар не зависит
/// от устройства индекса: сначала первый номер, потом второй.
fn close_pairs(boxes: &[Option<(f64, f64, f64, f64)>]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for i in 0..boxes.len() {
        let Some(a) = boxes[i] else { continue };
        for (j, b) in boxes.iter().enumerate().skip(i + 1) {
            let Some(b) = b else { continue };
            if a.0 <= b.2 && b.0 <= a.2 && a.1 <= b.3 && b.1 <= a.3 {
                out.push((i, j));
            }
        }
    }
    out
}

/// Проверка покрытия. Геометрия не изменяется.
pub fn check_items(items: &[Item], s: &Settings) -> Vec<Finding> {
    let mut findings: Vec<Finding> = Vec::new();

    // ── Пообъектные проверки ────────────────────────────────────────────
    for item in items {
        let fid = item.fid;
        if backend::is_empty(&item.geom) {
            findings.push(
                Finding::new(
                    Kind::Lost,
                    SEVERITY_REVIEW,
                    Text::of("пустая геометрия", "empty geometry"),
                )
                .of(fid),
            );
            continue;
        }
        if !backend::is_valid(&item.geom) {
            let why = backend::invalid_reason(&item.geom);
            findings.push(
                Finding::new(Kind::Invalid, SEVERITY_AUTO, Text::of(&why, &why))
                    .of(fid)
                    .value(backend::area(&item.geom))
                    .at(safe_point(&item.geom)),
            );
        }
        let area = backend::area(&item.geom);
        if area < s.area_threshold {
            findings.push(
                Finding::new(
                    Kind::TinyFeature,
                    SEVERITY_REVIEW,
                    Text::of(
                        &format!(
                            "площадь {} при пороге {}",
                            fmt(area),
                            fmt(s.area_threshold)
                        ),
                        &format!(
                            "area {} against the threshold {}",
                            fmt(area),
                            fmt(s.area_threshold)
                        ),
                    ),
                )
                .of(fid)
                .value(area)
                .at(safe_point(&item.geom)),
            );
        }
        // Вершинные проверки идут независимо от корректности геометрии:
        // именно у некорректных объектов артефактов больше всего.
        let n_parts = item.parts.len();
        for part in &item.parts {
            let Some(ext) = part.first() else { continue };
            let a = rings::ring_area(ext).abs();
            if n_parts > 1 && a < s.area_threshold {
                findings.push(
                    Finding::new(
                        Kind::TinyPart,
                        SEVERITY_AUTO,
                        Text::of(
                            &format!("часть площадью {} из {}", fmt(a), n_parts),
                            &format!("a part of area {} out of {}", fmt(a), n_parts),
                        ),
                    )
                    .of(fid)
                    .value(a)
                    .at(ext.first().copied()),
                );
            }
            let w = rings::ring_width(ext);
            if a >= s.area_threshold && w < s.tolerance {
                findings.push(
                    Finding::new(
                        Kind::Sliver,
                        SEVERITY_REVIEW,
                        Text::of(
                            "эффективная ширина меньше допуска",
                            "the effective width is below the tolerance",
                        ),
                    )
                    .of(fid)
                    .value(w)
                    .at(ext.first().copied()),
                );
            }
            for inner in &part[1..] {
                let ai = rings::ring_area(inner).abs();
                if ai < s.area_threshold {
                    findings.push(
                        Finding::new(
                            Kind::TinyHole,
                            SEVERITY_AUTO,
                            Text::of(
                                &format!(
                                    "дыра площадью {} при пороге {}",
                                    fmt(ai),
                                    fmt(s.area_threshold)
                                ),
                                &format!(
                                    "a hole of area {} against the threshold {}",
                                    fmt(ai),
                                    fmt(s.area_threshold)
                                ),
                            ),
                        )
                        .of(fid)
                        .value(ai)
                        .at(inner.first().copied()),
                    );
                }
            }
            for ring in part {
                let (_r, dups) = rings::drop_repeated_vertices(ring, true, 1e-9);
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
                        .at(ring.first().copied()),
                    );
                }
                let (_r, spikes) = rings::remove_spikes(ring, true, DEFAULT_SPIKE_ANGLE);
                if spikes > 0 {
                    findings.push(
                        Finding::new(
                            Kind::Spike,
                            SEVERITY_AUTO,
                            Text::of(
                                &format!("разворотов границы назад: {spikes}"),
                                &format!("border turns back on itself: {spikes}"),
                            ),
                        )
                        .of(fid)
                        .value(spikes as f64)
                        .at(ring.first().copied()),
                    );
                }
                for pt in rings::self_touch_points(ring) {
                    findings.push(
                        Finding::new(
                            Kind::SelfTouch,
                            SEVERITY_REVIEW,
                            Text::of(
                                "кольцо проходит через точку дважды",
                                "the ring passes through the point twice",
                            ),
                        )
                        .of(fid)
                        .value(1.0)
                        .at(Some(pt)),
                    );
                }
            }
        }
    }

    // ── Парные проверки ─────────────────────────────────────────────────
    if s.do_overlaps {
        let boxes: Vec<Option<(f64, f64, f64, f64)>> = items
            .iter()
            .map(|it| {
                if backend::is_empty(&it.geom) {
                    None
                } else {
                    backend::bounds(&it.geom)
                }
            })
            .collect();
        for (i, j) in close_pairs(&boxes) {
            let (a, b) = (&items[i].geom, &items[j].geom);
            if !backend::intersects(a, b) {
                continue;
            }
            let (fa, fb) = (items[i].fid, items[j].fid);
            if backend::equals(a, b) {
                findings.push(
                    Finding::new(
                        Kind::Duplicate,
                        SEVERITY_REVIEW,
                        Text::of(
                            "геометрии совпадают, атрибуты могут различаться",
                            "the geometries coincide, the attributes may differ",
                        ),
                    )
                    .pair(fa, fb)
                    .value(backend::area(a))
                    .at(safe_point(a)),
                );
                continue;
            }
            let inter = backend::intersection(a, b);
            let area = backend::area(&inter);
            if area <= AREA_EPS {
                continue;
            }
            if backend::contains(a, b) || backend::contains(b, a) {
                findings.push(
                    Finding::new(
                        Kind::Nested,
                        SEVERITY_REVIEW,
                        Text::of(
                            "один объект целиком внутри другого",
                            "one object entirely inside another",
                        ),
                    )
                    .pair(fa, fb)
                    .value(area)
                    .at(safe_point(&inter)),
                );
                continue;
            }
            let debris = overlap_is_debris(&inter, s.area_threshold, s.tolerance);
            findings.push(
                Finding::new(
                    Kind::Overlap,
                    if debris {
                        SEVERITY_AUTO
                    } else {
                        SEVERITY_REVIEW
                    },
                    if debris {
                        Text::of(
                            "полоса шириной меньше допуска",
                            "a strip narrower than the tolerance",
                        )
                    } else {
                        Text::of(
                            "перекрытие шире допуска, спор за площадь",
                            "the overlap is wider than the tolerance, a dispute over area",
                        )
                    },
                )
                .pair(fa, fb)
                .value(area)
                .at(safe_point(&inter)),
            );
        }
    }

    // ── Щели покрытия ───────────────────────────────────────────────────
    if s.do_gaps {
        let geoms: Vec<Geom> = items.iter().map(|it| it.geom.clone()).collect();
        let merged = backend::union_all(&geoms);
        for poly in backend::parts(&merged) {
            let rs = backend::rings_of(&poly);
            for inner in &rs[1..] {
                let hole = backend::geom_of(&[vec![inner.clone()]]);
                let a = backend::area(&hole);
                if s.cavity_area > 0.0 && a >= s.cavity_area {
                    continue;
                }
                let sev = if a < s.area_threshold {
                    SEVERITY_AUTO
                } else {
                    SEVERITY_REVIEW
                };
                findings.push(
                    Finding::new(
                        Kind::Gap,
                        sev,
                        Text::of(
                            &format!("дыра в объединении покрытия площадью {}", fmt(a)),
                            &format!("a hole in the union of the coverage, area {}", fmt(a)),
                        ),
                    )
                    .value(a)
                    .at(safe_point(&hole)),
                );
            }
        }
    }

    // ── Несогласованные узлы ────────────────────────────────────────────
    if s.do_unsnapped {
        let mut closed: Vec<Vec<Pt>> = Vec::new();
        let mut owner: Vec<i64> = Vec::new();
        for item in items {
            if backend::is_empty(&item.geom) {
                continue;
            }
            for part in &item.parts {
                for ring in part {
                    let mut r = ring.clone();
                    if let Some(&first) = r.first() {
                        r.push(first);
                    }
                    closed.push(r);
                    owner.push(item.fid);
                }
            }
        }
        if !closed.is_empty() {
            // Ноль означает, что вершина лежит точно на ребре соседа. Это
            // дефект вершинности при любом допуске. Ненулевое расстояние
            // означает лишь близость и целиком зависит от допуска.
            let on_edge_eps = (s.tolerance * 1e-6).max(1e-9);
            for e in snap::insert_events(&closed, s.tolerance) {
                let on_edge = e.dist <= on_edge_eps;
                let note = if on_edge {
                    Text::of(
                        "границы совпадают геометрически, узла нет",
                        "the borders coincide geometrically, there is no node",
                    )
                } else {
                    Text::of(
                        &format!("вершина в {} от ребра соседа", fmt(e.dist)),
                        &format!("vertex {} away from a neighbour edge", fmt(e.dist)),
                    )
                };
                let mut f = Finding::new(
                    if on_edge { Kind::OnEdge } else { Kind::Unsnapped },
                    SEVERITY_AUTO,
                    note,
                )
                .value(e.dist)
                .at(Some([e.x, e.y]));
                f.fid = owner.get(e.ring).copied();
                findings.push(f);
            }
        }
    }

    findings
}

/// Сводка по видам нарушений в порядке первого появления: сколько auto,
/// сколько review, максимум и медиана величины.
pub struct Slot {
    pub kind: Kind,
    pub auto: usize,
    pub review: usize,
    pub value_max: f64,
    pub value_med: f64,
}

pub fn summarize(findings: &[Finding]) -> Vec<Slot> {
    let mut out: Vec<Slot> = Vec::new();
    let mut values: Vec<Vec<f64>> = Vec::new();
    for f in findings {
        let at = match out.iter().position(|s| s.kind == f.kind) {
            Some(i) => i,
            None => {
                out.push(Slot {
                    kind: f.kind,
                    auto: 0,
                    review: 0,
                    value_max: 0.0,
                    value_med: 0.0,
                });
                values.push(Vec::new());
                out.len() - 1
            }
        };
        if f.severity == SEVERITY_AUTO {
            out[at].auto += 1;
        } else {
            out[at].review += 1;
        }
        if f.value > out[at].value_max {
            out[at].value_max = f.value;
        }
        values[at].push(f.value);
    }
    for (slot, vals) in out.iter_mut().zip(values.iter_mut()) {
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        slot.value_med = vals[vals.len() / 2];
    }
    out
}

/// Подсказка по допуску, вычисленная из самих данных.
pub struct Hint {
    pub count: usize,
    pub median: f64,
    pub p95: f64,
    pub max: f64,
    pub gap_at: Option<f64>,
    pub ceiling: Option<f64>,
    pub censored: bool,
    pub edge_p05: Option<f64>,
    pub min_width: Option<f64>,
}

pub fn tolerance_hint(
    findings: &[Finding],
    tolerance: f64,
    edge_p05: Option<f64>,
    min_width: Option<f64>,
) -> Option<Hint> {
    let mut d: Vec<f64> = findings
        .iter()
        .filter(|f| f.kind == Kind::Unsnapped && f.value > 0.0)
        .map(|f| f.value)
        .collect();
    if d.is_empty() {
        return None;
    }
    d.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let quantile = |q: f64| d[((q * d.len() as f64) as usize).min(d.len() - 1)];
    let median = quantile(0.5);
    // Медиана у самого допуска означает, что распределение обрезано.
    let censored = median > 0.5 * tolerance;
    let limits: Vec<f64> = [edge_p05, min_width]
        .into_iter()
        .flatten()
        .filter(|v| *v != 0.0)
        .collect();
    let ceiling = limits
        .iter()
        .copied()
        .fold(f64::INFINITY, f64::min)
        .is_finite()
        .then(|| 0.5 * limits.iter().copied().fold(f64::INFINITY, f64::min));
    // Естественный порог ищется как разрыв в распределении.
    let mut gap_at = None;
    if d.len() >= 8 {
        let mut best_ratio = 1.0;
        let low = (0.1 * d.len() as f64) as usize;
        let high = (0.9 * d.len() as f64) as usize;
        for i in low.max(1)..high.max(2) {
            let (before, after) = (d[i - 1], d[i]);
            if before <= 0.0 {
                continue;
            }
            let ratio = after / before;
            if ratio > best_ratio {
                best_ratio = ratio;
                gap_at = Some(before);
            }
        }
        if best_ratio < 3.0 {
            gap_at = None;
        }
    }
    Some(Hint {
        count: d.len(),
        median,
        p95: quantile(0.95),
        max: d[d.len() - 1],
        gap_at,
        ceiling,
        censored,
        edge_p05,
        min_width,
    })
}

/// Поля слоя находок, как их заводит диалог модуля.
pub fn finding_fields() -> J {
    json!([
        ["num", "int"],
        ["type", "string"],
        ["label", "string"],
        ["severity", "string"],
        ["fid_a", "long"],
        ["fid_b", "long"],
        ["value", "double"],
        ["note", "string"],
        ["grp", "string"]
    ])
}

/// Находки слоем точек: по объекту на находку, со сквозным номером.
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
    json!({
        "geometry": "point",
        "fields": finding_fields(),
        "features": features,
    })
}

fn tr(lang: Lang, ru: &str, en: &str) -> String {
    match lang {
        Lang::Ru => ru.to_string(),
        Lang::En => en.to_string(),
    }
}

/// Строки журнала, как их печатает диалог модуля. Первой строки с именем
/// и версией модуля здесь нет: у расширения своя версия.
pub fn report(
    items: usize,
    s: &Settings,
    findings: &[Finding],
    hint: Option<&Hint>,
    lang: Lang,
) -> Vec<String> {
    let mut out = Vec::new();
    out.push(
        tr(
            lang,
            "Объектов: %d, допуск %g, порог площади %g",
            "Objects: %d, tolerance %g, area threshold %g",
        )
        .replacen("%d", &items.to_string(), 1)
        .replacen("%g", &g_format(s.tolerance), 1)
        .replacen("%g", &g_format(s.area_threshold), 1),
    );
    out.push(String::new());
    out.push(tr(lang, "── Топология ──", "-- Topology --"));
    let summary = summarize(findings);
    if summary.is_empty() {
        out.push(tr(lang, "Нарушений не найдено.", "No violations found."));
    } else {
        out.push(format!(
            "{:<40} {:>8} {:>8} {:>12} {:>12}",
            "нарушение", "чинится", "решать", "медиана", "максимум"
        ));
        let mut order: Vec<&Slot> = summary.iter().collect();
        order.sort_by_key(|s| std::cmp::Reverse(s.auto + s.review));
        for slot in order {
            out.push(format!(
                "{:<40} {:>8} {:>8} {:>12.4} {:>12.4}",
                slot.kind.label(),
                slot.auto,
                slot.review,
                slot.value_med,
                slot.value_max
            ));
        }
    }
    if let Some(h) = hint {
        out.extend(hint_lines(h, lang));
    }
    out.push(String::new());
    let auto = findings
        .iter()
        .filter(|f| f.severity == SEVERITY_AUTO)
        .count();
    out.push(
        tr(
            lang,
            "Всего находок: %d, из них чинится автоматически: %d, решать человеку: %d",
            "Findings in total: %d, auto-fixed: %d, left to the operator: %d",
        )
        .replacen("%d", &findings.len().to_string(), 1)
        .replacen("%d", &auto.to_string(), 1)
        .replacen("%d", &(findings.len() - auto).to_string(), 1),
    );
    out
}

fn hint_lines(h: &Hint, lang: Lang) -> Vec<String> {
    let mut out = vec![
        String::new(),
        tr(
            lang,
            "── Расхождения вершин с рёбрами соседей ──",
            "-- Discrepancies between vertices and neighbour edges --",
        ),
        tr(
            lang,
            "Найдено: %d, медиана %s, 95 процентиль %s, максимум %s",
            "Found: %d, median %s, 95th percentile %s, maximum %s",
        )
        .replacen("%d", &h.count.to_string(), 1)
        .replacen("%s", &fmt(h.median), 1)
        .replacen("%s", &fmt(h.p95), 1)
        .replacen("%s", &fmt(h.max), 1),
    ];
    if let Some(v) = h.edge_p05.filter(|v| *v != 0.0) {
        out.push(
            tr(
                lang,
                "Пятый процентиль длины ребра: %s",
                "The fifth percentile of the edge length: %s",
            )
            .replacen("%s", &fmt(v), 1),
        );
    }
    if let Some(v) = h.min_width.filter(|v| *v != 0.0) {
        out.push(
            tr(
                lang,
                "Минимальная ширина объекта: %s",
                "The minimum width of a feature: %s",
            )
            .replacen("%s", &fmt(v), 1),
        );
    }
    match h.gap_at {
        Some(v) => out.push(
            tr(
                lang,
                "В распределении есть разрыв около %s. До него погрешность оцифровки, за ним разногласие между источниками. Допуск разумно взять чуть больше этой величины.",
                "The distribution has a break near %s. Below it lies digitising error, above it a disagreement between sources. It is reasonable to take a tolerance a little above this value.",
            )
            .replacen("%s", &fmt(v), 1),
        ),
        None => out.push(tr(
            lang,
            "Разрыва в распределении нет. Расхождения идут сплошь, и естественной границы между погрешностью и разногласием в этих данных не видно. Выбор допуска остаётся за вами.",
            "The distribution has no break. The discrepancies run continuously, and no natural boundary between error and disagreement is visible in this data. The choice of tolerance is left to you.",
        )),
    }
    if let Some(v) = h.ceiling.filter(|v| *v != 0.0) {
        out.push(
            tr(
                lang,
                "Выше %s допуск брать не следует. Он схлопнет короткие рёбра и узкие объекты.",
                "A tolerance above %s should not be taken. It would collapse short edges and narrow features.",
            )
            .replacen("%s", &fmt(v), 1),
        );
    }
    if h.censored {
        out.push(tr(
            lang,
            "Медиана расхождений близка к заданному допуску. Дальше него проверка не смотрит, поэтому распределение обрезано и настоящие расхождения крупнее. Повторите с допуском покрупнее, чтобы увидеть картину целиком.",
            "The median discrepancy is close to the given tolerance. The check does not look beyond it, so the distribution is truncated and the real discrepancies are larger. Repeat with a larger tolerance to see the whole picture.",
        ));
    }
    out
}

/// Число в записи `%g` языка Python: шесть значащих цифр без хвостовых нулей.
pub fn g_format(v: f64) -> String {
    if v == 0.0 {
        return "0".to_string();
    }
    let exp = v.abs().log10().floor() as i32;
    if !(-4..6).contains(&exp) {
        // Показатель пишется не меньше чем двумя цифрами, как в Python.
        let s = format!("{v:e}");
        return match s.split_once('e') {
            Some((m, e)) => {
                let (sign, digits) = e.split_at(usize::from(e.starts_with('-')));
                let sign = if sign.is_empty() { "+" } else { sign };
                format!("{m}e{sign}{digits:0>2}")
            }
            None => s,
        };
    }
    let places = (5 - exp).max(0) as usize;
    let text = format!("{v:.places$}");
    let text = if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.')
    } else {
        &text
    };
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_g_format() {
        assert_eq!(g_format(2.0), "2");
        assert_eq!(g_format(10.0), "10");
        assert_eq!(g_format(0.5), "0.5");
        assert_eq!(g_format(1.0), "1");
        assert_eq!(g_format(1e-6), "1e-06");
        assert_eq!(g_format(0.0001), "0.0001");
    }
}
