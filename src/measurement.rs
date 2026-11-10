//! Body measurements over time: body fat and blood pressure.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use sqlx::AssertSqlSafe;

use crate::db::{self, App, AppResult, Tx, user_err};
use crate::workout::{now, optional_past_time};

/// The JSON of one `measurement` row.
const ROW: &str = "json_build_object('id', id, 'kind', kind, 'value', value, 'unit', unit,
                                     'measured_at', measured_at, 'notes', NULLIF(notes, ''))";

#[derive(Deserialize, JsonSchema, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum MeasurementKind {
    /// Body fat percentage.
    BodyFat,
    /// Blood pressure, upper number ("128/82" → 128).
    BloodPressureSystolic,
    /// Blood pressure, lower number ("128/82" → 82).
    BloodPressureDiastolic,
}

impl MeasurementKind {
    fn as_str(self) -> &'static str {
        match self {
            MeasurementKind::BodyFat => "body_fat",
            MeasurementKind::BloodPressureSystolic => "blood_pressure_systolic",
            MeasurementKind::BloodPressureDiastolic => "blood_pressure_diastolic",
        }
    }

    fn from_db(s: &str) -> AppResult<Self> {
        serde_json::from_value(Value::String(s.to_owned())).map_err(|_| user_err(format!("unknown kind {s}")))
    }

    /// The unit values of this kind are stored in: the first of `units`.
    fn unit(self) -> &'static str {
        self.units()[0].0.as_str()
    }

    /// Accepted input units and their factor to the stored unit, which comes first.
    fn units(self) -> &'static [(Unit, f64)] {
        match self {
            MeasurementKind::BodyFat => &[(Unit::Percent, 1.0)],
            MeasurementKind::BloodPressureSystolic | MeasurementKind::BloodPressureDiastolic => &[(Unit::Mmhg, 1.0)],
        }
    }
}

#[derive(Deserialize, JsonSchema, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    Percent,
    Mmhg,
}

impl Unit {
    /// As stored in `measurement.unit`.
    fn as_str(self) -> &'static str {
        match self {
            Unit::Percent => "%",
            Unit::Mmhg => "mmHg",
        }
    }
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
#[serde(deny_unknown_fields)]
pub struct MeasurementAddArgs {
    /// The value in `unit` ("18.5").
    pub value: f64,
    /// Unit of `value`, default the kind's own: body_fat percent, blood pressure mmhg.
    pub unit: Option<Unit>,
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
#[serde(deny_unknown_fields)]
pub struct MeasurementListArgs {
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
#[serde(deny_unknown_fields)]
pub struct MeasurementUpdateArgs {
    pub id: i64,
    pub value: Option<f64>,
    /// Unit of `value`; default the measurement's own.
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
#[serde(deny_unknown_fields)]
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

/// Per kind: the latest value and the average over the last 28 days.
/// Null without data. Used by training_overview.
pub async fn latest_json(tx: &mut Tx, owner: &str) -> AppResult<Value> {
    let v: Option<Value> = sqlx::query_scalar(
        "SELECT json_object_agg(m.kind, json_build_object(
                    'value', m.value, 'unit', m.unit, 'measured_at', m.measured_at,
                    'avg_28d', (SELECT round(avg(a.value)::numeric, 1) FROM measurement a
                                WHERE a.owner_sub = $1 AND a.kind = m.kind AND a.measured_at >= now() - interval '28 days')))
         FROM (SELECT DISTINCT ON (kind) * FROM measurement WHERE owner_sub = $1
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
        let (f, s) = (MeasurementKind::BodyFat, MeasurementKind::BloodPressureSystolic);
        assert_eq!(convert(f, 18.5, None).ok(), Some(18.5));
        assert_eq!(convert(f, 18.5, Some(Unit::Percent)).ok(), Some(18.5));
        assert_eq!(convert(s, 128.0, Some(Unit::Mmhg)).ok(), Some(128.0));
        assert!(convert(f, 128.0, Some(Unit::Mmhg)).is_err());
        assert!(convert(s, 18.5, Some(Unit::Percent)).is_err());
    }

    #[test]
    fn stored_units_match_the_schema() {
        // The (kind, unit) pairs measurement_check allows (migration 0018). No wildcard: a new kind must add its pair.
        let schema_unit = |kind: MeasurementKind| match kind {
            MeasurementKind::BodyFat => "%",
            MeasurementKind::BloodPressureSystolic | MeasurementKind::BloodPressureDiastolic => "mmHg",
        };
        for kind in [
            MeasurementKind::BodyFat,
            MeasurementKind::BloodPressureSystolic,
            MeasurementKind::BloodPressureDiastolic,
        ] {
            assert_eq!(kind.unit(), schema_unit(kind), "{}", kind.as_str());
        }
    }
}
