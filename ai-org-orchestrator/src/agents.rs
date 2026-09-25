use inference_providers::ModelTier;

/// Route an agent to the appropriate model tier.
///
/// Heavy  → Opus / Qwen2.5-Coder-32B / llama-70b  (code generation, merging)
/// Light  → Sonnet / Groq-70b / llama-8b           (analysis, review, schema)
/// Micro  → Haiku / small local                     (verdicts, classification)
pub fn agent_tier(name: &str) -> ModelTier {
    let base = name.split('-').next().unwrap_or(name);
    match base {
        // Only the agents that generate large code artifacts need the top tier
        "TargetImplementer" | "MergeAgent" => ModelTier::Heavy,
        // Analysis and design agents: capable but not code-generation-heavy
        "MissionArchitect" | "InductiveReasoner" | "SchemaArchitect" | "MilestonePlanner"
        | "TestEngineer" => ModelTier::Light,
        // Reads and reasons about full code — needs Light, not Micro
        "MaintenanceReviewer" | "ProductionReadinessReviewer" => ModelTier::Light,
        // Verdict-only agents: APPROVED/REJECTED, COMPLIANT/NON_COMPLIANT
        _ => ModelTier::Micro,
    }
}

/// Hora depth: how much downstream work is lost if this agent call fails.
/// 0 = atomic (any provider ok), 3 = pipeline root (route to most reliable).
pub fn agent_hora_depth(name: &str) -> u8 {
    let base = name.split('-').next().unwrap_or(name);
    match base {
        "MissionArchitect" | "InductiveReasoner" => 3,
        "MergeAgent" | "SchemaArchitect" | "MilestonePlanner" => 2,
        "TargetImplementer" | "ContractReviewer" | "TestEngineer" | "MaintenanceReviewer"
        | "ProductionReadinessReviewer" => 1,
        _ => 0,
    }
}

// Skills are the canonical source of truth. The software loads them; it does not define them.
pub const MISSION_ARCHITECT: &str = include_str!("../../skills/mission-architect.md");
pub const CONTRACT_REVIEWER: &str = include_str!("../../skills/contract-reviewer.md");
pub const TEST_ENGINEER: &str = include_str!("../../skills/test-engineer.md");
pub const INDUCTIVE_REASONER: &str = include_str!("../../skills/inductive-reasoner.md");
pub const SCHEMA_ARCHITECT: &str = include_str!("../../skills/schema-architect.md");
pub const TARGET_IMPLEMENTER: &str = include_str!("../../skills/target-implementer.md");
pub const MERGE_AGENT: &str = include_str!("../../skills/merge-agent.md");
pub const SECURITY_REVIEWER: &str = include_str!("../../skills/security-reviewer.md");
pub const SECURITY_REVIEWER_FINAL: &str = include_str!("../../skills/security-reviewer-final.md");
pub const MAINTENANCE_REVIEWER: &str = include_str!("../../skills/maintenance-reviewer.md");
pub const IP_COUNSEL: &str = include_str!("../../skills/ip-counsel.md");
pub const FORMAL_METHODS_REVIEWER: &str = include_str!("../../skills/formal-methods-reviewer.md");
pub const PRODUCTION_READINESS_REVIEWER: &str =
    include_str!("../../skills/production-readiness-reviewer.md");
pub const MILESTONE_PLANNER: &str = include_str!("../../skills/milestone-planner.md");

/// One optional, domain-specific pattern TargetImplementer can be given on top of its core
/// skill — only when a milestone actually needs it. Keeps the always-loaded system prompt
/// small (and cheap) while still letting deep, per-domain guidance exist and grow without
/// polluting every call with rules that don't apply to that call's task. `description` is the
/// only part MilestonePlanner ever sees up front (in `implementer_pattern_catalog()`); the
/// full `content` is only ever assembled into a prompt for a milestone that actually
/// prescribes this pattern's `name` — see `resolve_implementer_patterns` and
/// `synthesis.rs::implement_one_milestone`.
pub struct ImplementerPattern {
    pub name: &'static str,
    pub description: &'static str,
    pub content: &'static str,
}

