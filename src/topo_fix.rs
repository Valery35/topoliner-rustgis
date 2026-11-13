// Topoliner для RustGIS - расширение-процесс RustGIS.
// Copyright (C) 2026 ООО «Информ++». Лицензия GPL-3.0-or-later.
// SPDX-License-Identifier: GPL-3.0-or-later

//! 1.03 Очистка топологии полигонов. Перенос `topo_checks.fix_items`
//! модуля QGIS вместе с тем, что делает поверх ядра диалог
//! `TopologyFixAlgorithm`.
//!
//! Порядок шагов существенен. Мусор исправляется молча, возможный смысл
//! уходит в слой оставшихся проблем. Широкие перекрытия не вычитаются,
//! крупные щели не заполняются, дубликаты и вложенные объекты
//! не разрешаются ни при каких настройках.

use rustgis_kit::Lang;

use crate::arcs::Parts;
use crate::audit::{Finding, Kind, SEVERITY_REVIEW, Text};
use crate::backend::{self, Geom};
use crate::rings::{self, Pt, ring_area};
use crate::rounding::fmt;
use crate::snap::{self, Mode, Nodes, Settings as SnapSettings};

/// Площадь, ниже которой пересечение считается пустым.
const AREA_EPS: f64 = 1e-9;

/// Кто уступает при мусорном перекрытии.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Winner {
    /// Площадь остаётся у более крупного объекта.
    Larger,
    /// Площадь остаётся у объекта с меньшим номером.
    First,
}

/// Настройки очистки.
pub struct FixSettings {
    pub tolerance: f64,
    pub area_threshold: f64,
    pub snap: bool,
    pub fix_invalid: bool,
    pub resolve_overlaps: bool,
    pub fill_gaps: bool,
    pub drop_tiny_features: bool,
    pub protect_narrow: bool,
    pub spike_angle: f64,
    pub cavity_area: f64,
    pub winner: Winner,
    /// Доля площади, потеря которой отменяет правку.
    pub max_area_loss: f64,
}

/// Счётчики очистки, как их печатает диалог модуля.
#[derive(Default)]
pub struct FixStats {
    pub dup_vertices: usize,
    pub spikes: usize,
    pub vertices_moved: usize,
    pub nodes_inserted: usize,
    pub tiny_parts: usize,
    pub tiny_holes: usize,
    pub made_valid: usize,
    pub valid_rejected: usize,
    pub overlaps_fixed: usize,
    pub overlaps_left: usize,
    pub gaps_filled: usize,
    pub gaps_left: usize,
    pub tiny_features_dropped: usize,
    pub features_lost: usize,
    pub rings_frozen: usize,
    pub max_move: f64,
    pub area_before: f64,
    pub area_after: f64,
}

/// Итог очистки.
pub struct Fixed {
    /// Геометрия по объектам входа, пустые части - объект исчез.
    pub shapes: Vec<Parts>,
    pub stats: FixStats,
    pub left: Vec<Finding>,
}

/// Мусорное ли перекрытие. Решает эффективная ширина, а не площадь.
fn overlap_is_debris(inter: &Geom, area_threshold: f64, tolerance: f64) -> bool {
    let area = backend::area(inter);
    if area < area_threshold {
        return true;
    }
    let perimeter = backend::boundary_length(inter);
    if perimeter <= 0.0 {
        return false;
    }
    (2.0 * area / perimeter) < tolerance
}

/// Точка для показа находки.
fn safe_point(g: &Geom) -> Option<Pt> {
    if backend::is_empty(g) {
        return None;
    }
    if let Some(p) = backend::point_on_surface(g) {
        return Some(p);
    }
    let (x0, y0, x1, y1) = backend::bounds(g)?;
    Some([0.5 * (x0 + x1), 0.5 * (y0 + y1)])
}

/// Геометрия из частей.
fn geom_of(parts: &Parts) -> Geom {
    backend::geom_of(parts)
}

