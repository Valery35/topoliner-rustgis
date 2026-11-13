// Topoliner для RustGIS - расширение-процесс RustGIS.
// Copyright (C) 2026 ООО «Информ++». Лицензия GPL-3.0-or-later.
// SPDX-License-Identifier: GPL-3.0-or-later

//! 1.08 Ввод контуров в покрытие. Перенос модуля `cut` плагина QGIS
//! вместе с диалогом `CutIntoCoverageAlgorithm`.
//!
//! Три режима наложения отвечают на один вопрос, кому достаётся площадь
//! под контуром. Наложение отдаёт её контуру, отсечение оставляет
//! соседям, врезка отдаёт контуру только то, что лежит внутри покрытия.
//!
//! Кромка разреза приходит из одного наложения. Контур собирается в одну
//! геометрию, и все вычитания и пересечения делаются против неё. Кромка
//! достаётся обеим сторонам одинаковой.

use std::collections::BTreeMap;

use rustgis_kit::Lang;
use serde_json::{Value as J, json};

use crate::arcs::Parts;
use crate::backend::{self, Geom};
use crate::rings::Pt;
use crate::rounding::fmt;
use crate::snap::{self, Nodes};

/// Кому достаётся спорная площадь.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Наложение: весь контур, соседи урезаются.
    Overlay,
    /// Отсечение: только свободное место, соседи как были.
    Clip,
    /// Врезка: только то, что внутри покрытия.
    Inset,
}

impl Mode {
    pub fn label(self, lang: Lang) -> &'static str {
        match (self, lang) {
            (Mode::Overlay, Lang::Ru) => "Наложение",
            (Mode::Overlay, Lang::En) => "Overlay",
            (Mode::Clip, Lang::Ru) => "Отсечение",
            (Mode::Clip, Lang::En) => "Clip",
            (Mode::Inset, Lang::Ru) => "Врезка",
            (Mode::Inset, Lang::En) => "Inset",
        }
    }
}

/// Объект врезки: ключ и его части. Ключ `(0, номер)` это сосед,
/// `(1, 0)` это новый объект.
type Piece = ((usize, i64), Parts);

/// Итог врезки одного контура.
#[derive(Default)]
pub struct Done {
    /// Новая геометрия объекта. Пустые части - объект поглощён целиком.
    pub changed: BTreeMap<i64, Parts>,
    /// Части нового объекта.
    pub created: Parts,
    /// Объекты, отдавшие площадь, в порядке обхода.
    pub donors: Vec<i64>,
    /// Площадь контура вне покрытия, доставшаяся новому объекту.
    pub outside: f64,
    /// Площадь контура вне покрытия, отброшенная.
    pub dropped: f64,
    /// Соседи, получившие недостающие узлы.
    pub noded: BTreeMap<i64, Parts>,
    pub nodes: usize,
}

/// Части геометрии: часть это внешнее кольцо и дыры, кольца замкнуты.
fn parts_of(g: &Geom) -> Parts {
    backend::parts(g)
        .iter()
        .map(backend::rings_of)
        .filter(|rings: &Vec<Vec<Pt>>| rings.first().is_some_and(|r| r.len() >= 4))
        .collect()
}

/// Геометрия, пригодная к записи, либо пустая.
fn valid(g: Geom) -> Option<Geom> {
    if backend::is_empty(&g) {
        return None;
    }
    let g = if backend::is_valid(&g) {
        g
    } else {
        backend::make_valid(&g)
    };
    (!backend::is_empty(&g)).then_some(g)
}

