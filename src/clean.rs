// Topoliner для RustGIS - расширение-процесс RustGIS.
// Copyright (C) 2026 ООО «Информ++». Лицензия GPL-3.0-or-later.
// SPDX-License-Identifier: GPL-3.0-or-later

//! 1.05 Сшивка узлов и вершин и 1.06 Вставка недостающих узлов. Перенос
//! того, что в модуле QGIS делают диалоги `TopologyCleanAlgorithm`
//! и `InsertNodesAlgorithm` поверх ядра `topo_core`.
//!
//! Атрибуты объектов сохраняются полностью, меняется только геометрия.
//! Вставка узлов не меняет ни форму, ни площадь: узел ставится
//! в проекцию точки на ребро, то есть строго на прямую этого ребра.

use rustgis_kit::Lang;
use serde_json::{Value as J, json};

use crate::arcs::{Parts, Shape};
use crate::backend;
use crate::rings::{Pt, ring_width, segment_length_stats};
use crate::rounding::{fmt, nice};
use crate::snap::{self, Mode, Nodes, Settings};

/// Плоский список колец всех объектов и адрес каждого кольца.
/// Адрес это номер объекта, номер части и номер кольца в части.
pub struct Flat {
    pub rings: Vec<Vec<Pt>>,
    pub owner: Vec<(usize, usize, usize)>,
}

/// Разбирает объекты в плоский список колец.
pub fn flatten(items: &[Shape]) -> Flat {
    let mut flat = Flat {
        rings: Vec::new(),
        owner: Vec::new(),
    };
    for (k, (_fid, parts)) in items.iter().enumerate() {
        for (p, part) in parts.iter().enumerate() {
            for (r, ring) in part.iter().enumerate() {
                flat.rings.push(ring.clone());
                flat.owner.push((k, p, r));
            }
        }
    }
    flat
}

/// Собирает кольца обратно по объектам. `new_rings` идёт в порядке
/// плоского списка, выпавшее кольцо - `None`.
pub fn rebuild(items: &[Shape], flat: &Flat, new_rings: &[Option<Vec<Pt>>]) -> Vec<Parts> {
    let mut out: Vec<Parts> = items
        .iter()
        .map(|(_fid, parts)| vec![Vec::new(); parts.len()])
        .collect();
    for (i, &(k, p, _r)) in flat.owner.iter().enumerate() {
        if let Some(ring) = new_rings.get(i).and_then(Option::as_ref)
            && let Some(part) = out.get_mut(k).and_then(|o| o.get_mut(p))
        {
            part.push(ring.clone());
        }
    }
    for shape in out.iter_mut() {
        // Часть без внешнего кольца собрать нельзя.
        shape.retain(|part| !part.is_empty());
    }
    out
}

/// Геометрия объекта для выхода прогона.
pub fn geometry_of(parts: &[Vec<Pt>], is_polygon: bool) -> J {
    let points = |ring: &Vec<Pt>| -> J {
        J::Array(ring.iter().map(|p| json!([p[0], p[1], J::Null])).collect())
    };
    if !is_polygon {
        return match parts.len() {
            1 => json!({"points": points(&parts[0])}),
            _ => json!({"parts": parts.iter().map(points).collect::<Vec<J>>()}),
        };
    }
    // Кольцо замыкается повтором первой вершины, как его отдаёт модуль.
    let closed: Vec<Vec<Pt>> = parts
        .iter()
        .map(|ring| {
            let mut r = ring.clone();
            if let Some(&first) = r.first()
                && r[r.len() - 1] != first
            {
                r.push(first);
            }
            r
        })
        .collect();
    json!({"rings": closed.iter().map(points).collect::<Vec<J>>()})
}

/// Геометрия объекта из его частей: один полигон кольцами, несколько -
/// списком полигонов.
pub fn shape_geometry(shape: &Parts, is_polygon: bool) -> J {
    if is_polygon && shape.len() > 1 {
        let polys: Vec<J> = shape
            .iter()
            .map(|part| geometry_of(part, true)["rings"].clone())
            .collect();
        return json!({"polygons": polys});
    }
    if !is_polygon && shape.len() > 1 {
        let lines: Vec<Vec<Pt>> = shape.iter().filter_map(|p| p.first().cloned()).collect();
        return geometry_of(&lines, false);
    }
    match shape.first() {
        Some(part) => geometry_of(part, is_polygon),
        None => J::Null,
    }
}

