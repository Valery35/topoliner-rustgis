// Topoliner для RustGIS - расширение-процесс RustGIS.
// Copyright (C) 2026 ООО «Информ++». Лицензия GPL-3.0-or-later.
// SPDX-License-Identifier: GPL-3.0-or-later

//! 2.02 Границы полигонов линиями. Перенос модуля `boundaries` плагина
//! QGIS.
//!
//! Обычный перевод полигонов в линии выдаёт границу дважды, по разу
//! от каждого соседа. Здесь граница между двумя телами выходит одной
//! линией, от узла до узла, и несёт признак того, с чем она граничит.

use std::collections::HashMap;

use rustgis_kit::Lang;
use serde_json::{Value as J, json};

use crate::arcs::{self, Path};
use crate::rings::Pt;
use crate::rounding::nice;

/// Что за граница.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Граница между двумя объектами.
    Shared,
    /// Внешний край покрытия.
    Outer,
    /// Край полости внутри объекта.
    Hole,
}

impl Kind {
    pub fn code(self) -> &'static str {
        match self {
            Kind::Shared => "shared",
            Kind::Outer => "outer",
            Kind::Hole => "hole",
        }
    }

    pub fn label_ru(self) -> &'static str {
        match self {
            Kind::Shared => "граница между объектами",
            Kind::Outer => "внешний край покрытия",
            Kind::Hole => "край полости",
        }
    }

    pub fn label_en(self) -> &'static str {
        match self {
            Kind::Shared => "border between objects",
            Kind::Outer => "outer edge of the coverage",
            Kind::Hole => "edge of a cavity",
        }
    }
}

/// Одна граница.
#[derive(Debug, Clone)]
pub struct Border {
    pub coords: Vec<Pt>,
    pub kind: Kind,
    pub fid_a: i64,
    pub fid_b: Option<i64>,
}

/// Кто пользуется каждой дугой, в порядке обхода колец.
pub fn users_of(paths: &[Path], arc_count: usize) -> Vec<Vec<usize>> {
    let mut users: Vec<Vec<usize>> = vec![Vec::new(); arc_count];
    for (ring_index, path) in paths.iter().enumerate() {
        for (arc_index, _reversed) in path {
            if let Some(slot) = users.get_mut(*arc_index) {
                slot.push(ring_index);
            }
        }
    }
    users
}

/// Извлекает границы покрытия. Каждая граница выдаётся один раз.
///
/// `items` - объекты покрытия: номер и части, часть это внешнее кольцо
/// и полости. `node_eps` - отклонение, в пределах которого вершина
/// считается лежащей на ребре соседа, ноль отключает достройку узлов.
pub fn extract_boundaries(items: &[arcs::Shape], grid: f64, node_eps: f64) -> Vec<Border> {
    let (mut rings, owners) = arcs::rings_with_owners(items);
    if rings.is_empty() {
        return Vec::new();
    }
    if node_eps > 0.0 {
        rings = arcs::insert_missing_nodes(&rings, node_eps, 8);
    }
    let (arc_list, paths) = arcs::build_arcs(&rings, grid);
    let users = users_of(&paths, arc_list.len());

    let mut out = Vec::new();
    for (arc_index, arc) in arc_list.iter().enumerate() {
        let holders = &users[arc_index];
        let Some(&first) = holders.first() else {
            continue;
        };
        let (fid_a, ring_a) = owners[first];
        let (kind, fid_b) = match holders.get(1) {
            Some(&second) => (Kind::Shared, Some(owners[second].0)),
            // Кольцо с номером больше нуля это полость объекта.
            None if ring_a > 0 => (Kind::Hole, None),
            None => (Kind::Outer, None),
        };
        out.push(Border {
            coords: arc.clone(),
            kind,
            fid_a,
            fid_b,
        });
    }
    out
}

/// Длина ломаной.
pub fn length_of(coords: &[Pt]) -> f64 {
    coords
        .windows(2)
        .map(|w| (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1]))
        .sum()
}

