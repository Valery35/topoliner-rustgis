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
use crate::{arcs, boundaries, clean, coverage, line_checks, snap, topo_fix, values};

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
    let findings = audit::check_items(&items, &s);

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
    let messages = audit::report(items.len(), &s, &findings, hint.as_ref(), lang);
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
    let done = topo_fix::fix_items(&layer.items, &s);
    let written = done.shapes.iter().filter(|p| !p.is_empty()).count();
    let mut out = serde_json::Map::new();
    out.insert(
        "OUTPUT".into(),
        clean::shapes_layer(&layer.items, &layer.attrs, &layer.fields, &done.shapes, true),
    );
    if flag(values, "REMAINS", false) {
        out.insert("REMAINS".into(), audit::findings_layer(&done.left, lang));
    }
    let messages = topo_fix::report(layer.items.len(), written, &done, &s, lang);
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
    messages.extend(line_checks::fix_report(layer.items.len(), written, &done, lang));
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
