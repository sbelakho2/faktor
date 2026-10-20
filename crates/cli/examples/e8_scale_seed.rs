//! Deterministic Edition-8 scale fixture seeder.
//!
//! This binary writes durable fixture rows through the SAME store APIs the
//! daemon uses, BEFORE the daemon starts. The daemon then serves those rows
//! over its real HTTP routes; nothing is injected behind a running process.
//!
//! Usage:
//!   cargo run -p faktor-cli --example e8_scale_seed -- \
//!     --data-dir /tmp/e8-scale --messages 100000 --sessions 2000 \
//!     --children 500 --claims 10000
//!
//! Output (stdout, one JSON object): the seeded ids the probe needs.

use std::collections::HashMap;
use std::path::PathBuf;

use faktor_core::blocker::ExecutionPhase;
use faktor_core::contract::EngineeringContract;
use faktor_core::id::{TaskId, TaskRevision, WorktreeId};
use faktor_core::state::{
    CheckExecution, CriterionBinding, CriterionVerification, TaskState, VerificationStatus,
};
use faktor_orchestrator::runtime::{ChildRuntime, ModelPolicy};
use faktor_orchestrator::ChildState;
use faktor_orchestrator::{TaskPlan, WorkItem, WorkKind};
use faktor_session::child::ChildOwnership;
use faktor_store::{Store, TaskRow, VerificationRecordRow};

fn parse_args() -> HashMap<String, usize> {
    let mut out = HashMap::new();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let Some(name) = flag.strip_prefix("--") else {
            panic!("unexpected argument {flag:?} (expected --name value)");
        };
        let value = args
            .next()
            .unwrap_or_else(|| panic!("--{name} needs a value"));
        if name == "data-dir" {
            continue;
        }
        out.insert(
            name.to_string(),
            value
                .parse()
                .unwrap_or_else(|_| panic!("--{name} must be an integer")),
        );
    }
    out
}

fn data_dir() -> PathBuf {
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        if flag == "--data-dir" {
            return PathBuf::from(args.next().expect("--data-dir needs a value"));
        }
    }
    panic!("--data-dir is required");
}

