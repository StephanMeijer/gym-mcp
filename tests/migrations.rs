//! Migration file names: sqlx keys migrations by the number before the first `_`
//! and does not reject two files with the same number, so check it here.

use std::collections::BTreeMap;

/// Errors for `.sql` names without a numeric prefix and for numbers used more than once.
/// Numbers compare as integers, like sqlx does, so `12_a.sql` and `0012_b.sql` collide.
fn check(names: &[&str]) -> Result<(), String> {
    let mut errors = Vec::new();
    let mut by_version: BTreeMap<i64, Vec<&str>> = BTreeMap::new();
    for &name in names.iter().filter(|n| n.ends_with(".sql")) {
        match name.split_once('_').and_then(|(n, _)| n.parse().ok()) {
            Some(version) => by_version.entry(version).or_default().push(name),
            None => errors.push(format!("{name}: no numeric prefix before `_`")),
        }
    }
    for (version, files) in by_version.into_iter().filter(|(_, f)| f.len() > 1) {
        errors.push(format!("{version:04} used by {}", files.join(", ")));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

#[test]
fn migration_numbers_are_unique() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/migrations");
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    if let Err(e) = check(&names) {
        panic!("bad migration file names in {dir}:\n{e}");
    }
}

#[test]
fn duplicate_number_is_rejected() {
    let err = check(&["0011_a.sql", "0012_abc.sql", "0012_xyz.sql"]).unwrap_err();
    assert_eq!(err, "0012 used by 0012_abc.sql, 0012_xyz.sql");
}

#[test]
fn same_number_with_different_padding_is_rejected() {
    assert!(check(&["12_a.sql", "0012_b.sql"]).is_err());
}

#[test]
fn name_without_numeric_prefix_is_rejected() {
    let err = check(&["0001_init.sql", "init.sql", "abc_x.sql"]).unwrap_err();
    assert_eq!(
        err,
        "init.sql: no numeric prefix before `_`\nabc_x.sql: no numeric prefix before `_`"
    );
}
