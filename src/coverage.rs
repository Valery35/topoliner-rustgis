// Topoliner для RustGIS - расширение-процесс RustGIS.
// Copyright (C) 2026 ООО «Информ++». Лицензия GPL-3.0-or-later.
// SPDX-License-Identifier: GPL-3.0-or-later

//! 2.03 Топологическая модель покрытия. Перенос модуля `coverage`
//! плагина QGIS.
//!
//! Та самая модель, что была в ArcInfo coverage. Полигон хранится
//! не своей границей, а ссылками на дуги, дуга хранится один раз и знает,
//! что лежит слева и справа. Правка дуги меняет обоих соседей сразу.

use std::collections::HashMap;

use rustgis_kit::Lang;
use serde_json::{Value as J, json};

use crate::arcs::{self, Path};
use crate::boundaries::{length_of, points_of};
use crate::rings::Pt;
use crate::rounding::nice;

/// Узел покрытия: конец дуги.
#[derive(Debug, Clone)]
pub struct Node {
    pub id: usize,
    pub x: f64,
    pub y: f64,
    pub degree: usize,
}

/// Дуга покрытия.
#[derive(Debug, Clone)]
pub struct Arc {
    pub id: usize,
    pub coords: Vec<Pt>,
    pub from_node: usize,
    pub to_node: usize,
    /// Объект, при обходе которого дуга проходится в записанном
    /// направлении. Второй сосед - `right`, у края покрытия его нет.
    pub left: Option<i64>,
    pub right: Option<i64>,
}

/// Покрытие узлами и дугами.
#[derive(Debug, Clone, Default)]
pub struct Coverage {
    pub nodes: Vec<Node>,
    pub arcs: Vec<Arc>,
    /// Для каждого исходного кольца список дуг.
    pub rings: Vec<Path>,
    /// Для каждого кольца номер объекта и номер кольца в части.
    pub owners: Vec<(i64, usize)>,
}

/// Степень узла словами.
pub fn degree_label(degree: usize, english: bool) -> &'static str {
    match (degree, english) {
        (1, false) => "висячий",
        (1, true) => "dangle",
        (2, false) => "псевдоузел",
        (2, true) => "pseudo node",
        (_, false) => "ветвление",
        (_, true) => "junction",
    }
}

/// Раскладывает покрытие на узлы и дуги.
pub fn build_coverage(items: &[arcs::Shape], grid: f64, node_eps: f64) -> Coverage {
    let (mut rings, owners) = arcs::rings_with_owners(items);
    if rings.is_empty() {
        return Coverage::default();
    }
    if node_eps > 0.0 {
        rings = arcs::insert_missing_nodes(&rings, node_eps, 8);
    }
    let (arc_list, paths) = arcs::build_arcs(&rings, grid);

    // ── Владельцы дуг, в порядке обхода колец ───────────────────────────
    let mut users: Vec<Vec<(usize, bool)>> = vec![Vec::new(); arc_list.len()];
    for (ring_index, path) in paths.iter().enumerate() {
        for &(arc_index, reversed) in path {
            if let Some(slot) = users.get_mut(arc_index) {
                slot.push((ring_index, reversed));
            }
        }
    }

    // ── Узлы ────────────────────────────────────────────────────────────
    // Одна и та же точка у разных дуг должна получить один номер, поэтому
    // ключ округляется до сетки.
    let mut node_by_key: HashMap<arcs::Key, usize> = HashMap::new();
    let mut nodes: Vec<Node> = Vec::new();
    let mut node_arcs: Vec<usize> = Vec::new();
    let mut node_id = |p: Pt, nodes: &mut Vec<Node>, node_arcs: &mut Vec<usize>| -> usize {
        let key = arcs::key_of(p, grid);
        *node_by_key.entry(key).or_insert_with(|| {
            let id = nodes.len();
            nodes.push(Node {
                id,
                x: p[0],
                y: p[1],
                degree: 0,
            });
            node_arcs.push(0);
            id
        })
    };

    let mut out_arcs: Vec<Arc> = Vec::new();
    let mut renumber: HashMap<usize, usize> = HashMap::new();
    for (arc_index, arc) in arc_list.iter().enumerate() {
        let holders = &users[arc_index];
        if holders.is_empty() || arc.len() < 2 {
            continue;
        }
        let (Some(&first), Some(&last)) = (arc.first(), arc.last()) else {
            continue;
        };
        let start = node_id(first, &mut nodes, &mut node_arcs);
        let end = node_id(last, &mut nodes, &mut node_arcs);

        let mut left: Option<i64> = None;
        let mut right: Option<i64> = None;
        for &(ring_index, reversed) in holders.iter().take(2) {
            let (fid, _ring_number) = owners[ring_index];
            if !reversed && left.is_none() {
                left = Some(fid);
            } else if right.is_none() {
                right = Some(fid);
            } else if left.is_none() {
                left = Some(fid);
            }
        }

        let id = out_arcs.len();
        renumber.insert(arc_index, id);
        out_arcs.push(Arc {
            id,
            coords: arc.clone(),
            from_node: start,
            to_node: end,
            left,
            right,
        });
        node_arcs[start] += 1;
        // Замкнутая дуга входит в свой узел дважды.
        node_arcs[end] += 1;
    }
    for (node, count) in nodes.iter_mut().zip(&node_arcs) {
        node.degree = *count;
    }

    // ── Кольца как списки дуг ───────────────────────────────────────────
    // Номера дуг могли сдвинуться, если какие-то отброшены.
    let ring_paths: Vec<Path> = paths
        .iter()
        .map(|path| {
            path.iter()
                .filter_map(|&(a, flag)| renumber.get(&a).map(|&id| (id, flag)))
                .collect()
        })
        .collect();

    Coverage {
        nodes,
        arcs: out_arcs,
        rings: ring_paths,
        owners,
    }
}

