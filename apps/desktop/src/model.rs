use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A read projection; mutations send patches, never a re-encoded record.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Memory {
    pub id: String,
    pub text: String,
    pub kind: String,
    pub status: String,
    #[serde(default)]
    pub revision: i64,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub anchors: Vec<Value>,
    #[serde(default)]
    pub sources: Vec<Value>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    pub memories: Vec<Memory>,
    pub total: usize,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    pub project: String,
    pub text: String,
    pub status: Option<String>,
    pub kind: Option<String>,
    pub cursor: Option<String>,
}

/// An edit remains bound to the project, id and revision that were inspected.
/// Observing a newer server record never changes the user's draft or its base.
#[derive(Clone, Debug)]
pub struct EditDraft {
    pub project: String,
    pub base: Memory,
    pub text: String,
    pub server: Option<Memory>,
}

impl EditDraft {
    pub fn new(project: String, memory: Memory) -> Self {
        Self {
            project,
            text: memory.text.clone(),
            base: memory,
            server: None,
        }
    }
    pub fn observe(&mut self, memory: Memory) -> Result<(), &'static str> {
        if memory.id != self.base.id || memory.revision < self.base.revision {
            return Err("The server reply does not belong to this draft revision");
        }
        self.server = Some(memory);
        Ok(())
    }
    pub fn rebase(&mut self) -> Result<i64, &'static str> {
        let server = self
            .server
            .take()
            .ok_or("Fetch the current server revision first")?;
        self.base = server;
        Ok(self.base.revision)
    }
}

/// Changes invalidate an in-flight read without relying on task cancellation.
#[derive(Default)]
pub struct Generation(u64);

impl Generation {
    pub fn advance(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(1);
        self.0
    }
    pub fn accepts(&self, ticket: u64) -> bool {
        self.0 == ticket
    }
}

pub fn preview(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let mut out: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        out.push('…');
    }
    out.replace(['\n', '\r'], " ")
}

pub fn string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

#[derive(Clone, Debug)]
pub struct GraphNode {
    pub id: String,
    pub label: String,
    pub memory_id: Option<String>,
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Debug, Default)]
pub struct Graph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<(usize, usize, String)>,
    pub truncated: bool,
    pub height: f32,
    pub width: f32,
}

