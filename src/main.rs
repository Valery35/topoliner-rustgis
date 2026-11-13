// Topoliner для RustGIS - расширение-процесс RustGIS.
// Copyright (C) 2026 ООО «Информ++». Лицензия GPL-3.0-or-later.
// SPDX-License-Identifier: GPL-3.0-or-later

//! Запуск расширения. `topoliner-rustgis rustgis-plugin` запускает
//! RustGIS по `plugin.json`: крейт сам различает сессию и один запуск.
//! Ключ `manifest` печатает манифест с разделом `tools`, собранным
//! из `tools.json`.

use std::process::ExitCode;

use topoliner_rustgis::{catalog, manifest, product};

fn main() -> ExitCode {
    let catalog = match catalog() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("tools.json: {e}");
            return ExitCode::FAILURE;
        }
    };
    match std::env::args().nth(1).as_deref() {
        Some("rustgis-plugin") => rustgis_kit::serve_stdio(&product::Topoliner { catalog }),
        Some("manifest") => match manifest::build(&catalog) {
            Ok(text) => {
                print!("{text}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprintln!("Topoliner для RustGIS. Запуск: topoliner-rustgis rustgis-plugin");
            ExitCode::from(2)
        }
    }
}
