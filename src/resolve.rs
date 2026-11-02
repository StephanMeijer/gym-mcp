//! Turns what a person says ("trianmore ndsm", "benchpres", "17") into a row id.

// Dynamic SQL only interpolates compile-time constants and enum-mapped literals; user input is always bound.
use sqlx::AssertSqlSafe;
use serde_json::json;

use crate::db::{AppResult, Tx, user_err};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Gym,
    Exercise,
    Equipment,
}

impl Kind {
    pub fn table(self) -> &'static str {
        match self {
            Kind::Gym => "gym",
            Kind::Exercise => "exercise",
            Kind::Equipment => "equipment",
        }
    }
}

pub struct Candidate {
    pub id: i64,
    pub name: String,
    pub aliases: Vec<String>,
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Hit(i64),
    /// Several plausible matches, best first.
    Ambiguous(Vec<(i64, String)>),
    /// Nothing good enough; closest first (may be empty).
    NotFound(Vec<(i64, String)>),
}

const ACCEPT: f64 = 0.55;
const MARGIN: f64 = 0.08;

fn words(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(ca != *cb)).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

fn word_score(q: &str, label_words: &[String]) -> f64 {
    let allowed = if q.chars().count() <= 5 { 1 } else { 2 };
    // Also compare against adjacent words run together, so "benchpres" meets "dumbbell bench press".
    let pairs: Vec<String> = label_words.windows(2).map(|w| w.concat()).collect();
    label_words
        .iter()
        .chain(&pairs)
        .map(|w| {
            if w == q || (q.len() >= 3 && w.starts_with(q)) {
                1.0
            } else if levenshtein(q, w) <= allowed {
                0.8
            } else {
                0.0
            }
        })
        .fold(0.0, f64::max)
}

/// 0..=1: the share of query words found in the label, minus a small penalty per unmatched label word.
fn label_score(query: &[String], label: &str) -> f64 {
    let lw = words(label);
    if lw.is_empty() || query.is_empty() {
        return 0.0;
    }
    if lw == query {
        return 2.0; // exact (case/punctuation-insensitive)
    }
    let hit: f64 = query.iter().map(|q| word_score(q, &lw)).sum::<f64>() / query.len() as f64;
    let extra = lw.len().saturating_sub(query.len()) as f64;
    (hit - 0.1 * extra).max(0.0)
}

