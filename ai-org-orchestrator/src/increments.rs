use crate::agents::INCREMENT_PLANNER;
use crate::manager::Manager;
use serde_json::json;

/// Rust lines of schema past which a mission that asks for it (`Increments: split`) is built
/// as a series of smaller jobs instead of one.
pub(crate) const SCHEMA_LINE_BUDGET: usize = 500;
pub(crate) const MAX_INCREMENTS: usize = 6;
const PLAN_ATTEMPTS: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Increment {
    pub slug: String,
    pub mission: String,
}

/// Lines of Rust in the schema, across every file it declares.
pub(crate) fn schema_rust_lines(schema: &str) -> usize {
    Manager::parse_file_sections(Manager::strip_fences(schema))
        .iter()
        .filter(|(path, _)| path.ends_with(".rs"))
        .map(|(_, content)| content.lines().count())
        .sum()
}

/// True when the mission asks to be split into increments once its schema is large.
pub(crate) fn split_requested(mission: &str) -> bool {
    mission.lines().any(|line| {
        let line = line.trim().trim_start_matches(['-', '*', ' ']).trim_start().to_lowercase();
        line.strip_prefix("increments:")
            .is_some_and(|v| v.trim().trim_matches(['*', '`', '.']).trim() == "split")
    })
}

pub(crate) fn parse_increments(text: &str) -> Vec<Increment> {
    let mut out: Vec<Increment> = Vec::new();
    let mut current: Option<(String, String)> = None;
    let finish = |current: &mut Option<(String, String)>, out: &mut Vec<Increment>| {
        if let Some((slug, body)) = current.take() {
            let mission = body.trim().to_string();
            if !slug.is_empty() && !mission.is_empty() {
                out.push(Increment { slug, mission });
            }
        }
    };
    for line in Manager::strip_fences(text).lines() {
        if let Some(name) = line.trim().strip_prefix("## INCREMENT:") {
            finish(&mut current, &mut out);
            current = Some((slugify(name), String::new()));
        } else if let Some((_, body)) = current.as_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    finish(&mut current, &mut out);
    out
}

fn slugify(name: &str) -> String {
    let slug: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    slug.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-")
}

/// The hand-off the queue worker reads to submit the increments as jobs.
pub(crate) fn increments_json(schema_lines: usize, increments: &[Increment]) -> String {
    serde_json::to_string_pretty(&json!({
        "schema_lines": schema_lines,
        "line_budget": SCHEMA_LINE_BUDGET,
        "increments": increments
            .iter()
            .map(|i| json!({"slug": i.slug, "mission": i.mission}))
            .collect::<Vec<_>>(),
    }))
    .expect("a json! value always serializes")
}

impl<'a> Manager<'a> {
    pub(crate) fn plan_increments(
        &self,
        schema_lines: usize,
    ) -> Result<Vec<Increment>, Box<dyn std::error::Error>> {
        let mission = self.ws.get_artifact("mission.md")?;
        let contract = self.ws.get_artifact("objective_contract.md").unwrap_or_default();
        let schema = self.ws.get_artifact("schema.rs")?;
        let base = format!(
            "# Schema size\n{schema_lines} lines of Rust against a budget of {SCHEMA_LINE_BUDGET}.\n\n\
             # Original mission\n{mission}\n\n# ObjectiveContract\n{contract}\n\n# Schema\n{schema}"
        );

        let mut task = base.clone();
        let mut last_problem = String::new();
        for attempt in 0..PLAN_ATTEMPTS {
            let reply = self.ws.checkpoint(&format!("IncrementPlanner/{attempt}"), || {
                self.run("IncrementPlanner", INCREMENT_PLANNER, &task)
            })?;
            let increments = parse_increments(&reply);
            if (2..=MAX_INCREMENTS).contains(&increments.len()) {
                return Ok(increments);
            }
            last_problem = format!(
                "the reply contained {} increment(s); it must contain between 2 and {MAX_INCREMENTS}, \
                 each introduced by a `## INCREMENT: <name>` line",
                increments.len()
            );
            task = format!("{base}\n\n# Problem with your previous reply\n{last_problem}");
        }
        Err(format!("IncrementPlanner could not produce a usable increment plan: {last_problem}").into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_lines_count_every_rust_file_and_ignore_the_manifest() {
        let schema = "// === Cargo.toml ===\n[dependencies]\n\n// === src/lib.rs ===\nfn a() {}\nfn b() {}\n\n// === src/more.rs ===\nfn c() {}\n";
        assert_eq!(schema_rust_lines(schema), 4);
    }

    #[test]
    fn a_schema_with_no_markers_is_counted_as_one_file() {
        assert_eq!(schema_rust_lines("```rust\nfn a() {}\nfn b() {}\n```"), 2);
    }

    #[test]
    fn increments_are_parsed_in_order_with_their_bodies() {
        let text = "```\n## INCREMENT: Core Data Model\nBuild the tables.\n- keep ids\n\n## INCREMENT: quotes & orders\nAdd quoting.\n```";
        let parsed = parse_increments(text);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].slug, "core-data-model");
        assert_eq!(parsed[0].mission, "Build the tables.\n- keep ids");
        assert_eq!(parsed[1].slug, "quotes-orders");
        assert_eq!(parsed[1].mission, "Add quoting.");
    }

    #[test]
    fn an_increment_with_no_body_or_no_name_is_dropped() {
        assert!(parse_increments("## INCREMENT: empty\n\n## INCREMENT:\ntext").is_empty());
        assert!(parse_increments("no headings at all").is_empty());
    }

    #[test]
    fn splitting_is_only_on_when_the_mission_says_so() {
        assert!(split_requested("# M\n- **Increments:** split\n"));
        assert!(split_requested("Increments: split"));
        assert!(!split_requested("Increments: none"));
        assert!(!split_requested("a very big schema"));
    }

    #[test]
    fn the_hand_off_file_names_each_increment() {
        let incs = vec![Increment { slug: "core".into(), mission: "m1".into() }];
        let v: serde_json::Value = serde_json::from_str(&increments_json(764, &incs)).unwrap();
        assert_eq!(v["schema_lines"], 764);
        assert_eq!(v["line_budget"], 500);
        assert_eq!(v["increments"][0]["slug"], "core");
        assert_eq!(v["increments"][0]["mission"], "m1");
    }
}
