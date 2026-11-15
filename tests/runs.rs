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

use rustgis_kit::{Lang, Product};
use topoliner_rustgis::product::{OPTIONS, Topoliner};

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

/// Значения прогона для расчёта: поля вместе со слоями входа, как их
/// собирает крейт.
fn run_values(r: &J) -> J {
    let mut out = r["values"].clone();
    if let (Some(o), Some(input)) = (out.as_object_mut(), r["input"].as_object()) {
        for (k, v) in input {
            o.insert(k.clone(), v.clone());
        }
    }
    out
}

/// Расчёт инструмента по значениям прогона.
fn compute(r: &J, lang: Lang) -> (J, Vec<String>) {
    let product = Topoliner { catalog: catalog() };
    let name = r["tool"].as_str().expect("tool");
    let tool = product.catalog.tool(name).expect("инструмента нет");
    product
        .compute(tool, &run_values(r), lang)
        .unwrap_or_else(|e| unreachable!("{name}: {e:?}"))
}

/// Вершины геометрии объекта выхода, по кольцам или частям.
///
/// Прогон модуля пишет полигон плоским списком колец в `parts`,
/// расширение отдаёт его в `rings` или `polygons`, как просит крейт.
/// Для сверки обе записи приводятся к списку колец.
fn vertices(g: &J) -> Vec<Vec<Vec<f64>>> {
    let point = |p: &J| -> Vec<f64> {
        (0..2)
            .map(|i| p.get(i).and_then(J::as_f64).unwrap_or(f64::NAN))
            .collect()
    };
    let line = |v: &J| -> Vec<Vec<f64>> {
        v.as_array().into_iter().flatten().map(&point).collect()
    };
    for key in ["parts", "rings"] {
        if let Some(list) = g[key].as_array() {
            return list.iter().map(&line).collect();
        }
    }
    if let Some(polys) = g["polygons"].as_array() {
        return polys
            .iter()
            .flat_map(|p| p.as_array().into_iter().flatten().map(&line))
            .collect();
    }
    if g.get("points").is_some() {
        return vec![line(&g["points"])];
    }
    if g.get("xyz").is_some_and(|p| !p.is_null()) {
        return vec![vec![point(&g["xyz"])]];
    }
    Vec::new()
}

/// Совпадают ли две вершины.
fn same_point(a: &[f64], b: &[f64]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-9)
}

/// Совпадают ли два кольца или две линии.
///
/// Замкнутое кольцо сравнивается как цикл: начальная вершина и обход
/// у GEOS и у крейта `geo` свои, а кольцо при этом то же самое. Линия
/// сравнивается вершина в вершину.
fn same_ring(a: &[Vec<f64>], b: &[Vec<f64>]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let closed = |r: &[Vec<f64>]| r.len() > 2 && same_point(&r[0], &r[r.len() - 1]);
    if !closed(a) || !closed(b) {
        return a.iter().zip(b).all(|(p, q)| same_point(p, q));
    }
    let body = |r: &[Vec<f64>]| r[..r.len() - 1].to_vec();
    let (ra, rb) = (body(a), body(b));
    let n = ra.len();
    let back: Vec<Vec<f64>> = rb.iter().rev().cloned().collect();
    for side in [&rb, &back] {
        for shift in 0..n {
            if (0..n).all(|i| same_point(&ra[i], &side[(i + shift) % n])) {
                return true;
            }
        }
    }
    false
}

/// Один выход против образца: количество объектов, значения полей
/// по порядку и вершины геометрии.
fn same_layer(case: &str, key: &str, got: &J, want: &J, lang: Lang) {
    let names: Vec<&str> = want["fields"]
        .as_array()
        .expect("fields")
        .iter()
        .filter_map(|f| f[0].as_str())
        .collect();
    let gf = got["features"].as_array().expect("features");
    let wf = want["features"].as_array().expect("features");
    if gf.len() != wf.len() {
        for f in gf {
            eprintln!("{case} {key} got  {}", f["values"]);
        }
        for f in wf {
            eprintln!("{case} {key} want {} {}", f["values"], f["geometry"]);
        }
    }
    assert_eq!(gf.len(), wf.len(), "{case} {key}: количество объектов");
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
                    assert!((x - y).abs() < 1e-9, "{case} {key} {k} {name}: {x} {y}")
                }
                _ => assert_eq!(a, b, "{case} {key} {k} {name}"),
            }
        }
        let (a, b) = (vertices(&g["geometry"]), vertices(&w["geometry"]));
        assert_eq!(a.len(), b.len(), "{case} {key} {k}: колец {a:?} {b:?}");
        for (ra, rb) in a.iter().zip(&b) {
            assert!(same_ring(ra, rb), "{case} {key} {k}: {ra:?} {rb:?}");
        }
    }
}