/// Точки линии для выхода прогона.
pub fn points_of(coords: &[Pt]) -> J {
    J::Array(
        coords
            .iter()
            .map(|p| json!([p[0], p[1], J::Null]))
            .collect(),
    )
}

/// Границы линейным слоем. `field` - поле, значения которого пишутся
/// по обе стороны границы, `values` - эти значения по номеру объекта.
pub fn borders_layer(
    borders: &[Border],
    field: Option<&str>,
    values: &HashMap<i64, String>,
    lang: Lang,
) -> J {
    let mut fields = vec![
        json!(["kind", "string"]),
        json!(["label", "string"]),
        json!(["fid_a", "long"]),
        json!(["fid_b", "long"]),
        json!(["length", "double"]),
    ];
    if field.is_some() {
        fields.push(json!(["val_a", "string"]));
        fields.push(json!(["val_b", "string"]));
    }
    let features: Vec<J> = borders
        .iter()
        .filter(|b| b.coords.len() >= 2)
        .map(|b| {
            let label = match lang {
                Lang::Ru => b.kind.label_ru(),
                Lang::En => b.kind.label_en(),
            };
            let mut vals = vec![
                json!(b.kind.code()),
                json!(label),
                json!(b.fid_a),
                json!(b.fid_b.unwrap_or(-1)),
                json!(nice(length_of(&b.coords))),
            ];
            if field.is_some() {
                let at = |fid: Option<i64>| {
                    fid.and_then(|f| values.get(&f))
                        .cloned()
                        .unwrap_or_default()
                };
                vals.push(json!(at(Some(b.fid_a))));
                vals.push(json!(at(b.fid_b)));
            }
            json!({"values": vals, "geometry": {"points": points_of(&b.coords)}})
        })
        .collect();
    json!({"geometry": "line", "fields": fields, "features": features})
}

/// Строки журнала, как их печатает диалог модуля.
pub fn report(items: usize, borders: &[Border], lang: Lang) -> Vec<String> {
    let count = |k: Kind| borders.iter().filter(|b| b.kind == k).count();
    let say = |ru: &str, en: &str, n: usize| {
        match lang {
            Lang::Ru => ru,
            Lang::En => en,
        }
        .replacen("%d", &n.to_string(), 1)
    };
    vec![
        match lang {
            Lang::Ru => "Чтение слоя...".to_string(),
            Lang::En => "Reading the layer...".to_string(),
        },
        say("Объектов: %d", "Objects: %d", items),
        String::new(),
        match lang {
            Lang::Ru => "── Результат ──".to_string(),
            Lang::En => "-- Result --".to_string(),
        },
        say(
            "Границ между объектами: %d",
            "Borders between objects: %d",
            count(Kind::Shared),
        ),
        say(
            "Внешний край покрытия:  %d",
            "Outer edge of the coverage: %d",
            count(Kind::Outer),
        ),
        say(
            "Краёв полостей:         %d",
            "Cavity edges:              %d",
            count(Kind::Hole),
        ),
        say(
            "Всего линий:            %d",
            "Lines in total:            %d",
            borders.len(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(x: f64, y: f64, w: f64) -> Vec<Pt> {
        vec![[x, y], [x + w, y], [x + w, y + w], [x, y + w]]
    }

    /// Полигон с островом: внешний край и общая граница с островом.
    #[test]
    fn an_island_shares_the_cavity_edge() {
        let shell = vec![vec![square(0.0, 0.0, 30.0), square(10.0, 10.0, 10.0)]];
        let island = vec![vec![square(10.0, 10.0, 10.0)]];
        let got = extract_boundaries(&[(1, shell), (2, island)], 1e-6, 1e-6);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0].kind, Kind::Outer);
        assert_eq!(got[0].fid_a, 1);
        assert_eq!(got[1].kind, Kind::Shared);
        assert_eq!((got[1].fid_a, got[1].fid_b), (1, Some(2)));
    }
}
