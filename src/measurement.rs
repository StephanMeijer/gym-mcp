//! Body measurements over time: weight, height, waist, body fat, resting heart rate, blood pressure.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use sqlx::AssertSqlSafe;

use crate::db::{self, App, AppResult, Tx, user_err};
use crate::workout::{now, optional_past_time};

const KG_PER_LB: f64 = 0.453_592_37;
const CM_PER_IN: f64 = 2.54;

/// The JSON of one `measurement` row.
const ROW: &str = "json_build_object('id', id, 'kind', kind, 'value', value, 'unit', unit,
                                     'measured_at', measured_at, 'notes', NULLIF(notes, ''))";

#[derive(Deserialize, JsonSchema, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum MeasurementKind {
    #[default]
    BodyWeight,
    /// Body height ("length").
    BodyHeight,
    /// Waist circumference, at the navel.
    WaistCircumference,
    /// Body fat percentage.
    BodyFat,
    /// Resting heart rate, ideally in the morning before getting up.
    RestingHeartRate,
    /// Blood pressure, upper number ("128/82" → 128).
    BloodPressureSystolic,
    /// Blood pressure, lower number ("128/82" → 82).
    BloodPressureDiastolic,
}

impl MeasurementKind {
    fn as_str(self) -> &'static str {
        match self {
            MeasurementKind::BodyWeight => "body_weight",
            MeasurementKind::BodyHeight => "body_height",
            MeasurementKind::WaistCircumference => "waist_circumference",
            MeasurementKind::BodyFat => "body_fat",
            MeasurementKind::RestingHeartRate => "resting_heart_rate",
            MeasurementKind::BloodPressureSystolic => "blood_pressure_systolic",
            MeasurementKind::BloodPressureDiastolic => "blood_pressure_diastolic",
        }
    }

    fn from_db(s: &str) -> AppResult<Self> {
        serde_json::from_value(Value::String(s.to_owned())).map_err(|_| user_err(format!("unknown kind {s}")))
    }

    /// The unit values of this kind are stored in.
    fn unit(self) -> &'static str {
        match self {
            MeasurementKind::BodyWeight => "kg",
            MeasurementKind::BodyHeight | MeasurementKind::WaistCircumference => "cm",
            MeasurementKind::BodyFat => "%",
            MeasurementKind::RestingHeartRate => "bpm",
            MeasurementKind::BloodPressureSystolic | MeasurementKind::BloodPressureDiastolic => "mmHg",
        }
    }

    /// Accepted input units and their factor to the stored unit.
    fn units(self) -> &'static [(Unit, f64)] {
        match self {
            MeasurementKind::BodyWeight => &[(Unit::Kg, 1.0), (Unit::Lb, KG_PER_LB)],
            MeasurementKind::BodyHeight | MeasurementKind::WaistCircumference => {
                &[(Unit::Cm, 1.0), (Unit::In, CM_PER_IN)]
            }
            MeasurementKind::BodyFat => &[(Unit::Percent, 1.0)],
            MeasurementKind::RestingHeartRate => &[(Unit::Bpm, 1.0)],
            MeasurementKind::BloodPressureSystolic | MeasurementKind::BloodPressureDiastolic => &[(Unit::Mmhg, 1.0)],
        }
    }
}

#[derive(Deserialize, JsonSchema, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    Kg,
    Lb,
    Cm,
    In,
    Percent,
    Bpm,
    Mmhg,
}

