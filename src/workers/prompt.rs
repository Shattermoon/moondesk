use super::types::{SpawnReceipt, WorkerExecutionProfile};

pub fn bootstrap_message(
    workspace_name: &str,
    assignment: &str,
    receipt: &SpawnReceipt,
    execution_profile: &WorkerExecutionProfile,
) -> String {
    format!(
        r#"Your assignment:
{assignment}

[MoonDesk worker contract]
You are {display_id}, a worker for the Anchor conversation in MoonDesk workspace "{workspace_name}".

Before doing any local work, call the MoonDesk `workers` tool with action=`claim` using the exact worker_id, task_id, and one-time claim_token below. If the claim fails, stop and report the failure in this chat instead of attempting local work.

Use only this workspace's MoonDesk connector for project work. Do not switch to a different workspace connector. Call `moondesk_instruction` before local work and obey the repository's current AGENTS.md and user instructions.

Stay within this assignment. Do not create workers of your own. Do not switch branches, reset, rebase, stash, remove worktrees, merge, publish, or modify unrelated work unless the assignment explicitly requires it.

Report only meaningful discoveries or blockers to the Anchor with `workers` action=`report`; do not spam routine progress. Treat an ambiguous mutation result as uncertain and inspect whether it already happened before retrying.

When the assignment is complete, call `workers` action=`finish` exactly once with RESULT / CHANGES / VALIDATION / BLOCKERS, then stop.

Worker identity:
worker_id: {worker_id}
task_id: {task_id}
claim_token: {claim_token}
expected_model: {model_label}
expected_reasoning_effort: {reasoning_effort:?}
moondesk_task_marker: moondesk-worker-task:{task_id}
"#,
        assignment = assignment,
        display_id = receipt.display_id,
        workspace_name = workspace_name,
        worker_id = receipt.worker_id,
        task_id = receipt.task_id,
        claim_token = receipt.claim_token,
        model_label = execution_profile.model_label,
        reasoning_effort = execution_profile.reasoning_effort,
    )
}

pub fn reuse_message(
    workspace_name: &str,
    assignment: &str,
    display_id: &str,
    worker_id: &super::types::WorkerId,
    task_id: &super::types::TaskId,
    execution_profile: &WorkerExecutionProfile,
) -> String {
    format!(
        r#"New assignment:
{assignment}

[MoonDesk worker continuation]
You are still {display_id}, the same durable worker for the Anchor conversation in MoonDesk workspace "{workspace_name}".

Before doing any local work on this assignment, call the MoonDesk `workers` tool with action=`start` using the exact worker_id and task_id below. If that start fails, stop and report the failure in this chat instead of attempting local work.

Use only this workspace's MoonDesk connector for project work. Call `moondesk_instruction` before local work and obey the repository's current AGENTS.md and user instructions.

Stay within this assignment. Do not create workers of your own. Do not switch branches, reset, rebase, stash, remove worktrees, merge, publish, or modify unrelated work unless the assignment explicitly requires it.

Report only meaningful discoveries or blockers to the Anchor with `workers` action=`report`; do not spam routine progress. Treat an ambiguous mutation result as uncertain and inspect whether it already happened before retrying.

When the assignment is complete, call `workers` action=`finish` exactly once with RESULT / CHANGES / VALIDATION / BLOCKERS, then stop.

Worker identity:
worker_id: {worker_id}
task_id: {task_id}
expected_model: {model_label}
expected_reasoning_effort: {reasoning_effort:?}
moondesk_task_marker: moondesk-worker-task:{task_id}
"#,
        assignment = assignment,
        display_id = display_id,
        workspace_name = workspace_name,
        worker_id = worker_id,
        task_id = task_id,
        model_label = execution_profile.model_label,
        reasoning_effort = execution_profile.reasoning_effort,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::managed_chat::types::ReasoningEffort;
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
        let profile = WorkerExecutionProfile {
            model_key: "gpt-5.6-sol".into(),
            model_label: "GPT-5.6 Sol".into(),
            reasoning_effort: ReasoningEffort::High,
        };
        let text = bootstrap_message("MoonDesk", "Audit auth", &receipt, &profile);
        assert!(text.contains("Audit auth"));
        assert!(text.contains("workspace \"MoonDesk\""));
        assert!(text.contains(&receipt.worker_id.to_string()));
        assert!(text.contains(&receipt.task_id.to_string()));
        assert!(text.contains("claim-secret"));
        assert!(text.contains("moondesk_instruction"));
        assert!(text.contains("Do not create workers of your own"));
        assert!(!text.contains("/mcp"));
        assert!(!text.contains("ngrok"));
    }

    #[test]
    fn reuse_prompt_starts_new_task_without_reclaiming_worker() {
        let worker_id = WorkerId::new();
        let task_id = TaskId::new();
        let profile = WorkerExecutionProfile {
            model_key: "gpt-5.6-sol-high".into(),
            model_label: "GPT-5.6 Sol".into(),
            reasoning_effort: ReasoningEffort::High,
        };
        let text = reuse_message(
            "MoonDesk",
            "Review the follow-up regression",
            "worker-1",
            &worker_id,
            &task_id,
            &profile,
        );
        assert!(text.contains("action=`start`"));
        assert!(text.contains(&worker_id.to_string()));
        assert!(text.contains(&task_id.to_string()));
        assert!(text.contains("same durable worker"));
        assert!(!text.contains("claim_token"));
    }
}
