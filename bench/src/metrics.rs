use run_events::{Event, EventKind};
use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct EventMetrics {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub model_calls: u64,
    pub retries: u64,
    pub gate_failures: u64,
    pub integration_attempts: u64,
    pub escalations: u64,
    pub plan_revised: bool,
    pub debugger_consulted: bool,
}

fn detail_u64(e: &Event, key: &str) -> u64 {
    e.detail
        .as_ref()
        .and_then(|d| d.get(key))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn detail_bool(e: &Event, key: &str) -> Option<bool> {
    e.detail
        .as_ref()
        .and_then(|d| d.get(key))
        .and_then(Value::as_bool)
}

fn has_detail_key(e: &Event, key: &str) -> bool {
    e.detail.as_ref().and_then(|d| d.get(key)).is_some()
}

/// Gate runs without a `variant` build the shared crate directory, which is what the final
/// integration gate runs against; the events carry no separate integration marker.
pub fn from_events(events: &[Event]) -> EventMetrics {
    let mut m = EventMetrics::default();
    for e in events {
        match e.kind {
            EventKind::AgentCall => {
                m.model_calls += 1;
                m.input_tokens += detail_u64(e, "input_tokens");
                m.output_tokens += detail_u64(e, "output_tokens");
                if e.agent == "DebugImplementer" {
                    m.debugger_consulted = true;
                }
            }
            EventKind::Retry => {
                m.retries += 1;
                if e.agent == "MilestonePlanner" {
                    m.escalations += 1;
                }
            }
            EventKind::GateResult => {
                if detail_bool(e, "passed") == Some(false) {
                    m.gate_failures += 1;
                }
                if !has_detail_key(e, "variant") {
                    m.integration_attempts += 1;
                }
            }
            EventKind::Decision if e.agent == "PlanReviewer" => m.plan_revised = true,
            _ => {}
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(agent: &str, input: u64, output: u64, ok: bool) -> Event {
        Event::new(agent, EventKind::AgentCall, "call").with_detail(
            json!({"provider": "p", "model": "m", "input_tokens": input, "output_tokens": output, "latency_ms": 5, "ok": ok}),
        )
    }

    fn gate(passed: bool, attempt: u64, variant: Option<&str>) -> Event {
        let mut d = json!({"passed": passed, "error_bytes": 0, "attempt": attempt});
        if let Some(v) = variant {
            d["variant"] = json!(v);
        }
        Event::new("QualityGate", EventKind::GateResult, "gate").with_detail(d)
    }

    #[test]
    fn no_events_gives_zeroes() {
        assert_eq!(from_events(&[]), EventMetrics::default());
    }

    #[test]
    fn tokens_and_calls_sum_over_agent_calls_including_failed_ones() {
        let m = from_events(&[
            call("SchemaArchitect", 100, 20, true),
            call("TargetImplementer", 50, 5, false),
            Event::new("orchestrator", EventKind::StageStart, "s"),
        ]);
        assert_eq!(
            (m.input_tokens, m.output_tokens, m.model_calls),
            (150, 25, 2)
        );
    }

    #[test]
    fn an_agent_call_without_detail_counts_as_a_call_with_no_tokens() {
        let m = from_events(&[Event::new("a", EventKind::AgentCall, "c")]);
        assert_eq!((m.model_calls, m.input_tokens), (1, 0));
    }

    #[test]
    fn retries_count_every_retry_event_and_escalations_only_planner_ones() {
        let m = from_events(&[
            Event::new(
                "TargetImplementer",
                EventKind::Retry,
                "reviewer rejected iteration 1, revising",
            ),
            Event::new(
                "MilestonePlanner",
                EventKind::Retry,
                "integration escalation 0: repair attempts exhausted",
            ),
            Event::new(
                "MilestonePlanner",
                EventKind::Retry,
                "milestone 'x' did not converge, escalating for a finer split",
            ),
        ]);
        assert_eq!((m.retries, m.escalations), (3, 2));
    }

    #[test]
    fn gate_failures_count_only_failed_gate_results() {
        let m = from_events(&[
            gate(false, 1, None),
            gate(true, 2, None),
            gate(false, 1, Some("m1")),
        ]);
        assert_eq!(m.gate_failures, 2);
    }

    #[test]
    fn integration_attempts_count_gate_runs_on_the_shared_directory() {
        let m = from_events(&[
            gate(false, 1, None),
            gate(false, 2, None),
            gate(true, 3, None),
            gate(false, 1, Some("m1")),
        ]);
        assert_eq!(m.integration_attempts, 3);
    }

    #[test]
    fn a_plan_reviewer_decision_means_the_plan_was_revised() {
        let revised = from_events(&[Event::new(
            "PlanReviewer",
            EventKind::Decision,
            "Revised the plan",
        )]);
        assert!(revised.plan_revised);
        let other = from_events(&[Event::new(
            "SchemaArchitect",
            EventKind::Decision,
            "flat layout",
        )]);
        assert!(!other.plan_revised);
        let only_call = from_events(&[call("PlanReviewer", 1, 1, true)]);
        assert!(!only_call.plan_revised);
    }

    #[test]
    fn the_debugger_is_consulted_only_by_an_agent_call_from_debug_implementer() {
        assert!(from_events(&[call("DebugImplementer", 1, 1, true)]).debugger_consulted);
        assert!(!from_events(&[call("TargetImplementer", 1, 1, true)]).debugger_consulted);
        let decision_only = Event::new("DebugImplementer", EventKind::Decision, "x");
        assert!(!from_events(&[decision_only]).debugger_consulted);
    }
}
