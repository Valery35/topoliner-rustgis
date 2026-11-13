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

/// Единицы в подписях полей, английские.
///
/// Модуль QGIS считает в системе координат слоя, и подписи у него
/// об этом и говорят. Слои RustGIS приходят в градусах, крейт переводит
/// их в метры зоны UTM, и расчёт всегда идёт в метрах. Подпись «в
/// единицах CRS слоя» в окне RustGIS была бы неправдой, поэтому она
/// заменяется. Правка живёт только здесь, `tools.json` модуля и его
/// руководство остаются как есть.
///
/// Длинные образцы стоят раньше коротких: замена идёт по порядку.
const UNITS_EN: &[(&str, &str)] = &[
    ("in layer CRS units", "in metres of the UTM zone"),
    ("in square CRS units", "in square metres of the UTM zone"),
    ("in CRS units", "in metres of the UTM zone"),
];

/// Те же единицы по-русски.
const UNITS_RU: &[(&str, &str)] = &[
    ("в единицах CRS слоя", "в метрах зоны UTM"),
    ("в кв. единицах CRS", "в кв. метрах зоны UTM"),
    ("в единицах CRS", "в метрах зоны UTM"),
];

fn units(text: &str, table: &[(&str, &str)]) -> String {
    let mut out = text.to_string();
    for (from, to) in table {
        out = out.replace(from, to);
    }
    out
}

/// Замена единиц во всех строках значения.
fn retell(v: &mut J, table: &[(&str, &str)]) {
    match v {
        J::String(s) => *s = units(s, table),
        J::Array(a) => a.iter_mut().for_each(|x| retell(x, table)),
        J::Object(m) => m.values_mut().for_each(|x| retell(x, table)),
        _ => {}
    }
}

/// Раздел `tools` манифеста и русские строки к нему, без скрытых полей
/// и с единицами окна RustGIS.
pub fn tools_section(catalog: &Catalog) -> (Vec<J>, serde_json::Map<String, J>) {
    let (mut tools, ru) = rustgis_kit::manifest::tools_section(catalog, &crate::product::OPTIONS);
    for t in tools.iter_mut() {
        if let Some(params) = t["params"].as_array_mut() {
            params.retain(|p| !HIDDEN.contains(&p["name"].as_str().unwrap_or("")));
        }
        retell(t, UNITS_EN);
    }
    // Ключ перевода - английская строка, поэтому меняются обе стороны.
    let ru: serde_json::Map<String, J> = ru
        .into_iter()
        .map(|(k, mut v)| {
            retell(&mut v, UNITS_RU);
            (units(&k, UNITS_EN), v)
        })
        .collect();
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