/// `value` in `unit` (default: the kind's own unit) converted to the kind's stored unit.
fn convert(kind: MeasurementKind, value: f64, unit: Option<Unit>) -> AppResult<f64> {
    let Some(unit) = unit else { return Ok(value) };
    match kind.units().iter().find(|(u, _)| *u == unit) {
        Some((_, factor)) => Ok(value * factor),
        None => Err(user_err(format!(
            "that unit doesn't fit {}; use {}",
            kind.as_str(),
            kind.units()
                .iter()
                .map(|(u, _)| format!("{u:?}").to_lowercase())
                .collect::<Vec<_>>()
                .join(" or ")
        ))),
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct MeasurementAddArgs {
    /// The value in `unit` ("82.4", or 180 with unit lb).
    pub value: f64,
    /// Unit of `value`, default the kind's own: body_weight kg (or lb), body_height / waist_circumference cm (or in),
    /// body_fat percent, resting_heart_rate bpm, blood pressure mmhg. lb and in are converted.
    pub unit: Option<Unit>,
    /// Default body_weight.
    #[serde(default)]
    pub kind: MeasurementKind,
    /// When it was measured, if not now. RFC 3339, or local time like "2026-11-02 07:30".
    pub measured_at: Option<String>,
    pub notes: Option<String>,
}

pub async fn measurement_add(app: &App, owner: &str, a: MeasurementAddArgs) -> AppResult<Value> {
    let mut tx = db::begin(app, owner).await?;
    let at = match optional_past_time(&mut tx, &a.measured_at).await? {
        Some(at) => at,
        None => now(&mut tx).await?,
    };
    let v: Value = sqlx::query_scalar(AssertSqlSafe(format!(
        "INSERT INTO measurement (owner_sub, kind, value, unit, measured_at, notes)
         VALUES ($1, $2, $3, $4, $5, coalesce($6, '')) RETURNING {ROW}"
    )))
    .bind(owner)
    .bind(a.kind.as_str())
    .bind(convert(a.kind, a.value, a.unit)?)
    .bind(a.kind.unit())
    .bind(at)
    .bind(&a.notes)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(v)
}

#[derive(Deserialize, JsonSchema)]
pub struct MeasurementListArgs {
    /// Default body_weight.
    #[serde(default)]
    pub kind: MeasurementKind,
    /// From (inclusive). RFC 3339 or local time/date like "2026-10-01".
    pub from: Option<String>,
    /// To (exclusive).
    pub to: Option<String>,
}

pub async fn measurement_list(app: &App, owner: &str, a: MeasurementListArgs) -> AppResult<Value> {
    let mut tx = db::begin_read(app, owner).await?;
    let v: Value = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT coalesce(json_agg({ROW} ORDER BY measured_at DESC), '[]'::json) FROM measurement
         WHERE owner_sub = $1 AND kind = $2
           AND measured_at >= coalesce($3::timestamptz, '-infinity')
           AND measured_at < coalesce($4::timestamptz, 'infinity')"
    )))
    .bind(owner)
    .bind(a.kind.as_str())
    .bind(&a.from)
    .bind(&a.to)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(v)
}

#[derive(Deserialize, JsonSchema)]
pub struct MeasurementUpdateArgs {
    pub id: i64,
    pub value: Option<f64>,
    /// Unit of `value`; default the measurement's own (kg / cm).
    pub unit: Option<Unit>,
    pub measured_at: Option<String>,
    pub notes: Option<String>,
}

pub async fn measurement_update(app: &App, owner: &str, a: MeasurementUpdateArgs) -> AppResult<Value> {
    let mut tx = db::begin(app, owner).await?;
    let kind: Option<String> = sqlx::query_scalar("SELECT kind FROM measurement WHERE id = $1 AND owner_sub = $2")
        .bind(a.id)
        .bind(owner)
        .fetch_optional(&mut *tx)
        .await?;
    let kind = MeasurementKind::from_db(&kind.ok_or_else(|| user_err(format!("no measurement with id {}", a.id)))?)?;
    let value = a.value.map(|v| convert(kind, v, a.unit)).transpose()?;
    let at = optional_past_time(&mut tx, &a.measured_at).await?;
    let v: Option<Value> = sqlx::query_scalar(AssertSqlSafe(format!(
        "UPDATE measurement SET value = coalesce($3, value), measured_at = coalesce($4, measured_at),
                                notes = coalesce($5, notes)
         WHERE id = $1 AND owner_sub = $2 RETURNING {ROW}"
    )))
    .bind(a.id)
    .bind(owner)
    .bind(value)
    .bind(at)
    .bind(&a.notes)
    .fetch_optional(&mut *tx)
    .await?;
    let v = v.ok_or_else(|| user_err(format!("no measurement with id {}", a.id)))?;
    tx.commit().await?;
    Ok(v)
}

