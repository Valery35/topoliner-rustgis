// Topoliner для RustGIS - расширение-процесс RustGIS.
// Copyright (C) 2026 ООО «Информ++». Лицензия GPL-3.0-or-later.
// SPDX-License-Identifier: GPL-3.0-or-later

//! Разбор покрытия на дуги. Перенос `topo_simplify.build_arcs` модуля
//! QGIS вместе с достройкой узлов из `boundaries.insert_missing_nodes`.
//!
//! Дуга это участок границы между узлами ветвления. Рёбра, общие для двух
//! колец, склеиваются в одну дугу, и она хранится один раз. Дуга
//! обрывается там, где сходятся три и более кольца или где меняется пара
//! соседей.
//!
//! Вершины приводятся к сетке опознания: соседи записывают одну и ту же
//! вершину с разной точностью, а стык дуг обязан совпадать точно. Это
//! не допуск, и увеличивать сетку до метров не следует.

use std::collections::{HashMap, HashSet};

use crate::rings::Pt;
use crate::snap::{self, Nodes};

/// Ключ вершины на сетке опознания.
pub type Key = (i64, i64);

/// Ключ вершины. Округление до сетки сводит вместе координаты соседей,
/// записанные с разной точностью. Спор решается к чётному, как у `round`
/// в Python.
pub fn key_of(p: Pt, grid: f64) -> Key {
    (
        (p[0] / grid).round_ties_even() as i64,
        (p[1] / grid).round_ties_even() as i64,
    )
}

/// Достраивает узлы там, где вершина одного кольца лежит на ребре другого.
///
/// Без этого общая граница не опознаётся: у соседей разная вершинность,
/// рёбра не совпадают, и один и тот же участок выходит двумя линиями
/// вместо одной. Вершины при этом не двигаются, форма не меняется.
///
/// Кольца приходят и уходят без повтора первой вершины в конце.
pub fn insert_missing_nodes(rings: &[Vec<Pt>], eps: f64, max_passes: usize) -> Vec<Vec<Pt>> {
    let mut current: Vec<Vec<Pt>> = rings
        .iter()
        .map(|r| {
            let mut out = r.clone();
            if let Some(&first) = r.first() {
                out.push(first);
            }
            out
        })
        .collect();
    for _ in 0..max_passes {
        let done = snap::insert_nodes(
            &current,
            Nodes {
                tolerance: eps,
                crossings: true,
                project_onto_edge: true,
            },
        );
        let added = done.inserted();
        current = done.rings.into_iter().map(Option::unwrap_or_default).collect();
        if added == 0 {
            break;
        }
    }
    current
        .into_iter()
        .map(|mut ring| {
            if ring.len() > 1 && ring[0] == ring[ring.len() - 1] {
                ring.pop();
            }
            ring
        })
        .collect()
}

/// Части объекта: часть это внешнее кольцо и полости.
pub type Parts = Vec<Vec<Vec<Pt>>>;

/// Объект покрытия: номер и его части.
pub type Shape = (i64, Parts);

/// Дуга как список вершин.
pub type Arc = Vec<Pt>;

/// Кольцо как список дуг: номер дуги и признак обратного прохода.
pub type Path = Vec<(usize, bool)>;

/// Раскладывает кольца на дуги. Кольца приходят без повтора первой
/// вершины в конце и считаются замкнутыми.
pub fn build_arcs(rings: &[Vec<Pt>], grid: f64) -> (Vec<Arc>, Vec<Path>) {
    let closed = vec![true; rings.len()];
    build_arcs_open(rings, &closed, grid)
}

