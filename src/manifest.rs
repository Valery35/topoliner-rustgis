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

/// Манифест как он лежит в репозитории.
pub const PLUGIN_JSON: &str = include_str!("../plugin.json");

/// Манифест с разделом `tools` из `tools.json` и переводами его строк.
pub fn build(catalog: &Catalog) -> Result<String, String> {
    let (tools, ru) = rustgis_kit::manifest::tools_section(catalog, &crate::product::OPTIONS);
    rustgis_kit::manifest::splice(PLUGIN_JSON, &tools, &ru)
}
