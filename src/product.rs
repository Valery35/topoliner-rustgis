// Topoliner для RustGIS - расширение-процесс RustGIS.
// Copyright (C) 2026 ООО «Информ++». Лицензия GPL-3.0-or-later.
// SPDX-License-Identifier: GPL-3.0-or-later

//! Topoliner как продукт крейта `rustgis-kit`: что у расширения своё.
//!
//! Общее с другими расширениями линейки - протокол RustGIS, окно
//! инструмента по `tools.json`, системы координат, итоги слоями - лежит
//! в крейте. Здесь расчёт инструментов по значениям прогона модуля
//! и команда «О программе».

use std::collections::HashMap;

use rustgis_kit::catalog::Tool;
use rustgis_kit::runner::no_command;
use rustgis_kit::{Call, Catalog, Failure, Lang, Options, Product, Store};
use serde_json::{Value as J, json};

use crate::audit::{self, Settings};
use crate::rings::segment_length_stats;
use crate::{
    arcs, assembly, boundaries, clean, coverage, cut, line_checks, simplify, snap, topo_fix,
    values,
};

/// Инструменты Topoliner в RustGIS, в порядке панели.
pub const OPTIONS: Options = Options {
    toolbox: "Topoliner",
    offered: &[
        "topologyaudit",
        "lineaudit",
        "topologyfix",
        "linefix",
        "topologyclean",
        "insertnodes",
        "assemblycheck",
        "cutintocoverage",
        "topologysimplify",
        "boundaries",
        "coverage",
    ],
    xy_tables: &[],
};

/// Topoliner для RustGIS.
pub struct Topoliner {
    pub catalog: Catalog,
}

impl Product for Topoliner {
    fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    fn options(&self) -> &Options {
        &OPTIONS
    }

    /// Высоты итогов: у находок своей высоты нет, но хранилище нужно всегда.
    fn store(&self) -> Store {
        Store::new(rustgis_kit::store::temp_home("topoliner").join("bodies"))
    }

    fn compute(&self, tool: &Tool, values: &J, lang: Lang) -> Result<(J, Vec<String>), Failure> {
        match tool.name.as_str() {
            "topologyaudit" => topology_audit(values, lang),
            "lineaudit" => line_audit(values, lang),
            "topologyfix" => topology_fix(values, lang),
            "linefix" => line_fix(values, lang),
            "assemblycheck" => assembly_check(values, lang),
            "cutintocoverage" => cut_into_coverage(values, lang),
            "topologysimplify" => topology_simplify(values, lang),
            "boundaries" => boundaries_of(values, lang),
            "coverage" => coverage_of(values, lang),
            "topologyclean" => topology_clean(values, lang),
            "insertnodes" => insert_nodes(values, lang),
            other => Err(Failure::Run(other.to_string())),
        }
    }

    fn command(&self, _host: &mut dyn Call, req: &J, lang: Lang) -> J {
        match req["id"].as_str().unwrap_or("") {
            "about" => json!({"ok": about(lang)}),
            id => no_command(id, lang),
        }
    }
}

fn about(lang: Lang) -> J {
    let (title, text) = match lang {
        Lang::Ru => (
            "О программе Topoliner",
            "Topoliner приводит в порядок топологию полигональных и линейных покрытий. Расширение считает так же, как модуль QGIS, и сверяется с его прогонами.",
        ),
        Lang::En => (
            "About Topoliner",
            "Topoliner puts the topology of polygon and line coverages in order. The extension computes as the QGIS plugin does and is checked against its runs.",
        ),
    };
    json!({
        "title": title,
        "program": "Topoliner",
        "version": env!("CARGO_PKG_VERSION"),
        "text": [text],
        "links": [
            {"label": "GitHub", "url": "https://github.com/Valery35/topoliner"},
        ],
    })
}

fn number(values: &J, key: &str) -> Option<f64> {
    values.get(key)?.as_f64()
}

fn flag(values: &J, key: &str, fallback: bool) -> bool {
    values.get(key).and_then(J::as_bool).unwrap_or(fallback)
}

