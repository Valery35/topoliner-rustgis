// Topoliner для RustGIS - расширение-процесс RustGIS.
// Copyright (C) 2026 ООО «Информ++». Лицензия GPL-3.0-or-later.
// SPDX-License-Identifier: GPL-3.0-or-later

//! Сторожа на гарантии продукта.
//!
//! Прогоны модуля в `fixtures/tool_runs.json` мелкие, по две-пять фигур.
//! Они доказывают совпадение расчёта, но не проверяют обещания, которые
//! продукт даёт на любых данных. Здесь покрытие крупнее, двадцать пять
//! полигонов с разладкой вершин, и проверяются сами обещания.
//!
//! Каждое обещание взято из раздела решений `AGENTS.md` модуля
//! и названо в заголовке теста.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use rustgis_kit::Lang;
use topoliner_rustgis::arcs::{Parts, Shape};
use topoliner_rustgis::audit::{SEVERITY_AUTO, Settings as AuditSettings};
use topoliner_rustgis::rings::Pt;
use topoliner_rustgis::simplify::{Method, SimplifySettings};
use topoliner_rustgis::snap::{Mode, Nodes, Settings as SnapSettings};
use topoliner_rustgis::topo_fix::{FixSettings, Winner};
use topoliner_rustgis::{arcs, audit, backend, coverage, cut, simplify, snap, topo_fix};

/// Сторона клетки покрытия.
const CELL: f64 = 100.0;

/// Разладка вершины: соседи нарисованы с расхождением до этой величины.
const JITTER: f64 = 0.4;

/// Покрытие пять на пять с разладкой вершин по общим границам.
///
/// Разладка невелика и лежит ниже допуска, то есть это именно тот мусор,
/// который инструменты обязаны убирать молча.
fn coverage_grid() -> Vec<Shape> {
    grid(5)
}

/// То же покрытие заданного размера.
fn grid(n: i64) -> Vec<Shape> {
    // Простой повторяемый генератор: своё зерно, без зависимостей.
    let mut seed: u64 = 12345;
    let mut noise = move || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let x = ((seed >> 33) as f64) / ((1u64 << 31) as f64);
        (x - 0.5) * 2.0 * JITTER
    };
    let mut out = Vec::new();
    for row in 0..n {
        for col in 0..n {
            let (x, y) = (col as f64 * CELL, row as f64 * CELL);
            // Края покрытия ровные, внутренние границы разлажены.
            let mut shift = |v: f64, edge: bool| if edge { v } else { v + noise() };
            let x0 = shift(x, col == 0);
            let y0 = shift(y, row == 0);
            let x1 = shift(x + CELL, col == n - 1);
            let y1 = shift(y + CELL, row == n - 1);
            let ring: Vec<Pt> = vec![
                [x0, y0],
                [x0 + CELL / 2.0, y0],
                [x1, y0],
                [x1, y1],
                [x0, y1],
                [x0, y0],
            ];
            out.push((row * n + col + 1, vec![vec![ring]]));
        }
    }
    out
}

fn area_of(shapes: &[Parts]) -> f64 {
    shapes
        .iter()
        .map(|p| backend::area(&backend::geom_of(p)))
        .sum()
}

fn vertices_of(shapes: &[Parts]) -> usize {
    shapes.iter().flatten().flatten().map(Vec::len).sum()
}

fn fix_settings(tolerance: f64) -> FixSettings {
    FixSettings {
        tolerance,
        area_threshold: 1.0,
        snap: true,
        fix_invalid: true,
        resolve_overlaps: true,
        fill_gaps: true,
        drop_tiny_features: false,
        protect_narrow: true,
        spike_angle: 1.0,
        cavity_area: 0.0,
        winner: Winner::Larger,
        max_area_loss: 0.25,
    }
}

/// Вставка узлов не меняет ни форму, ни площадь.
#[test]
fn inserting_nodes_keeps_the_area() {
    let items = coverage_grid();
    let before: Vec<Parts> = items.iter().map(|(_f, p)| p.clone()).collect();
    let mut rings: Vec<Vec<Pt>> = Vec::new();
    for parts in &before {
        for part in parts {
            rings.extend(part.iter().cloned());
        }
    }
    let done = snap::insert_nodes(
        &rings,
        Nodes {
            tolerance: 1.0,
            crossings: true,
            project_onto_edge: true,
        },
    );
    assert!(done.stats.nodes_inserted > 0, "узлов вставлять нечего");
    let after: Vec<Parts> = done
        .rings
        .iter()
        .map(|r| vec![vec![r.clone().unwrap_or_default()]])
        .collect();
    let (a, b) = (area_of(&before), area_of(&after));
    // Узел ставится в проекцию точки на ребро, то есть на прямую ребра.
    assert!((a - b).abs() / a < 1e-12, "площадь {a} против {b}");
    assert!(vertices_of(&after) > vertices_of(&before));
}

