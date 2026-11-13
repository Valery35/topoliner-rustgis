// Topoliner для RustGIS - расширение-процесс RustGIS.
// Copyright (C) 2026 ООО «Информ++». Лицензия GPL-3.0-or-later.
// SPDX-License-Identifier: GPL-3.0-or-later

//! Поиск мест, где вершина лежит на ребре соседа без узла. Перенос той
//! части `topo_core.clean_topology` модуля QGIS, которую зовёт проверка:
//! режим вставки узлов, без слияния вершин, без узлов в пересечениях
//! рёбер, только события (`detect_only`).
//!
//! Вставка узлов геометрию здесь не меняет: события читает проверка,
//! а кольца отбрасываются.

use std::collections::HashMap;

use crate::rings::{EPS, Pt, point_segment};

/// Событие вставки: точка, расстояние до ребра и номер кольца.
#[derive(Debug, Clone, PartialEq)]
pub struct Insert {
    pub x: f64,
    pub y: f64,
    pub dist: f64,
    pub ring: usize,
}

/// Кольцо после нормализации: вершины без повтора первой и признак
/// замкнутости. `None` - кольцо выродилось.
type Norm = (Option<Vec<Pt>>, bool);

/// Кандидат на вставку в сегмент: параметр проекции, точка и расстояние
/// до ребра.
type Candidate = (f64, f64, f64, f64);

fn norm_ring(coords: &[Pt]) -> (Vec<Pt>, bool) {
    let mut out = coords.to_vec();
    let mut closed = false;
    if out.len() >= 2 {
        let a = out[0];
        let b = out[out.len() - 1];
        if (a[0] - b[0]).abs() <= EPS && (a[1] - b[1]).abs() <= EPS {
            closed = true;
            out.pop();
        }
    }
    (out, closed)
}

fn drop_repeats(coords: &[Pt], closed: bool) -> Vec<Pt> {
    if coords.is_empty() {
        return Vec::new();
    }
    let mut out = vec![coords[0]];
    for &p in &coords[1..] {
        let q = out[out.len() - 1];
        if (p[0] - q[0]).abs() > EPS || (p[1] - q[1]).abs() > EPS {
            out.push(p);
        }
    }
    if closed {
        while out.len() > 1 {
            let (a, b) = (out[0], out[out.len() - 1]);
            if (a[0] - b[0]).abs() <= EPS && (a[1] - b[1]).abs() <= EPS {
                out.pop();
            } else {
                break;
            }
        }
    }
    out
}

/// Средняя длина ребра: по ней берётся ячейка сетки сегментов.
fn mean_segment_length(norm: &[Norm]) -> f64 {
    let mut total = 0.0;
    let mut n = 0usize;
    for (coords, closed) in norm {
        let Some(coords) = coords else { continue };
        let m = coords.len();
        if m == 0 {
            continue;
        }
        let last = if *closed { m } else { m.saturating_sub(1) };
        for i in 0..last {
            let a = coords[i];
            let b = coords[(i + 1) % m];
            total += (b[0] - a[0]).hypot(b[1] - a[1]);
            n += 1;
        }
    }
    if n > 0 { total / n as f64 } else { 0.0 }
}

fn origin_of(norm: &[Norm]) -> (f64, f64) {
    let mut min_x: Option<f64> = None;
    let mut min_y: Option<f64> = None;
    for (coords, _) in norm {
        let Some(coords) = coords else { continue };
        for p in coords {
            min_x = Some(min_x.map_or(p[0], |v: f64| v.min(p[0])));
            min_y = Some(min_y.map_or(p[1], |v: f64| v.min(p[1])));
        }
    }
    (min_x.unwrap_or(0.0), min_y.unwrap_or(0.0))
}

/// Сетка отрезков. Ячейка не мельче допуска, запрос смотрит окрестность
/// три на три. Отрезок кладётся во все ячейки своего охвата: это надмножество
/// тех, что он пересекает, а каждый кандидат потом проверяется точно.
struct SegmentGrid {
    cell: f64,
    ox: f64,
    oy: f64,
    cells: HashMap<(i64, i64), Vec<(usize, usize)>>,
}

impl SegmentGrid {
    fn new(cell: f64, ox: f64, oy: f64) -> Self {
        Self {
            cell: if cell > 0.0 { cell } else { 1.0 },
            ox,
            oy,
            cells: HashMap::new(),
        }
    }