/// Раскладывает на дуги кольца и разомкнутые линии. У разомкнутой линии
/// концы являются узлами и остаются неподвижными, как и точки ветвления.
pub fn build_arcs_open(rings: &[Vec<Pt>], closed: &[bool], grid: f64) -> (Vec<Arc>, Vec<Path>) {
    let shut = |ri: usize| closed.get(ri).copied().unwrap_or(true);
    let ring_keys: Vec<Vec<Key>> = rings
        .iter()
        .map(|r| r.iter().map(|&p| key_of(p, grid)).collect())
        .collect();

    // ── Рёбра и их владельцы ────────────────────────────────────────────
    let mut edge_owners: HashMap<(Key, Key), HashSet<usize>> = HashMap::new();
    let mut endpoints: HashSet<Key> = HashSet::new();
    for (ri, keys) in ring_keys.iter().enumerate() {
        let n = keys.len();
        if !shut(ri) && n >= 1 {
            endpoints.insert(keys[0]);
            endpoints.insert(keys[n - 1]);
        }
        let last = if shut(ri) { n } else { n.saturating_sub(1) };
        for i in 0..last {
            let a = keys[i];
            let b = keys[(i + 1) % n];
            if a == b {
                continue;
            }
            let ek = if a <= b { (a, b) } else { (b, a) };
            edge_owners.entry(ek).or_default().insert(ri);
        }
    }

    // ── Наборы владельцев вокруг вершины ────────────────────────────────
    let mut around: HashMap<Key, Vec<Vec<usize>>> = HashMap::new();
    for ((a, b), owners) in &edge_owners {
        let mut set: Vec<usize> = owners.iter().copied().collect();
        set.sort_unstable();
        around.entry(*a).or_default().push(set.clone());
        around.entry(*b).or_default().push(set);
    }

    // Вершина является узлом, если в ней сходятся не два ребра, либо
    // у сходящихся рёбер разные владельцы, либо это конец линии.
    let is_junction = |v: &Key| -> bool {
        if endpoints.contains(v) {
            return true;
        }
        match around.get(v) {
            Some(items) if items.len() == 2 => items[0] != items[1],
            _ => true,
        }
    };

    // Общая координата для каждого ключа: соседи могут записать одну и ту
    // же вершину с разной точностью, а стык дуг обязан совпадать точно.
    let mut canon: HashMap<Key, Pt> = HashMap::new();
    for (ri, ring) in rings.iter().enumerate() {
        for (i, &p) in ring.iter().enumerate() {
            canon.entry(ring_keys[ri][i]).or_insert(p);
        }
    }

    let mut arcs: Vec<Arc> = Vec::new();
    let mut arc_index: HashMap<Vec<Key>, usize> = HashMap::new();
    let mut paths: Vec<Path> = Vec::new();

    // Возвращает номер дуги и признак того, что при сборке её разворачивают.
    let mut register = |kseq: Vec<Key>, pts: Vec<Pt>, arcs: &mut Vec<Arc>| -> (usize, bool) {
        if let Some(&idx) = arc_index.get(&kseq) {
            return (idx, false);
        }
        let back: Vec<Key> = kseq.iter().rev().copied().collect();
        if let Some(&idx) = arc_index.get(&back) {
            // Та же дуга, пройденная в обратную сторону. Координаты берутся
            // у первой записи: дуга обязана быть у соседей буквально общей.
            return (idx, true);
        }
        let idx = arcs.len();
        let mut pts = pts;
        // Концы дуги приводим к общей координате узла.
        if let (Some(first), Some(k)) = (pts.first_mut(), kseq.first())
            && let Some(&c) = canon.get(k)
        {
            *first = c;
        }
        if let (Some(last), Some(k)) = (pts.last_mut(), kseq.last())
            && let Some(&c) = canon.get(k)
        {
            *last = c;
        }
        arcs.push(pts);
        arc_index.insert(kseq, idx);
        (idx, false)
    };

    for (ri, ring) in rings.iter().enumerate() {
        let n = ring.len();
        let keys = &ring_keys[ri];
        if n == 0 {
            paths.push(Vec::new());
            continue;
        }
        if !shut(ri) {
            // Разомкнутая линия: идём от начала к концу, разрезая на узлах.
            let mut path: Path = Vec::new();
            let mut i = 0;
            while i + 1 < n {
                let mut pts = vec![ring[i]];
                let mut kseq = vec![keys[i]];
                let mut j = i + 1;
                loop {
                    pts.push(ring[j]);
                    kseq.push(keys[j]);
                    if j == n - 1 || is_junction(&keys[j]) {
                        break;
                    }
                    j += 1;
                }
                path.push(register(kseq, pts, &mut arcs));
                i = j;
            }
            paths.push(path);
            continue;
        }

        let starts: Vec<usize> = (0..n).filter(|&i| is_junction(&keys[i])).collect();
        let mut path: Path = Vec::new();

        let Some(&s0) = starts.first() else {
            // Кольцо целиком является одной замкнутой дугой.
            let mut kseq = keys.clone();
            kseq.push(keys[0]);
            let mut pts = ring.clone();
            pts.push(ring[0]);
            path.push(register(kseq, pts, &mut arcs));
            paths.push(path);
            continue;
        };

        let mut i = s0;
        loop {
            let mut pts = vec![ring[i]];
            let mut kseq = vec![keys[i]];
            let mut j = (i + 1) % n;
            loop {
                pts.push(ring[j]);
                kseq.push(keys[j]);
                if is_junction(&keys[j]) {
                    break;
                }
                j = (j + 1) % n;
            }
            path.push(register(kseq, pts, &mut arcs));
            i = j;
            if i == s0 {
                break;
            }
        }
        paths.push(path);
    }

    (arcs, paths)
}