/// Вершина не смещается дальше допуска.
#[test]
fn snapping_never_moves_a_vertex_past_the_tolerance() {
    let tolerance = 2.0;
    let items = coverage_grid();
    let mut rings: Vec<Vec<Pt>> = Vec::new();
    for (_fid, parts) in &items {
        for part in parts {
            rings.extend(part.iter().cloned());
        }
    }
    let done = snap::clean(
        &rings,
        &SnapSettings {
            nodes: Nodes {
                tolerance,
                crossings: true,
                project_onto_edge: false,
            },
            mode: Mode::Both,
            frozen: Vec::new(),
            fixed: Vec::new(),
        },
    );
    assert!(done.stats.vertices_moved > 0, "сводить нечего");
    assert!(
        done.stats.max_move <= tolerance,
        "смещение {} при допуске {tolerance}",
        done.stats.max_move
    );
    for e in &done.events {
        assert!(e.dist <= tolerance, "{e:?}");
    }
}

/// Повторный запуск очистки по результату ничего не меняет.
#[test]
fn cleanup_is_idempotent() {
    let items = coverage_grid();
    let s = fix_settings(2.0);
    let first = topo_fix::fix_items(&items, &s);
    let again: Vec<Shape> = items
        .iter()
        .zip(&first.shapes)
        .map(|((fid, _p), parts)| (*fid, parts.clone()))
        .collect();
    let second = topo_fix::fix_items(&again, &s);
    assert_eq!(
        vertices_of(&first.shapes),
        vertices_of(&second.shapes),
        "второй проход изменил количество вершин"
    );
    let (a, b) = (area_of(&first.shapes), area_of(&second.shapes));
    assert!((a - b).abs() / a < 1e-9, "площадь {a} против {b}");
    assert_eq!(second.stats.vertices_moved, 0, "второй проход двигал вершины");
    assert_eq!(second.stats.nodes_inserted, 0, "второй проход вставлял узлы");
    assert_eq!(second.stats.overlaps_fixed, 0);
    assert_eq!(second.stats.gaps_filled, 0);
}

/// После очистки проверка не находит того, что чинится молча.
#[test]
fn cleanup_leaves_no_auto_findings() {
    let items = coverage_grid();
    let s = fix_settings(2.0);
    let fixed = topo_fix::fix_items(&items, &s);
    let checked: Vec<topoliner_rustgis::audit::Item> = items
        .iter()
        .zip(&fixed.shapes)
        .map(|((fid, _p), parts)| topoliner_rustgis::audit::Item {
            fid: *fid,
            geom: backend::geom_of(parts),
            parts: parts.clone(),
        })
        .collect();
    let findings = audit::check_items(
        &checked,
        &AuditSettings {
            tolerance: 2.0,
            area_threshold: 1.0,
            do_overlaps: true,
            do_gaps: true,
            do_unsnapped: true,
            cavity_area: 0.0,
        },
    );
    let auto: Vec<&audit::Finding> = findings
        .iter()
        .filter(|f| f.severity == SEVERITY_AUTO)
        .collect();
    for f in &auto {
        eprintln!("{} {:?} {}", f.kind.code(), f.xy, f.note.get(Lang::Ru));
    }
    assert!(auto.is_empty(), "осталось находок разряда auto: {}", auto.len());
}

/// Общая граница прореживается один раз, и соседям достаётся одна дуга.
#[test]
fn a_shared_border_is_thinned_once() {
    let items = coverage_grid();
    let s = fix_settings(2.0);
    // Упрощение работает на согласованном покрытии, поэтому сначала чистка.
    let clean: Vec<Shape> = items
        .iter()
        .zip(&topo_fix::fix_items(&items, &s).shapes)
        .map(|((fid, _p), parts)| (*fid, parts.clone()))
        .collect();
    let flat = simplify::flatten(&clean, true);
    let done = simplify::simplify_topology(
        &flat.rings,
        &flat.closed,
        &SimplifySettings {
            tolerance: 5.0,
            grid: 1e-6,
            min_points: 0,
            smooth: 0,
            method: Method::Douglas,
        },
    );
    assert!(done.stats.arcs_shared > 0, "общих дуг нет");
    assert!(done.stats.vertices_out < done.stats.vertices_in, "не проредило");
    let after = simplify::rebuild(&clean, &flat, &done.rings);
    // Модель покрытия по итогу: общая граница обязана быть одной дугой
    // с двумя соседями, а не двумя дугами рядом.
    let shapes: Vec<Shape> = clean
        .iter()
        .zip(&after)
        .map(|((fid, _p), parts)| (*fid, parts.clone()))
        .collect();
    let model = coverage::build_coverage(&shapes, 1e-6, 1e-6);
    let shared = model.arcs.iter().filter(|a| a.right.is_some()).count();
    assert_eq!(
        shared,
        done.stats.arcs_shared,
        "после упрощения общих границ стало другое количество"
    );
}