    fn at(&self, x: f64, y: f64) -> (i64, i64) {
        (
            ((x - self.ox) / self.cell).floor() as i64,
            ((y - self.oy) / self.cell).floor() as i64,
        )
    }

    fn add(&mut self, key: (usize, usize), a: Pt, b: Pt) {
        let (i0, j0) = self.at(a[0].min(b[0]), a[1].min(b[1]));
        let (i1, j1) = self.at(a[0].max(b[0]), a[1].max(b[1]));
        // предохранитель от гигантского охвата при мелкой ячейке
        let si = (((i1 - i0) / 500).max(1)) as usize;
        let sj = (((j1 - j0) / 500).max(1)) as usize;
        let mut i = i0;
        while i <= i1 {
            let mut j = j0;
            while j <= j1 {
                self.cells.entry((i, j)).or_default().push(key);
                j += sj as i64;
            }
            i += si as i64;
        }
    }

    /// Кандидаты около точки, по возрастанию номера кольца и сегмента:
    /// порядок обхода задаёт порядок событий, и он должен быть один
    /// от запуска к запуску.
    fn query(&self, x: f64, y: f64) -> Vec<(usize, usize)> {
        let (cx, cy) = self.at(x, y);
        let mut out: Vec<(usize, usize)> = Vec::new();
        for i in cx - 1..=cx + 1 {
            for j in cy - 1..=cy + 1 {
                if let Some(b) = self.cells.get(&(i, j)) {
                    out.extend_from_slice(b);
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// Вершины колец: есть ли у кольца своя вершина ближе радиуса.
struct RingVertexGrid {
    cell: f64,
    ox: f64,
    oy: f64,
    cells: HashMap<(usize, i64, i64), Vec<Pt>>,
}

impl RingVertexGrid {
    fn new(cell: f64, ox: f64, oy: f64) -> Self {
        Self {
            cell: if cell > 0.0 { cell } else { 1.0 },
            ox,
            oy,
            cells: HashMap::new(),
        }
    }

    fn add(&mut self, ring: usize, p: Pt) {
        let i = ((p[0] - self.ox) / self.cell).floor() as i64;
        let j = ((p[1] - self.oy) / self.cell).floor() as i64;
        self.cells.entry((ring, i, j)).or_default().push(p);
    }

    fn has_vertex(&self, ring: usize, x: f64, y: f64, radius: f64) -> bool {
        let cx = ((x - self.ox) / self.cell).floor() as i64;
        let cy = ((y - self.oy) / self.cell).floor() as i64;
        let r2 = radius * radius;
        for i in cx - 1..=cx + 1 {
            for j in cy - 1..=cy + 1 {
                let Some(bucket) = self.cells.get(&(ring, i, j)) else {
                    continue;
                };
                for p in bucket {
                    let dx = p[0] - x;
                    let dy = p[1] - y;
                    if dx * dx + dy * dy <= r2 {
                        return true;
                    }
                }
            }
        }
        false
    }
}

/// Ключ точки для словаря кандидатов: координаты как есть, по двоичной
/// записи. В модуле это ключ словаря Python, там сравнение тоже точное.
fn pt_key(p: Pt) -> (u64, u64) {
    (p[0].to_bits(), p[1].to_bits())
}

/// События вставки узлов по вершинам. `rings` - кольца, замкнутые повтором
/// первой вершины в конце. Ответ - события в том же порядке, что у модуля.
pub fn insert_events(rings: &[Vec<Pt>], tolerance: f64) -> Vec<Insert> {
    if tolerance <= 0.0 || rings.is_empty() {
        return Vec::new();
    }
    // Шаг 1. Нормализация и отбраковка вырожденных колец.
    let mut norm: Vec<Norm> = Vec::with_capacity(rings.len());
    for r in rings {
        let (coords, closed) = norm_ring(r);
        let coords = drop_repeats(&coords, closed);
        let degenerate = coords.len() < if closed { 3 } else { 2 };
        norm.push((if degenerate { None } else { Some(coords) }, closed));
    }

    // Шаг 2. Точки-кандидаты: все вершины рабочих колец без повторов.
    let mut pts: Vec<Pt> = Vec::new();
    let mut seen: HashMap<(u64, u64), ()> = HashMap::new();
    for (coords, _) in &norm {
        let Some(coords) = coords else { continue };
        for &p in coords {
            if seen.insert(pt_key(p), ()).is_none() {
                pts.push(p);
            }
        }
    }

    let (ox, oy) = origin_of(&norm);
    let cell = tolerance.max(mean_segment_length(&norm));
    let mut seg_grid = SegmentGrid::new(cell, ox, oy);
    for (ri, (coords, closed)) in norm.iter().enumerate() {
        let Some(coords) = coords else { continue };
        let n = coords.len();
        let last = if *closed { n } else { n.saturating_sub(1) };
        for si in 0..last {
            seg_grid.add((ri, si), coords[si], coords[(si + 1) % n]);
        }
    }
    let mut own = RingVertexGrid::new(tolerance.max(1e-9), ox, oy);
    for (ri, (coords, _)) in norm.iter().enumerate() {
        let Some(coords) = coords else { continue };
        for &p in coords {
            own.add(ri, p);
        }
    }

    // Шаг 3. Кандидаты на вставку, сгруппированные по сегменту. Порядок
    // ключей - порядок первого появления, как у словаря Python.
    let mut order: Vec<(usize, usize)> = Vec::new();
    let mut inserts: HashMap<(usize, usize), Vec<Candidate>> = HashMap::new();
    let tol2 = tolerance * tolerance;
    for &v in &pts {
        for (ri, si) in seg_grid.query(v[0], v[1]) {
            let Some(coords) = norm[ri].0.as_ref() else {
                continue;
            };
            let n = coords.len();
            let a = coords[si];
            let b = coords[(si + 1) % n];
            // Узел не нужен, если в пределах допуска у ребра уже есть вершина.
            let ex = v[0] - a[0];
            let ey = v[1] - a[1];
            if ex * ex + ey * ey <= tol2 {
                continue;
            }
            let ex = v[0] - b[0];
            let ey = v[1] - b[1];
            if ex * ex + ey * ey <= tol2 {
                continue;
            }
            let (dist, t) = point_segment(v, a, b);
            if dist * dist > tol2 || t <= 0.0 || t >= 1.0 {
                continue;
            }
            // Точка уже есть среди вершин этого кольца: узла достаточно.
            if own.has_vertex(ri, v[0], v[1], tolerance) {
                continue;
            }
            let key = (ri, si);
            if inserts.entry(key).or_default().is_empty() {
                order.push(key);
            }
            if let Some(list) = inserts.get_mut(&key) {
                list.push((t, v[0], v[1], dist));
            }
        }
    }

    // Шаг 4. События: по кольцам в порядке появления, внутри кольца
    // сегменты с конца, внутри сегмента по параметру проекции.
    let mut by_ring: Vec<usize> = Vec::new();
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for (ri, si) in &order {
        let slot = groups.entry(*ri).or_default();
        if slot.is_empty() {
            by_ring.push(*ri);
        }
        slot.push(*si);
    }
    let mut events = Vec::new();
    for ri in by_ring {
        let Some(segs) = groups.get(&ri) else { continue };
        let mut segs = segs.clone();
        segs.sort_unstable_by(|a, b| b.cmp(a));
        for si in segs {
            let Some(items) = inserts.get(&(ri, si)) else {
                continue;
            };
            let mut items = items.clone();
            items.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            let mut prev_t = -1.0;
            for (t, px, py, dist) in items {
                if t - prev_t <= 0.0 {
                    continue;
                }
                events.push(Insert {
                    x: px,
                    y: py,
                    dist,
                    ring: ri,
                });
                prev_t = t;
            }
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Т-образный стык: вершина соседа лежит на ребре без узла.
    #[test]
    fn a_vertex_on_a_neighbour_edge() {
        let left = vec![
            [0.0, 0.0],
            [10.0, 0.0],
            [10.0, 10.0],
            [0.0, 10.0],
            [0.0, 0.0],
        ];
        let right = vec![
            [10.0, 0.0],
            [20.0, 0.0],
            [20.0, 10.0],
            [10.0, 10.0],
            [10.0, 5.0],
            [10.0, 0.0],
        ];
        let events = insert_events(&[left, right], 2.0);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].ring, 0);
        assert!(events[0].dist <= 1e-9, "{:?}", events[0]);
        assert_eq!([events[0].x, events[0].y], [10.0, 5.0]);
    }
}
