// Topoliner для RustGIS - расширение-процесс RustGIS.
// Copyright (C) 2026 ООО «Информ++». Лицензия GPL-3.0-or-later.
// SPDX-License-Identifier: GPL-3.0-or-later

//! Topoliner как продукт крейта `rustgis-kit`: что у расширения своё.
//!
//! Общее с другими расширениями линейки - протокол RustGIS, окно
//! инструмента по `tools.json`, системы координат, итоги слоями - лежит
//! в крейте. Здесь расчёт инструментов по значениям прогона модуля
//! и команда «О программе».

use rustgis_kit::catalog::Tool;
use rustgis_kit::runner::no_command;
use rustgis_kit::{Call, Catalog, Failure, Lang, Options, Product, Store};
use serde_json::{Value as J, json};

use crate::audit::{self, Settings};
use crate::rings::segment_length_stats;
use crate::values;

/// Инструменты Topoliner в RustGIS, в порядке панели.
pub const OPTIONS: Options = Options {
    toolbox: "Topoliner",
    offered: &["topologyaudit"],
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