/// 1.01 Проверка топологии полигонов.
fn topology_audit(values: &J, lang: Lang) -> Result<(J, Vec<String>), Failure> {
    if !values["INPUT"].is_array() {
        return Err(Failure::Values("INPUT".into()));
    }
    let items = values::items_of(&values["INPUT"]);
    if items.is_empty() {
        return Err(Failure::Run(match lang {
            Lang::Ru => "Во входном слое нет геометрий.".into(),
            Lang::En => "The input layer has no geometries.".into(),
        }));
    }
    let s = Settings {
        tolerance: number(values, "TOLERANCE").unwrap_or(2.0),
        area_threshold: number(values, "AREA").unwrap_or(1.0),
        do_overlaps: flag(values, "DO_OVERLAPS", true),
        do_gaps: flag(values, "DO_GAPS", true),
        do_unsnapped: flag(values, "DO_NODES", true),
        cavity_area: number(values, "CAVITY").unwrap_or(0.0),
    };
    if s.tolerance <= 0.0 {
        return Err(Failure::Values("TOLERANCE".into()));
    }
    let keys = group_keys(values, items.iter().map(|it| it.fid));
    let findings = match &keys {
        Some(keys) => audit::check_grouped(&items, &s, keys),
        None => audit::check_items(&items, &s),
    };

    // Ограничители допуска: слишком крупный допуск схлопывает короткие
    // рёбра и узкие объекты. Считаются по кольцам входного слоя.
    let rings = values::closed_rings(&items);
    let mut edge_p05 = None;
    let mut min_width = None;
    if !rings.is_empty() {
        let (_median, p05, count) = segment_length_stats(&rings);
        if count > 0 {
            edge_p05 = Some(p05);
        }
        let mut widths: Vec<f64> = rings
            .iter()
            .map(|r| crate::rings::ring_width(r))
            .filter(|w| *w > 0.0)
            .collect();
        widths.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        if !widths.is_empty() {
            // Не минимум, а пятый процентиль: одно волосяное кольцо
            // не должно диктовать допуск для всего слоя.
            min_width = Some(widths[widths.len() / 20]);
        }
    }
    let hint = audit::tolerance_hint(&findings, s.tolerance, edge_p05, min_width);
    let mut messages = audit::report(items.len(), &s, &findings, hint.as_ref(), lang);
    if let Some(keys) = &keys {
        messages.insert(1, grouping_line(values, keys, lang));
    }
    let out = json!({"OUTPUT": audit::findings_layer(&findings, lang)});
    Ok((out, messages))
}

/// Объекты покрытия: номер и части с кольцами.
fn coverage_items(values: &J, lang: Lang) -> Result<Vec<arcs::Shape>, Failure> {
    if !values["INPUT"].is_array() {
        return Err(Failure::Values("INPUT".into()));
    }
    let items: Vec<arcs::Shape> = values::items_of(&values["INPUT"])
        .into_iter()
        .filter(|it| !it.parts.is_empty())
        .map(|it| (it.fid, it.parts))
        .collect();
    if items.is_empty() {
        return Err(Failure::Run(match lang {
            Lang::Ru => "Во входном слое нет полигонов.".into(),
            Lang::En => "The input layer has no polygons.".into(),
        }));
    }
    Ok(items)
}

