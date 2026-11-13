// Topoliner для RustGIS - расширение-процесс RustGIS.
// Copyright (C) 2026 ООО «Информ++». Лицензия GPL-3.0-or-later.
// SPDX-License-Identifier: GPL-3.0-or-later

//! 1.07 Контроль сборки по атрибуту. Перенос `topo_checks.check_assembly`
//! модуля QGIS вместе с диалогом.
//!
//! Объединение объектов одной группы должно давать ровно одну часть без
//! внутренних колец. Частей больше одной - внутри группы остался разрыв.
//! Появилось внутреннее кольцо - внутри группы осталась полость.
//!
//! Проверка ловит то, чего не находит поиск щелей по всему покрытию.
//! Зазор, выходящий на внешний край покрытия, дырой в объединении
//! не является, но при сборке группы он разрезает её на части.

use std::collections::BTreeMap;

use rustgis_kit::Lang;
use serde_json::Value as J;

use crate::arcs::Parts;
use crate::audit::{Finding, Kind, SEVERITY_AUTO, SEVERITY_REVIEW, Text};
use crate::backend::{self, Geom};
use crate::rings::{Pt, point_segment};
use crate::rounding::fmt;

/// Сводка по группе.
pub struct Group {
    pub key: String,
    pub features: usize,
    pub parts: usize,
    pub bodies: usize,
    pub splits: usize,
    pub separate: usize,
    pub holes: usize,
    /// Площадь для полигонов, длина для линий.
    pub measure: f64,
}

/// Настройки проверки сборки.
pub struct Settings {
    pub area_threshold: f64,
    pub max_gap: f64,
    pub ignore_holes: bool,
    pub is_line: bool,
}

/// Расстояние между двумя геометриями. Ноль, если они пересекаются.
fn distance(a: &Geom, b: &Geom) -> f64 {
    if backend::intersects(a, b) {
        return 0.0;
    }
    let edges = |g: &Geom| -> Vec<(Pt, Pt)> {
        let mut out = Vec::new();
        for p in backend::parts(g) {
            for ring in backend::rings_of(&p) {
                for w in ring.windows(2) {
                    out.push((w[0], w[1]));
                }
            }
        }
        out
    };
    let (ea, eb) = (edges(a), edges(b));
    let mut best = f64::INFINITY;
    for &(p, q) in &ea {
        for &(r, t) in &eb {
            for (point, seg) in [(p, (r, t)), (q, (r, t)), (r, (p, q)), (t, (p, q))] {
                best = best.min(point_segment(point, seg.0, seg.1).0);
            }
        }
    }
    best
}

/// Разбивает части на тела по связности: ребро есть, если расстояние
/// между частями не больше `max_gap`. При нулевом пороге все части
/// считаются одним телом, то есть группа обязана быть цельной.
fn group_parts(parts: &[Geom], max_gap: f64) -> Vec<Vec<usize>> {
    if parts.is_empty() {
        return Vec::new();
    }
    if max_gap <= 0.0 {
        return vec![(0..parts.len()).collect()];
    }
    let n = parts.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    let boxes: Vec<Option<(f64, f64, f64, f64)>> = parts.iter().map(backend::bounds).collect();
    for i in 0..n {
        for j in i + 1..n {
            if find(&mut parent, i) == find(&mut parent, j) {
                continue;
            }
            // Расстояние между охватами не больше настоящего, это дешёвый отсев.
            let (Some(a), Some(b)) = (boxes[i], boxes[j]) else {
                continue;
            };
            let dx = (b.0 - a.2).max(a.0 - b.2).max(0.0);
            let dy = (b.1 - a.3).max(a.1 - b.3).max(0.0);
            if dx.hypot(dy) > max_gap {
                continue;
            }
            if distance(&parts[i], &parts[j]) <= max_gap {
                let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                if ri != rj {
                    parent[rj] = ri;
                }
            }
        }
    }
    let mut bodies: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..n {
        let root = find(&mut parent, i);
        bodies.entry(root).or_default().push(i);
    }
    bodies.into_values().collect()
}

/// Точка для показа находки.
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

