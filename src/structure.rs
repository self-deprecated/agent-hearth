//! `doctor`'s hints on the workspace's structure, from the last fresh golden build's
//! `cargo --timings` report: what holds the build up, which a maintainer could change. Hints
//! only, never failures, and no speed claim: none of them is measured to pay.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// A unit of the build, as `cargo --timings` reports it.
#[derive(Debug, Deserialize)]
struct Unit {
    i: usize,
    name: String,
    version: String,
    mode: String,
    /// `""` for a library, `" build-script"`, `" build-script (run)"`, `" \"name\" (test)"`.
    target: String,
    #[serde(default)]
    features: Vec<String>,
    start: f64,
    duration: f64,
    #[serde(default)]
    unblocked_units: Vec<usize>,
    /// Units that could start once this one's metadata was ready.
    #[serde(default)]
    unblocked_rmeta_units: Vec<usize>,
}

impl Unit {
    fn end(&self) -> f64 {
        self.start + self.duration
    }

    fn runs_build_script(&self) -> bool {
        self.mode == "run-custom-build"
    }

    fn label(&self) -> String {
        format!("{} {}{}", self.name, self.version, self.target)
    }
}

/// A build script that runs this long on the critical path is worth a hint.
const SLOW_SCRIPT: f64 = 10.0;
/// A workspace crate compiling this long on the critical path with the machine less than half
/// busy is worth a hint.
const IDLE: f64 = 10.0;
/// Compiling a crate again with other features costing this much is worth a hint.
const DUPLICATE: f64 = 2.0;

/// The newest timings report among the goldens in `goldens`.
pub fn report(goldens: &Path) -> Option<PathBuf> {
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in fs::read_dir(goldens).ok()?.flatten() {
        let path = entry.path().join(crate::golden::TIMINGS);
        if let Ok(modified) = fs::metadata(&path).and_then(|m| m.modified())
            && newest.as_ref().is_none_or(|(time, _)| modified > *time)
        {
            newest = Some((modified, path));
        }
    }
    newest.map(|(_, path)| path)
}

/// The hints the report `html` gives for a workspace whose members are `members`.
pub fn hints(html: &str, members: &[String]) -> Vec<String> {
    // `CPU_USAGE`: [seconds since the start, percent of the machine busy].
    let (Some(units), Some(cpu)) = (
        constant::<Vec<Unit>>(html, "UNIT_DATA"),
        constant::<Vec<(f64, f64)>>(html, "CPU_USAGE"),
    ) else {
        return Vec::new();
    };
    let mut hints = Vec::new();
    let total = units.iter().map(Unit::end).fold(0.0, f64::max);
    let path = critical_path(&units);
    for (n, unit) in path.iter().enumerate() {
        if unit.runs_build_script() && unit.duration >= SLOW_SCRIPT {
            let waiting = path
                .get(n + 1)
                .map_or("the build's end".to_owned(), |next| next.label());
            hints.push(format!(
                "{}'s build script ran {:.0}s on the golden build's critical path ({:.0}s in all), \
                 and {waiting} waited for it: a native library built once, apart from the \
                 crates that need it, or prebuilt, would take it off",
                unit.name, unit.duration, total
            ));
        }
    }
    for unit in &path {
        if unit.target.is_empty() && members.contains(&unit.name) {
            let idle = idle(unit, &cpu);
            if idle >= IDLE {
                hints.push(format!(
                    "{} compiled for {:.0}s on the golden build's critical path, {idle:.0}s of it \
                     with the machine less than half busy: what depends on it waits, and \
                     splitting it would let more of it build at once",
                    unit.name, unit.duration
                ));
            }
        }
    }
    hints.extend(duplicates(&units));
    hints
}

/// The value of `const <name> = …;` in the report's script.
fn constant<T: serde::de::DeserializeOwned>(html: &str, name: &str) -> Option<T> {
    let start = html.find(&format!("const {name} = "))? + name.len() + 9;
    let rest = &html[start..];
    // The JSON array ends at the first `]` alone on a line.
    let end = rest.find("\n];").map(|end| end + 2)?;
    serde_json::from_str(&rest[..end]).ok()
}

/// From the unit that finished last back to the first: each unit and the one whose end (or
/// metadata, for a unit waiting only on that) let it start, the last it waited for.
fn critical_path(units: &[Unit]) -> Vec<&Unit> {
    let by_index: BTreeMap<usize, &Unit> = units.iter().map(|unit| (unit.i, unit)).collect();
    let mut unblocked_by: BTreeMap<usize, &Unit> = BTreeMap::new();
    for unit in units {
        for &next in unit
            .unblocked_units
            .iter()
            .chain(&unit.unblocked_rmeta_units)
        {
            unblocked_by.insert(next, unit);
        }
    }
    let Some(mut unit) = units.iter().max_by(|a, b| a.end().total_cmp(&b.end())) else {
        return Vec::new();
    };
    let mut path = vec![unit];
    while let Some(&before) = unblocked_by.get(&unit.i) {
        if path.iter().any(|seen| seen.i == before.i) || !by_index.contains_key(&before.i) {
            break;
        }
        unit = before;
        path.push(unit);
    }
    path.reverse();
    path
}

/// Seconds of `unit`'s compilation during which the machine was less than half busy.
fn idle(unit: &Unit, cpu: &[(f64, f64)]) -> f64 {
    let mut seconds = 0.0;
    for pair in cpu.windows(2) {
        let ((at, busy), (next, _)) = (pair[0], pair[1]);
        let (from, to) = (at.max(unit.start), next.min(unit.end()));
        if to > from && busy < 50.0 {
            seconds += to - from;
        }
    }
    seconds
}