impl Graph {
    /// Layout only the server-authorized subgraph. Missing memory rows never
    /// become phantom ID nodes; structural nodes come only from admitted edges.
    pub fn from_reply(value: &Value) -> Self {
        use std::collections::{BTreeMap, HashMap};
        let memories: HashMap<String, String> = value["memories"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| {
                m["id"]
                    .as_str()
                    .map(|id| (format!("mem:{id}"), string(m, "text")))
            })
            .collect();
        let mut labels = BTreeMap::new();
        for (id, text) in &memories {
            labels.insert(id.clone(), preview(text, 58));
        }
        let admitted: Vec<_> = value["edges"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| {
                let from = string(e, "from");
                let to = string(e, "to");
                if from.is_empty()
                    || to.is_empty()
                    || [&from, &to]
                        .iter()
                        .any(|id| id.starts_with("mem:") && !memories.contains_key(*id))
                {
                    return None;
                }
                for id in [&from, &to] {
                    labels.entry(id.clone()).or_insert_with(|| {
                        id.split_once(':').map(|(_, v)| v).unwrap_or(id).to_string()
                    });
                }
                Some((from, to, string(e, "relation")))
            })
            .collect();
        let truncated = value["truncated"].as_bool().unwrap_or(false) || labels.len() > 200;
        // Three columns by category: memories, files/symbols, other structure.
        let mut rows = [0usize; 3];
        let nodes: Vec<_> = labels
            .into_iter()
            .take(200)
            .map(|(id, label)| {
                let memory_id = id.strip_prefix("mem:").map(str::to_string);
                let column = if memory_id.is_some() {
                    0
                } else if id.starts_with("file:") || id.starts_with("symbol:") {
                    1
                } else {
                    2
                };
                let y = 30. + rows[column] as f32 * 100.;
                rows[column] += 1;
                GraphNode {
                    id,
                    label,
                    memory_id,
                    x: 30. + column as f32 * 270.,
                    y,
                }
            })
            .collect();
        let indices: HashMap<_, _> = nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id.as_str(), i))
            .collect();
        let edges = admitted
            .into_iter()
            .filter_map(|(a, b, relation)| {
                Some((
                    *indices.get(a.as_str())?,
                    *indices.get(b.as_str())?,
                    relation,
                ))
            })
            .collect();
        Self {
            nodes,
            edges,
            truncated,
            width: 840.,
            height: 80. + *rows.iter().max().unwrap_or(&0) as f32 * 100.,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory(id: &str, revision: i64, text: &str) -> Memory {
        serde_json::from_value(serde_json::json!({"id":id,"revision":revision,"text":text,"kind":"fact","status":"active"})).unwrap()
    }

    #[test]
    fn observing_a_conflict_keeps_the_draft_and_original_revision_until_explicit_rebase() {
        let mut draft = EditDraft::new("p_original".into(), memory("m_original", 1, "Original"));
        draft.text = "User draft".into();
        draft
            .observe(memory("m_original", 2, "Concurrent edit"))
            .unwrap();
        assert_eq!(draft.base.revision, 1);
        assert_eq!(draft.text, "User draft");
        assert_eq!(draft.server.as_ref().unwrap().text, "Concurrent edit");
        assert_eq!(draft.rebase().unwrap(), 2);
        assert_eq!(draft.base.text, "Concurrent edit");
        assert_eq!(draft.text, "User draft");
        assert_eq!(draft.project, "p_original");
    }

    #[test]
    fn a_draft_cannot_rebase_from_a_different_record_or_an_older_revision() {
        let mut draft = EditDraft::new("p_original".into(), memory("m_original", 3, "Original"));
        assert!(draft.observe(memory("other", 4, "Wrong record")).is_err());
        assert!(draft.observe(memory("m_original", 2, "Old reply")).is_err());
        assert!(draft.rebase().is_err());
        assert_eq!(draft.base.revision, 3);
    }

    #[test]
    fn a_late_project_or_query_result_cannot_publish() {
        let mut generation = Generation::default();
        let old = generation.advance();
        assert!(generation.accepts(old));
        let next = generation.advance();
        assert!(!generation.accepts(old));
        assert!(generation.accepts(next));
    }

    #[test]
    fn preview_preserves_unicode_and_discloses_truncation() {
        assert_eq!(preview("日本語\ntext", 3), "日本語…");
        assert_eq!(preview("Türkçe", 6), "Türkçe");
        assert_eq!(preview("", 0), "");
    }

    #[test]
    fn graph_never_reconstructs_a_missing_memory_id() {
        let reply = serde_json::json!({"memories":[{"id":"visible","text":"Public fact"}],"edges":[
            {"from":"mem:visible","to":"file:src/main.rs","relation":"about_file"},
            {"from":"mem:hidden","to":"file:private.txt","relation":"about_file"}
        ]});
        let graph = Graph::from_reply(&reply);
        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.edges.len(), 1);
        assert!(
            !graph
                .nodes
                .iter()
                .any(|n| n.id.contains("hidden") || n.id.contains("private"))
        );
        let second = Graph::from_reply(&reply);
        assert_eq!(graph.nodes[0].id, second.nodes[0].id);
    }

    #[test]
    fn graph_budget_is_explicit_and_cycles_are_bounded() {
        let memories: Vec<_> = (0..250)
            .map(|i| serde_json::json!({"id":i.to_string(),"text":"Memory"}))
            .collect();
        let graph = Graph::from_reply(
            &serde_json::json!({"memories":memories,"edges":[{"from":"mem:0","to":"mem:0","relation":"related_to"}]}),
        );
        assert_eq!(graph.nodes.len(), 200);
        assert!(graph.truncated);
        assert!(Graph::from_reply(&Value::Null).nodes.is_empty());
    }
}