/// Вставляет недостающие узлы в набор объектов.
///
/// Разрез кладёт вершину на границу донора с третьим объектом, и у этого
/// третьего узла в том месте нет. Ответ - только те, у кого количество
/// вершин изменилось, и количество вставленных узлов.
fn insert_nodes(pieces: &[Piece], tolerance: f64) -> (Vec<Piece>, usize) {
    let mut rings: Vec<Vec<Pt>> = Vec::new();
    // Адрес объекта: ключ и номера его колец в плоском списке.
    type Address = ((usize, i64), Vec<Vec<usize>>);
    let mut index: Vec<Address> = Vec::new();
    for (key, parts) in pieces {
        let mut idx_parts = Vec::new();
        for part in parts {
            let mut idx_ring = Vec::new();
            for ring in part {
                idx_ring.push(rings.len());
                rings.push(ring.clone());
            }
            idx_parts.push(idx_ring);
        }
        index.push((*key, idx_parts));
    }
    if rings.is_empty() {
        return (Vec::new(), 0);
    }
    let done = snap::insert_nodes(
        &rings,
        Nodes {
            tolerance,
            crossings: true,
            project_onto_edge: true,
        },
    );
    let inserted = done.stats.nodes_inserted;
    if inserted == 0 {
        return (Vec::new(), 0);
    }
    let mut out = Vec::new();
    for (key, idx_parts) in index {
        let before: usize = idx_parts.iter().flatten().map(|&i| rings[i].len()).sum();
        let mut after = 0;
        let mut parts: Parts = Vec::new();
        for part in &idx_parts {
            let mut new_part: Vec<Vec<Pt>> = Vec::new();
            for &i in part {
                let Some(Some(ring)) = done.rings.get(i) else {
                    continue;
                };
                if ring.is_empty() {
                    continue;
                }
                after += ring.len();
                new_part.push(ring.clone());
            }
            if !new_part.is_empty() {
                parts.push(new_part);
            }
        }
        if after != before && !parts.is_empty() {
            out.push((key, parts));
        }
    }
    (out, inserted)
}

/// Достраивает недостающие узлы в двух операндах наложения.
///
/// GEOS нодирует оба аргумента перед пересечением и вычитанием, поэтому
/// кромка у обеих сторон выходит из одной нодированной дуги. Крейт `geo`
/// этого не делает, и вершина одной стороны не попадает в границу другой.
/// Здесь узлы достраиваются заранее, тем же приёмом, что у 1.06.
fn noded_pair(a: &Geom, b: &Geom, eps: f64) -> (Geom, Geom) {
    if eps <= 0.0 {
        return (a.clone(), b.clone());
    }
    let (pa, pb) = (parts_of(a), parts_of(b));
    let mut rings: Vec<Vec<Pt>> = Vec::new();
    let mut counts: Vec<usize> = Vec::new();
    for parts in [&pa, &pb] {
        let before = rings.len();
        for part in parts.iter() {
            rings.extend(part.iter().cloned());
        }
        counts.push(rings.len() - before);
    }
    let done = snap::insert_nodes(
        &rings,
        Nodes {
            tolerance: eps,
            crossings: true,
            project_onto_edge: true,
        },
    );
    if done.stats.nodes_inserted == 0 {
        return (a.clone(), b.clone());
    }
    // Кольца раскладываются обратно по частям в том же порядке.
    let mut at = 0usize;
    let mut built: Vec<Geom> = Vec::new();
    for parts in [&pa, &pb] {
        let mut out: Parts = Vec::new();
        for part in parts.iter() {
            let mut new_part: Vec<Vec<Pt>> = Vec::new();
            for _ in part {
                if let Some(Some(ring)) = done.rings.get(at) {
                    new_part.push(ring.clone());
                }
                at += 1;
            }
            if !new_part.is_empty() {
                out.push(new_part);
            }
        }
        built.push(backend::geom_of(&out));
    }
    match (built.first(), built.get(1)) {
        (Some(x), Some(y)) => (x.clone(), y.clone()),
        _ => (a.clone(), b.clone()),
    }
}

/// Возвращает в границу итога вершины его слагаемых.
///
/// GEOS при объединении сохраняет вершины операндов, даже если они лежат
/// на прямой. Крейт `geo` такие вершины убирает, и граница нового объекта
/// теряет точки стыка с соседями. Здесь они достраиваются обратно.
fn keep_vertices(g: &Geom, sources: &[Geom], eps: f64) -> Geom {
    if eps <= 0.0 || backend::is_empty(g) {
        return g.clone();
    }
    let parts = parts_of(g);
    let mut rings: Vec<Vec<Pt>> = Vec::new();
    for part in &parts {
        rings.extend(part.iter().cloned());
    }
    let own = rings.len();
    for s in sources {
        for part in parts_of(s) {
            rings.extend(part);
        }
    }
    let done = snap::insert_nodes(
        &rings,
        Nodes {
            tolerance: eps,
            crossings: false,
            project_onto_edge: true,
        },
    );
    let mut at = 0usize;
    let mut out: Parts = Vec::new();
    for part in &parts {
        let mut new_part: Vec<Vec<Pt>> = Vec::new();
        for _ in part {
            if at < own
                && let Some(Some(ring)) = done.rings.get(at)
            {
                new_part.push(ring.clone());
            }
            at += 1;
        }
        if !new_part.is_empty() {
            out.push(new_part);
        }
    }
    backend::geom_of(&out)
}

