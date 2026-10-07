use super::types::SpawnReceipt;

pub fn bootstrap_message(workspace_name: &str, assignment: &str, receipt: &SpawnReceipt) -> String {
    format!(
        r#"{assignment}

(MoonDesk: you are {display_id}, a worker for Core in workspace "{workspace_name}". Before starting, call `workers` action=`claim` with worker_id=`{worker_id}`, task_id=`{task_id}`, claim_token=`{claim_token}`; if claim fails, stop. For project work, use only this workspace's MoonDesk connector, call `moondesk_instruction` before local work, and obey current AGENTS.md and user instructions. Stay on this assignment, do not create workers, and do not change branches or unrelated files unless Core explicitly assigns it. Report meaningful findings to Core with `workers` action=`report`, and call `workers` action=`finish` once with RESULT / CHANGES / VALIDATION / BLOCKERS when done.)

[moondesk-worker-task:{task_id}]
"#,
        assignment = assignment,
        display_id = receipt.display_id,
        workspace_name = workspace_name,
        worker_id = receipt.worker_id,
        task_id = receipt.task_id,
        claim_token = receipt.claim_token,
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

(MoonDesk: you are still {display_id}, the same durable worker for Core in workspace "{workspace_name}". Before starting, call `workers` action=`start` with worker_id=`{worker_id}` and task_id=`{task_id}`; if start fails, stop. For project work, use only this workspace's MoonDesk connector, call `moondesk_instruction` before local work, and obey current AGENTS.md and user instructions. Stay on this assignment, do not create workers, and do not change branches or unrelated files unless Core explicitly assigns it. Report meaningful findings to Core with `workers` action=`report`, and call `workers` action=`finish` once with RESULT / CHANGES / VALIDATION / BLOCKERS when done.)

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
            family_id: WorkerFamilyId::new(),
            worker_id: WorkerId::new(),
            task_id: TaskId::new(),
            display_id: "worker-1".into(),
            claim_token: "claim-secret".into(),
        };
        let text = bootstrap_message("MoonDesk", "Audit auth", &receipt);
        assert!(text.starts_with("Audit auth\n\n"));
        assert!(text.contains("worker for Core"));
        assert!(text.contains("workspace \"MoonDesk\""));
        assert!(text.contains(&receipt.worker_id.to_string()));
        assert!(text.contains(&receipt.task_id.to_string()));
        assert!(text.contains("claim-secret"));
        assert!(text.contains("moondesk_instruction"));
        assert!(text.contains("do not create workers"));
        assert!(text.len() < 1_200, "worker bootstrap should stay compact");
        assert!(!text.contains("expected_model"));
        assert!(!text.contains("expected_reasoning_effort"));
        assert!(text.contains(&format!("moondesk-worker-task:{}", receipt.task_id)));
        assert!(!text.contains("/mcp"));
        assert!(!text.contains("ngrok"));
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
        assert!(text.contains("worker for Core"));
        assert!(text.contains("action=`start`"));
        assert!(text.contains(&worker_id.to_string()));
        assert!(text.contains(&task_id.to_string()));
        assert!(text.contains("same durable worker"));
        assert!(
            text.len() < 1_100,
            "worker continuation should stay compact"
        );
        assert!(!text.contains("claim_token"));
    }
}