/// Crates compiled more than once with different features: one build with the union of the
/// features would do, unless a feature changes behaviour another user relies on.
fn duplicates(units: &[Unit]) -> Vec<String> {
    let mut groups: BTreeMap<(&str, &str, &str, &str), Vec<&Unit>> = BTreeMap::new();
    for unit in units.iter().filter(|unit| !unit.runs_build_script()) {
        let key = (&*unit.name, &*unit.version, &*unit.mode, &*unit.target);
        groups.entry(key).or_default().push(unit);
    }
    let mut found: Vec<(f64, String)> = Vec::new();
    for ((name, version, _, target), group) in groups {
        let mut features: Vec<&Vec<String>> = group.iter().map(|unit| &unit.features).collect();
        features.sort();
        features.dedup();
        if features.len() < 2 {
            continue;
        }
        let total: f64 = group.iter().map(|unit| unit.duration).sum();
        let longest = group.iter().map(|unit| unit.duration).fold(0.0, f64::max);
        let extra = total - longest;
        if extra >= DUPLICATE {
            let sets: Vec<String> = features
                .iter()
                .map(|set| format!("[{}]", set.join(", ")))
                .collect();
            found.push((
                extra,
                format!(
                    "{name} {version}{target} compiled {} times with different features ({}): \
                     {extra:.0}s more than once; enabling the same features everywhere would \
                     build it once",
                    group.len(),
                    sets.join(" / ")
                ),
            ));
        }
    }
    found.sort_by(|a, b| b.0.total_cmp(&a.0));
    found.into_iter().map(|(_, hint)| hint).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A report with `units` (index, name, mode, target, features, start, duration, unblocked)
    /// and `cpu` samples (time, percent busy).
    #[allow(clippy::type_complexity)]
    fn html(
        units: &[(usize, &str, &str, &str, &[&str], f64, f64, &[usize])],
        cpu: &[(f64, f64)],
    ) -> String {
        let units: Vec<serde_json::Value> = units
            .iter()
            .map(
                |(i, name, mode, target, features, start, duration, unblocked)| {
                    serde_json::json!({
                        "i": i, "name": name, "version": "1.0.0", "mode": mode, "target": target,
                        "features": features, "start": start, "duration": duration,
                        "unblocked_units": unblocked, "unblocked_rmeta_units": [], "sections": [],
                    })
                },
            )
            .collect();
        format!(
            "<script>\nconst UNIT_DATA = {};\nconst CONCURRENCY_DATA = [\n];\nconst CPU_USAGE = {};\n</script>",
            serde_json::to_string_pretty(&units).unwrap(),
            serde_json::to_string_pretty(&cpu).unwrap()
        )
    }

    #[test]
    fn a_slow_build_script_and_an_idle_crate_on_the_critical_path_are_hinted() {
        let report = html(
            &[
                (0, "cc", "todo", "", &[], 0.0, 1.0, &[1]),
                (
                    1,
                    "jemalloc-sys",
                    "run-custom-build",
                    " build-script (run)",
                    &[],
                    1.0,
                    60.0,
                    &[2],
                ),
                (2, "jemalloc-sys", "todo", "", &[], 61.0, 1.0, &[3]),
                (3, "hub", "todo", "", &[], 62.0, 30.0, &[4]),
                (4, "app", "todo", "", &[], 92.0, 5.0, &[]),
                (5, "other", "todo", "", &[], 1.0, 20.0, &[]),
            ],
            &[(0.0, 90.0), (21.0, 20.0), (97.0, 0.0)],
        );
        let hints = hints(&report, &["hub".to_owned(), "app".to_owned()]);
        assert_eq!(hints.len(), 2, "{hints:#?}");
        assert!(
            hints[0].starts_with("jemalloc-sys's build script ran 60s"),
            "{}",
            hints[0]
        );
        assert!(
            hints[0].contains("jemalloc-sys 1.0.0 waited"),
            "{}",
            hints[0]
        );
        assert!(hints[1].starts_with("hub compiled for 30s"), "{}", hints[1]);
        assert!(
            hints[1].contains("30s of it with the machine less than half busy"),
            "{}",
            hints[1]
        );
    }

    #[test]
    fn a_crate_compiled_with_two_feature_sets_is_hinted_once_it_costs_something() {
        let report = html(
            &[
                (0, "syn", "todo", "", &["full"], 0.0, 8.0, &[]),
                (1, "syn", "todo", "", &["derive"], 0.0, 5.0, &[]),
                (2, "tiny", "todo", "", &["a"], 0.0, 1.0, &[]),
                (3, "tiny", "todo", "", &["b"], 0.0, 1.0, &[]),
            ],
            &[(0.0, 100.0), (8.0, 0.0)],
        );
        let hints = hints(&report, &[]);
        assert_eq!(hints.len(), 1, "{hints:#?}");
        assert!(
            hints[0].starts_with("syn 1.0.0 compiled 2 times"),
            "{}",
            hints[0]
        );
        assert!(hints[0].contains("[derive] / [full]"), "{}", hints[0]);
    }

    #[test]
    fn no_report_or_another_format_gives_no_hints() {
        assert!(hints("", &[]).is_empty());
        assert!(hints("const UNIT_DATA = {\n];", &[]).is_empty());
    }
}