/// Площадь набора объектов.
pub fn area_of(shapes: &[Parts]) -> f64 {
    shapes
        .iter()
        .map(|parts| backend::area(&backend::geom_of(parts)))
        .sum()
}

/// Количество некорректных геометрий.
pub fn invalid_count(shapes: &[Parts]) -> usize {
    shapes
        .iter()
        .filter(|parts| {
            let g = backend::geom_of(parts);
            !backend::is_empty(&g) && !backend::is_valid(&g)
        })
        .count()
}

/// Слой объектов с полями входа и новой геометрией.
pub fn shapes_layer(
    items: &[Shape],
    attrs: &[J],
    fields: &[(String, String)],
    shapes: &[Parts],
    is_polygon: bool,
) -> J {
    let features: Vec<J> = items
        .iter()
        .enumerate()
        .filter_map(|(k, _)| {
            let shape = shapes.get(k)?;
            if shape.is_empty() {
                return None;
            }
            let vals: Vec<J> = fields
                .iter()
                .map(|(name, _)| attrs.get(k).map_or(J::Null, |a| a[name].clone()))
                .collect();
            Some(json!({"values": vals, "geometry": shape_geometry(shape, is_polygon)}))
        })
        .collect();
    let field_list: Vec<J> = fields.iter().map(|(n, t)| json!([n, t])).collect();
    json!({
        "geometry": if is_polygon { "polygon" } else { "line" },
        "fields": field_list,
        "features": features,
    })
}

/// Точки правок слоем. `ring` - номер исходного кольца, у 1.06 его нет.
pub fn edits_layer(events: &[snap::Insert], order: Option<&[usize]>) -> J {
    let features: Vec<J> = events
        .iter()
        .map(|e| {
            let mut vals = vec![json!(e.kind.code()), json!(nice(e.dist))];
            if let Some(order) = order {
                let src = order.get(e.ring).map_or(-1, |&i| i as i64);
                vals.push(json!(src));
            }
            json!({"values": vals, "geometry": {"xyz": [e.x, e.y, J::Null]}})
        })
        .collect();
    let mut fields = vec![json!(["kind", "string"]), json!(["dist", "double"])];
    if order.is_some() {
        fields.push(json!(["ring", "int"]));
    }
    json!({"geometry": "point", "fields": fields, "features": features})
}

fn pick(lang: Lang, ru: &str, en: &str) -> String {
    match lang {
        Lang::Ru => ru.to_string(),
        Lang::En => en.to_string(),
    }
}

/// Подстановка `%d` и `%s` по порядку.
fn say(text: String, args: &[String]) -> String {
    let mut out = text;
    for a in args {
        let at = out
            .find("%d")
            .into_iter()
            .chain(out.find("%s"))
            .chain(out.find("%g"))
            .min();
        if let Some(i) = at {
            out.replace_range(i..i + 2, a);
        }
    }
    out
}

/// Число в записи `%g` языка Python.
pub fn g_format(v: f64) -> String {
    crate::audit::g_format(v)
}

/// Настройки сшивки 1.05 из значений прогона.
pub struct CleanRun {
    pub tolerance: f64,
    pub mode: Mode,
    pub priority: usize,
    pub crossings: bool,
    pub protect: bool,
    pub validate: bool,
    pub report: bool,
    /// Кольца слоя-эталона. Они не изменяются и дают вершины-лидеры.
    pub fixed: Vec<Vec<Pt>>,
}