/// Все прогоны инструмента: выходы и строки журнала как у модуля,
/// на русском и на английском.
fn runs_match_the_module(tool: &str) {
    let cases = runs_of(tool);
    assert!(!cases.is_empty(), "в прогонах нет {tool}");
    for r in &cases {
        let case = r["case"].as_str().unwrap_or("");
        for lang in [Lang::Ru, Lang::En] {
            let (got, messages) = compute(r, lang);
            let outputs = r["output"].as_object().expect("output");
            assert_eq!(
                got.as_object().map(serde_json::Map::len),
                Some(outputs.len()),
                "{case}: количество выходов"
            );
            for (key, want) in outputs {
                same_layer(case, key, &got[key], want, lang);
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
                // Английского образца у прогона нет, сверять не с чем.
                // Но русская буква в английском журнале - это строка,
                // прошедшая мимо перевода, и её видно без образца.
                for line in &messages {
                    assert!(
                        !line.chars().any(|c| ('а'..='я').contains(&c)
                            || ('А'..='Я').contains(&c)
                            || c == 'ё'
                            || c == 'Ё'),
                        "{case}: русская строка в английском журнале: {line}"
                    );
                }
            }
        }
    }
}

/// Все прогоны файла разобраны по инструментам, пропущенных нет.
#[test]
fn every_run_is_checked() {
    let all = runs();
    let list = all["runs"].as_array().expect("runs");
    let mine: usize = OPTIONS
        .offered
        .iter()
        .map(|name| list.iter().filter(|r| r["tool"] == *name).count())
        .sum();
    assert_eq!(mine, list.len(), "в файле есть прогоны чужих инструментов");
    assert_eq!(OPTIONS.offered.len(), 11);
}

/// 1.01 Проверка топологии полигонов.
#[test]
fn topology_audit_matches_the_module() {
    runs_match_the_module("topologyaudit");
}

/// 1.02 Проверка топологии линий.
#[test]
fn line_audit_matches_the_module() {
    runs_match_the_module("lineaudit");
}

/// 1.03 Очистка топологии полигонов.
#[test]
fn topology_fix_matches_the_module() {
    runs_match_the_module("topologyfix");
}

/// 1.04 Очистка топологии линий.
#[test]
fn line_fix_matches_the_module() {
    runs_match_the_module("linefix");
}

/// 1.07 Контроль сборки по атрибуту.
#[test]
fn assembly_check_matches_the_module() {
    runs_match_the_module("assemblycheck");
}

/// 1.08 Ввод контуров в покрытие.
#[test]
fn cut_into_coverage_matches_the_module() {
    runs_match_the_module("cutintocoverage");
}

/// 2.01 Топологическое упрощение.
#[test]
fn topology_simplify_matches_the_module() {
    runs_match_the_module("topologysimplify");
}

/// 2.02 Границы полигонов линиями.
#[test]
fn boundaries_match_the_module() {
    runs_match_the_module("boundaries");
}

/// 2.03 Топологическая модель покрытия.
#[test]
fn coverage_matches_the_module() {
    runs_match_the_module("coverage");
}

/// 1.05 Сшивка узлов и вершин.
#[test]
fn topology_clean_matches_the_module() {
    runs_match_the_module("topologyclean");
}