/// Вводит нарисованный контур в участок покрытия.
pub fn cut_into_coverage(
    items: &[(i64, Parts)],
    cut_parts: &Parts,
    area_threshold: f64,
    node_eps: f64,
    mode: Mode,
) -> Done {
    let mut done = Done::default();
    let Some(cut) = valid(backend::geom_of(cut_parts)) else {
        return done;
    };
    let covers: Vec<(i64, Geom)> = items
        .iter()
        .filter_map(|(fid, parts)| valid(backend::geom_of(parts)).map(|g| (*fid, g)))
        .collect();
    if covers.is_empty() {
        // Покрытия рядом нет. Врезке нечего резать, остальным режимам
        // достаётся весь контур.
        if mode == Mode::Inset {
            done.dropped = backend::area(&cut);
            return done;
        }
        done.outside = backend::area(&cut);
        done.created = parts_of(&cut);
        return done;
    }

    let mut untouched: Vec<i64> = Vec::new();
    let mut taken: Vec<Geom> = Vec::new();
    for (fid, geometry) in &covers {
        // Отсечение соседей не трогает, у них контур площади не берёт.
        if mode == Mode::Clip || !backend::intersects(geometry, &cut) {
            untouched.push(*fid);
            continue;
        }
        let (geometry, edge) = noded_pair(geometry, &cut, node_eps);
        let geometry = &geometry;
        let both = [geometry.clone(), edge.clone()];
        let inside = valid(keep_vertices(
            &backend::intersection(geometry, &edge),
            &both,
            node_eps,
        ));
        let rest = valid(keep_vertices(
            &backend::difference(geometry, &edge),
            &both,
            node_eps,
        ));
        let inside_area = inside.as_ref().map_or(0.0, backend::area);
        let rest_area = rest.as_ref().map_or(0.0, backend::area);

        // Отрезанный кусок мельче порога. Объект остаётся как был.
        let Some(inside) = inside.filter(|_| inside_area >= area_threshold) else {
            untouched.push(*fid);
            continue;
        };
        // Остаток мельче порога. Объект уходит под контур целиком.
        match rest.filter(|_| rest_area >= area_threshold) {
            None => {
                done.changed.insert(*fid, Vec::new());
                done.donors.push(*fid);
                taken.push(geometry.clone());
            }
            Some(rest) => {
                done.changed.insert(*fid, parts_of(&rest));
                done.donors.push(*fid);
                taken.push(inside);
            }
        }
    }

    // Часть контура за пределами покрытия. Врезка её отбрасывает,
    // остальные режимы отдают новому объекту.
    let pieces: Vec<Geom> = covers.iter().map(|(_f, g)| g.clone()).collect();
    let covered = keep_vertices(&backend::union_all(&pieces), &pieces, node_eps);
    let (edge, covered) = noded_pair(&cut, &covered, node_eps);
    let outside = valid(keep_vertices(
        &backend::difference(&edge, &covered),
        &[edge.clone(), covered.clone()],
        node_eps,
    ));
    let mut outside_area = outside.as_ref().map_or(0.0, backend::area);
    match outside {
        Some(outside) if outside_area >= area_threshold && mode != Mode::Inset => {
            taken.push(outside);
        }
        _ => {
            done.dropped = outside_area;
            outside_area = 0.0;
        }
    }
    done.outside = outside_area;

    let created = (!taken.is_empty())
        .then(|| valid(keep_vertices(&backend::union_all(&taken), &taken, node_eps)))
        .flatten();
    done.created = created.as_ref().map(parts_of).unwrap_or_default();

    // ── Недостающие узлы ────────────────────────────────────────────────
    // Кромка разреза заканчивается на границе донора с третьим объектом,
    // и у этого третьего узла в той точке нет.
    if node_eps > 0.0 {
        let mut pieces: Vec<Piece> = Vec::new();
        for (fid, geometry) in &covers {
            let parts = match done.changed.get(fid) {
                Some(parts) => parts.clone(),
                None => parts_of(geometry),
            };
            if !parts.is_empty() {
                pieces.push(((0, *fid), parts));
            }
        }
        if !done.created.is_empty() {
            pieces.push(((1, 0), done.created.clone()));
        }
        let (fixed, nodes) = insert_nodes(&pieces, node_eps);
        done.nodes = nodes;
        for (key, parts) in fixed {
            if key.0 == 1 {
                done.created = parts;
            } else if let std::collections::btree_map::Entry::Occupied(mut slot) =
                done.changed.entry(key.1)
            {
                slot.insert(parts);
            } else {
                done.noded.insert(key.1, parts);
            }
        }
    }
    done
}