#[derive(Deserialize, JsonSchema)]
pub struct MeasurementDeleteArgs {
    pub id: i64,
}

pub async fn measurement_delete(app: &App, owner: &str, a: MeasurementDeleteArgs) -> AppResult<Value> {
    let mut tx = db::begin(app, owner).await?;
    let v: Option<Value> = sqlx::query_scalar(AssertSqlSafe(format!(
        "DELETE FROM measurement WHERE id = $1 AND owner_sub = $2 RETURNING {ROW}"
    )))
    .bind(a.id)
    .bind(owner)
    .fetch_optional(&mut *tx)
    .await?;
    let v = v.ok_or_else(|| user_err(format!("no measurement with id {}", a.id)))?;
    tx.commit().await?;
    Ok(serde_json::json!({"deleted": v}))
}

/// Latest body weight and its change against the measurement closest to 4 weeks before it;
/// null without data, change/since null with only one measurement. Used by training_overview.
pub async fn body_weight_json(tx: &mut Tx, owner: &str) -> AppResult<Value> {
    let v: Option<Value> = sqlx::query_scalar(
        "WITH bw AS (SELECT value, measured_at FROM measurement WHERE owner_sub = $1 AND kind = 'body_weight'),
         latest AS (SELECT * FROM bw ORDER BY measured_at DESC LIMIT 1),
         base AS (SELECT b.* FROM bw b, latest l WHERE b.measured_at < l.measured_at
                  ORDER BY abs(extract(epoch FROM b.measured_at - (l.measured_at - interval '28 days'))) LIMIT 1)
         SELECT json_build_object('latest_kg', l.value, 'measured_at', l.measured_at,
                                  'change_4w_kg', round((l.value - b.value)::numeric, 2), 'since', b.measured_at)
         FROM latest l LEFT JOIN base b ON true",
    )
    .bind(owner)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(v.unwrap_or(Value::Null))
}

/// Per kind other than body weight: the latest value and the average over the last 28 days.
/// Null without data. Used by training_overview.
pub async fn latest_json(tx: &mut Tx, owner: &str) -> AppResult<Value> {
    let v: Option<Value> = sqlx::query_scalar(
        "SELECT json_object_agg(m.kind, json_build_object(
                    'value', m.value, 'unit', m.unit, 'measured_at', m.measured_at,
                    'avg_28d', (SELECT round(avg(a.value)::numeric, 1) FROM measurement a
                                WHERE a.owner_sub = $1 AND a.kind = m.kind AND a.measured_at >= now() - interval '28 days')))
         FROM (SELECT DISTINCT ON (kind) * FROM measurement WHERE owner_sub = $1 AND kind <> 'body_weight'
               ORDER BY kind, measured_at DESC) m",
    )
    .bind(owner)
    .fetch_one(&mut **tx)
    .await?;
    Ok(v.unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_convert_per_kind() {
        let (w, h) = (MeasurementKind::BodyWeight, MeasurementKind::BodyHeight);
        assert_eq!(convert(w, 80.0, None).ok(), Some(80.0));
        assert!((convert(w, 180.0, Some(Unit::Lb)).ok().unwrap() - 81.646_627).abs() < 1e-6);
        assert_eq!(convert(h, 183.0, None).ok(), Some(183.0));
        assert!((convert(h, 72.0, Some(Unit::In)).ok().unwrap() - 182.88).abs() < 1e-9);
        assert!(convert(h, 80.0, Some(Unit::Kg)).is_err());
        assert!(convert(w, 183.0, Some(Unit::Cm)).is_err());
        assert!(
            (convert(MeasurementKind::WaistCircumference, 34.0, Some(Unit::In))
                .ok()
                .unwrap()
                - 86.36)
                .abs()
                < 1e-9
        );
        assert_eq!(
            convert(MeasurementKind::BodyFat, 18.5, Some(Unit::Percent)).ok(),
            Some(18.5)
        );
        assert_eq!(
            convert(MeasurementKind::BloodPressureSystolic, 128.0, Some(Unit::Mmhg)).ok(),
            Some(128.0)
        );
        assert!(convert(MeasurementKind::RestingHeartRate, 52.0, Some(Unit::Mmhg)).is_err());
    }
}
