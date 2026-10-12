use super::types::{OperationId, SpawnReceipt};

fn task_briefing(context: &str, assignment: &str) -> String {
    let context = context.trim();
    if context.is_empty() {
        return format!("Your assignment:\n{assignment}");
    }
    format!("Shared Core context:\n{context}\n\nYour assignment:\n{assignment}")
}

pub fn bootstrap_message(
    workspace_name: &str,
    context: &str,
    assignment: &str,
    receipt: &SpawnReceipt,
    claim_operation_id: &OperationId,
) -> String {
    let briefing = task_briefing(context, assignment);
    format!(
        r#"{briefing}

(MoonDesk worker contract: you are {display_id}, a worker for Core in workspace "{workspace_name}". The Shared Core context preserves prior goals, decisions, constraints, validation, and sibling ownership; use it to avoid rediscovering settled context, while Your assignment defines your owned scope. Core may later refine this assignment; follow the latest Core direction. Before starting, call `workers` action=`claim` with worker_id=`{worker_id}`, task_id=`{task_id}`, claim_token=`{claim_token}`, operation_id=`{claim_operation_id}`; if claim fails, stop. For project work, use only this workspace's MoonDesk connector, call `moondesk_instruction` before local work, and obey current AGENTS.md and user instructions. Work independently until the assignment is complete, do not create workers, and do not change branches or unrelated files unless Core explicitly assigns it. Report meaningful discoveries or blockers to Core with `workers` action=`report`; do not spam routine progress. Call `workers` action=`finish` once with RESULT / CHANGES / VALIDATION / BLOCKERS when done.)

[moondesk-worker-task:{task_id}]
"#,
        briefing = briefing,
        display_id = receipt.display_id,
        workspace_name = workspace_name,
        worker_id = receipt.worker_id,
        task_id = receipt.task_id,
        claim_token = receipt.claim_token,
        claim_operation_id = claim_operation_id,
    )
}

pub fn reuse_message(
    workspace_name: &str,
    assignment: &str,
    display_id: &str,
    worker_id: &super::types::WorkerId,
    task_id: &super::types::TaskId,
) -> String {
    format!(
        r#"{assignment}

(MoonDesk worker contract: you are still {display_id}, the same durable worker for Core in workspace "{workspace_name}". This is Core speaking to you again in the conversation you already know. Use your existing conversation history and prior work rather than starting over, and treat the assignment above as the newest direction. Before starting, call `workers` action=`start` with worker_id=`{worker_id}` and task_id=`{task_id}`; if start fails, stop. For project work, use only this workspace's MoonDesk connector, call `moondesk_instruction` before local work, and obey current AGENTS.md and user instructions. Work independently until the assignment is complete, do not create workers, and do not change branches or unrelated files unless Core explicitly assigns it. Report meaningful discoveries or blockers to Core with `workers` action=`report`; do not spam routine progress. Call `workers` action=`finish` once with RESULT / CHANGES / VALIDATION / BLOCKERS when done.)

[moondesk-worker-task:{task_id}]
"#,
        assignment = assignment,
        display_id = display_id,
        workspace_name = workspace_name,
        worker_id = worker_id,
        task_id = task_id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workers::types::{TaskId, WorkerFamilyId, WorkerId};

    #[test]
    fn bootstrap_contains_claim_and_workspace_contract_without_secret_mcp_route() {
        let receipt = SpawnReceipt {
            request_fingerprint: "a".repeat(64),
            sequence: 0,
            family_id: WorkerFamilyId::new(),
            worker_id: WorkerId::new(),
            task_id: TaskId::new(),
            display_id: "worker-1".into(),
            claim_token: "claim-secret".into(),
            claim_operation_id: None,
        };
        let claim_operation_id = OperationId::new();
        let text = bootstrap_message(
            "MoonDesk",
            "PR #141 review; preserve user constraints and avoid overlapping the runtime reviewer.",
            "Audit auth",
            &receipt,
            &claim_operation_id,
        );
        assert!(text.starts_with("Shared Core context:\nPR #141 review"));
        assert!(text.contains("Your assignment:\nAudit auth"));
        assert!(text.contains("worker for Core"));
        assert!(text.contains("workspace \"MoonDesk\""));
        assert!(text.contains(&receipt.worker_id.to_string()));
        assert!(text.contains(&receipt.task_id.to_string()));
        assert!(text.contains("claim-secret"));
        assert!(text.contains(&claim_operation_id.to_string()));
        assert!(text.contains("operation_id"));
        assert!(text.contains("moondesk_instruction"));
        assert!(text.contains("do not create workers"));
        assert!(text.len() < 2_000, "worker bootstrap should stay compact");
        assert!(!text.contains("expected_model"));
        assert!(!text.contains("expected_reasoning_effort"));
        assert!(text.contains(&format!("moondesk-worker-task:{}", receipt.task_id)));
        assert!(!text.contains("/mcp"));
        assert!(!text.contains("ngrok"));
    }

    #[test]
    fn task_briefing_keeps_self_contained_assignments_clean_without_context() {
        assert_eq!(
            task_briefing("", "Inspect the failing test"),
            "Your assignment:\nInspect the failing test"
        );
    }

    #[test]
    fn reuse_prompt_starts_new_task_without_reclaiming_worker() {
        let worker_id = WorkerId::new();
        let task_id = TaskId::new();
        let text = reuse_message(
            "MoonDesk",
            "Review the follow-up regression",
            "worker-1",
            &worker_id,
            &task_id,
        );
        assert!(text.starts_with("Review the follow-up regression\n\n"));
        assert!(!text.contains("Shared Core context:"));
        assert!(text.contains("conversation you already know"));
        assert!(text.contains("existing conversation history and prior work"));
        assert!(text.contains("worker for Core"));
        assert!(text.contains("action=`start`"));
        assert!(text.contains(&worker_id.to_string()));
        assert!(text.contains(&task_id.to_string()));
        assert!(text.contains("same durable worker"));
        assert!(
            text.len() < 1_900,
            "worker continuation should stay compact"
        );
        assert!(!text.contains("claim_token"));
    }
}