/// Охват набора частей.
pub fn bbox(parts: &Parts) -> Option<(f64, f64, f64, f64)> {
    backend::bounds(&backend::geom_of(parts))
}

/// Задевают ли друг друга два охвата.
pub fn overlap(a: (f64, f64, f64, f64), b: (f64, f64, f64, f64)) -> bool {
    a.0 <= b.2 && b.0 <= a.2 && a.1 <= b.3 && b.1 <= a.3
}

/// Атрибуты, унаследованные от соседей.
///
/// Поле, у которого все отдавшие площадь соседи имеют одно значение,
/// получает это значение. Поле с разными значениями остаётся пустым.
/// Ключ слоя не переносится никогда.
pub fn inherited_values(rows: &[&J], fields: &[(String, String)]) -> Vec<J> {
    fields
        .iter()
        .map(|(name, _kind)| {
            if name.eq_ignore_ascii_case("fid") || rows.is_empty() {
                return J::Null;
            }
            let first = &rows[0][name];
            if rows[1..].iter().any(|row| &row[name] != first) {
                return J::Null;
            }
            first.clone()
        })
        .collect()
}

fn pick(lang: Lang, ru: &str, en: &str) -> String {
    match lang {
        Lang::Ru => ru.to_string(),
        Lang::En => en.to_string(),
    }
}

/// Счётчики врезки по всем контурам.
#[derive(Default)]
pub struct Totals {
    pub cuts: usize,
    pub changed: usize,
    pub swallowed: usize,
    pub added: usize,
    pub nodes: usize,
    pub outside: f64,
    pub dropped: f64,
    pub area_before: f64,
    pub area_after: f64,
    pub items: usize,
    pub written: usize,
}

/// Строки журнала врезки.
pub fn report(t: &Totals, mode: Mode, area_threshold: f64, lang: Lang) -> Vec<String> {
    let one = |ru: &str, en: &str, n: usize| pick(lang, ru, en).replacen("%d", &n.to_string(), 1);
    let mut out = vec![
        pick(
            lang,
            "Объектов %d, контуров %d, порог площади %g",
            "Objects %d, outlines %d, area threshold %g",
        )
        .replacen("%d", &t.items.to_string(), 1)
        .replacen("%d", &t.cuts.to_string(), 1)
        .replacen("%g", &crate::audit::g_format(area_threshold), 1),
        pick(lang, "Режим наложения: %s", "Overlay mode: %s")
            .replacen("%s", mode.label(lang), 1),
    ];
    out.push(String::new());
    out.push(pick(lang, "── Результат ──", "-- Result --"));
    out.push(one(
        "Контуров врезано:            %d",
        "Outlines cut in:             %d",
        t.cuts,
    ));
    out.push(one(
        "Объектов изменено:           %d",
        "Objects changed:             %d",
        t.changed,
    ));
    out.push(one(
        "Объектов поглощено целиком:  %d",
        "Objects swallowed whole:     %d",
        t.swallowed,
    ));
    out.push(one(
        "Объектов добавлено:          %d",
        "Objects added:               %d",
        t.added,
    ));
    out.push(one(
        "Узлов вставлено соседям:     %d",
        "Nodes added to neighbours:   %d",
        t.nodes,
    ));
    out.push(String::new());
    out.push(
        pick(lang, "Площадь до/после: %s / %s", "Area before/after: %s / %s")
            .replacen("%s", &fmt(t.area_before), 1)
            .replacen("%s", &fmt(t.area_after), 1),
    );
    out.push(
        pick(lang, "Легло вне покрытия: %s", "Fell outside the coverage: %s")
            .replacen("%s", &fmt(t.outside), 1),
    );
    if t.dropped != 0.0 {
        out.push(
            pick(
                lang,
                "Отброшено за краем покрытия: %s",
                "Discarded beyond the coverage edge: %s",
            )
            .replacen("%s", &fmt(t.dropped), 1),
        );
    }
    out.push(
        pick(lang, "Объектов на входе/выходе: %d / %d", "Objects in/out: %d / %d")
            .replacen("%d", &t.items.to_string(), 1)
            .replacen("%d", &t.written.to_string(), 1),
    );
    let gap = t.area_after - t.area_before - t.outside;
    if gap.abs() > 1e-6_f64.max(1e-9 * t.area_before.abs().max(1.0)) {
        out.push(
            pick(
                lang,
                "Расхождение площади: %s. Врезка обязана площадь только перераспределять.",
                "Area discrepancy: %s. Cutting in must only redistribute the area.",
            )
            .replacen("%s", &fmt(gap), 1),
        );
    }
    out
}

