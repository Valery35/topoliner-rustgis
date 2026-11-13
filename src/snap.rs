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

/// Синус угла, ниже которого отрезки считаются параллельными. Один
/// микрорадиан это шесть стотысячных градуса: настоящее пересечение под
/// таким углом не встречается, а вычисляется оно неустойчиво.
const PARALLEL_SIN: f64 = 1e-6;

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

/// Сегмент кольца: номер кольца и номер сегмента в нём.
type SegKey = (usize, usize);

/// Пара сегментов, которую уже проверили на пересечение.
type Pair = (SegKey, SegKey);

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

/// Накопленные вставки: ключи в порядке первого появления, как у словаря
/// Python, и кандидаты по сегментам.
#[derive(Default)]
struct Pending {
    order: Vec<(usize, usize)>,
    by_segment: HashMap<(usize, usize), Vec<Candidate>>,
}

impl Pending {
    fn add(&mut self, key: (usize, usize), item: Candidate) {
        let list = self.by_segment.entry(key).or_default();
        if list.is_empty() {
            self.order.push(key);
        }
        list.push(item);
    }
}

/// Вставляет накопленные точки в сегменты и пишет события.
///
/// Кольца обходятся в порядке первого появления, внутри кольца сегменты
/// идут с конца: вставка сдвигает номера, и обход с конца их не портит.
/// Внутри сегмента точки идут по параметру проекции.
fn apply(norm: &mut [Norm], pending: Pending, events: &mut Vec<Insert>) -> usize {
    let mut by_ring: Vec<usize> = Vec::new();
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for (ri, si) in &pending.order {
        let slot = groups.entry(*ri).or_default();
        if slot.is_empty() {
            by_ring.push(*ri);
        }
        slot.push(*si);
    }
    let mut count = 0;
    for ri in by_ring {
        let Some(segs) = groups.get(&ri) else { continue };
        let mut segs = segs.clone();
        segs.sort_unstable_by(|a, b| b.cmp(a));
        for si in segs {
            let Some(items) = pending.by_segment.get(&(ri, si)) else {
                continue;
            };
            let mut items = items.clone();
            items.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            let mut add: Vec<Pt> = Vec::new();
            let mut prev_t = -1.0;
            for &(t, px, py, dist) in &items {
                if t - prev_t <= 0.0 {
                    continue;
                }
                add.push([px, py]);
                events.push(Insert {
                    x: px,
                    y: py,
                    dist,
                    ring: ri,
                });
                prev_t = t;
            }
            if !add.is_empty()
                && let Some(coords) = norm[ri].0.as_mut()
            {
                count += add.len();
                let at = (si + 1).min(coords.len());
                coords.splice(at..at, add);
            }
        }
    }
    count
}

/// Нормализация набора колец: вершины без повтора первой, без подряд
/// идущих совпадений, вырожденные кольца - `None`.
fn normalize(rings: &[Vec<Pt>]) -> Vec<Norm> {
    rings
        .iter()
        .map(|r| {
            let (coords, closed) = norm_ring(r);
            let coords = drop_repeats(&coords, closed);
            let degenerate = coords.len() < if closed { 3 } else { 2 };
            (if degenerate { None } else { Some(coords) }, closed)
        })
        .collect()
}

/// Сетка сегментов по всем кольцам набора.
fn segments_of(norm: &[Norm], cell: f64, ox: f64, oy: f64) -> SegmentGrid {
    let mut grid = SegmentGrid::new(cell, ox, oy);
    for (ri, (coords, closed)) in norm.iter().enumerate() {
        let Some(coords) = coords else { continue };
        let n = coords.len();
        let last = if *closed { n } else { n.saturating_sub(1) };
        for si in 0..last {
            grid.add((ri, si), coords[si], coords[(si + 1) % n]);
        }
    }
    grid
}

/// Собственные вершины колец: узел не нужен там, где вершина уже есть.
fn vertices_of(norm: &[Norm], cell: f64, ox: f64, oy: f64) -> RingVertexGrid {
    let mut own = RingVertexGrid::new(cell, ox, oy);
    for (ri, (coords, _)) in norm.iter().enumerate() {
        let Some(coords) = coords else { continue };
        for &p in coords {
            own.add(ri, p);
        }
    }
    own
}

/// Концы сегмента кольца.
fn segment(norm: &[Norm], ri: usize, si: usize) -> Option<(Pt, Pt)> {
    let coords = norm[ri].0.as_ref()?;
    let n = coords.len();
    Some((*coords.get(si)?, coords[(si + 1) % n]))
}

