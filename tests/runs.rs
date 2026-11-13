// Topoliner для RustGIS - расширение-процесс RustGIS.
// Copyright (C) 2026 ООО «Информ++». Лицензия GPL-3.0-or-later.
// SPDX-License-Identifier: GPL-3.0-or-later

//! Сверка расчётов с прогонами модуля QGIS (`fixtures/tool_runs.json`)
//! и раздела `tools` манифеста с `tools.json`.
//!
//! Прогон снят настоящим запуском алгоритма Processing без окна, поэтому
//! в него входит всё, что делает диалог поверх ядра: округление величин,
//! состав полей, порядок объектов и пояснения.
//!
//! `TOPOLINER_MANIFEST=1 cargo test` переписывает раздел `tools`
//! и его строки в `plugin.json`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use serde_json::{Value as J, json};

use rustgis_kit::Lang;
use topoliner_rustgis::audit::Settings;
use topoliner_rustgis::product::OPTIONS;
use topoliner_rustgis::{audit, rings, values};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn runs() -> J {
    let path = root().join("fixtures/tool_runs.json");
    serde_json::from_slice(&std::fs::read(path).expect("fixtures/tool_runs.json")).expect("json")
}

fn runs_of(tool: &str) -> Vec<J> {
    runs()["runs"]
        .as_array()
        .expect("runs")
        .iter()
        .filter(|r| r["tool"] == tool)
        .cloned()
        .collect()
}

fn catalog() -> rustgis_kit::Catalog {
    let path = root().join("tools.json");
    rustgis_kit::Catalog::parse(std::fs::read(path).expect("tools.json")).expect("catalog")
}

fn number(values: &J, key: &str, fallback: f64) -> f64 {
    values.get(key).and_then(J::as_f64).unwrap_or(fallback)
}

fn flag(values: &J, key: &str) -> bool {
    values.get(key).and_then(J::as_bool).unwrap_or(true)
}

/// Расчёт 1.01 по значениям прогона: слой находок и строки журнала.
fn audit_run(r: &J, lang: Lang) -> (J, Vec<String>) {
    let v = &r["values"];
    let items = values::items_of(&r["input"]["INPUT"]);
    let s = Settings {
        tolerance: number(v, "TOLERANCE", 2.0),
        area_threshold: number(v, "AREA", 1.0),
        do_overlaps: flag(v, "DO_OVERLAPS"),
        do_gaps: flag(v, "DO_GAPS"),
        do_unsnapped: flag(v, "DO_NODES"),
        cavity_area: number(v, "CAVITY", 0.0),
    };
    let findings = audit::check_items(&items, &s);
    let closed = values::closed_rings(&items);
    let (_m, p05, count) = rings::segment_length_stats(&closed);
    let edge_p05 = (count > 0).then_some(p05);
    let mut widths: Vec<f64> = closed
        .iter()
        .map(|r| rings::ring_width(r))
        .filter(|w| *w > 0.0)
        .collect();
    widths.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let min_width = (!widths.is_empty()).then(|| widths[widths.len() / 20]);
    let hint = audit::tolerance_hint(&findings, s.tolerance, edge_p05, min_width);
    let messages = audit::report(items.len(), &s, &findings, hint.as_ref(), lang);
    (audit::findings_layer(&findings, lang), messages)
}

/// Все прогоны 1.01: поля находок, их точки, порядок и строки журнала -
/// как у модуля. Строки сверяются на обоих языках.
#[test]
fn topology_audit_matches_the_module() {
    let cases = runs_of("topologyaudit");
    assert!(!cases.is_empty(), "в прогонах нет topologyaudit");
    for r in &cases {
        let case = r["case"].as_str().unwrap_or("");
        let want = &r["output"]["OUTPUT"];
        let names: Vec<&str> = want["fields"]
            .as_array()
            .expect("fields")
            .iter()
            .filter_map(|f| f[0].as_str())
            .collect();
        for lang in [Lang::Ru, Lang::En] {
            let (got, messages) = audit_run(r, lang);
            let gf = got["features"].as_array().expect("features");
            let wf = want["features"].as_array().expect("features");
            if gf.len() != wf.len() {
                for f in gf {
                    eprintln!("{case} got  {}", f["values"]);
                }
                for f in wf {
                    eprintln!("{case} want {} {}", f["values"], f["geometry"]);
                }
            }
            assert_eq!(gf.len(), wf.len(), "{case}: количество находок");
            for (k, (g, w)) in gf.iter().zip(wf).enumerate() {
                for (i, name) in names.iter().enumerate() {
                    let a = &g["values"][i];
                    // строки, зависящие от языка, лежат в values_en
                    let b = match (lang, w["values_en"].get(name)) {
                        (Lang::En, Some(x)) => x,
                        _ => &w["values"][i],
                    };
                    match (a.as_f64(), b.as_f64()) {
                        (Some(x), Some(y)) => {
                            assert!((x - y).abs() < 1e-9, "{case} {k} {name}: {x} {y}")
                        }
                        _ => assert_eq!(a, b, "{case} {k} {name}"),
                    }
                }
                for i in 0..2 {
                    let a = g["geometry"]["xyz"][i].as_f64();
                    let b = w["geometry"]["xyz"][i].as_f64();
                    match (a, b) {
                        (Some(x), Some(y)) => {
                            assert!((x - y).abs() < 1e-9, "{case} {k} xyz[{i}]: {x} {y}")
                        }
                        _ => assert_eq!(a, b, "{case} {k} xyz[{i}]"),
                    }
                }
            }
            // Строки журнала. Первая строка прогона - имя и версия модуля,
            // у расширения своя версия, поэтому сверка идёт со второй.
            if lang == Lang::Ru {
                let want_lines: Vec<&str> = r["report"]
                    .as_array()
                    .expect("report")
                    .iter()
                    .skip(1)
                    .filter_map(|l| l[1].as_str())
                    .collect();
                assert_eq!(messages, want_lines, "{case}: журнал");
            } else {
                assert_eq!(messages.len(), r["report"].as_array().expect("report").len() - 1);
            }
        }
    }
}