pub const IMPLEMENTER_PATTERNS: &[ImplementerPattern] = &[
    ImplementerPattern {
        name: "path-handling",
        description: "Working with std::path::Path/PathBuf — Display/ToString conversions, \
                       accepting paths generically with AsRef<Path>.",
        content: include_str!("../../skills/target-implementer/patterns/path-handling.md"),
    },
    ImplementerPattern {
        name: "move-semantics",
        description: "Ownership/borrow pitfalls: using a value right after moving it into a \
                       struct literal or a collection push.",
        content: include_str!("../../skills/target-implementer/patterns/move-semantics.md"),
    },
    ImplementerPattern {
        name: "error-handling",
        description: "thiserror's #[source]/derive requirements and chrono's fallible \
                       DateTime::from_timestamp — Result/Option correctness gotchas.",
        content: include_str!("../../skills/target-implementer/patterns/error-handling.md"),
    },
    ImplementerPattern {
        name: "trait-objects",
        description: "Box<dyn Trait> field ergonomics: the Default/Debug derive traps and the \
                       Send bound a trait object needs to cross an .await.",
        content: include_str!("../../skills/target-implementer/patterns/trait-objects.md"),
    },
];

/// Render every pattern's name and description (never its content) for MilestonePlanner, so it
/// can prescribe patterns by name for a milestone without ever seeing — or spending tokens on
/// — the guidance itself unless it actually asks for it.
pub fn implementer_pattern_catalog() -> String {
    IMPLEMENTER_PATTERNS
        .iter()
        .map(|p| format!("- {}: {}", p.name, p.description))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Concatenate the full content of exactly the named patterns, in catalog order (not the
/// order they were prescribed in) so the assembled prompt is deterministic regardless of how
/// a milestone listed them. An unrecognized name is silently skipped here — the milestone plan
/// is LLM output and can typo a name; that's not worth failing a whole milestone over, and
/// `synthesis.rs` separately logs a deviation for a name it can't resolve.
pub fn resolve_implementer_patterns(names: &[String]) -> String {
    IMPLEMENTER_PATTERNS
        .iter()
        .filter(|p| names.iter().any(|n| n == p.name))
        .map(|p| p.content)
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod pattern_tests {
    use super::*;

    #[test]
    fn catalog_lists_every_pattern_by_name_and_description_only() {
        let catalog = implementer_pattern_catalog();
        for p in IMPLEMENTER_PATTERNS {
            assert!(catalog.contains(p.name), "catalog missing name {}", p.name);
            assert!(catalog.contains(p.description), "catalog missing description for {}", p.name);
            // The catalog is what MilestonePlanner sees up front -- it must never leak the
            // full pattern content, or there's no point keeping it separate from the core skill.
            assert!(
                !catalog.contains(p.content),
                "catalog leaked full content for {}",
                p.name
            );
        }
    }

    #[test]
    fn resolve_returns_only_the_named_patterns() {
        let names = vec!["trait-objects".to_string()];
        let resolved = resolve_implementer_patterns(&names);
        let trait_objects =
            IMPLEMENTER_PATTERNS.iter().find(|p| p.name == "trait-objects").unwrap();
        assert!(resolved.contains(trait_objects.content));

        let move_semantics =
            IMPLEMENTER_PATTERNS.iter().find(|p| p.name == "move-semantics").unwrap();
        assert!(!resolved.contains(move_semantics.content));
    }

    #[test]
    fn resolve_silently_ignores_an_unknown_name() {
        let names = vec!["not-a-real-pattern".to_string()];
        let resolved = resolve_implementer_patterns(&names);
        assert!(resolved.is_empty());
    }

    #[test]
    fn resolve_with_no_names_is_empty() {
        assert!(resolve_implementer_patterns(&[]).is_empty());
    }
}