/// Кандидаты на вставку по вершинам: вершина лежит в пределах допуска
/// от чужого ребра, но не совпадает с его концами.
fn by_vertices(norm: &[Norm], tolerance: f64, project: bool) -> Pending {
    let (ox, oy) = origin_of(norm);
    let cell = tolerance.max(mean_segment_length(norm));
    let seg_grid = segments_of(norm, cell, ox, oy);
    let own = vertices_of(norm, tolerance.max(1e-9), ox, oy);

    // Точки-кандидаты: все вершины колец без повторов по координатам.
    let mut pts: Vec<Pt> = Vec::new();
    let mut seen: HashMap<(u64, u64), ()> = HashMap::new();
    for (coords, _) in norm {
        let Some(coords) = coords else { continue };
        for &p in coords {
            if seen.insert(pt_key(p), ()).is_none() {
                pts.push(p);
            }
        }
    }

    let mut pending = Pending::default();
    let tol2 = tolerance * tolerance;
    for &v in &pts {
        for (ri, si) in seg_grid.query(v[0], v[1]) {
            let Some((a, b)) = segment(norm, ri, si) else {
                continue;
            };
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
            // Проекция лежит на прямой ребра, поэтому форма не меняется вовсе.
            let p = if project {
                [a[0] + t * (b[0] - a[0]), a[1] + t * (b[1] - a[1])]
            } else {
                v
            };
            pending.add((ri, si), (t, p[0], p[1], dist));
        }
    }
    pending
}

/// Точка пересечения двух отрезков строго внутри обоих.
///
/// Параллельность проверяется по синусу угла, а не по величине
/// определителя. Определитель растёт вместе с длиной отрезков, и
/// абсолютный порог при координатах в миллионы метров не отсекает ничего.
fn crossing(a: (Pt, Pt), b: (Pt, Pt), near_vertex: f64) -> Option<(f64, f64, f64, f64)> {
    let (ax, ay) = (a.1[0] - a.0[0], a.1[1] - a.0[1]);
    let (bx, by) = (b.1[0] - b.0[0], b.1[1] - b.0[1]);
    let den = ax * by - ay * bx;
    let (la, lb) = (ax.hypot(ay), bx.hypot(by));
    if la <= 0.0 || lb <= 0.0 || den.abs() <= PARALLEL_SIN * la * lb {
        return None;
    }
    let (dx, dy) = (b.0[0] - a.0[0], b.0[1] - a.0[1]);
    let t = (dx * by - dy * bx) / den;
    let u = (dx * ay - dy * ax) / den;
    if t <= 0.0 || t >= 1.0 || u <= 0.0 || u >= 1.0 {
        return None;
    }
    let (px, py) = (a.0[0] + t * ax, a.0[1] + t * ay);
    // Пересечение, попавшее в существующую вершину, узла не требует.
    for v in [a.0, a.1, b.0, b.1] {
        if (px - v[0]).hypot(py - v[1]) <= near_vertex {
            return None;
        }
    }
    Some((px, py, t, u))
}

/// Кандидаты на вставку в точках пересечения рёбер. Нужны для перехлёстов,
/// где рёбра пересекаются крест-накрест, но общих вершин нет.
fn by_crossings(norm: &[Norm], tolerance: f64, project: bool) -> Pending {
    let (ox, oy) = origin_of(norm);
    let cell = tolerance.max(mean_segment_length(norm)).max(1e-9);
    let seg_grid = segments_of(norm, cell, ox, oy);
    let own = vertices_of(norm, tolerance.max(1e-9), ox, oy);

    let mut pending = Pending::default();
    let mut buckets: Vec<&Vec<(usize, usize)>> = seg_grid.cells.values().collect();
    // Обход ячеек в одном порядке от запуска к запуску.
    buckets.sort_unstable();
    let mut checked: HashMap<Pair, ()> = HashMap::new();
    for bucket in buckets {
        for (i, &ka) in bucket.iter().enumerate() {
            for &kb in bucket.iter().skip(i + 1) {
                let pair = if ka <= kb { (ka, kb) } else { (kb, ka) };
                if checked.insert(pair, ()).is_some() {
                    continue;
                }
                // Соседние сегменты одного кольца имеют общую вершину.
                if ka.0 == kb.0 && ka.1.abs_diff(kb.1) <= 1 {
                    continue;
                }
                let (Some(sa), Some(sb)) = (segment(norm, ka.0, ka.1), segment(norm, kb.0, kb.1))
                else {
                    continue;
                };
                let Some((px, py, t, u)) = crossing(sa, sb, tolerance) else {
                    continue;
                };
                // Точку считают по параметру одного из рёбер, поэтому
                // на втором она лежит лишь в пределах округления. Каждая
                // вставка кладётся на своё ребро.
                let (a, b) = if project {
                    (
                        [sa.0[0] + t * (sa.1[0] - sa.0[0]), sa.0[1] + t * (sa.1[1] - sa.0[1])],
                        [sb.0[0] + u * (sb.1[0] - sb.0[0]), sb.0[1] + u * (sb.1[1] - sb.0[1])],
                    )
                } else {
                    ([px, py], [px, py])
                };
                // Почти параллельные рёбра дают неустойчивую точку: проекции
                // на одно и на другое ребро расходятся дальше допуска.
                if (a[0] - b[0]).hypot(a[1] - b[1]) > tolerance {
                    continue;
                }
                if !own.has_vertex(ka.0, a[0], a[1], tolerance) {
                    pending.add(ka, (t, a[0], a[1], 0.0));
                }
                if !own.has_vertex(kb.0, b[0], b[1], tolerance) {
                    pending.add(kb, (u, b[0], b[1], 0.0));
                }
            }
        }
    }
    pending
}