/// Кольца объектов и их владельцы: номер объекта и номер кольца в части.
/// Первое кольцо части внешнее, остальные это полости.
pub fn rings_with_owners(items: &[Shape]) -> (Vec<Vec<Pt>>, Vec<(i64, usize)>) {
    let mut rings = Vec::new();
    let mut owners = Vec::new();
    for (fid, parts) in items {
        for part in parts {
            for (ring_index, ring) in part.iter().enumerate() {
                let mut coords = ring.clone();
                if coords.len() > 1 && coords[0] == coords[coords.len() - 1] {
                    coords.pop();
                }
                if coords.len() < 3 {
                    continue;
                }
                rings.push(coords);
                owners.push((*fid, ring_index));
            }
        }
    }
    (rings, owners)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(x: f64, y: f64, w: f64) -> Vec<Pt> {
        vec![[x, y], [x + w, y], [x + w, y + w], [x, y + w]]
    }

    /// Два соседних квадрата: общая граница одна дуга, внешние края свои.
    #[test]
    fn a_shared_border_is_one_arc() {
        let a = square(0.0, 0.0, 10.0);
        let b = square(10.0, 0.0, 10.0);
        let (arcs, paths) = build_arcs(&[a, b], 1e-6);
        assert_eq!(arcs.len(), 3, "{arcs:?}");
        assert_eq!(paths.len(), 2);
        // общая дуга у обоих колец, у второго - в обратную сторону
        let shared: Vec<usize> = paths[0].iter().map(|(i, _)| *i).collect();
        let other: Vec<usize> = paths[1].iter().map(|(i, _)| *i).collect();
        let common: Vec<usize> = shared
            .iter()
            .copied()
            .filter(|i| other.contains(i))
            .collect();
        assert_eq!(common.len(), 1);
        let arc = &arcs[common[0]];
        assert_eq!(arc.first(), Some(&[10.0, 0.0]));
        assert_eq!(arc.last(), Some(&[10.0, 10.0]));
    }

    /// Одинокое кольцо узлов не имеет и выходит одной замкнутой дугой.
    #[test]
    fn a_lone_ring_is_one_closed_arc() {
        let (arcs, paths) = build_arcs(&[square(0.0, 0.0, 10.0)], 1e-6);
        assert_eq!(arcs.len(), 1);
        assert_eq!(paths[0].len(), 1);
        assert_eq!(arcs[0].len(), 5);
        assert_eq!(arcs[0].first(), arcs[0].last());
    }

    #[test]
    fn the_key_rounds_to_even() {
        assert_eq!(key_of([0.5, 1.5], 1.0), (0, 2));
        assert_eq!(key_of([10.0000004, 0.0], 1e-6), (10000000, 0));
    }
}
