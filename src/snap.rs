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

/// Вид правки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Вершина притянута к лидеру кластера.
    Move,
    /// Узел вставлен в ребро по вершине соседа.
    Insert,
    /// Узел вставлен в точке пересечения рёбер.
    Cross,
}

impl Kind {
    /// Имя вида правки в поле `kind` слоя точек.
    pub fn code(self) -> &'static str {
        match self {
            Kind::Move => "move",
            Kind::Insert => "insert",
            Kind::Cross => "cross",
        }
    }
}

/// Событие правки: точка, расстояние и номер кольца.
#[derive(Debug, Clone, PartialEq)]
pub struct Insert {
    pub x: f64,
    pub y: f64,
    pub dist: f64,
    pub ring: usize,
    pub kind: Kind,
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
                    kind: Kind::Insert,
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
fn by_vertices(norm: &[Norm], tolerance: f64, project: bool, fixed: &[Vec<Pt>]) -> Pending {
    let (ox, oy) = origin_of(norm);
    let cell = tolerance.max(mean_segment_length(norm));
    let seg_grid = segments_of(norm, cell, ox, oy);
    let own = vertices_of(norm, tolerance.max(1e-9), ox, oy);

    // Точки-кандидаты: вершины эталона, затем вершины рабочих колец,
    // без повторов по координатам.
    let mut pts: Vec<Pt> = Vec::new();
    let mut seen: HashMap<(u64, u64), ()> = HashMap::new();
    for r in fixed {
        let (coords, _) = norm_ring(r);
        for p in coords {
            if seen.insert(pt_key(p), ()).is_none() {
                pts.push(p);
            }
        }
    }
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

/// Сетка точек с поиском ближайшей в радиусе. Нужна слиянию вершин.
struct PointGrid {
    cell: f64,
    xs: Vec<f64>,
    ys: Vec<f64>,
    cells: HashMap<(i64, i64), Vec<usize>>,
}

impl PointGrid {
    fn new(cell: f64) -> Self {
        Self {
            cell: if cell > 0.0 { cell } else { 1.0 },
            xs: Vec::new(),
            ys: Vec::new(),
            cells: HashMap::new(),
        }
    }

    fn add(&mut self, p: Pt) {
        let idx = self.xs.len();
        self.xs.push(p[0]);
        self.ys.push(p[1]);
        let key = (
            (p[0] / self.cell).floor() as i64,
            (p[1] / self.cell).floor() as i64,
        );
        self.cells.entry(key).or_default().push(idx);
    }

    /// Ближайшая точка в радиусе и расстояние до неё.
    fn nearest(&self, p: Pt, radius: f64) -> Option<(usize, f64)> {
        let cx = (p[0] / self.cell).floor() as i64;
        let cy = (p[1] / self.cell).floor() as i64;
        let mut best: Option<usize> = None;
        let mut best_d2 = radius * radius;
        let span = ((radius / self.cell).ceil() as i64).max(1);
        for i in cx - span..=cx + span {
            for j in cy - span..=cy + span {
                let Some(bucket) = self.cells.get(&(i, j)) else {
                    continue;
                };
                for &idx in bucket {
                    let dx = self.xs[idx] - p[0];
                    let dy = self.ys[idx] - p[1];
                    let d2 = dx * dx + dy * dy;
                    if d2 <= best_d2 {
                        best_d2 = d2;
                        best = Some(idx);
                    }
                }
            }
        }
        best.map(|idx| (idx, best_d2.sqrt()))
    }
}

/// Что делает сшивка.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Слияние вершин и вставка узлов.
    Both,
    /// Только вставка узлов.
    Insert,
    /// Только слияние вершин.
    Merge,
}

impl Mode {
    fn merges(self) -> bool {
        matches!(self, Mode::Merge | Mode::Both)
    }

