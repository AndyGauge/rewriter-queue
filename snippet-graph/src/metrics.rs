use crate::model::NodeId;
use std::collections::BTreeMap;

#[derive(Debug, Default)]
pub struct RunMetrics {
    pub retries: BTreeMap<NodeId, usize>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub wall_ms: u64,
}

impl RunMetrics {
    pub fn record_retry(&mut self, id: &NodeId) {
        *self.retries.entry(id.clone()).or_insert(0) += 1;
    }

    pub fn to_json(&self) -> String {
        let retries: BTreeMap<&str, usize> = self
            .retries
            .iter()
            .map(|(k, v)| (k.0.as_str(), *v))
            .collect();
        serde_json::json!({
            "retries": retries,
            "total_retries": self.retries.values().sum::<usize>(),
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
            "wall_ms": self.wall_ms,
        })
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_retries_per_node() {
        let mut m = RunMetrics::default();
        let a = NodeId("a".into());
        let b = NodeId("b".into());
        m.record_retry(&a);
        m.record_retry(&a);
        m.record_retry(&b);
        assert_eq!(m.retries[&a], 2);
        assert_eq!(m.retries[&b], 1);
    }

    #[test]
    fn json_has_all_fields() {
        let mut m = RunMetrics {
            input_tokens: 10,
            output_tokens: 20,
            wall_ms: 30,
            ..Default::default()
        };
        m.record_retry(&NodeId("a".into()));
        m.record_retry(&NodeId("a".into()));
        let v: serde_json::Value = serde_json::from_str(&m.to_json()).unwrap();
        assert_eq!(v["retries"]["a"], 2);
        assert_eq!(v["total_retries"], 2);
        assert_eq!(v["input_tokens"], 10);
        assert_eq!(v["output_tokens"], 20);
        assert_eq!(v["wall_ms"], 30);
    }
}