/// Every candidate with its best score over name + aliases, best first. 2.0 = exact.
pub fn rank(query: &str, candidates: &[Candidate]) -> Vec<(f64, i64, String)> {
    let q = words(query);
    let mut scored: Vec<(f64, i64, String)> = candidates
        .iter()
        .map(|c| {
            let best = std::iter::once(&c.name)
                .chain(&c.aliases)
                .map(|l| label_score(&q, l))
                .fold(0.0, f64::max);
            (best, c.id, c.name.clone())
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored
}

pub fn resolve(query: &str, candidates: &[Candidate]) -> Outcome {
    if let Ok(id) = query.trim().parse::<i64>()
        && candidates.iter().any(|c| c.id == id)
    {
        return Outcome::Hit(id);
    }
    let scored = rank(query, candidates);
    let exact: Vec<_> = scored.iter().filter(|s| s.0 >= 2.0).collect();
    if exact.len() == 1 {
        return Outcome::Hit(exact[0].1);
    }
    let top5 = |min: f64| scored.iter().filter(|s| s.0 > min).take(5).map(|s| (s.1, s.2.clone())).collect();
    match scored.as_slice() {
        [] => Outcome::NotFound(vec![]),
        [first, rest @ ..] if first.0 >= ACCEPT => match rest.first() {
            Some(second) if second.0 > first.0 - MARGIN => Outcome::Ambiguous(top5(first.0 - MARGIN)),
            _ => Outcome::Hit(first.1),
        },
        _ => Outcome::NotFound(top5(0.0)),
    }
}

pub async fn candidates(tx: &mut Tx, owner: &str, kind: Kind) -> AppResult<Vec<Candidate>> {
    let sql = format!("SELECT id, name, aliases FROM {} WHERE owner_sub = $1", kind.table());
    let rows: Vec<(i64, String, Vec<String>)> = sqlx::query_as(AssertSqlSafe(sql)).bind(owner).fetch_all(&mut **tx).await?;
    Ok(rows.into_iter().map(|(id, name, aliases)| Candidate { id, name, aliases }).collect())
}

fn create_hint(kind: Kind) -> &'static str {
    match kind {
        Kind::Gym => "gym_create",
        Kind::Exercise => "exercise_create",
        Kind::Equipment => "gym_equipment_set",
    }
}

pub async fn lookup(tx: &mut Tx, owner: &str, kind: Kind, query: &str) -> AppResult<Outcome> {
    Ok(resolve(query, &candidates(tx, owner, kind).await?))
}

/// Turns a non-hit into an error the LLM can act on.
fn require(kind: Kind, query: &str, outcome: Outcome) -> AppResult<i64> {
    match outcome {
        Outcome::Hit(id) => Ok(id),
        Outcome::Ambiguous(c) => Err(user_err(
            json!({"error": format!("'{query}' matches several {}s; ask which one or pass its id", kind.table()),
                   "candidates": as_json(&c)}).to_string(),
        )),
        Outcome::NotFound(c) => Err(user_err(
            json!({"error": format!("no {} matches '{query}'", kind.table()),
                   "closest": as_json(&c),
                   "hint": format!("pick one of the closest, or create it with {}", create_hint(kind))}).to_string(),
        )),
    }
}

pub async fn id(tx: &mut Tx, owner: &str, kind: Kind, query: &str) -> AppResult<i64> {
    require(kind, query, lookup(tx, owner, kind, query).await?)
}

/// Like `id`, but `None` when nothing matches (for upserts); ambiguity is still an error.
pub async fn existing_id(tx: &mut Tx, owner: &str, kind: Kind, query: &str) -> AppResult<Option<i64>> {
    match lookup(tx, owner, kind, query).await? {
        Outcome::NotFound(_) => Ok(None),
        other => require(kind, query, other).map(Some),
    }
}

fn as_json(c: &[(i64, String)]) -> serde_json::Value {
    c.iter().map(|(id, name)| json!({"id": id, "name": name})).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cands() -> Vec<Candidate> {
        let c = |id, name: &str, aliases: &[&str]| Candidate {
            id,
            name: name.into(),
            aliases: aliases.iter().map(|s| s.to_string()).collect(),
        };
        vec![
            c(1, "Trainmore NDSM", &[]),
            c(2, "Trainmore Zuid", &[]),
            c(3, "Basic-Fit Amsterdam Noord", &["bf noord"]),
            c(4, "Bench press", &["bench"]),
            c(5, "Incline bench press", &[]),
        ]
    }

    #[test]
    fn exact_and_alias() {
        assert_eq!(resolve("trainmore ndsm", &cands()), Outcome::Hit(1));
        assert_eq!(resolve("BF Noord", &cands()), Outcome::Hit(3));
        assert_eq!(resolve("3", &cands()), Outcome::Hit(3));
    }

    #[test]
    fn word_order_and_typos() {
        assert_eq!(resolve("ndsm trainmore", &cands()), Outcome::Hit(1));
        assert_eq!(resolve("trianmore ndsm", &cands()), Outcome::Hit(1));
        assert_eq!(resolve("ndsm", &cands()), Outcome::Hit(1));
        assert_eq!(resolve("benchpres", &cands()), Outcome::Hit(4));
    }

    #[test]
    fn ambiguous_and_missing() {
        assert!(matches!(resolve("trainmore", &cands()), Outcome::Ambiguous(c) if c.len() == 2));
        assert!(matches!(resolve("leg press", &cands()), Outcome::NotFound(_)));
        assert_eq!(resolve("anything", &[]), Outcome::NotFound(vec![]));
    }
}