/// Кромка разреза приходит из одного наложения и достаётся обеим
/// сторонам одинаковой.
#[test]
fn the_cut_edge_is_one_arc() {
    let items = coverage_grid();
    let s = fix_settings(2.0);
    let clean: Vec<Shape> = items
        .iter()
        .zip(&topo_fix::fix_items(&items, &s).shapes)
        .map(|((fid, _p), parts)| (*fid, parts.clone()))
        .collect();
    // Контур поперёк общей границы двух средних клеток.
    let cut: Parts = vec![vec![vec![
        [230.0, 180.0],
        [270.0, 180.0],
        [270.0, 220.0],
        [230.0, 220.0],
        [230.0, 180.0],
    ]]];
    let done = cut::cut_into_coverage(&clean, &cut, 1.0, 1e-6, cut::Mode::Overlay);
    assert!(!done.donors.is_empty(), "контур никого не задел");
    assert!(!done.created.is_empty());

    let mut shapes: Vec<Shape> = clean
        .iter()
        .map(|(fid, parts)| {
            let parts = done
                .changed
                .get(fid)
                .or_else(|| done.noded.get(fid))
                .cloned()
                .unwrap_or_else(|| parts.clone());
            (*fid, parts)
        })
        .filter(|(_f, p)| !p.is_empty())
        .collect();
    shapes.push((-1, done.created.clone()));

    let model = coverage::build_coverage(&shapes, 1e-6, 1e-6);
    // Новый объект стоит в покрытии как все: каждая его дуга либо общая
    // с соседом, либо лежит на внешнем крае покрытия.
    let mine: Vec<&coverage::Arc> = model
        .arcs
        .iter()
        .filter(|a| a.left == Some(-1) || a.right == Some(-1))
        .collect();
    assert!(!mine.is_empty(), "нового объекта в модели нет");
    let shared = mine.iter().filter(|a| a.right.is_some()).count();
    assert!(shared > 0, "кромка не стала общей дугой");
    // Двух дуг рядом быть не должно: у общей кромки ровно два соседа.
    for a in &mine {
        assert!(a.left != a.right, "дуга сама с собой: {a:?}");
    }
    // Висячих узлов внутри покрытия нет.
    let dangles = model.nodes.iter().filter(|n| n.degree == 1).count();
    assert_eq!(dangles, 0, "в модели остались висячие узлы");
}

/// Те же обещания на покрытии в четыреста полигонов.
///
/// Идёт не каждый прогон: он минуты, а не секунды. Запуск -
/// `cargo test -- --ignored`.
#[test]
#[ignore = "долгий, для проверки на объёме"]
fn a_bigger_coverage_holds_the_guarantees() {
    let items = grid(20);
    let s = fix_settings(2.0);
    let before = area_of(&items.iter().map(|(_f, p)| p.clone()).collect::<Vec<Parts>>());
    let fixed = topo_fix::fix_items(&items, &s);
    assert_eq!(fixed.stats.features_lost, 0, "объекты потерялись");
    let after = area_of(&fixed.shapes);
    // Сшивка перераспределяет площадь, но не создаёт её из ничего.
    assert!(
        (after - before).abs() / before < 0.01,
        "площадь {before} против {after}"
    );
    let checked: Vec<topoliner_rustgis::audit::Item> = items
        .iter()
        .zip(&fixed.shapes)
        .map(|((fid, _p), parts)| topoliner_rustgis::audit::Item {
            fid: *fid,
            geom: backend::geom_of(parts),
            parts: parts.clone(),
        })
        .collect();
    let findings = audit::check_items(
        &checked,
        &AuditSettings {
            tolerance: 2.0,
            area_threshold: 1.0,
            do_overlaps: true,
            do_gaps: true,
            do_unsnapped: true,
            cavity_area: 0.0,
        },
    );
    let auto = findings
        .iter()
        .filter(|f| f.severity == SEVERITY_AUTO)
        .count();
    assert_eq!(auto, 0, "осталось находок разряда auto: {auto}");
}

/// Разбор покрытия на дуги и сборка обратно не теряет объекты.
#[test]
fn the_coverage_model_covers_every_feature() {
    let items = coverage_grid();
    let s = fix_settings(2.0);
    let clean: Vec<Shape> = items
        .iter()
        .zip(&topo_fix::fix_items(&items, &s).shapes)
        .map(|((fid, _p), parts)| (*fid, parts.clone()))
        .collect();
    let model = coverage::build_coverage(&clean, 1e-6, 1e-6);
    assert_eq!(model.owners.len(), clean.len(), "кольца потерялись");
    assert_eq!(model.rings.len(), clean.len());
    for path in &model.rings {
        assert!(!path.is_empty(), "кольцо без дуг");
    }
    // Каждая дуга кому-то принадлежит.
    for a in &model.arcs {
        assert!(a.left.is_some(), "дуга без соседа слева: {a:?}");
    }
    // Разбор на границы даёт столько же общих дуг, сколько модель.
    let borders = topoliner_rustgis::boundaries::extract_boundaries(&clean, 1e-6, 1e-6);
    let shared_borders = borders
        .iter()
        .filter(|b| b.kind == topoliner_rustgis::boundaries::Kind::Shared)
        .count();
    let shared_arcs = model.arcs.iter().filter(|a| a.right.is_some()).count();
    assert_eq!(shared_borders, shared_arcs);
    let _ = arcs::key_of([0.0, 0.0], 1e-6);
}
