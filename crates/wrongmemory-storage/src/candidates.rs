//! Review workflow (WM-504): accept, reject, resolve. Ported from
//! `sqlite-store-candidates.ts` / `shared/candidate-lifecycle.ts`.
//!
//! Difference from the baseline, deliberately stronger: SAGE claims the
//! candidate, mutates the memory, and reverts the claim on failure in
//! separate steps. Here the claim, the memory mutation(s), the candidate's
//! resolution fields and the audit row commit in ONE transaction, so a crash
//! can never leave an accepted candidate without its memory (WM-INV-05).
//! A repeated accept/resolve of an already-resolved candidate returns
//! `alreadyResolved` with the original memory id (natural idempotency).

use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use serde_json::{Map, Value, json};
use wrongmemory_contracts::{ErrorBody, ErrorCode, MemoryCandidate, RememberParams};

use crate::MutationCtx;
use crate::store::{SageStore, StoreResult, Viewer, audit, sql_err};
use crate::text::normalize_text;

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateOutcome {
    pub candidate_id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_memory_id: Option<String>,
    pub applied: bool,
    pub already_resolved: bool,
    /// Members of a consolidation proposal that were superseded.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub superseded: Vec<String>,
    /// Members skipped because they changed since the proposal.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped_members: Vec<String>,
    /// The target mutation failed; the candidate stays pending for a retry
    /// (`resolveCandidate` reports `applied: false` with the error).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn not_found() -> ErrorBody {
    ErrorBody::new(ErrorCode::NotFound, "candidate not found")
}