/// Прогон 1.01 через RustGIS целиком: слой приходит в градусах WGS 84,
/// крейт переводит его в метры зоны UTM, обратно уходит слой находок
/// в градусах. Величины сверяются с прогоном модуля.
#[test]
fn topology_audit_through_rustgis() {
    let case = runs_of("topologyaudit")
        .into_iter()
        .find(|r| r["case"] == "small_overlap")
        .expect("нет прогона small_overlap");
    // метры прогона кладутся около Уфы: инструмент считает в метрах зоны
    let utm = rustgis_kit::geo::Utm::at(56.0, 54.7);
    let [e0, n0] = utm.forward(56.0, 54.7).map(|v| (v / 1000.0).round() * 1000.0);
    let features: Vec<J> = case["input"]["INPUT"]
        .as_array()
        .expect("INPUT")
        .iter()
        .map(|f| {
            let ring: Vec<J> = f["parts"][0]
                .as_array()
                .expect("ring")
                .iter()
                .map(|p| {
                    let [lon, lat] = utm.inverse(
                        e0 + p[0].as_f64().unwrap_or(0.0),
                        n0 + p[1].as_f64().unwrap_or(0.0),
                    );
                    json!([lon, lat])
                })
                .collect();
            json!({"type": "Feature", "row": f["id"], "properties": f["attrs"],
                   "geometry": {"type": "Polygon", "coordinates": [ring]}})
        })
        .collect();
    let v = &case["values"];
    let req = json!({
        "abi": 1, "kind": "tool", "id": "topologyaudit", "lang": "ru",
        "params": {"input": "Покрытие", "tolerance": v["TOLERANCE"], "area": v["AREA"],
                   "cavity": v["CAVITY"], "do_overlaps": true, "do_gaps": true,
                   "do_nodes": true},
        "layers": {"Покрытие": {"type": "FeatureCollection", "features": features}},
    });
    let product = topoliner_rustgis::product::Topoliner { catalog: catalog() };
    let got = rustgis_kit::runner::run_tool(&product, &req, Lang::Ru);
    assert!(got["ok"].is_object(), "{got}");
    let layer = &got["ok"]["layers"][0];
    assert_eq!(layer["name"], "Находки");
    let f = &layer["geojson"]["features"][0];
    let want = &case["output"]["OUTPUT"]["features"][0]["values"];
    assert_eq!(f["properties"]["type"], want[1]);
    assert_eq!(f["properties"]["severity"], want[3]);
    let value = f["properties"]["value"].as_f64().unwrap_or(0.0);
    let wanted = want[6].as_f64().unwrap_or(0.0);
    // градусы туда и обратно рядами UTM стоят долей процента площади
    assert!((value - wanted).abs() / wanted < 1e-4, "{value} {wanted}");
    assert_eq!(f["geometry"]["type"], "Point");
    let p = &f["geometry"]["coordinates"];
    assert!((p[0].as_f64().unwrap_or(0.0) - 56.0).abs() < 0.01, "{p}");
    assert!((p[1].as_f64().unwrap_or(0.0) - 54.7).abs() < 0.01, "{p}");
    let messages = got["ok"]["messages"].as_array().expect("messages");
    assert_eq!(messages[0], case["report"][1][1]);

    // инструмента нет - понятная ошибка, а не молчание
    let bad = json!({"kind": "tool", "id": "linefix", "params": {}, "layers": {}});
    assert!(
        rustgis_kit::runner::run_tool(&product, &bad, Lang::En)["error"].is_string(),
        "чужой инструмент"
    );
}

/// Раздел `tools` манифеста собирается из `tools.json`, и у каждой строки
/// есть русский перевод.
#[test]
fn manifest_tools_follow_tools_json() {
    let path = root().join("plugin.json");
    let cat = catalog();
    let (tools, ru) = rustgis_kit::manifest::tools_section(&cat, &OPTIONS);
    if std::env::var_os("TOPOLINER_MANIFEST").is_some() {
        let text = std::fs::read_to_string(&path).expect("plugin.json");
        let text = rustgis_kit::manifest::splice(&text, &tools, &ru).expect("splice");
        std::fs::write(&path, text).expect("write");
    }
    let m: J = serde_json::from_slice(&std::fs::read(&path).expect("plugin.json")).expect("json");
    assert_eq!(m["tools"], json!(tools), "plugin.json: TOPOLINER_MANIFEST=1");
    for (k, v) in &ru {
        assert_eq!(&m["i18n"]["ru"][k], v, "нет перевода: {k}");
    }
    // правила RustGIS: имена, количество полей, пункты списков
    for t in &tools {
        let id = t["id"].as_str().expect("id");
        assert!(
            id.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "{id}"
        );
        let ps = t["params"].as_array().expect("params");
        assert!(!ps.is_empty() && ps.len() <= 32, "{id}");
        for p in ps {
            let n = p["name"].as_str().expect("name");
            assert!(n.starts_with(|c: char| c.is_ascii_lowercase()), "{n}");
            assert!(
                n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "{n}"
            );
            if p["kind"] == "choice" {
                let ch = p["choices"].as_array().expect("choices");
                assert!((1..=64).contains(&ch.len()));
                assert!(ch.contains(&p["default"]));
            }
        }
    }
    // манифест описывает ровно те инструменты, что предлагает продукт
    assert_eq!(tools.len(), OPTIONS.offered.len());
}