/// 1.05 Сшивка узлов и вершин: выходы прогона и строки журнала.
pub fn topology_clean(
    items: &[Shape],
    attrs: &[J],
    fields: &[(String, String)],
    is_polygon: bool,
    run: &CleanRun,
    lang: Lang,
) -> (J, Vec<String>) {
    let flat = flatten(items);
    let before: Vec<Parts> = items.iter().map(|(_f, p)| p.clone()).collect();
    let area_before = if is_polygon { area_of(&before) } else { 0.0 };
    let invalid_before = if run.validate && is_polygon {
        invalid_count(&before)
    } else {
        0
    };

    // ── Приоритет: чьи вершины становятся лидерами ──────────────────────
    let mut order: Vec<usize> = (0..flat.rings.len()).collect();
    if run.priority == 1 {
        let size = |i: usize| -> f64 {
            let ring = &flat.rings[i];
            let (mut x0, mut y0) = (f64::INFINITY, f64::INFINITY);
            let (mut x1, mut y1) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
            for p in ring {
                x0 = x0.min(p[0]);
                y0 = y0.min(p[1]);
                x1 = x1.max(p[0]);
                y1 = y1.max(p[1]);
            }
            -((x1 - x0) * (y1 - y0))
        };
        order.sort_by(|a, b| {
            size(*a)
                .partial_cmp(&size(*b))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    let ordered: Vec<Vec<Pt>> = order.iter().map(|&i| flat.rings[i].clone()).collect();
    let frozen: Vec<usize> = if run.protect && is_polygon {
        order
            .iter()
            .enumerate()
            .filter(|(_pos, src)| ring_width(&flat.rings[**src]) < run.tolerance)
            .map(|(pos, _)| pos)
            .collect()
    } else {
        Vec::new()
    };

    let done = snap::clean(
        &ordered,
        &Settings {
            nodes: Nodes {
                tolerance: run.tolerance,
                crossings: run.crossings,
                project_onto_edge: false,
            },
            mode: run.mode,
            frozen,
            fixed: run.fixed.clone(),
        },
    );

    let mut new_rings: Vec<Option<Vec<Pt>>> = vec![None; flat.rings.len()];
    for (pos, &src) in order.iter().enumerate() {
        new_rings[src] = done.rings.get(pos).cloned().flatten();
    }
    let after = rebuild(items, &flat, &new_rings);
    let area_after = if is_polygon { area_of(&after) } else { 0.0 };
    let invalid_after = if run.validate && is_polygon {
        invalid_count(&after)
    } else {
        0
    };
    let written = after.iter().filter(|s| !s.is_empty()).count();

    let mut out = serde_json::Map::new();
    out.insert(
        "OUTPUT".into(),
        shapes_layer(items, attrs, fields, &after, is_polygon),
    );
    if run.report {
        out.insert("REPORT".into(), edits_layer(&done.events, Some(&order)));
    }

    let s = &done.stats;
    let mut lines = vec![pick(lang, "Чтение слоя...", "Reading the layer...")];
    if !run.fixed.is_empty() {
        lines.push(say(
            pick(lang, "Эталон: колец %d", "Reference layer: %d rings"),
            &[run.fixed.len().to_string()],
        ));
    }
    lines.push(say(
        pick(
            lang,
            "Сшивка: колец %d, вершин %d, допуск %g",
            "Snapping: %d rings, %d vertices, tolerance %g",
        ),
        &[
            flat.rings.len().to_string(),
            flat.rings.iter().map(Vec::len).sum::<usize>().to_string(),
            g_format(run.tolerance),
        ],
    ));
    let (median, p05, n_seg) = segment_length_stats(&flat.rings);
    lines.push(say(
        pick(
            lang,
            "Длина ребра: медиана %s, пятый процентиль %s (рёбер %d)",
            "Edge length: median %s, fifth percentile %s (%d edges)",
        ),
        &[fmt(median), fmt(p05), n_seg.to_string()],
    ));
    if is_polygon {
        let mut widths: Vec<f64> = flat
            .rings
            .iter()
            .map(|r| ring_width(r))
            .filter(|w| *w > 0.0)
            .collect();
        let narrow = widths.iter().filter(|w| **w < run.tolerance).count();
        widths.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        if !widths.is_empty() {
            lines.push(say(
                pick(
                    lang,
                    "Ширина колец: минимум %s, медиана %s",
                    "Ring width: minimum %s, median %s",
                ),
                &[fmt(widths[0]), fmt(widths[widths.len() / 2])],
            ));
        }
        if narrow > 0 && run.protect {
            lines.push(say(
                pick(
                    lang,
                    "Колец уже допуска: %d. Они оставлены без изменений и служат опорой для соседей.",
                    "Rings narrower than the tolerance: %d. They are left unchanged and serve as support for the neighbours.",
                ),
                &[narrow.to_string()],
            ));
        }
    }
    if p05 > 0.0 && run.tolerance > p05 {
        lines.push(say(
            pick(
                lang,
                "Допуск больше пяти процентов самых коротких рёбер (%s), мелкие изгибы будут сглажены.",
                "The tolerance exceeds the fifth percentile of edge length (%s), small bends will be smoothed out.",
            ),
            &[fmt(p05)],
        ));
    }
    lines.push(String::new());
    lines.push(pick(lang, "── Результат ──", "-- Result --"));
    lines.push(say(
        pick(lang, "Вершин сдвинуто:      %d", "Vertices moved:       %d"),
        &[s.vertices_moved.to_string()],
    ));
    lines.push(say(
        pick(
            lang,
            "Смещение макс/сред:   %s / %s",
            "Shift max/mean:       %s / %s",
        ),
        &[fmt(s.max_move), fmt(s.mean_move)],
    ));
    if s.rings_frozen > 0 {
        lines.push(say(
            pick(
                lang,
                "Колец не изменялось:  %d (уже допуска)",
                "Rings left unchanged: %d (narrower than the tolerance)",
            ),
            &[s.rings_frozen.to_string()],
        ));
    }
    lines.push(say(
        pick(
            lang,
            "Узлов вставлено:      %d (из них в пересечениях рёбер: %d)",
            "Nodes inserted:       %d (of which at edge intersections: %d)",
        ),
        &[
            s.nodes_inserted.to_string(),
            s.nodes_crossing.to_string(),
        ],
    ));
    lines.push(say(
        pick(
            lang,
            "Вершин было/стало:    %d / %d",
            "Vertices before/after: %d / %d",
        ),
        &[s.vertices_in.to_string(), s.vertices_out.to_string()],
    ));
    if is_polygon {
        let delta = area_after - area_before;
        let rel = if area_before != 0.0 {
            100.0 * delta / area_before
        } else {
            0.0
        };
        let text = pick(
            lang,
            "Площадь до/после:     %s / %s (%s, %+.3f %%)",
            "Area before/after:    %s / %s (%s, %+.3f %%)",
        );
        let text = say(text, &[fmt(area_before), fmt(area_after), fmt(delta)]);
        let text = text.replace("%+.3f", &format!("{rel:+.3}"));
        lines.push(text.replace("%%", "%"));
    }
    if run.validate {
        lines.push(say(
            pick(
                lang,
                "Некорректных геометрий до/после: %d / %d",
                "Invalid geometries before/after: %d / %d",
            ),
            &[invalid_before.to_string(), invalid_after.to_string()],
        ));
    }
    lines.push(say(
        pick(lang, "Объектов записано:    %d", "Objects written:      %d"),
        &[written.to_string()],
    ));
    (J::Object(out), lines)
}

/// Настройки вставки узлов 1.06 из значений прогона.
pub struct NodesRun {
    pub eps: f64,
    pub crossings: bool,
    pub max_passes: usize,
    pub report: bool,
    /// Кольца опорного слоя. Их вершины дают узлы входу, сами они
    /// не изменяются.
    pub fixed: Vec<Vec<Pt>>,
}

/// 1.06 Вставка недостающих узлов: выходы прогона и строки журнала.
pub fn insert_nodes(
    items: &[Shape],
    attrs: &[J],
    fields: &[(String, String)],
    is_polygon: bool,
    run: &NodesRun,
    lang: Lang,
) -> (J, Vec<String>) {
    let flat = flatten(items);
    let before: Vec<Parts> = items.iter().map(|(_f, p)| p.clone()).collect();
    let area_before = if is_polygon { area_of(&before) } else { 0.0 };
    let vertices_before: usize = flat.rings.iter().map(Vec::len).sum();

    let mut lines = vec![pick(lang, "Чтение слоя...", "Reading the layer...")];
    if !run.fixed.is_empty() {
        lines.push(say(
            pick(lang, "Опорный слой: колец %d", "Donor layer: %d rings"),
            &[run.fixed.len().to_string()],
        ));
    }
    lines.push(say(
        pick(
            lang,
            "Колец %d, вершин %d, отклонение %g",
            "Rings %d, vertices %d, deviation %g",
        ),
        &[
            flat.rings.len().to_string(),
            vertices_before.to_string(),
            g_format(run.eps),
        ],
    ));

    // Вставленный узел сам может лечь на ребро третьего объекта, поэтому
    // одного прохода мало. Повторяем, пока узлы находятся.
    let mut current = flat.rings.clone();
    let mut events: Vec<snap::Insert> = Vec::new();
    let (mut inserted, mut crossing, mut passes) = (0usize, 0usize, 0usize);
    let mut rings: Vec<Option<Vec<Pt>>> = current.iter().cloned().map(Some).collect();
    for step in 0..run.max_passes {
        let done = snap::clean(
            &current,
            &Settings {
                nodes: Nodes {
                    tolerance: run.eps,
                    crossings: run.crossings,
                    project_onto_edge: true,
                },
                mode: Mode::Insert,
                frozen: Vec::new(),
                fixed: run.fixed.clone(),
            },
        );
        let added = done.stats.nodes_inserted;
        passes = step + 1;
        inserted += added;
        crossing += done.stats.nodes_crossing;
        events.extend(done.events.iter().cloned());
        rings = done.rings;
        current = rings.iter().cloned().map(Option::unwrap_or_default).collect();
        if added == 0 {
            break;
        }
        lines.push(say(
            pick(lang, "Проход %d: узлов %d", "Pass %d: %d nodes"),
            &[passes.to_string(), added.to_string()],
        ));
    }

    let new_rings: Vec<Option<Vec<Pt>>> = rings
        .into_iter()
        .map(|r| r.filter(|list| !list.is_empty()))
        .collect();
    let after = rebuild(items, &flat, &new_rings);
    let area_after = if is_polygon { area_of(&after) } else { 0.0 };
    let vertices_after: usize = after.iter().flatten().flatten().map(Vec::len).sum();
    let written = after.iter().filter(|s| !s.is_empty()).count();
    let touched = items
        .iter()
        .zip(&after)
        .filter(|((_f, b), a)| {
            let count = |parts: &Parts| -> usize {
                parts.iter().flatten().map(Vec::len).sum()
            };
            count(b) != count(a)
        })
        .count();

    let mut out = serde_json::Map::new();
    out.insert(
        "OUTPUT".into(),
        shapes_layer(items, attrs, fields, &after, is_polygon),
    );
    if run.report {
        out.insert("REPORT".into(), edits_layer(&events, None));
    }

    lines.push(String::new());
    lines.push(pick(lang, "── Результат ──", "-- Result --"));
    lines.push(say(
        pick(
            lang,
            "Узлов вставлено всего: %d",
            "Nodes inserted in total: %d",
        ),
        &[inserted.to_string()],
    ));
    lines.push(say(
        pick(
            lang,
            "  на рёбрах соседей:   %d",
            "  on neighbour edges:    %d",
        ),
        &[(inserted - crossing).to_string()],
    ));
    lines.push(say(
        pick(
            lang,
            "  в пересечениях рёбер: %d",
            "  at edge intersections: %d",
        ),
        &[crossing.to_string()],
    ));
    lines.push(say(
        pick(lang, "Проходов: %d", "Passes: %d"),
        &[passes.to_string()],
    ));
    if crossing as f64 > 0.5 * inserted.max(1) as f64 {
        lines.push(pick(
            lang,
            "Больше половины узлов пришлось на пересечения рёбер. Это признак того, что объекты слоя накладываются друг на друга, то есть слой не является единым покрытием. Посмотрите перекрытия инструментом 1.01. Если наложение входит в замысел, снимите галочку об узлах в пересечениях. Тогда инструмент достроит только недостающие общие вершины.",
            "More than half of the nodes fell on edge intersections. This means the objects of the layer overlap each other, that is the layer is not a single coverage. Look at the overlaps with tool 1.01. If the overlap is by design, clear the edge intersection option. The tool will then only add the missing shared vertices.",
        ));
    }
    lines.push(say(
        pick(lang, "Объектов изменено:    %d", "Objects changed:      %d"),
        &[touched.to_string()],
    ));
    lines.push(say(
        pick(
            lang,
            "Вершин было/стало:    %d / %d",
            "Vertices before/after: %d / %d",
        ),
        &[vertices_before.to_string(), vertices_after.to_string()],
    ));
    if is_polygon {
        lines.push(
            pick(
                lang,
                "Площадь до/после:     %.6f / %.6f",
                "Area before/after:    %.6f / %.6f",
            )
            .replacen("%.6f", &format!("{area_before:.6}"), 1)
            .replacen("%.6f", &format!("{area_after:.6}"), 1),
        );
        let delta = area_after - area_before;
        if delta == 0.0 {
            lines.push(pick(
                lang,
                "Площадь не изменилась ни на единицу.",
                "The area did not change by a single unit.",
            ));
        } else {
            let rel = if area_before != 0.0 {
                delta.abs() / area_before
            } else {
                0.0
            };
            if rel < 1e-12 {
                lines.push(
                    pick(
                        lang,
                        "Расхождение площади %.3e, это ошибка округления.",
                        "The area differs by %.3e, this is a rounding error.",
                    )
                    .replace("%.3e", &format!("{:.3e}", delta.abs())),
                );
            } else {
                lines.push(say(
                    pick(
                        lang,
                        "Площадь изменилась на %s. Инструмент обещает не менять её вовсе, поэтому проверьте отклонение от ребра. Скорее всего оно завышено.",
                        "The area changed by %s. The tool promises not to change it at all, so check the deviation from the edge. It is most likely too large.",
                    ),
                    &[fmt(delta)],
                ));
            }
        }
    }
    lines.push(say(
        pick(
            lang,
            "Объектов на входе/выходе: %d / %d",
            "Objects in/out: %d / %d",
        ),
        &[items.len().to_string(), written.to_string()],
    ));
    (J::Object(out), lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Т-образный стык: узел вставляется в ребро, площадь не меняется.
    #[test]
    fn a_t_junction_gets_a_node() {
        let square = |x: f64, w: f64| -> Vec<Pt> {
            vec![[x, 0.0], [x + w, 0.0], [x + w, 10.0], [x, 10.0], [x, 0.0]]
        };
        let right = vec![
            [10.0, 0.0],
            [20.0, 0.0],
            [20.0, 10.0],
            [10.0, 10.0],
            [10.0, 5.0],
            [10.0, 0.0],
        ];
        let items: Vec<Shape> = vec![
            (1, vec![vec![square(0.0, 10.0)]]),
            (2, vec![vec![right]]),
        ];
        let attrs = vec![json!({"id": 1}), json!({"id": 2})];
        let (out, lines) = insert_nodes(
            &items,
            &attrs,
            &[("id".to_string(), "int".to_string())],
            true,
            &NodesRun {
                eps: 1e-6,
                crossings: true,
                max_passes: 10,
                report: true,
                fixed: Vec::new(),
            },
            Lang::Ru,
        );
        let f = &out["OUTPUT"]["features"][0];
        let ring = f["geometry"]["rings"][0].as_array().cloned().unwrap_or_default();
        assert_eq!(ring.len(), 6, "{:?}", f["geometry"]);
        assert_eq!(out["REPORT"]["features"][0]["values"][0], "insert");
        assert!(
            lines.iter().any(|l| l.contains("Узлов вставлено всего: 1")),
            "{lines:?}"
        );
    }
}
