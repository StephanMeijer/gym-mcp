//! Body measurements over time (body weight for now).

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use sqlx::AssertSqlSafe;

use crate::db::{self, App, AppResult, Tx, user_err};
use crate::workout::{not_future, now, parse_time};

const KG_PER_LB: f64 = 0.453_592_37;

/// The JSON of one `measurement` row.
const ROW: &str = "json_build_object('id', id, 'kind', kind, 'value', value, 'unit', unit,
                                     'measured_at', measured_at, 'notes', NULLIF(notes, ''))";

#[derive(Deserialize, JsonSchema, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum MeasurementKind {
    #[default]
    BodyWeight,
}

impl MeasurementKind {
    fn as_str(self) -> &'static str {
        match self {
            MeasurementKind::BodyWeight => "body_weight",
        }
    }

    /// The unit values of this kind are stored in.
    fn unit(self) -> &'static str {
        match self {
            MeasurementKind::BodyWeight => "kg",
        }
    }
}

#[derive(Deserialize, JsonSchema, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    #[default]
    Kg,
    Lb,
}

fn to_kg(value: f64, unit: Unit) -> f64 {
    match unit {
        Unit::Kg => value,
        Unit::Lb => value * KG_PER_LB,
    }
}

async fn measured_at(tx: &mut Tx, s: &Option<String>) -> AppResult<Option<chrono::DateTime<chrono::Utc>>> {
    let Some(s) = s else { return Ok(None) };
    let at = parse_time(tx, s).await?;
    not_future(at, now(tx).await?)?;
    Ok(Some(at))
}

#[derive(Deserialize, JsonSchema)]
pub struct MeasurementAddArgs {
    /// The value in `unit` ("82.4", or 180 with unit lb).
    pub value: f64,
    /// Unit of `value`; lb is converted to kg. Default kg.
    #[serde(default)]
    pub unit: Unit,
    /// Default body_weight.
    #[serde(default)]
    pub kind: MeasurementKind,
    /// When it was measured, if not now. RFC 3339, or local time like "2026-11-02 07:30".
    pub measured_at: Option<String>,
    pub notes: Option<String>,
}

pub async fn measurement_add(app: &App, owner: &str, a: MeasurementAddArgs) -> AppResult<Value> {
    let mut tx = db::begin(app, owner).await?;
    let at = match measured_at(&mut tx, &a.measured_at).await? {
        Some(at) => at,
        None => now(&mut tx).await?,
    };
    let v: Value = sqlx::query_scalar(AssertSqlSafe(format!(
        "INSERT INTO measurement (owner_sub, kind, value, unit, measured_at, notes)
         VALUES ($1, $2, $3, $4, $5, coalesce($6, '')) RETURNING {ROW}"
    )))
    .bind(owner)
    .bind(a.kind.as_str())
    .bind(to_kg(a.value, a.unit))
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
    /// Unit of `value`; lb is converted to kg. Default kg.
    #[serde(default)]
    pub unit: Unit,
    pub measured_at: Option<String>,
    pub notes: Option<String>,
}

pub async fn measurement_update(app: &App, owner: &str, a: MeasurementUpdateArgs) -> AppResult<Value> {
    let mut tx = db::begin(app, owner).await?;
    let at = measured_at(&mut tx, &a.measured_at).await?;
    let v: Option<Value> = sqlx::query_scalar(AssertSqlSafe(format!(
        "UPDATE measurement SET value = coalesce($3, value), measured_at = coalesce($4, measured_at),
                                notes = coalesce($5, notes)
         WHERE id = $1 AND owner_sub = $2 RETURNING {ROW}"
    )))
    .bind(a.id)
    .bind(owner)
    .bind(a.value.map(|v| to_kg(v, a.unit)))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lb_converts_to_kg() {
        assert_eq!(to_kg(80.0, Unit::Kg), 80.0);
        assert!((to_kg(180.0, Unit::Lb) - 81.646_627).abs() < 1e-6);
    }
}
