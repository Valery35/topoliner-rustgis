// Topoliner для RustGIS - расширение-процесс RustGIS.
// Copyright (C) 2026 ООО «Информ++». Лицензия GPL-3.0-or-later.
// SPDX-License-Identifier: GPL-3.0-or-later

//! Манифест расширения `plugin.json`.
//!
//! Окно инструмента рисует сам RustGIS по разделу `tools` манифеста.
//! Раздел и русские строки к нему собирает крейт из `tools.json` модуля,
//! вставляет в текст манифеста `rustgis_kit::manifest::splice`. Тест
//! `tests/manifest.rs` сверяет раздел с файлом и по переменной окружения
//! `TOPOLINER_MANIFEST` переписывает его.

use rustgis_kit::Catalog;
use serde_json::Value as J;

/// Манифест как он лежит в репозитории.
pub const PLUGIN_JSON: &str = include_str!("../plugin.json");

/// Поля, которых в окне RustGIS нет.
///
/// `keep_z` хранит высоту в итоге. Слои RustGIS приходят без Z, хранить
/// нечего, и в расчёте поле ведёт себя как выключенное. В окне модуля
/// QGIS оно остаётся как было.
pub const HIDDEN: &[&str] = &["keep_z"];

/// Раздел `tools` манифеста и русские строки к нему, без скрытых полей.
pub fn tools_section(catalog: &Catalog) -> (Vec<J>, serde_json::Map<String, J>) {
    let (mut tools, ru) = rustgis_kit::manifest::tools_section(catalog, &crate::product::OPTIONS);
    for t in tools.iter_mut() {
        if let Some(params) = t["params"].as_array_mut() {
            params.retain(|p| !HIDDEN.contains(&p["name"].as_str().unwrap_or("")));
        }
    }
    // Перевод нужен только тем строкам, что остались в разделе.
    let mut kept: std::collections::HashSet<String> = std::collections::HashSet::new();
    for t in &tools {
        collect(t, &mut kept);
    }
    let ru = ru.into_iter().filter(|(k, _)| kept.contains(k)).collect();
    (tools, ru)
}

/// Все строки значения, по которым ищется перевод.
fn collect(v: &J, out: &mut std::collections::HashSet<String>) {
    match v {
        J::String(s) => {
            out.insert(s.clone());
        }
        J::Array(a) => a.iter().for_each(|x| collect(x, out)),
        J::Object(m) => m.values().for_each(|x| collect(x, out)),
        _ => {}
    }
}

/// Манифест с разделом `tools` из `tools.json` и переводами его строк.
pub fn build(catalog: &Catalog) -> Result<String, String> {
    let (tools, ru) = tools_section(catalog);
    rustgis_kit::manifest::splice(PLUGIN_JSON, &tools, &ru)
}
