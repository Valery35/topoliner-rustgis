// Topoliner для RustGIS - расширение-процесс RustGIS.
// Copyright (C) 2026 ООО «Информ++». Лицензия GPL-3.0-or-later.
// SPDX-License-Identifier: GPL-3.0-or-later

//! Полигональные операции. То же место, что `geom_backend` в модуле QGIS,
//! только за ним не GEOS, а крейт `geo`.
//!
//! Геометрия здесь всегда мультиполигон. Пустой мультиполигон означает
//! пустую геометрию. Операции повторяют набор `_BackendBase` модуля
//! в той части, которую зовёт проверка топологии.
//!
//! Причина некорректности выдаётся словами GEOS, потому что её видит
//! пользователь в поле `note`, и прогоны модуля сверяются с ней дословно.
//! Связность внутренности крейт `geo` не проверяет, и этим его правила
//! от GEOS отличаются.

use geo::algorithm::validation::{InvalidPolygon, RingRole, Validation};
use geo::algorithm::repair_polygon::MakeValid;
use geo::{
    Area, BooleanOps, BoundingRect, Contains, Coord, InteriorPoint, LineString, MultiPolygon,
    Polygon, Relate, unary_union,
};

use crate::rings::Pt;

/// Геометрия объекта.
pub type Geom = MultiPolygon<f64>;

/// Пустая геометрия.
pub fn empty() -> Geom {
    MultiPolygon::new(Vec::new())
}

/// Кольцо координат в кольцо крейта. Кольцо приходит без повтора первой
/// вершины, здесь оно замыкается.
fn ring_of(points: &[Pt]) -> LineString<f64> {
    let mut cs: Vec<Coord<f64>> = points.iter().map(|p| Coord { x: p[0], y: p[1] }).collect();
    if let Some(&first) = cs.first()
        && cs.last() != Some(&first)
    {
        cs.push(first);
    }
    LineString::new(cs)
}

/// Часть из колец: внешнее кольцо, затем дыры.
pub fn polygon_of(rings: &[Vec<Pt>]) -> Option<Polygon<f64>> {
    let outer = rings.first()?;
    if outer.len() < 3 {
        return None;
    }
    let holes: Vec<LineString<f64>> = rings[1..]
        .iter()
        .filter(|r| r.len() >= 3)
        .map(|r| ring_of(r))
        .collect();
    Some(Polygon::new(ring_of(outer), holes))
}

/// Геометрия из частей: часть это внешнее кольцо и дыры.
pub fn geom_of(parts: &[Vec<Vec<Pt>>]) -> Geom {
    MultiPolygon::new(parts.iter().filter_map(|p| polygon_of(p)).collect())
}

/// Одиночные полигоны геометрии.
pub fn parts(g: &Geom) -> Vec<Polygon<f64>> {
    g.0.clone()
}

/// Кольца одиночного полигона: внешнее, затем дыры. Вершины идут
/// с повтором первой в конце, как их отдаёт GEOS.
pub fn rings_of(p: &Polygon<f64>) -> Vec<Vec<Pt>> {
    let one = |ls: &LineString<f64>| -> Vec<Pt> { ls.0.iter().map(|c| [c.x, c.y]).collect() };
    let mut out = vec![one(p.exterior())];
    out.extend(p.interiors().iter().map(one));
    out
}

pub fn is_empty(g: &Geom) -> bool {
    g.0.is_empty() || g.0.iter().all(|p| p.exterior().0.len() < 4)
}

pub fn area(g: &Geom) -> f64 {
    g.unsigned_area()
}

/// Длина границы: периметр внешних колец и дыр.
pub fn boundary_length(g: &Geom) -> f64 {
    let mut total = 0.0;
    for p in &g.0 {
        for ls in std::iter::once(p.exterior()).chain(p.interiors()) {
            for w in ls.0.windows(2) {
                total += (w[1].x - w[0].x).hypot(w[1].y - w[0].y);
            }
        }
    }
    total
}

/// Охват: (xmin, ymin, xmax, ymax).
pub fn bounds(g: &Geom) -> Option<(f64, f64, f64, f64)> {
    let r = g.bounding_rect()?;
    Some((r.min().x, r.min().y, r.max().x, r.max().y))
}

