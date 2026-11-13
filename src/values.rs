// Topoliner для RustGIS - расширение-процесс RustGIS.
// Copyright (C) 2026 ООО «Информ++». Лицензия GPL-3.0-or-later.
// SPDX-License-Identifier: GPL-3.0-or-later

//! Значения прогона модуля в объекты проверки.
//!
//! Поле-слой приходит списком объектов `{"id", "attrs", "polygons",
//! "parts"}`. Член `polygons` держит деление на части и дыры, и тогда
//! оно берётся готовым. Член `parts` это плоский список колец без такого
//! деления, и в нём вложенность восстанавливается по чётности. Кольцо
//! внутри нечётного количества других колец это дыра, остальные - внешние
//! кольца частей.
//!
//! Плоский список остаётся у прогонов модуля, снятых до крейта 0.1.3.

use serde_json::Value as J;

use crate::audit::Item;
use crate::backend;
use crate::rings::{EPS, Pt};

/// Кольцо из списка вершин: без повтора первой, короче трёх вершин
/// отброшено.
fn ring_of(part: &J) -> Option<Vec<Pt>> {
    let mut ring: Vec<Pt> = part
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| Some([p.get(0)?.as_f64()?, p.get(1)?.as_f64()?]))
        .collect();
    if ring.len() >= 2 {
        let (a, b) = (ring[0], ring[ring.len() - 1]);
        if (a[0] - b[0]).abs() < 1e-12 && (a[1] - b[1]).abs() < 1e-12 {
            ring.pop();
        }
    }
    (ring.len() >= 3).then_some(ring)
}

/// Кольца объекта плоским списком, без деления на части.
fn rings_of(feature: &J) -> Vec<Vec<Pt>> {
    feature["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(ring_of)
        .collect()
}

/// Части объекта, если слой принёс их готовыми.
fn polygons_of(feature: &J) -> Option<Vec<Vec<Vec<Pt>>>> {
    let list = feature.get("polygons")?.as_array()?;
    Some(
        list.iter()
            .map(|poly| {
                poly.as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(ring_of)
                    .collect::<Vec<Vec<Pt>>>()
            })
            .filter(|rings| !rings.is_empty())
            .collect(),
    )
}

/// Лежит ли точка внутри кольца (луч вправо).
fn point_in_ring(p: Pt, ring: &[Pt]) -> bool {
    let mut inside = false;
    let n = ring.len();
    for i in 0..n {
        let a = ring[i];
        let b = ring[(i + 1) % n];
        if (a[1] > p[1]) != (b[1] > p[1]) {
            let dy = b[1] - a[1];
            if dy.abs() > EPS {
                let x = a[0] + (p[1] - a[1]) / dy * (b[0] - a[0]);
                if x > p[0] {
                    inside = !inside;
                }
            }
        }
    }
    inside
}

/// Кольца в части: внешнее кольцо и его дыры. Деление идёт по глубине
/// вложенности, площадь задаёт порядок проверки.
pub fn parts_of(rings: Vec<Vec<Pt>>) -> Vec<Vec<Vec<Pt>>> {
    let n = rings.len();
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![rings];
    }
    let areas: Vec<f64> = rings.iter().map(|r| crate::rings::ring_area(r).abs()).collect();
    // Глубина кольца: сколько колец покрупнее его содержат.
    let mut depth = vec![0usize; n];
    let mut inside: Vec<Vec<usize>> = vec![Vec::new(); n];
    for i in 0..n {
        for j in 0..n {
            if i == j || areas[j] <= areas[i] {
                continue;
            }
            if point_in_ring(rings[i][0], &rings[j]) {
                depth[i] += 1;
                inside[i].push(j);
            }
        }
    }
    let mut parts: Vec<Vec<Vec<Pt>>> = Vec::new();
    let mut slot: Vec<Option<usize>> = vec![None; n];
    // Внешние кольца - чётной глубины, в исходном порядке.
    for i in 0..n {
        if depth[i].is_multiple_of(2) {
            slot[i] = Some(parts.len());
            parts.push(vec![rings[i].clone()]);
        }
    }
    for i in 0..n {
        if depth[i].is_multiple_of(2) {
            continue;
        }
        // Дыра уходит самому глубокому из содержащих её колец.
        let host = inside[i]
            .iter()
            .copied()
            .filter(|j| depth[*j] == depth[i] - 1)
            .min_by(|a, b| {
                areas[*a]
                    .partial_cmp(&areas[*b])
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        if let Some(h) = host
            && let Some(k) = slot[h]
        {
            parts[k].push(rings[i].clone());
        }
    }
    parts
}

/// Объекты проверки из значения поля-слоя.
pub fn items_of(layer: &J) -> Vec<Item> {
    layer
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(i, f)| {
            let parts = polygons_of(f).unwrap_or_else(|| parts_of(rings_of(f)));
            Item {
                fid: f["id"].as_i64().unwrap_or(i as i64),
                geom: backend::geom_of(&parts),
                parts,
            }
        })
        .collect()
}

/// Кольца всех объектов, замкнутые повтором первой вершины: по ним
/// считаются ограничители допуска.
pub fn closed_rings(items: &[Item]) -> Vec<Vec<Pt>> {
    let mut out = Vec::new();
    for it in items {
        for part in &it.parts {
            for ring in part {
                let mut r = ring.clone();
                if let Some(&first) = r.first() {
                    r.push(first);
                }
                if r.len() >= 4 {
                    out.push(r);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Слой с готовым делением на части: дыра уже при своём кольце.
    #[test]
    fn ready_made_parts_are_taken_as_they_are() {
        let f = json!({"id": 2, "polygons": [[
            [[0.0, 0.0, null], [10.0, 0.0, null], [10.0, 10.0, null], [0.0, 10.0, null], [0.0, 0.0, null]],
            [[4.0, 4.0, null], [6.0, 4.0, null], [6.0, 6.0, null], [4.0, 6.0, null], [4.0, 4.0, null]]
        ]]});
        let parts = polygons_of(&f).unwrap_or_default();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].len(), 2);
        let g = backend::geom_of(&parts);
        assert!((backend::area(&g) - 96.0).abs() < 1e-9);
    }

    #[test]
    fn a_hole_goes_to_its_shell() {
        let f = json!({"id": 2, "parts": [
            [[0.0, 10.0, null], [100.0, 10.0, null], [100.0, 110.0, null], [0.0, 110.0, null], [0.0, 10.0, null]],
            [[50.0, 60.0, null], [51.0, 60.0, null], [51.0, 61.0, null], [50.0, 61.0, null], [50.0, 60.0, null]]
        ]});
        let parts = parts_of(rings_of(&f));
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].len(), 2);
        assert_eq!(parts[0][1][0], [50.0, 60.0]);
        let g = backend::geom_of(&parts);
        assert!((backend::area(&g) - 9999.0).abs() < 1e-9);
    }
}