/// Узлы точечным слоем.
pub fn nodes_layer(cov: &Coverage, lang: Lang) -> J {
    let english = lang == Lang::En;
    let features: Vec<J> = cov
        .nodes
        .iter()
        .map(|n| {
            json!({
                "values": [n.id, n.degree, degree_label(n.degree, english)],
                "geometry": {"xyz": [n.x, n.y, J::Null]},
            })
        })
        .collect();
    json!({
        "geometry": "point",
        "fields": [["node_id", "int"], ["degree", "int"], ["kind", "string"]],
        "features": features,
    })
}

/// Дуги линейным слоем.
pub fn arcs_layer(cov: &Coverage) -> J {
    let features: Vec<J> = cov
        .arcs
        .iter()
        .map(|a| {
            json!({
                "values": [
                    a.id,
                    a.from_node,
                    a.to_node,
                    a.left.unwrap_or(-1),
                    a.right.unwrap_or(-1),
                    nice(length_of(&a.coords)),
                ],
                "geometry": {"points": points_of(&a.coords)},
            })
        })
        .collect();
    json!({
        "geometry": "line",
        "fields": [
            ["arc_id", "int"], ["from_node", "int"], ["to_node", "int"],
            ["left_fid", "long"], ["right_fid", "long"], ["length", "double"]
        ],
        "features": features,
    })
}

/// Строки журнала, как их печатает диалог модуля.
pub fn report(items: usize, cov: &Coverage, lang: Lang) -> Vec<String> {
    let shared = cov.arcs.iter().filter(|a| a.right.is_some()).count();
    let dangles = cov.nodes.iter().filter(|n| n.degree == 1).count();
    let pseudo = cov.nodes.iter().filter(|n| n.degree == 2).count();
    let pick = |ru: &'static str, en: &'static str| match lang {
        Lang::Ru => ru.to_string(),
        Lang::En => en.to_string(),
    };
    vec![
        pick("Чтение слоя...", "Reading the layer..."),
        pick("Объектов: %d", "Objects: %d").replacen("%d", &items.to_string(), 1),
        String::new(),
        pick("── Результат ──", "-- Result --"),
        pick("Узлов: %d, дуг: %d", "Nodes: %d, arcs: %d")
            .replacen("%d", &cov.nodes.len().to_string(), 1)
            .replacen("%d", &cov.arcs.len().to_string(), 1),
        pick(
            "Дуг между двумя объектами: %d",
            "Arcs between two objects: %d",
        )
        .replacen("%d", &shared.to_string(), 1),
        pick(
            "Висячих узлов: %d, псевдоузлов: %d",
            "Dangling nodes: %d, pseudo nodes: %d",
        )
        .replacen("%d", &dangles.to_string(), 1)
        .replacen("%d", &pseudo.to_string(), 1),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(x: f64, y: f64, w: f64) -> Vec<Pt> {
        vec![[x, y], [x + w, y], [x + w, y + w], [x, y + w]]
    }

    /// Четыре квадрата: узел степени 4 в середине, восемь дуг.
    #[test]
    fn a_two_by_two_grid() {
        let items: Vec<(i64, Vec<Vec<Vec<Pt>>>)> = vec![
            (1, vec![vec![square(0.0, 0.0, 10.0)]]),
            (2, vec![vec![square(10.0, 0.0, 10.0)]]),
            (3, vec![vec![square(0.0, 10.0, 10.0)]]),
            (4, vec![vec![square(10.0, 10.0, 10.0)]]),
        ];
        let cov = build_coverage(&items, 1e-6, 1e-6);
        assert_eq!(cov.nodes.len(), 5, "{:?}", cov.nodes);
        assert_eq!(cov.arcs.len(), 8);
        let middle = cov
            .nodes
            .iter()
            .find(|n| n.degree == 4)
            .map(|n| (n.x, n.y));
        assert_eq!(middle, Some((10.0, 10.0)));
        let shared = cov.arcs.iter().filter(|a| a.right.is_some()).count();
        assert_eq!(shared, 4);
    }
}