fn main() {
    let counts = parse_args();
    let data_dir = data_dir();
    let messages = counts.get("messages").copied().unwrap_or(0);
    let sessions = counts.get("sessions").copied().unwrap_or(0);
    let children = counts.get("children").copied().unwrap_or(0);
    let claims = counts.get("claims").copied().unwrap_or(0);

    let store = Store::open(data_dir.join("store"), true).expect("open store");
    let workspace = store
        .create_workspace("/e8-scale")
        .expect("create scale workspace");

    let main_session = store
        .create_session(workspace, "scale transcript", "p1", "m")
        .expect("create main session");
    let sid = main_session.id;

    for index in 0..messages {
        let seq = index as i64 + 1;
        if index % 2 == 0 {
            let needle = if index % 1_000 == 0 {
                format!(" user-needle-{index}")
            } else {
                String::new()
            };
            store
                .put_message(
                    sid,
                    seq,
                    "user",
                    serde_json::json!({
                        "text": format!("turn {index}: durable user payload with enough text to page.{}", needle),
                        "files": [],
                    }),
                )
                .expect("put user message");
        } else {
            let mid = store
                .put_message(sid, seq, "assistant", serde_json::json!({ "parts": [] }))
                .expect("put assistant message");
            let needle = if index % 1_000 == 1 {
                format!(" assistant-needle-{index} ünïcode-prefix")
            } else {
                String::new()
            };
            store
                .put_part(
                    mid,
                    "text",
                    serde_json::json!({
                        "text": format!("model answer {index}: durable assistant part body.{}", needle)
                    }),
                )
                .expect("put assistant part");
        }
    }

    let mut small_session = String::new();
    if messages >= 10 {
        let small = store
            .create_session(workspace, "scale small transcript", "p1", "m")
            .expect("create small session");
        let small_count = messages / 10;
        for index in 0..small_count {
            let seq = index as i64 + 1;
            if index % 2 == 0 {
                let needle = if index == 0 {
                    " small-needle-0".to_string()
                } else if index == small_count - 1 {
                    " small-needle-last".to_string()
                } else {
                    String::new()
                };
                store
                    .put_message(
                        small.id,
                        seq,
                        "user",
                        serde_json::json!({
                            "text": format!("small turn {index}: durable payload.{}", needle),
                            "files": [],
                        }),
                    )
                    .expect("put small message");
            } else {
                let mid = store
                    .put_message(
                        small.id,
                        seq,
                        "assistant",
                        serde_json::json!({ "parts": [] }),
                    )
                    .expect("put small assistant");
                let needle = if index == small_count - 1 {
                    " small-needle-last"
                } else {
                    ""
                };
                store
                    .put_part(
                        mid,
                        "text",
                        serde_json::json!({
                            "text": format!("small answer {index}: durable part body.{}", needle),
                        }),
                    )
                    .expect("put small part");
            }
        }
        small_session = small.id.to_string();
    }

    let mut history_first = String::new();
    let mut history_last = String::new();
    for index in 0..sessions {
        let row = store
            .create_session(
                workspace,
                &format!("history conversation {index:04}"),
                "p1",
                "m",
            )
            .expect("create history session");
        if index == 0 {
            history_first = row.id.to_string();
        }
        history_last = row.id.to_string();
    }

    let mut claim_task = String::new();
    let mut claim_record = String::new();
    if claims > 0 {
        let task_id = TaskId::new(7);
        let revision = TaskRevision::new(1);
        store
            .upsert_task(&TaskRow {
                task_id,
                session_id: sid,
                goal: "scale proof".into(),
                acceptance_criteria: Vec::new(),
                plan: Vec::new(),
                attachments: Vec::new(),
                contract: EngineeringContract::default(),
                max_tokens: None,
                max_turns: None,
                spent_tokens: 0,
                spent_turns: 0,
                state: TaskState::Running,
                revision,
                created_ms: 1,
                updated_ms: 1,
            })
            .expect("upsert scale task");
        let criteria: Vec<CriterionVerification> = (0..claims)
            .map(|index| CriterionVerification {
                criterion_key: format!("AC-{index:05}"),
                passed: index % 2 == 0,
                evidence: Some(format!("evidence-{index}")),
                // A criterion with no binding is honestly UNAVAILABLE, not
                // a pass/fail. Bind each to its own check so the proof's
                // pass/fail counts are the exact seeded truth.
                binding: Some(CriterionBinding::RequiredCheck {
                    check_id: format!("check-{index:05}"),
                    command_digest: "00".repeat(32),
                }),
            })
            .collect();
        let checks: Vec<CheckExecution> = (0..claims)
            .map(|index| CheckExecution {
                check: format!("check-{index:05}"),
                program: "cargo".into(),
                args: vec!["test".into(), format!("-p{index}")],
                category: "unit".into(),
                required: index % 2 == 0,
                status: if index % 2 == 0 {
                    VerificationStatus::Passed
                } else {
                    VerificationStatus::Failed
                },
                started_ms: index as i64,
                finished_ms: Some(index as i64 + 1),
                exit: Some(if index % 2 == 0 { 0 } else { 1 }),
                summary: Some(format!("claim row {index}")),
            })
            .collect();
        let record_id = store
            .verification_record_put(&VerificationRecordRow {
                id: Default::default(),
                session_id: Some(sid),
                task_id,
                revision,
                workspace_id: workspace,
                worktree_id: WorktreeId::new(1),
                tree_hash: Some("00".repeat(32)),
                criteria,
                checks,
                changed_files: Vec::new(),
                unrelated_changes: Vec::new(),
                reviewer: None,
                status: VerificationStatus::Passed,
                started_ms: 1,
                completed_ms: Some(2),
            })
            .expect("put verification record");
        claim_task = task_id.to_string();
        claim_record = record_id.to_string();
    }

    if children > 0 {
        let run = "scale-run";
        let plan = TaskPlan {
            goal: "scale child projection".into(),
            non_goals: Vec::new(),
            constraints: Vec::new(),
            work_items: (0..children)
                .map(|index| {
                    WorkItem::new(
                        format!("item-{index:04}"),
                        format!("work {index}"),
                        WorkKind::Analysis,
                    )
                })
                .collect(),
        };
        store
            .upsert_memory_fact(
                sid,
                "orchestrator_plan",
                run,
                &serde_json::json!({ "plan": plan, "default_model": "m" }).to_string(),
            )
            .expect("plan fact");
        for index in 0..children {
            let child_session = store
                .create_session(workspace, &format!("scale child {index:04}"), "p1", "m")
                .expect("create child session");
            let row = ChildRuntime {
                child_id: format!("child-{index:04}"),
                parent_session_id: sid.raw(),
                run_id: run.into(),
                item_id: format!("item-{index:04}"),
                kind: WorkKind::Analysis,
                session_id: child_session.id.raw(),
                operation_id: 0,
                workspace_id: workspace.raw(),
                worktree_id: 1,
                ownership: ChildOwnership::ReadOnlyShared,
                ownership_paths: Vec::new(),
                state: ChildState::Done,
                budget_max_tokens: None,
                permissions: Default::default(),
                model_policy: ModelPolicy::default(),
                blocker_kind: None,
                blocker_reason: None,
                blocker_dependency: None,
                blocker_resolution: None,
                last_progress_ms: None,
                created_ms: index as i64 + 1,
                updated_ms: index as i64 + 1,
                base_snapshot_id: None,
                run_base_snapshot: None,
                env_snapshot_id: None,
                execution_phase: ExecutionPhase::default(),
            };
            store
                .upsert_memory_fact(
                    sid,
                    "orchestrator_registry",
                    &format!("{run}/child-{index:04}"),
                    &serde_json::to_string(&row).expect("child row json"),
                )
                .expect("child fact");
        }
    }

    println!(
        "{{\"workspace\":{},\"session\":\"{}\",\"small_session\":{},\"history_first\":{},\"history_last\":{},\"claim_task\":{},\"claim_record\":{},\"messages\":{},\"sessions\":{},\"children\":{},\"claims\":{}}}",
        workspace.raw(),
        sid,
        if small_session.is_empty() {
            "null".to_string()
        } else {
            format!("\"{small_session}\"")
        },
        if history_first.is_empty() {
            "null".to_string()
        } else {
            format!("\"{history_first}\"")
        },
        if history_last.is_empty() {
            "null".to_string()
        } else {
            format!("\"{history_last}\"")
        },
        if claim_task.is_empty() {
            "null".to_string()
        } else {
            claim_task
        },
        if claim_record.is_empty() {
            "null".to_string()
        } else {
            format!("\"{claim_record}\"")
        },
        messages,
        sessions,
        children,
        claims
    );
}