/// 1.06 Вставка недостающих узлов.
#[test]
fn insert_nodes_matches_the_module() {
    runs_match_the_module("insertnodes");
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
    let (tools, ru) = topoliner_rustgis::manifest::tools_section(&cat);
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
    // скрытых полей в окне нет
    for t in &tools {
        for p in t["params"].as_array().expect("params") {
            assert!(
                !topoliner_rustgis::manifest::HIDDEN.contains(&p["name"].as_str().unwrap_or("")),
                "{}: {}",
                t["id"],
                p["name"]
            );
        }
    }
    // Единицы в окне - метры зоны UTM, а не система координат слоя.
    // Расчёт всегда идёт в метрах, и подпись обязана это говорить.
    let text = serde_json::to_string(&m).expect("manifest");
    assert!(!text.contains("CRS units"), "в манифесте осталось CRS units");
    assert!(!text.contains("единицах CRS"), "в манифесте осталось единицах CRS");
    assert!(text.contains("metres of the UTM zone"));
    assert!(text.contains("метрах зоны UTM") || text.contains("метры зоны UTM"));
}

/// Своя вкладка ленты: все одиннадцать инструментов лежат в её меню,
/// лишнего в ней нет, и каждая подпись переведена.
///
/// Правила раздела `tab` описаны в `docs/plugins.md` самого RustGIS.
/// Инструмент своего значка не имеет и в ленте выглядит сундуком,
/// поэтому инструменты стоят в меню, а значки стоят на меню и на команде.
#[test]
fn ribbon_tab_holds_every_tool() {
    let path = root().join("plugin.json");
    let m: J = serde_json::from_slice(&std::fs::read(&path).expect("plugin.json")).expect("json");
    let tab = &m["tab"];
    let ru = &m["i18n"]["ru"];
    let says = |s: &str| {
        assert!(
            ru[s].is_string(),
            "подпись вкладки без перевода: {s}"
        );
        assert!(!s.contains('\n'), "подпись вкладки в одну строку: {s}");
        assert!((1..=32).contains(&s.chars().count()) || s.ends_with('.'), "длина подписи: {s}");
    };
    // Имя вкладки не переводится, оно же имя продукта.
    assert_eq!(tab["label"], json!("Topoliner"));
    let groups = tab["groups"].as_array().expect("groups");
    assert!((1..=8).contains(&groups.len()), "групп: {}", groups.len());
    // Собираем всё, на что вкладка ссылается.
    let mut named: Vec<String> = Vec::new();
    fn walk(item: &J, named: &mut Vec<String>, says: &dyn Fn(&str)) {
        match item.get("menu").and_then(J::as_str) {
            Some(label) => {
                says(label);
                let icon = item["icon"].as_str().expect("меню без значка");
                assert!(!icon.is_empty(), "меню без значка: {label}");
                says(item["help"].as_str().expect("меню без подсказки"));
                let items = item["items"].as_array().expect("меню без пунктов");
                assert!((1..=32).contains(&items.len()), "пунктов меню: {label}");
                for it in items {
                    walk(it, named, says);
                }
            }
            None => named.push(item.as_str().expect("пункт - строка").to_string()),
        }
    }
    for g in groups {
        says(g["label"].as_str().expect("группа без подписи"));
        let items = g["items"].as_array().expect("группа без пунктов");
        assert!(!items.is_empty() && items.len() <= 48, "пунктов группы");
        for it in items {
            walk(it, &mut named, &says);
        }
    }
    // Каждый инструмент продукта стоит во вкладке ровно один раз.
    let mut tools: Vec<String> = named
        .iter()
        .filter_map(|n| n.strip_prefix("tool:").map(str::to_string))
        .collect();
    tools.sort();
    let mut offered: Vec<String> = OPTIONS.offered.iter().map(|s| (*s).to_string()).collect();
    offered.sort();
    assert_eq!(tools, offered, "вкладка и состав продукта разошлись");
    // Остальные пункты - команды манифеста, чужого во вкладке нет.
    let commands: Vec<&str> = m["commands"]
        .as_array()
        .expect("commands")
        .iter()
        .filter_map(|c| c["id"].as_str())
        .collect();
    for n in &named {
        if n.starts_with("tool:") {
            continue;
        }
        assert!(commands.contains(&n.as_str()), "вкладка зовёт чужое: {n}");
    }
}
