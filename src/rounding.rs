// Topoliner для RustGIS - расширение-процесс RustGIS.
// Copyright (C) 2026 ООО «Информ++». Лицензия GPL-3.0-or-later.
// SPDX-License-Identifier: GPL-3.0-or-later

//! Округление измеренных величин до значащих цифр. Перенос модуля
//! `rounding` плагина QGIS.
//!
//! Величины округляются до значащих цифр, а не до знаков после запятой.
//! Фиксированное количество знаков одинаково плохо работает на разных
//! масштабах. Целые величины не округляются вовсе: в поле `value` наряду
//! с площадью и смещением попадает количество объектов.

/// Четыре значащие цифры, как в модуле.
pub const DIGITS: i32 = 4;

/// Предел на количество знаков после запятой.
const MAX_PLACES: i32 = 12;

/// Количество знаков после запятой для заданного количества значащих цифр.
pub fn places_for(value: f64, digits: i32) -> usize {
    if !value.is_finite() || value == 0.0 {
        return 0;
    }
    let exponent = value.abs().log10().floor() as i32;
    (digits - 1 - exponent).clamp(0, MAX_PLACES) as usize
}

/// Округлённая величина. Целое возвращается как есть.
///
/// Округление идёт через десятичную запись: так же, как `round(x, n)`
/// в Python, к ближайшему с разрешением спора к чётному.
pub fn nice(value: f64) -> f64 {
    if !value.is_finite() || value == value.trunc() {
        return value;
    }
    let places = places_for(value, DIGITS);
    format!("{value:.places$}").parse().unwrap_or(value)
}

/// Величина строкой, без хвостовых нулей.
pub fn fmt(value: f64) -> String {
    if !value.is_finite() {
        // в модуле это str(float), то есть inf, -inf и nan
        return format!("{value}");
    }
    if value == value.trunc() {
        return format!("{}", value as i64);
    }
    let places = places_for(value, DIGITS);
    let text = format!("{value:.places$}");
    let text = if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.')
    } else {
        &text
    };
    if text.is_empty() {
        "0".to_string()
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn significant_digits_not_decimal_places() {
        assert_eq!(fmt(0.49884683), "0.4988");
        assert_eq!(fmt(1.0), "1");
        assert_eq!(fmt(0.0), "0");
        assert_eq!(fmt(12345.6789), "12346");
        assert_eq!(nice(0.49884683), 0.4988);
        assert_eq!(nice(2.0), 2.0);
        // количество не округляется: оно целое
        assert_eq!(nice(12345.0), 12345.0);
        assert_eq!(places_for(0.0000038, DIGITS), 9);
    }
}