/// Проверяет, собирается ли каждая группа объектов в одно целое тело.
/// `items` - номер объекта, его части и значение группирующего поля.
pub fn check_assembly(items: &[(i64, Parts, String)], s: &Settings) -> (Vec<Finding>, Vec<Group>) {
    // Группы обходятся по возрастанию ключа, как в модуле.
    let mut groups: BTreeMap<String, Vec<Geom>> = BTreeMap::new();
    for (_fid, parts, key) in items {
        let g = backend::geom_of(parts);
        if backend::is_empty(&g) {
            continue;
        }
        groups.entry(key.clone()).or_default().push(g);
    }

    let mut findings: Vec<Finding> = Vec::new();
    let mut summary: Vec<Group> = Vec::new();
    for (key, members) in groups {
        let merged = backend::union_all(&members);
        let mut parts: Vec<Geom> = backend::parts(&merged)
            .into_iter()
            .map(|p| backend::geom_of(&[backend::rings_of(&p)]))
            .collect();
        let measure_of = |g: &Geom| -> f64 {
            if s.is_line {
                backend::boundary_length(g)
            } else {
                backend::area(g)
            }
        };
        parts.sort_by(|a, b| {
            measure_of(b)
                .partial_cmp(&measure_of(a))
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut holes = 0;
        let mut splits = 0;
        // Две части принадлежат одному телу, если расстояние между ними
        // не больше порога. Разные тела нарушением не являются.
        let bodies = group_parts(&parts, s.max_gap);
        let separate = bodies.len().saturating_sub(1);
        for body in &bodies {
            if body.len() < 2 {
                continue;
            }
            let mut ordered = body.clone();
            ordered.sort_by(|a, b| {
                measure_of(&parts[*b])
                    .partial_cmp(&measure_of(&parts[*a]))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            for &extra in &ordered[1..] {
                let gap = body
                    .iter()
                    .filter(|&&other| other != extra)
                    .map(|&other| distance(&parts[extra], &parts[other]))
                    .fold(f64::INFINITY, f64::min);
                let gap = if gap.is_finite() { gap } else { 0.0 };
                splits += 1;
                let mut f = Finding::new(
                    Kind::GroupSplit,
                    SEVERITY_REVIEW,
                    Text::of(
                        &format!("разрыв до ближайшей части {}", fmt(gap)),
                        &format!("gap to the nearest part {}", fmt(gap)),
                    ),
                )
                .value(measure_of(&parts[extra]))
                .at(safe_point(&parts[extra]));
                f.key = key.clone();
                findings.push(f);
            }
        }

        if !s.ignore_holes && !s.is_line {
            for part in &parts {
                for p in backend::parts(part) {
                    let rs = backend::rings_of(&p);
                    for inner in &rs[1..] {
                        let hole = backend::geom_of(&[vec![inner.clone()]]);
                        let a = backend::area(&hole);
                        let severity = if a < s.area_threshold {
                            SEVERITY_AUTO
                        } else {
                            SEVERITY_REVIEW
                        };
                        holes += 1;
                        let mut f = Finding::new(
                            Kind::GroupHole,
                            severity,
                            Text::of(
                                &format!("полость внутри группы площадью {}", fmt(a)),
                                &format!("a cavity inside the group, area {}", fmt(a)),
                            ),
                        )
                        .value(a)
                        .at(safe_point(&hole));
                        f.key = key.clone();
                        findings.push(f);
                    }
                }
            }
        }

        summary.push(Group {
            key,
            features: members.len(),
            parts: parts.len(),
            // При нулевом пороге разрыва телами показываются сами части,
            // иначе колонка всегда равна единице и ничего не говорит.
            bodies: if s.max_gap <= 0.0 {
                parts.len()
            } else {
                bodies.len()
            },
            splits,
            separate,
            holes,
            measure: measure_of(&merged),
        });
    }
    (findings, summary)
}

fn pick(lang: Lang, ru: &str, en: &str) -> String {
    match lang {
        Lang::Ru => ru.to_string(),
        Lang::En => en.to_string(),
    }
}

/// Находки слоем точек, поля те же, что у 1.01.
pub fn findings_layer(findings: &[Finding], lang: Lang) -> J {
    crate::audit::findings_layer(findings, lang)
}

/// Строки журнала контроля сборки.
pub fn report(fields: &[String], groups: &[Group], s: &Settings, lang: Lang) -> Vec<String> {
    let bad: Vec<&Group> = groups
        .iter()
        .filter(|g| g.splits > 0 || g.holes > 0)
        .collect();
    let separate = groups.iter().filter(|g| g.separate > 0).count();
    let mut out = vec![
        String::new(),
        pick(lang, "── Сборка по %s ──", "-- Assembly by %s --")
            .replacen("%s", &fields.join(", "), 1),
        pick(
            lang,
            "Групп: %d, без дефектов сборки: %d",
            "Groups: %d, without assembly defects: %d",
        )
        .replacen("%d", &groups.len().to_string(), 1)
        .replacen("%d", &(groups.len() - bad.len()).to_string(), 1),
    ];
    if separate > 0 {
        out.push(
            pick(
                lang,
                "Групп из нескольких отдельных тел: %d (разрыв больше заданного порога, нарушением не считается)",
                "Groups made of several separate bodies: %d (the gap exceeds the given threshold and is not a violation)",
            )
            .replacen("%d", &separate.to_string(), 1),
        );
    }
    if bad.is_empty() {
        out.push(pick(
            lang,
            "Дефектов сборки не найдено.",
            "No assembly defects found.",
        ));
        return out;
    }
    out.push(String::new());
    out.push(format!(
        "{:<24} {:>6} {:>7} {:>7} {:>14}",
        pick(lang, "группа", "group"),
        pick(lang, "тел", "bodies"),
        pick(lang, "разрыв", "split"),
        pick(lang, "колец", "rings"),
        if s.is_line {
            pick(lang, "длина", "length")
        } else {
            pick(lang, "площадь", "area")
        }
    ));
    let mut order: Vec<&&Group> = bad.iter().collect();
    order.sort_by_key(|g| std::cmp::Reverse(g.splits + g.holes));
    for g in order.iter().take(40) {
        let name: String = g.key.chars().take(24).collect();
        out.push(format!(
            "{:<24} {:>6} {:>7} {:>7} {:>14.2}",
            name, g.bodies, g.splits, g.holes, g.measure
        ));
    }
    if bad.len() > 40 {
        out.push(
            pick(
                lang,
                "... и ещё %d групп, см. слой находок",
                "... and %d more groups, see the findings layer",
            )
            .replacen("%d", &(bad.len() - 40).to_string(), 1),
        );
    }
    out.push(
        pick(
            lang,
            "Групп с дефектами сборки: %d. Смотрите поле note. Там стоит расстояние, которого не хватило допуску. Если разрывы измеряются сотнями метров, значит группы не обязаны быть цельными и нужно задать максимальный разрыв.",
            "Groups with assembly defects: %d. See the note field. It holds the distance the tolerance was missing. If the gaps are hundreds of metres, the groups need not be whole and a maximum gap should be set.",
        )
        .replacen("%d", &bad.len().to_string(), 1),
    );
    out
}

/// Значение группирующих полей объекта, как его собирает диалог модуля.
pub fn group_key(attrs: &J, fields: &[String]) -> String {
    fields
        .iter()
        .map(|n| match &attrs[n] {
            J::Null => String::new(),
            J::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect::<Vec<String>>()
        .join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Parts {
        vec![vec![vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1], [x0, y0]]]]
    }

    /// Зазор разрезает группу на две части.
    #[test]
    fn a_gap_splits_a_group() {
        let items = vec![
            (1, rect(0.0, 0.0, 20.0, 9.95), "B1".to_string()),
            (2, rect(0.0, 10.0, 20.0, 20.0), "B1".to_string()),
        ];
        let (findings, groups) = check_assembly(
            &items,
            &Settings {
                area_threshold: 1.0,
                max_gap: 0.0,
                ignore_holes: false,
                is_line: false,
            },
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].kind, Kind::GroupSplit);
        assert_eq!(findings[0].key, "B1");
        assert_eq!(groups[0].splits, 1);
        assert_eq!(groups[0].bodies, 2);
    }
}