/// Что делать со вставкой узлов.
#[derive(Debug, Clone, Copy)]
pub struct Nodes {
    /// Допуск в единицах координат, больше нуля.
    pub tolerance: f64,
    /// Вставлять ли узлы в точках пересечения рёбер.
    pub crossings: bool,
    /// Вставлять проекцию точки на ребро вместо самой точки. Проекция
    /// лежит на прямой ребра, поэтому форма кольца не меняется вовсе
    /// и самопересечение возникнуть не может.
    pub project_onto_edge: bool,
}

/// Итог вставки узлов.
pub struct Done {
    /// Кольца с узлами, замкнутые повтором первой вершины. `None` -
    /// кольцо выродилось.
    pub rings: Vec<Option<Vec<Pt>>>,
    /// Вставки по вершинам, в порядке модуля. Узлы пересечений сюда
    /// не идут: проверке нужны только события вершин.
    pub events: Vec<Insert>,
    /// Сколько узлов вставлено всего.
    pub inserted: usize,
}

/// Вставка узлов в набор колец. `rings` - кольца, замкнутые повтором
/// первой вершины в конце.
pub fn insert_nodes(rings: &[Vec<Pt>], opts: Nodes) -> Done {
    let mut done = Done {
        rings: rings.iter().map(|r| Some(r.clone())).collect(),
        events: Vec::new(),
        inserted: 0,
    };
    if opts.tolerance <= 0.0 || rings.is_empty() {
        return done;
    }
    let mut norm = normalize(rings);
    if opts.crossings {
        let pending = by_crossings(&norm, opts.tolerance, opts.project_onto_edge);
        let mut skip = Vec::new();
        done.inserted += apply(&mut norm, pending, &mut skip);
    }
    let pending = by_vertices(&norm, opts.tolerance, opts.project_onto_edge);
    done.inserted += apply(&mut norm, pending, &mut done.events);

    // Сборка: повторы снимаются ещё раз, кольцо замыкается.
    done.rings = norm
        .into_iter()
        .map(|(coords, closed)| {
            let coords = drop_repeats(&coords?, closed);
            if coords.len() < if closed { 3 } else { 2 } {
                return None;
            }
            let mut out = coords;
            if closed && let Some(&first) = out.first() {
                out.push(first);
            }
            Some(out)
        })
        .collect();
    done
}

/// События вставки узлов по вершинам, без изменения колец и без узлов
/// в пересечениях рёбер. Их читает проверка топологии.
pub fn insert_events(rings: &[Vec<Pt>], tolerance: f64) -> Vec<Insert> {
    if tolerance <= 0.0 || rings.is_empty() {
        return Vec::new();
    }
    let mut norm = normalize(rings);
    let pending = by_vertices(&norm, tolerance, false);
    let mut events = Vec::new();
    apply(&mut norm, pending, &mut events);
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(x: f64, y: f64, w: f64) -> Vec<Pt> {
        vec![
            [x, y],
            [x + w, y],
            [x + w, y + w],
            [x, y + w],
            [x, y],
        ]
    }

    /// Т-образный стык: вершина соседа лежит на ребре без узла.
    #[test]
    fn a_vertex_on_a_neighbour_edge() {
        let left = square(0.0, 0.0, 10.0);
        let right = vec![
            [10.0, 0.0],
            [20.0, 0.0],
            [20.0, 10.0],
            [10.0, 10.0],
            [10.0, 5.0],
            [10.0, 0.0],
        ];
        let events = insert_events(&[left.clone(), right.clone()], 2.0);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].ring, 0);
        assert!(events[0].dist <= 1e-9, "{:?}", events[0]);
        assert_eq!([events[0].x, events[0].y], [10.0, 5.0]);

        // Сама вставка: у левого кольца появляется узел, форма та же.
        let done = insert_nodes(
            &[left, right],
            Nodes {
                tolerance: 2.0,
                crossings: true,
                project_onto_edge: true,
            },
        );
        assert_eq!(done.inserted, 1);
        let got = done.rings[0].clone().unwrap_or_default();
        assert_eq!(got.len(), 6);
        assert!(got.contains(&[10.0, 5.0]), "{got:?}");
        assert_eq!(crate::rings::ring_area(&got[..got.len() - 1]).abs(), 100.0);
    }

    /// Чистое покрытие вставки не требует, и кольца остаются как были.
    #[test]
    fn a_clean_coverage_needs_no_nodes() {
        let a = square(0.0, 0.0, 10.0);
        let b = square(10.0, 0.0, 10.0);
        let done = insert_nodes(
            &[a.clone(), b.clone()],
            Nodes {
                tolerance: 1e-6,
                crossings: true,
                project_onto_edge: true,
            },
        );
        assert_eq!(done.inserted, 0);
        assert_eq!(done.rings[0].clone().unwrap_or_default(), a);
        assert_eq!(done.rings[1].clone().unwrap_or_default(), b);
    }
}
