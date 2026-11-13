// Topoliner для RustGIS - расширение-процесс RustGIS.
// Copyright (C) 2026 ООО «Информ++». Лицензия GPL-3.0-or-later.
// SPDX-License-Identifier: GPL-3.0-or-later

//! Topoliner для RustGIS: топология полигональных и линейных покрытий
//! без QGIS. Расчёты повторяют модуль QGIS и сверяются с его прогонами
//! (`fixtures/tool_runs.json`, тест `tests/runs.rs`).
//!
//! Связь с RustGIS, окна инструментов по `tools.json`, системы координат
//! и итоги слоями даёт крейт `rustgis-kit`. Своё у продукта:
//!
//! - [`product`] - трейт `Product`: каталог, настройки, расчёт, команды;
//! - [`audit`] - 1.01 Проверка топологии полигонов;
//! - [`boundaries`] - 2.02 Границы полигонов линиями;
//! - [`coverage`] - 2.03 Топологическая модель покрытия;
//! - [`rings`], [`snap`], [`arcs`], [`backend`] - геометрия: кольца,
//!   вставка узлов, разбор на дуги, булевы операции;
//! - [`rounding`] - округление величин до значащих цифр;
//! - [`values`] - значения прогона модуля в объекты проверки;
//! - [`manifest`] - раздел `tools` манифеста `plugin.json`.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod arcs;
pub mod audit;
pub mod backend;
pub mod boundaries;
pub mod coverage;
pub mod manifest;
pub mod product;
pub mod rings;
pub mod rounding;
pub mod snap;
pub mod values;

/// Описания инструментов, выгрузка модуля (`tools/export_tools.py`).
/// Обновляются вместе с прогонами `fixtures/tool_runs.json`.
pub const TOOLS_JSON: &[u8] = include_bytes!("../tools.json");

/// Каталог инструментов из [`TOOLS_JSON`].
pub fn catalog() -> Result<rustgis_kit::Catalog, String> {
    rustgis_kit::Catalog::parse(TOOLS_JSON)
}