    fn inserts(self) -> bool {
        matches!(self, Mode::Insert | Mode::Both)
    }
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

/// Настройки сшивки целиком.
pub struct Settings {
    pub nodes: Nodes,
    pub mode: Mode,
    /// Номера колец, которые нельзя изменять. Их вершины неподвижны
    /// и служат опорой для соседей.
    pub frozen: Vec<usize>,
    /// Кольца эталона. Они не изменяются и дают вершины-лидеры.
    pub fixed: Vec<Vec<Pt>>,
}

impl Settings {
    /// Только вставка узлов, без эталона и замороженных колец.
    pub fn nodes_only(nodes: Nodes) -> Self {
        Self {
            nodes,
            mode: Mode::Insert,
            frozen: Vec::new(),
            fixed: Vec::new(),
        }
    }
}

/// Счётчики сшивки, как их печатает диалог модуля.
#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub vertices_in: usize,
    pub vertices_out: usize,
    pub vertices_moved: usize,
    pub max_move: f64,
    pub mean_move: f64,
    pub nodes_inserted: usize,
    pub nodes_crossing: usize,
    pub rings_degenerate: usize,
    pub rings_frozen: usize,
}

/// Итог сшивки.
pub struct Done {
    /// Кольца, замкнутые повтором первой вершины. `None` - кольцо
    /// выродилось.
    pub rings: Vec<Option<Vec<Pt>>>,
    /// События правок в порядке модуля: сначала слияния, потом узлы
    /// в пересечениях рёбер, потом узлы по вершинам.
    pub events: Vec<Insert>,
    pub stats: Stats,
}

impl Done {
    /// Сколько узлов вставлено всего.
    pub fn inserted(&self) -> usize {
        self.stats.nodes_inserted
    }
}

/// Сшивка набора колец. `rings` - кольца, замкнутые повтором первой
/// вершины в конце.
pub fn clean(rings: &[Vec<Pt>], opts: &Settings) -> Done {
    let tolerance = opts.nodes.tolerance;
    let mut done = Done {
        rings: rings.iter().map(|r| Some(r.clone())).collect(),
        events: Vec::new(),
        stats: Stats::default(),
    };
    if tolerance <= 0.0 || rings.is_empty() {
        return done;
    }
    let mut norm: Vec<Norm> = rings
        .iter()
        .map(|r| {
            let (coords, closed) = norm_ring(r);
            (Some(coords), closed)
        })
        .collect();
    done.stats.vertices_in = norm
        .iter()
        .map(|(c, _)| c.as_ref().map_or(0, Vec::len))
        .sum();
    done.stats.rings_frozen = opts.frozen.len();

    // ── Шаг 1. Слияние вершин ───────────────────────────────────────────
    if opts.mode.merges() {
        let mut grid = PointGrid::new(tolerance);
        // Эталон первым: его вершины гарантированно становятся лидерами.
        for r in &opts.fixed {
            let (coords, _) = norm_ring(r);
            for p in coords {
                if grid.nearest(p, tolerance).is_none() {
                    grid.add(p);
                }
            }
        }
        // Неизменяемые кольца обходятся до остальных: их вершины
        // становятся опорой, а сами они не двигаются.
        for &ri in &opts.frozen {
            if let Some(Some(coords)) = norm.get(ri).map(|(c, _)| c.clone()) {
                for p in coords {
                    grid.add(p);
                }
            }
        }
        let mut sum_move = 0.0;
        for (ri, (coords, _closed)) in norm.iter_mut().enumerate() {
            if opts.frozen.contains(&ri) {
                continue;
            }
            let Some(coords) = coords else { continue };
            for p in coords.iter_mut() {
                match grid.nearest(*p, tolerance) {
                    None => grid.add(*p),
                    Some((idx, dist)) => {
                        let leader = [grid.xs[idx], grid.ys[idx]];
                        if dist > EPS {
                            done.stats.vertices_moved += 1;
                            sum_move += dist;
                            done.stats.max_move = done.stats.max_move.max(dist);
                            done.events.push(Insert {
                                x: leader[0],
                                y: leader[1],
                                dist,
                                ring: ri,
                                kind: Kind::Move,
                            });
                        }
                        *p = leader;
                    }
                }
            }
        }
        if done.stats.vertices_moved > 0 {
            done.stats.mean_move = sum_move / done.stats.vertices_moved as f64;
        }
    }

    // ── Шаг 2. Чистка дублей и отбраковка вырожденных колец ─────────────
    for (coords, closed) in norm.iter_mut() {
        let Some(list) = coords else { continue };
        let cleaned = drop_repeats(list, *closed);
        if cleaned.len() < if *closed { 3 } else { 2 } {
            done.stats.rings_degenerate += 1;
            *coords = None;
        } else {
            *coords = Some(cleaned);
        }
    }

    // ── Шаг 3. Узлы ─────────────────────────────────────────────────────
    if opts.mode.inserts() {
        if opts.nodes.crossings {
            let pending = by_crossings(&norm, tolerance, opts.nodes.project_onto_edge);
            let mut cross = Vec::new();
            done.stats.nodes_crossing = apply(&mut norm, pending, &mut cross);
            for e in &mut cross {
                e.kind = Kind::Cross;
            }
            done.stats.nodes_inserted += done.stats.nodes_crossing;
            done.events.append(&mut cross);
        }
        let pending = by_vertices(&norm, tolerance, opts.nodes.project_onto_edge, &opts.fixed);
        let mut inserts = Vec::new();
        done.stats.nodes_inserted += apply(&mut norm, pending, &mut inserts);
        done.events.append(&mut inserts);
    }

    // ── Шаг 4. Сборка ───────────────────────────────────────────────────
    done.rings = norm
        .into_iter()
        .map(|(coords, closed)| {
            let coords = drop_repeats(&coords?, closed);
            if coords.len() < if closed { 3 } else { 2 } {
                done.stats.rings_degenerate += 1;
                return None;
            }
            let mut out = coords;
            if closed && let Some(&first) = out.first() {
                out.push(first);
            }
            done.stats.vertices_out += out.len();
            Some(out)
        })
        .collect();
    done
}

/// Вставка узлов без слияния вершин.
pub fn insert_nodes(rings: &[Vec<Pt>], opts: Nodes) -> Done {
    clean(rings, &Settings::nodes_only(opts))
}

/// События вставки узлов по вершинам, без изменения колец и без узлов
/// в пересечениях рёбер. Их читает проверка топологии.
pub fn insert_events(rings: &[Vec<Pt>], tolerance: f64) -> Vec<Insert> {
    if tolerance <= 0.0 || rings.is_empty() {
        return Vec::new();
    }
    let mut norm = normalize(rings);
    let pending = by_vertices(&norm, tolerance, false, &[]);
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
        assert_eq!(done.inserted(), 1);
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
        assert_eq!(done.inserted(), 0);
        assert_eq!(done.rings[0].clone().unwrap_or_default(), a);
        assert_eq!(done.rings[1].clone().unwrap_or_default(), b);
    }
}