impl SageStore {
    fn read_candidate(
        &self,
        id: &str,
    ) -> StoreResult<Option<(Map<String, Value>, MemoryCandidate)>> {
        let data: Option<String> = self
            .conn
            .query_row("SELECT data FROM candidates WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()
            .map_err(sql_err)?;
        Ok(data.and_then(|d| {
            let c: MemoryCandidate = serde_json::from_str(&d).ok()?;
            match serde_json::from_str::<Value>(&d).ok()? {
                Value::Object(m) => Some((m, c)),
                _ => None,
            }
        }))
    }

    fn write_candidate(&self, m: &Map<String, Value>) -> StoreResult<()> {
        let c: MemoryCandidate = serde_json::from_value(Value::Object(m.clone()))
            .map_err(|e| ErrorBody::new(ErrorCode::Internal, e.to_string()))?;
        self.conn
            .execute(
                "UPDATE candidates SET data = ?2, status = ?3, updated_at = ?4 WHERE id = ?1",
                params![
                    c.id,
                    Value::Object(m.clone()).to_string(),
                    c.status,
                    c.updated_at
                ],
            )
            .map_err(sql_err)?;
        Ok(())
    }

    /// Visibility for review actions mirrors the list: a session-scoped
    /// proposal belongs to its session.
    fn visible_candidate(
        &self,
        id: &str,
        viewer: &Viewer,
    ) -> StoreResult<(Map<String, Value>, MemoryCandidate)> {
        let (m, c) = self.read_candidate(id)?.ok_or_else(not_found)?;
        if c.scope == "session"
            && (c.owner_session_id.is_none() || c.owner_session_id != viewer.session_id)
        {
            return Err(not_found());
        }
        Ok((m, c))
    }

    /// Whether `viewer` may see candidate `id` (absent and hidden are both
    /// `false`). Used to filter change events about review actions.
    pub fn candidate_visible(&self, id: &str, viewer: &Viewer) -> StoreResult<bool> {
        match self.visible_candidate(id, viewer) {
            Ok(_) => Ok(true),
            Err(e) if e.code == ErrorCode::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn already(c: &MemoryCandidate) -> CandidateOutcome {
        CandidateOutcome {
            error: None,
            candidate_id: c.id.clone(),
            status: c.status.clone(),
            memory_id: c.memory_id.clone(),
            decision: None,
            target_memory_id: c.target_memory_id.clone(),
            applied: false,
            already_resolved: true,
            superseded: vec![],
            skipped_members: vec![],
        }
    }

    /// `acceptCandidate`: create the authoritative memory from the proposal.
    pub fn accept_candidate(
        &mut self,
        id: &str,
        ctx: &MutationCtx,
    ) -> StoreResult<CandidateOutcome> {
        self.require_writable()?;
        let viewer = Viewer {
            session_id: ctx.session_id.clone(),
        };
        self.in_transaction(|s| {
            let (mut m, c) = s.visible_candidate(id, &viewer)?;
            if c.status != "pending" {
                return Ok(Self::already(&c));
            }
            if c.kind == "memory_review" {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidRequest,
                    "memory_review candidates are resolved (keep/archive/delete), not accepted.",
                ));
            }
            let params = RememberParams {
                text: c.text.clone(),
                kind: Some(c.kind.clone()),
                scope: Some(c.scope.clone()),
                persistence: None,
                tags: c.tags.clone(),
                anchors: c.anchors.clone(),
                audience: c.audience.clone(),
                sources: c.sources.clone(),
                validity: c.validity.clone(),
                importance: Some(c.importance),
                confidence: Some(c.confidence),
                freshness: None,
                expires_at: None,
                supersedes: vec![],
                contradicts: vec![],
            };
            // The memory is owned by the proposal's session, not the curator's.
            let inner = MutationCtx {
                session_id: c.owner_session_id.clone().or(ctx.session_id.clone()),
                idempotency_key: None,
                ..ctx.clone()
            };
            let outcome = s.remember_full(&params, &inner)?;
            let keeper = outcome.id.clone();
            let mut superseded = Vec::new();
            let mut skipped = Vec::new();
            if let (Some(ids), Some(revs)) = (&c.supersedes_memory_ids, &c.supersedes_revisions) {
                for (member, rev) in ids.iter().zip(revs) {
                    if *member == keeper {
                        continue;
                    }
                    let current = s.get(member, &Viewer { session_id: inner.session_id.clone() })?;
                    match current {
                        Some(r) if r.status == "active" && r.revision == *rev => {
                            let mut input = Map::new();
                            input.insert("status".into(), json!("superseded"));
                            input.insert("supersededBy".into(), json!(keeper));
                            input.insert("expectedRevision".into(), json!(rev));
                            s.update(member, &input, &inner, false)?;
                            superseded.push(member.clone());
                        }
                        _ => skipped.push(member.clone()),
                    }
                }
            }
            m.insert("status".into(), json!("accepted"));
            m.insert("memoryId".into(), json!(keeper));
            m.insert("updatedAt".into(), json!(ctx.now_iso));
            m.insert(
                "resolvedBy".into(),
                json!({"principalId": ctx.principal_id, "client": ctx.client_label}),
            );
            s.write_candidate(&m)?;
            if !superseded.is_empty() {
                audit(
                    &s.conn,
                    "memory.candidate_accepted",
                    &ctx.now_iso,
                    json!({"memoryId": keeper, "details": {"candidateId": id, "superseded": superseded}}),
                )
                .map_err(sql_err)?;
            }
            Ok(CandidateOutcome {
            error: None,
                candidate_id: id.into(),
                status: "accepted".into(),
                memory_id: Some(keeper),
                decision: None,
                target_memory_id: c.target_memory_id.clone(),
                applied: true,
                already_resolved: false,
                superseded,
                skipped_members: skipped,
            })
        })
    }

    /// `rejectCandidate`.
    pub fn reject_candidate(
        &mut self,
        id: &str,
        reason: &str,
        ctx: &MutationCtx,
    ) -> StoreResult<CandidateOutcome> {
        self.require_writable()?;
        let viewer = Viewer {
            session_id: ctx.session_id.clone(),
        };
        self.in_transaction(|s| {
            let (mut m, c) = s.visible_candidate(id, &viewer)?;
            if c.status != "pending" {
                return Ok(Self::already(&c));
            }
            m.insert("status".into(), json!("rejected"));
            m.insert("reason".into(), json!(normalize_text(reason)));
            m.insert("updatedAt".into(), json!(ctx.now_iso));
            m.insert(
                "resolvedBy".into(),
                json!({"principalId": ctx.principal_id, "client": ctx.client_label}),
            );
            s.write_candidate(&m)?;
            audit(
                &s.conn,
                "memory.candidate_rejected",
                &ctx.now_iso,
                json!({"reason": reason, "details": {"candidateId": id}}),
            )
            .map_err(sql_err)?;
            Ok(CandidateOutcome {
                error: None,
                candidate_id: id.into(),
                status: "rejected".into(),
                memory_id: None,
                decision: None,
                target_memory_id: c.target_memory_id,
                applied: true,
                already_resolved: false,
                superseded: vec![],
                skipped_members: vec![],
            })
        })
    }

    /// `resolveCandidate` for review proposals: keep | archive | delete.
    /// A stale `targetRevision` aborts the whole resolution (candidate stays
    /// pending) and returns `REVISION_CONFLICT`.
    pub fn resolve_candidate(
        &mut self,
        id: &str,
        decision: &str,
        reason: Option<&str>,
        ctx: &MutationCtx,
        delete_authorized: bool,
    ) -> StoreResult<CandidateOutcome> {
        self.require_writable()?;
        if !matches!(decision, "keep" | "archive" | "delete") {
            return Err(ErrorBody::new(
                ErrorCode::InvalidRequest,
                "decision must be keep, archive, or delete",
            ));
        }
        let mut mutation_failed = false;
        let result = self.resolve_in_tx(
            id,
            decision,
            reason,
            ctx,
            delete_authorized,
            &mut mutation_failed,
        );
        // Only a failed target mutation is a retryable revert; the
        // transaction rolled back, so the candidate is still pending.
        if let Err(e) = &result
            && mutation_failed
            && let Ok(Some((_, c))) = self.read_candidate(id)
            && c.status == "pending"
        {
            let fallback = match &c.review_reason {
                Some(r) => format!("Review resolve ({r})"),
                None => format!("Reviewed: {decision}"),
            };
            let _ = audit(
                &self.conn,
                "memory.candidate_resolution_reverted",
                &ctx.now_iso,
                json!({"memoryId": c.target_memory_id, "reason": reason.map(normalize_text).unwrap_or(fallback),
                       "details": {"candidateId": id, "decision": decision, "mutationError": e.message}}),
            );
            return Ok(CandidateOutcome {
                candidate_id: id.to_string(),
                status: "pending".into(),
                decision: Some(decision.to_string()),
                target_memory_id: c.target_memory_id.clone(),
                applied: false,
                error: Some(e.message.clone()),
                ..Default::default()
            });
        }
        result
    }

    fn resolve_in_tx(
        &mut self,
        id: &str,
        decision: &str,
        reason: Option<&str>,
        ctx: &MutationCtx,
        delete_authorized: bool,
        mutation_failed: &mut bool,
    ) -> StoreResult<CandidateOutcome> {
        let viewer = &Viewer {
            session_id: ctx.session_id.clone(),
        };
        self.in_transaction(|s| {
            let (mut m, c) = s.visible_candidate(id, viewer)?;
            if c.status != "pending" {
                return Ok(Self::already(&c));
            }
            let mut applied = decision == "keep";
            if decision != "keep"
                && let Some(target) = &c.target_memory_id
            {
                match s.get(target, viewer)? {
                    None => {}
                    Some(t) if t.persistence.as_deref() == Some("permanent") => {}
                    Some(_) => {
                        let mut input = Map::new();
                        input.insert(
                            "status".into(),
                            json!(if decision == "delete" { "deleted" } else { "archived" }),
                        );
                        if let Some(rev) = c.target_revision {
                            input.insert("expectedRevision".into(), json!(rev));
                        }
                        let inner = MutationCtx { idempotency_key: None, ..ctx.clone() };
                        if let Err(e) = s.update(target, &input, &inner, delete_authorized) {
                            // A missing grant stays an error; any other
                            // target failure is a retryable revert.
                            *mutation_failed = e.code != ErrorCode::Forbidden;
                            return Err(e);
                        }
                        applied = true;
                    }
                }
            }
            let reason = reason.map(normalize_text).unwrap_or_else(|| match &c.review_reason {
                Some(r) => format!("Review resolve ({r})"),
                None => format!("Reviewed: {decision}"),
            });
            m.insert(
                "status".into(),
                json!(if decision == "keep" { "rejected" } else { "accepted" }),
            );
            m.insert("reason".into(), json!(reason));
            m.insert("updatedAt".into(), json!(ctx.now_iso));
            m.insert(
                "resolvedBy".into(),
                json!({"principalId": ctx.principal_id, "client": ctx.client_label, "decision": decision}),
            );
            s.write_candidate(&m)?;
            audit(
                &s.conn,
                "memory.candidate_resolved",
                &ctx.now_iso,
                json!({"memoryId": c.target_memory_id, "reason": reason, "details": {"candidateId": id, "decision": decision, "applied": applied}}),
            )
            .map_err(sql_err)?;
            Ok(CandidateOutcome {
            error: None,
                candidate_id: id.into(),
                status: if decision == "keep" { "rejected" } else { "accepted" }.into(),
                memory_id: None,
                decision: Some(decision.into()),
                target_memory_id: c.target_memory_id,
                applied,
                already_resolved: false,
                superseded: vec![],
                skipped_members: vec![],
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::{MutationCtx, SageStore, Viewer};
    use serde_json::{Map, json};
    use wrongmemory_contracts::{ErrorCode, ProposeParams, RememberParams};

    const NOW: &str = "2026-11-01T12:00:00.000Z";

    fn ctx() -> MutationCtx {
        MutationCtx {
            principal_id: "host".into(),
            client_label: "t".into(),
            session_id: None,
            idempotency_key: None,
            now_iso: NOW.into(),
        }
    }

    fn store() -> (tempfile::TempDir, SageStore) {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("s").join("sage.db");
        SageStore::create_managed(&p, NOW).unwrap();
        let (s, _) = SageStore::open(&p, true, Some(tmp.path().into()), NOW).unwrap();
        (tmp, s)
    }

    #[test]
    fn accept_creates_memory_atomically_and_is_idempotent() {
        let (_t, mut s) = store();
        let prop = s
            .propose(
                &ProposeParams {
                    text: "Release tags are signed with the project GPG key".into(),
                    ..Default::default()
                },
                &ctx(),
            )
            .unwrap();
        let a = s.accept_candidate(&prop.id, &ctx()).unwrap();
        assert!(a.applied && !a.already_resolved);
        let mid = a.memory_id.clone().unwrap();
        assert!(s.get(&mid, &Viewer::default()).unwrap().is_some());
        let again = s.accept_candidate(&prop.id, &ctx()).unwrap();
        assert!(again.already_resolved);
        assert_eq!(again.memory_id.as_deref(), Some(mid.as_str()));
    }

    #[test]
    fn resolve_with_stale_revision_rolls_back_everything() {
        let (_t, mut s) = store();
        let target = s
            .remember(
                &RememberParams {
                    text: "The cache warms on boot from the snapshot".into(),
                    ..Default::default()
                },
                &ctx(),
            )
            .unwrap()
            .id;
        let prop = s
            .propose(
                &ProposeParams {
                    text: "Archive the boot cache note".into(),
                    target_memory_id: Some(target.clone()),
                    target_revision: Some(1),
                    suggested_action: Some("archive".into()),
                    ..Default::default()
                },
                &ctx(),
            )
            .unwrap();
        // Someone edits the target first.
        let mut input = Map::new();
        input.insert("importance".into(), json!(0.9));
        s.update(&target, &input, &ctx(), false).unwrap();
        // Baseline shape: `applied: false` with the mutation error.
        let out = s
            .resolve_candidate(&prop.id, "archive", None, &ctx(), false)
            .unwrap();
        assert!(!out.applied);
        assert_eq!(out.status, "pending");
        assert!(out.error.unwrap().contains("revision changed"));
        let (list, _, _) = s
            .list_candidates("pending", 10, &Viewer::default())
            .unwrap();
        assert_eq!(
            list.len(),
            1,
            "candidate must stay pending after a failed resolve"
        );
        assert_eq!(
            s.get(&target, &Viewer::default()).unwrap().unwrap().status,
            "active"
        );
    }

    #[test]
    fn resolve_delete_requires_grant_and_keep_rejects() {
        let (_t, mut s) = store();
        let target = s
            .remember(
                &RememberParams {
                    text: "Old deploy script lives in tools/deploy.sh".into(),
                    ..Default::default()
                },
                &ctx(),
            )
            .unwrap()
            .id;
        let mk = |s: &mut SageStore, text: &str| {
            s.propose(
                &ProposeParams {
                    text: text.into(),
                    target_memory_id: Some(target.clone()),
                    ..Default::default()
                },
                &ctx(),
            )
            .unwrap()
            .id
        };
        let p1 = mk(&mut s, "Delete obsolete deploy script note");
        assert_eq!(
            s.resolve_candidate(&p1, "delete", None, &ctx(), false)
                .unwrap_err()
                .code,
            ErrorCode::Forbidden
        );
        let r = s
            .resolve_candidate(&p1, "delete", None, &ctx(), true)
            .unwrap();
        assert!(r.applied);
        assert!(s.get(&target, &Viewer::default()).unwrap().unwrap().status == "deleted");
        let p2 = mk(&mut s, "Keep the deploy script note as is");
        let k = s
            .resolve_candidate(&p2, "keep", None, &ctx(), false)
            .unwrap();
        assert_eq!(k.status, "rejected");
    }

    #[test]
    fn hygiene_review_of_session_memory_stays_in_its_session() {
        let (_t, mut s) = store();
        let sess = MutationCtx {
            session_id: Some("s1".into()),
            ..ctx()
        };
        for text in [
            "The integration suite always runs against the staging database cluster",
            "The integration suite never runs against the staging database cluster",
        ] {
            s.remember(
                &RememberParams {
                    text: text.into(),
                    scope: Some("session".into()),
                    ..Default::default()
                },
                &sess,
            )
            .unwrap();
        }
        let report = s
            .hygiene(&crate::hygiene::HygieneOptions::default(), &sess)
            .unwrap();
        assert!(
            report.contradicted > 0,
            "fixture must produce a contradiction review"
        );
        for other in [None, Some("s2".to_string())] {
            let (list, _, _) = s
                .list_candidates(
                    "pending",
                    50,
                    &Viewer {
                        session_id: other.clone(),
                    },
                )
                .unwrap();
            assert!(list.is_empty(), "review leaked to {other:?}: {list:?}");
        }
        let (own, _, _) = s
            .list_candidates(
                "pending",
                50,
                &Viewer {
                    session_id: Some("s1".into()),
                },
            )
            .unwrap();
        assert!(!own.is_empty());
        assert!(
            own.iter()
                .all(|c| c.scope == "session" && c.owner_session_id.as_deref() == Some("s1"))
        );
    }
}