/// Строка журнала про один контур.
pub fn cut_line(number: usize, donors: usize, outside: f64, lang: Lang) -> String {
    pick(
        lang,
        "Контур %d: соседей затронуто %d, вне покрытия %s",
        "Outline %d: neighbours affected %d, outside the coverage %s",
    )
    .replacen("%d", &number.to_string(), 1)
    .replacen("%d", &donors.to_string(), 1)
    .replacen("%s", &fmt(outside), 1)
}

/// Слой объектов покрытия после врезки.
pub fn coverage_layer(records: &[(Vec<J>, Parts)], fields: &[(String, String)]) -> J {
    let features: Vec<J> = records
        .iter()
        .filter(|(_v, parts)| !parts.is_empty())
        .map(|(values, parts)| {
            json!({
                "values": values,
                "geometry": crate::clean::shape_geometry(parts, true),
            })
        })
        .collect();
    let field_list: Vec<J> = fields.iter().map(|(n, t)| json!([n, t])).collect();
    json!({"geometry": "polygon", "fields": field_list, "features": features})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Parts {
        vec![vec![vec![
            [x0, y0],
            [x1, y0],
            [x1, y1],
            [x0, y1],
            [x0, y0],
        ]]]
    }

    /// Наложение: контур поперёк общей границы режет обоих соседей.
    #[test]
    fn an_overlay_cuts_both_neighbours() {
        let items = vec![
            (1, rect(0.0, 0.0, 10.0, 10.0)),
            (2, rect(10.0, 0.0, 20.0, 10.0)),
        ];
        let cut = rect(8.0, -2.0, 12.0, 4.0);
        let done = cut_into_coverage(&items, &cut, 1.0, 1e-6, Mode::Overlay);
        assert_eq!(done.donors, vec![1, 2]);
        assert_eq!(done.changed.len(), 2);
        assert!((done.outside - 8.0).abs() < 1e-9, "{}", done.outside);
        let created = backend::area(&backend::geom_of(&done.created));
        assert!((created - 24.0).abs() < 1e-9, "{created}");
        // Кромка разреза общая: узлы у соседей и у нового объекта уже есть.
        assert_eq!(done.nodes, 0);
    }

    /// Отсечение соседей не трогает, новый объект ложится в свободное место.
    #[test]
    fn a_clip_leaves_the_neighbours_alone() {
        let items = vec![
            (1, rect(0.0, 0.0, 10.0, 10.0)),
            (2, rect(10.0, 0.0, 20.0, 10.0)),
        ];
        let cut = rect(8.0, -2.0, 12.0, 4.0);
        let done = cut_into_coverage(&items, &cut, 1.0, 1e-6, Mode::Clip);
        assert!(done.donors.is_empty());
        assert!(done.changed.is_empty());
        let created = backend::area(&backend::geom_of(&done.created));
        assert!((created - 8.0).abs() < 1e-9, "{created}");
        // Кромка легла на границы соседей, и у них появились узлы.
        assert_eq!(done.nodes, 2);
        assert_eq!(done.noded.len(), 2);
    }
}