pub fn is_valid(g: &Geom) -> bool {
    g.0.iter().all(Validation::is_valid)
}

/// Причина некорректности словами GEOS.
pub fn invalid_reason(g: &Geom) -> String {
    for p in &g.0 {
        if let Err(e) = p.check_validation() {
            return geos_words(&e);
        }
    }
    "Invalid geometry".to_string()
}

fn geos_words(e: &InvalidPolygon) -> String {
    match e {
        InvalidPolygon::TooFewPointsInRing(_) => "Too few points in geometry component",
        InvalidPolygon::SelfIntersection(RingRole::Exterior) => "Self-intersection",
        InvalidPolygon::SelfIntersection(_) => "Ring Self-intersection",
        InvalidPolygon::NonFiniteCoord(..) => "Invalid Coordinate",
        InvalidPolygon::InteriorRingNotContainedInExteriorRing(_) => "Hole lies outside shell",
        InvalidPolygon::IntersectingRingsOnALine(..) => "Self-intersection",
        InvalidPolygon::IntersectingRingsOnAnArea(..) => "Holes are nested",
    }
    .to_string()
}

/// Размер общего охвата набора геометрий.
///
/// Сетка булевой операции строится по охвату всех её участников сразу,
/// поэтому и шаг округления итога считается по нему же. Размер
/// отдельной геометрии тут не годится: на разнесённых объектах он
/// намного меньше общего, и след перевода в целые остаётся в итоге.
fn extent_of(geoms: &[&Geom]) -> f64 {
    let mut all: Option<(f64, f64, f64, f64)> = None;
    for g in geoms {
        let Some(b) = bounds(g) else { continue };
        all = Some(match all {
            None => b,
            Some(a) => (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)),
        });
    }
    all.map_or(0.0, |(x0, y0, x1, y1)| (x1 - x0).max(y1 - y0))
}

/// Булевы операции идут по сетке, шаг которой зависит от охвата: крейт
/// `i_overlay` переводит координаты в целые числа. Ровная координата
/// входа возвращается со следом этого перевода, порядка охвата на два
/// в тридцать первой. Итог округляется до разряда, который заведомо
/// крупнее шага сетки и заведомо мельче любой осмысленной величины.
fn snapped(g: Geom, extent: f64) -> Geom {
    if extent <= 0.0 || !extent.is_finite() {
        return g;
    }
    let places = (8 - extent.log10().floor() as i32).clamp(0, 12) as usize;
    let at = |v: f64| -> f64 { format!("{v:.places$}").parse().unwrap_or(v) };
    MultiPolygon::new(
        g.0.into_iter()
            .map(|p| {
                let ring = |ls: &LineString<f64>| {
                    LineString::new(
                        ls.0.iter()
                            .map(|c| Coord {
                                x: at(c.x),
                                y: at(c.y),
                            })
                            .collect(),
                    )
                };
                let holes: Vec<LineString<f64>> = p.interiors().iter().map(ring).collect();
                Polygon::new(ring(p.exterior()), holes)
            })
            .collect(),
    )
}

pub fn intersection(a: &Geom, b: &Geom) -> Geom {
    snapped(a.intersection(b), extent_of(&[a, b]))
}

pub fn difference(a: &Geom, b: &Geom) -> Geom {
    snapped(a.difference(b), extent_of(&[a, b]))
}

/// Исправление некорректной геометрии, как `makeValid` в QGIS. Крейт
/// `geo` правит полигон триангуляцией, GEOS строит объединение колец,
/// и на сложных случаях их итоги различаются.
pub fn make_valid(g: &Geom) -> Geom {
    let mut out: Vec<Polygon<f64>> = Vec::new();
    for p in &g.0 {
        if p.is_valid() {
            out.push(p.clone());
            continue;
        }
        match p.make_valid() {
            Ok(fixed) => out.extend(fixed.0),
            // Исправить не удалось, объект остаётся как был.
            Err(_) => out.push(p.clone()),
        }
    }
    MultiPolygon::new(out)
}