/// Части из геометрии: внешнее кольцо и дыры, вершины без повтора первой.
fn to_parts(g: &Geom) -> Parts {
    backend::parts(g)
        .iter()
        .map(|p| {
            backend::rings_of(p)
                .into_iter()
                .filter_map(|mut r| {
                    if r.len() >= 2 && r[0] == r[r.len() - 1] {
                        r.pop();
                    }
                    (r.len() >= 3).then_some(r)
                })
                .collect::<Vec<Vec<Pt>>>()
        })
        .filter(|rings: &Vec<Vec<Pt>>| !rings.is_empty())
        .collect()
}

/// Очистка покрытия. Порядок шагов существенен.
pub fn fix_items(items: &[(i64, Parts)], s: &FixSettings) -> Fixed {
    let fids: Vec<i64> = items.iter().map(|(f, _)| *f).collect();
    let source: Vec<Geom> = items.iter().map(|(_f, p)| geom_of(p)).collect();
    let mut stats = FixStats {
        area_before: source.iter().map(backend::area).sum(),
        ..FixStats::default()
    };
    let mut left: Vec<Finding> = Vec::new();

    // ── Шаг 1. Разбор и артефакты вершин ────────────────────────────────
    let clean_vertices = |parts: Parts, stats: &mut FixStats| -> Parts {
        let mut out: Parts = Vec::new();
        for rings in parts {
            let mut new_rings: Vec<Vec<Pt>> = Vec::new();
            let mut broken = false;
            for (k, ring) in rings.iter().enumerate() {
                let (ring, dups) = rings::drop_repeated_vertices(ring, true, 1e-9);
                stats.dup_vertices += dups;
                let (ring, spikes) = rings::remove_spikes(&ring, true, s.spike_angle);
                stats.spikes += spikes;
                if ring.len() >= 3 {
                    new_rings.push(ring);
                } else if k == 0 {
                    // Часть без внешнего кольца собрать нельзя.
                    broken = true;
                    break;
                }
            }
            if !broken && !new_rings.is_empty() {
                out.push(new_rings);
            }
        }
        out
    };
    let drop_tiny = |parts: Parts, stats: &mut FixStats| -> Parts {
        let mut parts = parts;
        if parts.len() > 1 {
            let keep: Parts = parts
                .iter()
                .filter(|r| {
                    r.first()
                        .is_some_and(|ext| ring_area(ext).abs() >= s.area_threshold)
                })
                .cloned()
                .collect();
            if !keep.is_empty() && keep.len() < parts.len() {
                stats.tiny_parts += parts.len() - keep.len();
                parts = keep;
            }
        }
        for rings in parts.iter_mut() {
            let holes: Vec<Vec<Pt>> = rings[1..]
                .iter()
                .filter(|h| ring_area(h).abs() >= s.area_threshold)
                .cloned()
                .collect();
            if holes.len() != rings.len() - 1 {
                stats.tiny_holes += (rings.len() - 1) - holes.len();
                rings.truncate(1);
                rings.extend(holes);
            }
        }
        parts
    };

    // Кольца внутри идут без повтора первой вершины, как их отдаёт
    // разбор геометрии в модуле.
    let open = |parts: &Parts| -> Parts {
        parts
            .iter()
            .map(|rings| {
                rings
                    .iter()
                    .filter_map(|r| {
                        let mut r = r.clone();
                        if r.len() >= 2 && r[0] == r[r.len() - 1] {
                            r.pop();
                        }
                        (r.len() >= 3).then_some(r)
                    })
                    .collect::<Vec<Vec<Pt>>>()
            })
            .filter(|rings: &Vec<Vec<Pt>>| !rings.is_empty())
            .collect()
    };
    let mut all_parts: Vec<Parts> = items
        .iter()
        .map(|(_f, p)| clean_vertices(open(p), &mut stats))
        .collect();

    // ── Шаг 2. Микрочасти и микродыры ───────────────────────────────────
    for parts in all_parts.iter_mut() {
        *parts = drop_tiny(std::mem::take(parts), &mut stats);
    }

    // ── Шаг 2а. Корректность до сшивки ──────────────────────────────────
    // Сшивка двигает вершины и на кольцах, идущих друг по другу, ломает
    // то, что до неё чинилось без потери площади.
    if s.fix_invalid {
        for parts in all_parts.iter_mut() {
            if parts.is_empty() {
                continue;
            }
            let g = geom_of(parts);
            if backend::is_empty(&g) || backend::is_valid(&g) {
                continue;
            }
            let before = backend::area(&g);
            let fixed = backend::make_valid(&g);
            if backend::is_empty(&fixed) {
                continue;
            }
            let after = backend::area(&fixed);
            let loss = if before <= 0.0 {
                0.0
            } else {
                (before - after).abs() / before
            };
            if loss <= s.max_area_loss {
                // Исправление само оставляет иглы и лоскуты на месте
                // бывших самопересечений, поэтому чистка повторяется.
                let cleaned = clean_vertices(to_parts(&fixed), &mut stats);
                *parts = drop_tiny(cleaned, &mut stats);
                stats.made_valid += 1;
            }
        }
    }

    // ── Шаг 3. Сшивка ───────────────────────────────────────────────────
    if s.snap {
        let mut flat: Vec<Vec<Pt>> = Vec::new();
        let mut addr: Vec<(usize, usize, usize)> = Vec::new();
        for (i, parts) in all_parts.iter().enumerate() {
            for (j, rings) in parts.iter().enumerate() {
                for (k, ring) in rings.iter().enumerate() {
                    let mut closed = ring.clone();
                    if let Some(&first) = ring.first() {
                        closed.push(first);
                    }
                    flat.push(closed);
                    addr.push((i, j, k));
                }
            }
        }
        if !flat.is_empty() {
            // Крупные кольца обходятся первыми и притягивают мелкие.
            let mut order: Vec<usize> = (0..flat.len()).collect();
            order.sort_by(|a, b| {
                ring_area(&flat[*b])
                    .abs()
                    .partial_cmp(&ring_area(&flat[*a]).abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let frozen: Vec<usize> = if s.protect_narrow {
                order
                    .iter()
                    .enumerate()
                    .filter(|(_pos, t)| rings::ring_width(&flat[**t]) < s.tolerance)
                    .map(|(pos, _)| pos)
                    .collect()
            } else {
                Vec::new()
            };
            stats.rings_frozen = frozen.len();
            let ordered: Vec<Vec<Pt>> = order.iter().map(|&t| flat[t].clone()).collect();
            let done = snap::clean(
                &ordered,
                &SnapSettings {
                    nodes: Nodes {
                        tolerance: s.tolerance,
                        crossings: true,
                        project_onto_edge: false,
                    },
                    mode: Mode::Both,
                    frozen,
                    fixed: Vec::new(),
                },
            );
            stats.vertices_moved = done.stats.vertices_moved;
            stats.nodes_inserted = done.stats.nodes_inserted;
            stats.max_move = done.stats.max_move;
            let mut new_flat: Vec<Option<Vec<Pt>>> = vec![None; flat.len()];
            for (pos, &t) in order.iter().enumerate() {
                new_flat[t] = done.rings.get(pos).cloned().flatten().map(|mut r| {
                    // Кольцо приходит замкнутым, внутри работаем без замыкания.
                    if r.len() >= 2 && r[0] == r[r.len() - 1] {
                        r.pop();
                    }
                    r
                });
            }
            // Кольца раскладываются обратно по частям, выпавшие убираются.
            let mut rebuilt: Vec<Parts> = all_parts
                .iter()
                .map(|parts| vec![Vec::new(); parts.len()])
                .collect();
            for (t, &(i, j, _k)) in addr.iter().enumerate() {
                if let Some(ring) = new_flat[t].clone() {
                    rebuilt[i][j].push(ring);
                }
            }
            for parts in rebuilt.iter_mut() {
                parts.retain(|rings| !rings.is_empty());
            }
            all_parts = rebuilt;
        }
    }

    // ── Шаг 4. Сборка и корректность ────────────────────────────────────
    let mut out: Vec<Option<Geom>> = Vec::with_capacity(all_parts.len());
    for (i, parts) in all_parts.iter().enumerate() {
        let mut g = geom_of(parts);
        if backend::is_empty(&g) {
            out.push(None);
            continue;
        }
        if s.fix_invalid && !backend::is_valid(&g) {
            let before = backend::area(&g);
            let fixed = backend::make_valid(&g);
            // Лоскуты от разрешения самопересечений отбрасываются.
            let keep: Vec<Vec<Vec<Pt>>> = to_parts(&fixed)
                .into_iter()
                .filter(|p| backend::area(&geom_of(&vec![p.clone()])) >= s.area_threshold)
                .collect();
            let fixed = if keep.is_empty() {
                fixed
            } else {
                geom_of(&keep)
            };
            let after = backend::area(&fixed);
            let loss = if before <= 0.0 {
                0.0
            } else {
                (before - after).abs() / before
            };
            if backend::is_empty(&fixed) || loss > s.max_area_loss {
                // Результат не должен быть хуже входа.
                stats.valid_rejected += 1;
                if !backend::is_empty(&source[i]) {
                    g = source[i].clone();
                }
                left.push(
                    Finding::new(
                        Kind::Invalid,
                        SEVERITY_REVIEW,
                        Text::of(
                            "сшивка испортила объект, исправление не помогло, возвращена исходная геометрия",
                            "snapping spoiled the object, the repair did not help, the original geometry was restored",
                        ),
                    )
                    .of(fids[i])
                    .value(loss)
                    .at(safe_point(&g)),
                );
            } else {
                g = fixed;
                stats.made_valid += 1;
            }
        }
        out.push(Some(g));
    }

    // ── Шаг 5. Микрообъекты ─────────────────────────────────────────────
    for i in 0..out.len() {
        let Some(g) = out[i].clone() else { continue };
        if backend::area(&g) < s.area_threshold {
            if s.drop_tiny_features {
                out[i] = None;
                stats.tiny_features_dropped += 1;
            } else {
                left.push(
                    Finding::new(
                        Kind::TinyFeature,
                        SEVERITY_REVIEW,
                        Text::of(
                            "объект мельче порога, удаление не выполнялось",
                            "the object is smaller than the threshold, no deletion was performed",
                        ),
                    )
                    .of(fids[i])
                    .value(backend::area(&g))
                    .at(safe_point(&g)),
                );
            }
        }
    }

    // ── Шаг 6. Перекрытия ───────────────────────────────────────────────
    if s.resolve_overlaps {
        let live: Vec<usize> = (0..out.len())
            .filter(|&i| out[i].as_ref().is_some_and(|g| !backend::is_empty(g)))
            .collect();
        for (at, &i) in live.iter().enumerate() {
            for &j in live.iter().skip(at + 1) {
                let (Some(a), Some(b)) = (out[i].clone(), out[j].clone()) else {
                    continue;
                };
                if !backend::intersects(&a, &b) {
                    continue;
                }
                let inter = backend::intersection(&a, &b);
                let area = backend::area(&inter);
                if area <= AREA_EPS {
                    continue;
                }
                let same = backend::equals(&a, &b);
                if same || backend::contains(&a, &b) || backend::contains(&b, &a) {
                    stats.overlaps_left += 1;
                    left.push(
                        Finding::new(
                            if same { Kind::Duplicate } else { Kind::Nested },
                            SEVERITY_REVIEW,
                            Text::of(
                                "совпадение или вложение решается человеком",
                                "coincidence or nesting is decided by the operator",
                            ),
                        )
                        .pair(fids[i], fids[j])
                        .value(area)
                        .at(safe_point(&inter)),
                    );
                    continue;
                }
                if !overlap_is_debris(&inter, s.area_threshold, s.tolerance) {
                    stats.overlaps_left += 1;
                    left.push(
                        Finding::new(
                            Kind::Overlap,
                            SEVERITY_REVIEW,
                            Text::of(
                                "перекрытие шире допуска, это спор за площадь",
                                "the overlap is wider than the tolerance, a dispute over area",
                            ),
                        )
                        .pair(fids[i], fids[j])
                        .value(area)
                        .at(safe_point(&inter)),
                    );
                    continue;
                }
                // Мусорное перекрытие: вычитаем у проигравшего.
                let loser = match s.winner {
                    Winner::First => {
                        if fids[i] <= fids[j] {
                            j
                        } else {
                            i
                        }
                    }
                    Winner::Larger => {
                        if backend::area(&a) >= backend::area(&b) {
                            j
                        } else {
                            i
                        }
                    }
                };
                let keeper = if loser == j { i } else { j };
                let (Some(lg), Some(kg)) = (out[loser].clone(), out[keeper].clone()) else {
                    continue;
                };
                let cut = backend::difference(&lg, &kg);
                let before = backend::area(&lg);
                if backend::is_empty(&cut)
                    || (before > 0.0 && (before - backend::area(&cut)) / before > s.max_area_loss)
                {
                    stats.overlaps_left += 1;
                    left.push(
                        Finding::new(
                            Kind::Overlap,
                            SEVERITY_REVIEW,
                            Text::of(
                                "вычитание съедало слишком много площади",
                                "the subtraction was taking too much area",
                            ),
                        )
                        .pair(fids[i], fids[j])
                        .value(area)
                        .at(safe_point(&inter)),
                    );
                    continue;
                }
                let keep: Vec<Vec<Vec<Pt>>> = to_parts(&cut)
                    .into_iter()
                    .filter(|p| backend::area(&geom_of(&vec![p.clone()])) >= s.area_threshold)
                    .collect();
                out[loser] = Some(if keep.is_empty() { cut } else { geom_of(&keep) });
                stats.overlaps_fixed += 1;
            }
        }
    }

    // ── Шаг 7. Щели ─────────────────────────────────────────────────────
    if s.fill_gaps {
        let live: Vec<usize> = (0..out.len())
            .filter(|&i| out[i].as_ref().is_some_and(|g| !backend::is_empty(g)))
            .collect();
        if !live.is_empty() {
            let merged = backend::union_all(
                &live
                    .iter()
                    .filter_map(|&i| out[i].clone())
                    .collect::<Vec<Geom>>(),
            );
            for poly in backend::parts(&merged) {
                let rs = backend::rings_of(&poly);
                for inner in &rs[1..] {
                    let hole = backend::geom_of(&[vec![inner.clone()]]);
                    let area = backend::area(&hole);
                    if s.cavity_area > 0.0 && area >= s.cavity_area {
                        continue;
                    }
                    if area >= s.area_threshold {
                        stats.gaps_left += 1;
                        left.push(
                            Finding::new(
                                Kind::Gap,
                                SEVERITY_REVIEW,
                                Text::of(
                                    "щель крупнее порога, не заполнялась",
                                    "the gap is larger than the threshold, it was not filled",
                                ),
                            )
                            .value(area)
                            .at(safe_point(&hole)),
                        );
                        continue;
                    }
                    // Щель отходит соседу с наибольшей общей границей.
                    let mut best: Option<usize> = None;
                    let mut best_len = 0.0;
                    for &i in &live {
                        let Some(g) = out[i].as_ref() else { continue };
                        if backend::is_empty(g) || !backend::intersects(g, &hole) {
                            continue;
                        }
                        let shared = backend::shared_boundary(&hole, g);
                        if shared > best_len {
                            best_len = shared;
                            best = Some(i);
                        }
                    }
                    let Some(best) = best else {
                        stats.gaps_left += 1;
                        left.push(
                            Finding::new(
                                Kind::Gap,
                                SEVERITY_REVIEW,
                                Text::of("сосед не найден", "no neighbour found"),
                            )
                            .value(area)
                            .at(safe_point(&hole)),
                        );
                        continue;
                    };
                    let Some(g) = out[best].clone() else { continue };
                    let merged_geom = backend::union_all(&[g, hole]);
                    if backend::is_empty(&merged_geom) {
                        stats.gaps_left += 1;
                        continue;
                    }
                    out[best] = Some(merged_geom);
                    stats.gaps_filled += 1;
                }
            }
        }
    }

    // ── Перепроверка остатков ───────────────────────────────────────────
    // Нарушение могло исчезнуть позже, при обработке соседней пары.
    let by_fid: std::collections::HashMap<i64, Geom> = out
        .iter()
        .enumerate()
        .filter_map(|(i, g)| g.clone().map(|g| (fids[i], g)))
        .collect();
    left.retain(|item| {
        if item.kind != Kind::Overlap {
            return true;
        }
        let (Some(a), Some(b)) = (
            item.fid.and_then(|f| by_fid.get(&f)),
            item.fid_b.and_then(|f| by_fid.get(&f)),
        ) else {
            return true;
        };
        if !backend::intersects(a, b) {
            stats.overlaps_left = stats.overlaps_left.saturating_sub(1);
            return false;
        }
        if backend::area(&backend::intersection(a, b)) <= AREA_EPS {
            stats.overlaps_left = stats.overlaps_left.saturating_sub(1);
            return false;
        }
        true
    });

    // ── Итог ────────────────────────────────────────────────────────────
    let mut shapes: Vec<Parts> = Vec::with_capacity(out.len());
    for (i, g) in out.into_iter().enumerate() {
        match g.filter(|g| !backend::is_empty(g)) {
            None => {
                if !backend::is_empty(&source[i]) {
                    stats.features_lost += 1;
                    left.push(
                        Finding::new(
                            Kind::Lost,
                            SEVERITY_REVIEW,
                            Text::of(
                                "объект исчез при исправлении",
                                "the object disappeared during repair",
                            ),
                        )
                        .of(fids[i])
                        .at(safe_point(&source[i])),
                    );
                }
                shapes.push(Vec::new());
            }
            Some(g) => {
                stats.area_after += backend::area(&g);
                shapes.push(to_parts(&g));
            }
        }
    }
    Fixed {
        shapes,
        stats,
        left,
    }
}

/// Очистка по группам. Объекты разных групп не сшиваются между собой
/// и не спорят за площадь.
pub fn fix_grouped(items: &[(i64, Parts)], s: &FixSettings, keys: &[String]) -> Fixed {
    let mut groups: std::collections::BTreeMap<&str, Vec<usize>> =
        std::collections::BTreeMap::new();
    for (i, key) in keys.iter().enumerate() {
        groups.entry(key.as_str()).or_default().push(i);
    }
    let mut shapes: Vec<Parts> = vec![Vec::new(); items.len()];
    let mut stats = FixStats::default();
    let mut left: Vec<Finding> = Vec::new();
    for (key, members) in groups {
        let part: Vec<(i64, Parts)> = members.iter().map(|&i| items[i].clone()).collect();
        let done = fix_items(&part, s);
        for (at, &i) in members.iter().enumerate() {
            if let Some(parts) = done.shapes.get(at) {
                shapes[i] = parts.clone();
            }
        }
        for mut f in done.left {
            f.key = key.to_string();
            left.push(f);
        }
        let d = done.stats;
        stats.dup_vertices += d.dup_vertices;
        stats.spikes += d.spikes;
        stats.vertices_moved += d.vertices_moved;
        stats.nodes_inserted += d.nodes_inserted;
        stats.tiny_parts += d.tiny_parts;
        stats.tiny_holes += d.tiny_holes;
        stats.made_valid += d.made_valid;
        stats.valid_rejected += d.valid_rejected;
        stats.overlaps_fixed += d.overlaps_fixed;
        stats.overlaps_left += d.overlaps_left;
        stats.gaps_filled += d.gaps_filled;
        stats.gaps_left += d.gaps_left;
        stats.tiny_features_dropped += d.tiny_features_dropped;
        stats.features_lost += d.features_lost;
        stats.rings_frozen += d.rings_frozen;
        stats.max_move = stats.max_move.max(d.max_move);
        stats.area_before += d.area_before;
        stats.area_after += d.area_after;
    }
    Fixed {
        shapes,
        stats,
        left,
    }
}

fn pick(lang: Lang, ru: &str, en: &str) -> String {
    match lang {
        Lang::Ru => ru.to_string(),
        Lang::En => en.to_string(),
    }
}

/// Строки журнала очистки полигонов.
pub fn report(items: usize, written: usize, f: &Fixed, s: &FixSettings, lang: Lang) -> Vec<String> {
    let st = &f.stats;
    let one = |ru: &str, en: &str, n: usize| pick(lang, ru, en).replacen("%d", &n.to_string(), 1);
    let mut out = vec![
        pick(
            lang,
            "Объектов: %d, допуск %g, порог площади %g",
            "Objects: %d, tolerance %g, area threshold %g",
        )
        .replacen("%d", &items.to_string(), 1)
        .replacen("%g", &crate::audit::g_format(s.tolerance), 1)
        .replacen("%g", &crate::audit::g_format(s.area_threshold), 1),
        String::new(),
        pick(lang, "── Исправлено молча ──", "-- Fixed silently --"),
        one(
            "Повторяющихся вершин снято:  %d",
            "Repeated vertices removed:   %d",
            st.dup_vertices,
        ),
        one("Игл снято:                   %d", "Spikes removed:              %d", st.spikes),
        pick(
            lang,
            "Вершин сведено:              %d (макс. смещение %s)",
            "Vertices merged:             %d (max shift %s)",
        )
        .replacen("%d", &st.vertices_moved.to_string(), 1)
        .replacen("%s", &fmt(st.max_move), 1),
        one(
            "Узлов вставлено:             %d",
            "Nodes inserted:              %d",
            st.nodes_inserted,
        ),
        one(
            "Микрочастей удалено:         %d",
            "Tiny parts removed:          %d",
            st.tiny_parts,
        ),
        one(
            "Микродыр залито:             %d",
            "Tiny holes filled:           %d",
            st.tiny_holes,
        ),
    ];
    if st.rings_frozen > 0 {
        out.push(one(
            "Колец не изменялось:         %d (уже допуска)",
            "Rings left unchanged:        %d (narrower than the tolerance)",
            st.rings_frozen,
        ));
    }
    out.push(one(
        "Геометрий исправлено:        %d",
        "Geometries repaired:         %d",
        st.made_valid,
    ));
    out.push(one(
        "Перекрытий убрано:           %d",
        "Overlaps removed:            %d",
        st.overlaps_fixed,
    ));
    out.push(one(
        "Щелей заполнено:             %d",
        "Gaps filled:                 %d",
        st.gaps_filled,
    ));
    if st.tiny_features_dropped > 0 {
        out.push(one(
            "Микрообъектов удалено:       %d",
            "Tiny objects deleted:        %d",
            st.tiny_features_dropped,
        ));
    }
    out.push(String::new());
    out.push(pick(lang, "── Оставлено человеку ──", "-- Left to the operator --"));
    out.extend(crate::audit::summary_lines(&f.left, lang));
    let (before, after) = (st.area_before, st.area_after);
    let rel = if before != 0.0 {
        100.0 * (after - before) / before
    } else {
        0.0
    };
    out.push(String::new());
    out.push(
        pick(
            lang,
            "Площадь до/после: %s / %s (%+.3f %%)",
            "Area before/after: %s / %s (%+.3f %%)",
        )
        .replacen("%s", &fmt(before), 1)
        .replacen("%s", &fmt(after), 1)
        .replace("%+.3f", &format!("{rel:+.3}"))
        .replace("%%", "%"),
    );
    out.push(
        pick(lang, "Объектов на входе/выходе: %d / %d", "Objects in/out: %d / %d")
            .replacen("%d", &items.to_string(), 1)
            .replacen("%d", &written.to_string(), 1),
    );
    if st.features_lost > 0 {
        out.push(one(
            "Объектов исчезло: %d, см. слой оставшихся проблем",
            "Objects disappeared: %d, see the remaining problems layer",
            st.features_lost,
        ));
    }
    if st.valid_rejected > 0 {
        out.push(one(
            "Исправлений отменено из-за потери площади: %d",
            "Repairs cancelled due to area loss: %d",
            st.valid_rejected,
        ));
    }
    if rel.abs() > 1.0 {
        out.push(pick(
            lang,
            "Суммарная площадь изменилась более чем на процент. Проверьте пороги. Скорее всего порог площади завышен.",
            "The total area changed by more than one per cent. Check the thresholds. The area threshold is most likely too high.",
        ));
    }
    out
}
