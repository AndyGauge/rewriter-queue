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
        "MissionArchitect" | "InductiveReasoner" | "SchemaArchitect"
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
        "MergeAgent" | "SchemaArchitect" => 2,
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