pub fn union_all(geoms: &[Geom]) -> Geom {
    let live: Vec<&Geom> = geoms.iter().filter(|g| !is_empty(g)).collect();
    if live.is_empty() {
        return empty();
    }
    let extent = extent_of(&live);
    snapped(unary_union(live), extent)
}

/// Длина общей границы двух геометрий.
///
/// GEOS считает её пересечением двух границ, у крейта `geo` пересечения
/// линий нет. Здесь берутся рёбра первой геометрии, середина которых
/// лежит на границе второй, и складываются их длины. Для выбора соседа
/// с наибольшей общей границей этого достаточно.
pub fn shared_boundary(a: &Geom, b: &Geom) -> f64 {
    let mut edges: Vec<(Pt, Pt)> = Vec::new();
    for p in &b.0 {
        for ls in std::iter::once(p.exterior()).chain(p.interiors()) {
            for w in ls.0.windows(2) {
                edges.push(([w[0].x, w[0].y], [w[1].x, w[1].y]));
            }
        }
    }
    let mut total = 0.0;
    for p in &a.0 {
        for ls in std::iter::once(p.exterior()).chain(p.interiors()) {
            for w in ls.0.windows(2) {
                let mid = [0.5 * (w[0].x + w[1].x), 0.5 * (w[0].y + w[1].y)];
                let on = edges.iter().any(|&(c, d)| {
                    crate::rings::point_segment(mid, c, d).0 <= 1e-9
                });
                if on {
                    total += (w[1].x - w[0].x).hypot(w[1].y - w[0].y);
                }
            }
        }
    }
    total
}

pub fn intersects(a: &Geom, b: &Geom) -> bool {
    a.relate(b).is_intersects()
}

pub fn equals(a: &Geom, b: &Geom) -> bool {
    a.relate(b).is_equal_topo()
}

pub fn contains(a: &Geom, b: &Geom) -> bool {
    a.contains(b)
}

/// Точка для показа находки: внутренняя точка геометрии, как её даёт
/// `pointOnSurface` в QGIS. Для вырожденной и некорректной геометрии
/// GEOS отдаёт первую вершину, и здесь порядок тот же.
pub fn point_on_surface(g: &Geom) -> Option<Pt> {
    if is_empty(g) {
        return None;
    }
    if is_valid(g)
        && let Some(p) = g.interior_point()
    {
        return Some([p.x(), p.y()]);
    }
    // Некорректная геометрия: GEOS возвращает первую вершину.
    first_vertex(g)
}

fn first_vertex(g: &Geom) -> Option<Pt> {
    let c = g.0.first()?.exterior().0.first()?;
    Some([c.x, c.y])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(x: f64, y: f64, w: f64) -> Vec<Vec<Pt>> {
        vec![vec![[x, y], [x + w, y], [x + w, y + w], [x, y + w]]]
    }

    #[test]
    fn overlap_of_two_squares() {
        let a = geom_of(&[square(0.0, 0.0, 10.0)]);
        let b = geom_of(&[square(9.9, 0.0, 10.1)]);
        assert!(intersects(&a, &b));
        assert!(!equals(&a, &b) && !contains(&a, &b));
        let inter = intersection(&a, &b);
        assert!((area(&inter) - 1.0).abs() < 1e-9, "{:?}", inter);
        // внутренняя точка полосы - её середина, как у GEOS
        assert_eq!(point_on_surface(&inter), Some([9.95, 5.0]));
        // объединение покрытия: одна часть без дыр
        let u = union_all(&[a, b]);
        assert_eq!(parts(&u).len(), 1);
        assert!((area(&u) - 201.01).abs() < 1e-9, "{}", area(&u));
    }

    #[test]
    fn a_self_touching_ring_is_invalid() {
        let g = geom_of(&[vec![vec![
            [300.0, 0.0],
            [310.0, 0.0],
            [310.0, 10.0],
            [305.0, 10.0],
            [305.0001, 25.0],
            [305.0, 10.0],
            [300.0, 10.0],
        ]]]);
        assert!(!is_valid(&g));
        assert_eq!(invalid_reason(&g), "Self-intersection");
        assert!((area(&g) - 100.0).abs() < 1e-9);
        assert_eq!(point_on_surface(&g), Some([300.0, 0.0]));
    }
}