/// Поля группировки, как их задаёт диалог модуля.
fn group_fields(values: &J) -> Vec<String> {
    match &values["FIELDS"] {
        J::Array(a) => a.iter().filter_map(J::as_str).map(str::to_string).collect(),
        J::String(s) => s
            .split([',', ';'])
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// Ключ группы для каждого объекта, если группировка задана.
fn group_keys(values: &J, fids: impl Iterator<Item = i64>) -> Option<Vec<String>> {
    let fields = group_fields(values);
    if fields.is_empty() {
        return None;
    }
    let attrs: std::collections::HashMap<i64, &J> = values["INPUT"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| f["id"].as_i64().map(|id| (id, &f["attrs"])))
        .collect();
    Some(
        fids.map(|fid| {
            attrs
                .get(&fid)
                .map_or_else(String::new, |a| assembly::group_key(a, &fields))
        })
        .collect(),
    )
}

/// Строка журнала о группировке.
fn grouping_line(values: &J, keys: &[String], lang: Lang) -> String {
    let unique: std::collections::BTreeSet<&String> = keys.iter().collect();
    match lang {
        Lang::Ru => "Группировка по %s: групп %d",
        Lang::En => "Grouped by %s: %d groups",
    }
    .replacen("%s", &group_fields(values).join(", "), 1)
    .replacen("%d", &unique.len().to_string(), 1)
}

/// Кольца слоя-эталона, замкнутые повтором первой вершины.
///
/// Эталон не изменяется, его вершины становятся лидерами при слиянии
/// и дают узлы рёбрам входа при вставке.
fn reference_rings(values: &J) -> Vec<Vec<crate::rings::Pt>> {
    let Some(layer) = values.get("REFERENCE").filter(|v| v.is_array()) else {
        return Vec::new();
    };
    let is_polygon = values["@REFERENCE"]["geometry"] != "line";
    let mut rings = Vec::new();
    for (_fid, parts) in values::shapes_of(layer, is_polygon) {
        for part in parts {
            rings.extend(part);
        }
    }
    rings
}

/// Сетка опознания общих вершин и отклонение при их поиске.
fn grid_and_eps(values: &J) -> (f64, f64) {
    (
        number(values, "GRID").unwrap_or(1e-6),
        number(values, "NODE_EPS").unwrap_or(1e-6),
    )
}

/// 2.02 Границы полигонов линиями.
fn boundaries_of(values: &J, lang: Lang) -> Result<(J, Vec<String>), Failure> {
    let items = coverage_items(values, lang)?;
    let (grid, node_eps) = grid_and_eps(values);
    let field = values
        .get("FIELD")
        .and_then(J::as_str)
        .filter(|s| !s.is_empty());
    let mut side: HashMap<i64, String> = HashMap::new();
    if let Some(name) = field {
        for f in values["INPUT"].as_array().into_iter().flatten() {
            let Some(id) = f["id"].as_i64() else { continue };
            match &f["attrs"][name] {
                J::Null => {}
                J::String(s) => {
                    side.insert(id, s.clone());
                }
                other => {
                    side.insert(id, other.to_string());
                }
            }
        }
    }
    let borders = boundaries::extract_boundaries(&items, grid, node_eps);
    let messages = boundaries::report(items.len(), &borders, lang);
    let out = json!({"OUTPUT": boundaries::borders_layer(&borders, field, &side, lang)});
    Ok((out, messages))
}

/// 1.02 Проверка топологии линий.
fn line_audit(values: &J, lang: Lang) -> Result<(J, Vec<String>), Failure> {
    if !values["INPUT"].is_array() {
        return Err(Failure::Values("INPUT".into()));
    }
    // Мультилиния разбирается на части, у каждой свой номер объекта.
    let mut items: Vec<(i64, Vec<crate::rings::Pt>)> = Vec::new();
    for (i, f) in values["INPUT"].as_array().into_iter().flatten().enumerate() {
        let fid = f["id"].as_i64().unwrap_or(i as i64);
        for (_fid, parts) in values::shapes_of(&json!([f]), false) {
            for part in parts {
                if let Some(line) = part.into_iter().next() {
                    items.push((fid, line));
                }
            }
        }
    }
    if items.is_empty() {
        return Err(Failure::Run(match lang {
            Lang::Ru => "Во входном слое нет линий.".into(),
            Lang::En => "The input layer has no lines.".into(),
        }));
    }
    let s = line_checks::Settings {
        tolerance: number(values, "TOLERANCE").unwrap_or(2.0),
        min_length: number(values, "MIN_LENGTH").unwrap_or(0.0),
        do_dangles: flag(values, "DO_DANGLES", true),
        do_crossings: flag(values, "DO_CROSSINGS", true),
        do_pseudo: flag(values, "DO_PSEUDO", false),
    };
    let findings = line_checks::check_lines(&items, &s);
    let messages = line_checks::report(items.len(), &s, &findings, lang);
    let out = json!({"OUTPUT": line_checks::findings_layer(&findings, lang)});
    Ok((out, messages))
}

/// 1.08 Ввод контуров в покрытие.
fn cut_into_coverage(values: &J, lang: Lang) -> Result<(J, Vec<String>), Failure> {
    let layer = input_layer(values, lang)?;
    if !values["CUT"].is_array() {
        return Err(Failure::Values("CUT".into()));
    }
    let cuts = values::shapes_of(&values["CUT"], true);
    if cuts.is_empty() {
        return Err(Failure::Run(match lang {
            Lang::Ru => "В слое контуров нет геометрий.".into(),
            Lang::En => "The outline layer has no geometries.".into(),
        }));
    }
    let mode = match values.get("MODE").and_then(J::as_u64).unwrap_or(0) {
        1 => cut::Mode::Clip,
        2 => cut::Mode::Inset,
        _ => cut::Mode::Overlay,
    };
    let area = number(values, "AREA").unwrap_or(0.0);
    let node_eps = number(values, "NODE_EPS").unwrap_or(1e-6);

    // Состояние покрытия: объекты в порядке входа, дальше созданные.
    let mut order: Vec<i64> = layer.items.iter().map(|(fid, _)| *fid).collect();
    let mut objects: std::collections::BTreeMap<i64, (Vec<J>, arcs::Parts)> =
        std::collections::BTreeMap::new();
    for ((fid, parts), attrs) in layer.items.iter().zip(&layer.attrs) {
        let row: Vec<J> = layer
            .fields
            .iter()
            .map(|(name, _)| attrs[name].clone())
            .collect();
        objects.insert(*fid, (row, parts.clone()));
    }
    let mut t = cut::Totals {
        cuts: cuts.len(),
        items: layer.items.len(),
        area_before: layer
            .items
            .iter()
            .map(|(_f, p)| crate::backend::area(&crate::backend::geom_of(p)))
            .sum(),
        ..cut::Totals::default()
    };
    let mut lines: Vec<String> = Vec::new();
    let mut next_key: i64 = -1;
    for (number, (_cut_fid, cut_parts)) in cuts.iter().enumerate() {
        let Some(box_of) = cut::bbox(cut_parts) else {
            continue;
        };
        let near: Vec<(i64, arcs::Parts)> = order
            .iter()
            .filter_map(|key| objects.get(key).map(|(_v, p)| (*key, p.clone())))
            .filter(|(_k, p)| cut::bbox(p).is_some_and(|b| cut::overlap(b, box_of)))
            .collect();
        let done = cut::cut_into_coverage(&near, cut_parts, area, node_eps, mode);

        // Атрибуты снимаются до правки: поглощённый сосед исчезает.
        let rows: Vec<J> = done
            .donors
            .iter()
            .filter_map(|key| objects.get(key))
            .map(|(row, _p)| {
                J::Object(
                    layer
                        .fields
                        .iter()
                        .zip(row)
                        .map(|((name, _), v)| (name.clone(), v.clone()))
                        .collect(),
                )
            })
            .collect();

        for (key, parts) in &done.changed {
            if parts.is_empty() {
                objects.remove(key);
                t.swallowed += 1;
            } else if let Some(slot) = objects.get_mut(key) {
                slot.1 = parts.clone();
                t.changed += 1;
            }
        }
        for (key, parts) in &done.noded {
            if let Some(slot) = objects.get_mut(key)
                && !parts.is_empty()
            {
                slot.1 = parts.clone();
            }
        }
        t.nodes += done.nodes;
        if !done.created.is_empty() {
            let refs: Vec<&J> = rows.iter().collect();
            objects.insert(
                next_key,
                (cut::inherited_values(&refs, &layer.fields), done.created),
            );
            order.push(next_key);
            next_key -= 1;
            t.added += 1;
        }
        t.outside += done.outside;
        t.dropped += done.dropped;
        if cuts.len() <= 20 {
            lines.push(cut::cut_line(
                number + 1,
                done.donors.len(),
                done.outside,
                lang,
            ));
        }
    }

    let records: Vec<(Vec<J>, arcs::Parts)> = order
        .iter()
        .filter_map(|key| objects.get(key).cloned())
        .collect();
    t.written = records.iter().filter(|(_v, p)| !p.is_empty()).count();
    t.area_after = records
        .iter()
        .map(|(_v, p)| crate::backend::area(&crate::backend::geom_of(p)))
        .sum();
    let out = json!({"OUTPUT": cut::coverage_layer(&records, &layer.fields)});
    let mut messages = cut::report(&t, mode, area, lang);
    // Строки про контуры идут сразу после режима наложения.
    for (i, line) in lines.into_iter().enumerate() {
        messages.insert(2 + i, line);
    }
    Ok((out, messages))
}

/// 1.07 Контроль сборки по атрибуту.
fn assembly_check(values: &J, lang: Lang) -> Result<(J, Vec<String>), Failure> {
    let layer = input_layer(values, lang)?;
    let fields: Vec<String> = match &values["FIELDS"] {
        J::Array(a) => a.iter().filter_map(J::as_str).map(str::to_string).collect(),
        J::String(s) => s
            .split([',', ';'])
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    };
    if fields.is_empty() {
        return Err(Failure::Values(match lang {
            Lang::Ru => "Укажите хотя бы одно поле группировки.".into(),
            Lang::En => "Give at least one grouping field.".into(),
        }));
    }
    let items: Vec<(i64, arcs::Parts, String)> = layer
        .items
        .iter()
        .zip(&layer.attrs)
        .map(|((fid, parts), attrs)| {
            (*fid, parts.clone(), assembly::group_key(attrs, &fields))
        })
        .collect();
    let s = assembly::Settings {
        area_threshold: number(values, "AREA").unwrap_or(0.0),
        max_gap: number(values, "MAX_GAP").unwrap_or(0.0),
        ignore_holes: flag(values, "IGNORE_HOLES", false),
        is_line: !layer.is_polygon,
    };
    let (findings, groups) = assembly::check_assembly(&items, &s);
    let messages = assembly::report(&fields, &groups, &s, lang);
    let out = json!({"OUTPUT": assembly::findings_layer(&findings, lang)});
    Ok((out, messages))
}

/// 2.01 Топологическое упрощение.
fn topology_simplify(values: &J, lang: Lang) -> Result<(J, Vec<String>), Failure> {
    let layer = input_layer(values, lang)?;
    let s = simplify::SimplifySettings {
        tolerance: number(values, "TOLERANCE").unwrap_or(1.0),
        grid: number(values, "GRID").unwrap_or(1e-6),
        min_points: values.get("MIN_POINTS").and_then(J::as_u64).unwrap_or(0) as usize,
        smooth: values.get("SMOOTH").and_then(J::as_u64).unwrap_or(0) as usize,
        method: match values.get("METHOD").and_then(J::as_u64).unwrap_or(0) {
            1 => simplify::Method::Visvalingam,
            _ => simplify::Method::Douglas,
        },
    };
    let flat = simplify::flatten(&layer.items, layer.is_polygon);
    let measure = |shapes: &[arcs::Parts]| -> f64 {
        shapes
            .iter()
            .map(|parts| {
                if layer.is_polygon {
                    crate::backend::area(&crate::backend::geom_of(parts))
                } else {
                    parts
                        .iter()
                        .filter_map(|p| p.first())
                        .map(|line| boundaries::length_of(line))
                        .sum()
                }
            })
            .sum()
    };
    let before: Vec<arcs::Parts> = layer.items.iter().map(|(_f, p)| p.clone()).collect();
    let done = simplify::simplify_topology(&flat.rings, &flat.closed, &s);
    let after = simplify::rebuild(&layer.items, &flat, &done.rings);
    let written = after.iter().filter(|p| !p.is_empty()).count();
    let out = json!({
        "OUTPUT": clean::shapes_layer(
            &layer.items, &layer.attrs, &layer.fields, &after, layer.is_polygon),
    });
    let outcome = simplify::Outcome {
        items: layer.items.len(),
        written,
        is_polygon: layer.is_polygon,
        before: measure(&before),
        after: measure(&after),
    };
    let messages = simplify::report(&outcome, &s, &done.stats, lang);
    Ok((out, messages))
}

/// 1.03 Очистка топологии полигонов.
fn topology_fix(values: &J, lang: Lang) -> Result<(J, Vec<String>), Failure> {
    let layer = input_layer(values, lang)?;
    let s = topo_fix::FixSettings {
        tolerance: number(values, "TOLERANCE").unwrap_or(2.0),
        area_threshold: number(values, "AREA").unwrap_or(1.0),
        snap: flag(values, "DO_SNAP", true),
        fix_invalid: flag(values, "DO_VALID", true),
        resolve_overlaps: flag(values, "DO_OVERLAPS", true),
        fill_gaps: flag(values, "DO_GAPS", true),
        drop_tiny_features: flag(values, "DROP_TINY", false),
        protect_narrow: true,
        spike_angle: number(values, "SPIKE").unwrap_or(1.0),
        cavity_area: number(values, "CAVITY").unwrap_or(0.0),
        winner: match values.get("WINNER").and_then(J::as_u64).unwrap_or(0) {
            1 => topo_fix::Winner::First,
            _ => topo_fix::Winner::Larger,
        },
        max_area_loss: 0.25,
    };
    let keys = group_keys(values, layer.items.iter().map(|(fid, _)| *fid));
    let done = match &keys {
        Some(keys) => topo_fix::fix_grouped(&layer.items, &s, keys),
        None => topo_fix::fix_items(&layer.items, &s),
    };
    let written = done.shapes.iter().filter(|p| !p.is_empty()).count();
    let mut out = serde_json::Map::new();
    out.insert(
        "OUTPUT".into(),
        clean::shapes_layer(&layer.items, &layer.attrs, &layer.fields, &done.shapes, true),
    );
    if flag(values, "REMAINS", false) {
        out.insert("REMAINS".into(), audit::findings_layer(&done.left, lang));
    }
    let mut messages = topo_fix::report(layer.items.len(), written, &done, &s, lang);
    if let Some(keys) = &keys {
        messages.insert(1, grouping_line(values, keys, lang));
    }
    Ok((J::Object(out), messages))
}

/// 1.04 Очистка топологии линий.
fn line_fix(values: &J, lang: Lang) -> Result<(J, Vec<String>), Failure> {
    let layer = input_layer(values, lang)?;
    // Мультилиния разбирается на части, у каждой свой номер объекта.
    let mut items: Vec<(i64, Vec<crate::rings::Pt>)> = Vec::new();
    let mut parts_of: Vec<Vec<usize>> = Vec::new();
    for (fid, parts) in &layer.items {
        let mut indices = Vec::new();
        for part in parts {
            if let Some(line) = part.first() {
                indices.push(items.len());
                items.push((*fid, line.clone()));
            }
        }
        parts_of.push(indices);
    }
    let s = line_checks::FixSettings {
        tolerance: number(values, "TOLERANCE").unwrap_or(2.0),
        min_length: number(values, "MIN_LENGTH").unwrap_or(0.0),
        trim_overshoots: flag(values, "DO_TRIM", true),
        close_undershoots: flag(values, "DO_CLOSE", true),
        snap: flag(values, "DO_SNAP", true),
        drop_short: flag(values, "DROP_SHORT", false),
        spike_angle: number(values, "SPIKE").unwrap_or(1.0),
    };
    let done = line_checks::fix_lines(&items, &s);
    // Объект собирается обратно из своих частей.
    let shapes: Vec<arcs::Parts> = parts_of
        .iter()
        .map(|indices| {
            indices
                .iter()
                .filter_map(|&k| done.lines.get(k).cloned().flatten())
                .map(|line| vec![line])
                .collect()
        })
        .collect();
    let written = shapes.iter().filter(|s| !s.is_empty()).count();
    let mut out = serde_json::Map::new();
    out.insert(
        "OUTPUT".into(),
        clean::shapes_layer(&layer.items, &layer.attrs, &layer.fields, &shapes, false),
    );
    if flag(values, "REMAINS", false) {
        out.insert(
            "REMAINS".into(),
            line_checks::findings_layer(&done.left, lang),
        );
    }
    let mut messages = vec![
        match lang {
            Lang::Ru => "Линий: %d, допуск %g".to_string(),
            Lang::En => "Lines: %d, tolerance %g".to_string(),
        }
        .replacen("%d", &items.len().to_string(), 1)
        .replacen("%g", &audit::g_format(s.tolerance), 1),
    ];
    let lost = layer.items.len() - written;
    messages.extend(line_checks::fix_report(
        layer.items.len(),
        written,
        lost,
        &done,
        lang,
    ));
    Ok((J::Object(out), messages))
}

/// Слой входа для сшивки: объекты с частями, их атрибуты и поля.
struct Layer {
    items: Vec<arcs::Shape>,
    attrs: Vec<J>,
    fields: Vec<(String, String)>,
    is_polygon: bool,
}

fn input_layer(values: &J, lang: Lang) -> Result<Layer, Failure> {
    if !values["INPUT"].is_array() {
        return Err(Failure::Values("INPUT".into()));
    }
    let info = &values["@INPUT"];
    let is_polygon = info["geometry"] != "line";
    let items = values::shapes_of(&values["INPUT"], is_polygon);
    if items.is_empty() {
        return Err(Failure::Run(match lang {
            Lang::Ru => "Во входном слое нет пригодных геометрий.".into(),
            Lang::En => "The input layer has no usable geometries.".into(),
        }));
    }
    // Атрибуты идут в порядке объектов, как их отдал слой.
    let mut attrs = Vec::new();
    for f in values["INPUT"].as_array().into_iter().flatten() {
        let fid = f["id"].as_i64();
        if items.iter().any(|(id, _)| Some(*id) == fid) {
            attrs.push(f["attrs"].clone());
        }
    }
    let numeric: Vec<&str> = info["numeric"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(J::as_str)
        .collect();
    let fields: Vec<(String, String)> = info["fields"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(J::as_str)
        .map(|n| {
            let kind = if numeric.contains(&n) { "double" } else { "string" };
            (n.to_string(), kind.to_string())
        })
        .collect();
    Ok(Layer {
        items,
        attrs,
        fields,
        is_polygon,
    })
}

/// 1.05 Сшивка узлов и вершин.
fn topology_clean(values: &J, lang: Lang) -> Result<(J, Vec<String>), Failure> {
    let layer = input_layer(values, lang)?;
    let tolerance = number(values, "TOLERANCE").unwrap_or(2.0);
    if tolerance <= 0.0 {
        return Err(Failure::Values("TOLERANCE".into()));
    }
    let mode = match values.get("MODE").and_then(J::as_u64).unwrap_or(0) {
        1 => snap::Mode::Insert,
        2 => snap::Mode::Merge,
        _ => snap::Mode::Both,
    };
    let run = clean::CleanRun {
        tolerance,
        mode,
        priority: values.get("PRIORITY").and_then(J::as_u64).unwrap_or(1) as usize,
        crossings: flag(values, "CROSSINGS", true),
        protect: flag(values, "PROTECT", true),
        validate: flag(values, "VALIDATE", true),
        report: flag(values, "REPORT", false),
        fixed: reference_rings(values),
    };
    Ok(clean::topology_clean(
        &layer.items,
        &layer.attrs,
        &layer.fields,
        layer.is_polygon,
        &run,
        lang,
    ))
}

/// 1.06 Вставка недостающих узлов.
fn insert_nodes(values: &J, lang: Lang) -> Result<(J, Vec<String>), Failure> {
    let layer = input_layer(values, lang)?;
    let run = clean::NodesRun {
        eps: number(values, "EPS").unwrap_or(1e-6),
        crossings: flag(values, "CROSSINGS", true),
        max_passes: values.get("MAX_PASSES").and_then(J::as_u64).unwrap_or(10) as usize,
        report: flag(values, "REPORT", false),
        fixed: reference_rings(values),
    };
    Ok(clean::insert_nodes(
        &layer.items,
        &layer.attrs,
        &layer.fields,
        layer.is_polygon,
        &run,
        lang,
    ))
}

/// 2.03 Топологическая модель покрытия.
fn coverage_of(values: &J, lang: Lang) -> Result<(J, Vec<String>), Failure> {
    let items = coverage_items(values, lang)?;
    let (grid, node_eps) = grid_and_eps(values);
    let model = coverage::build_coverage(&items, grid, node_eps);
    let messages = coverage::report(items.len(), &model, lang);
    let out = json!({
        "NODES": coverage::nodes_layer(&model, lang),
        "ARCS": coverage::arcs_layer(&model),
    });
    Ok((out, messages))
}
