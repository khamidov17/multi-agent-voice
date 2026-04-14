//! Code-enforced workflow engine — Google ADK LoopAgent/SequentialAgent patterns.
//!
//! Moves multi-agent routing decisions from prompts to Rust code.
//! LLMs do the thinking; Rust controls the flow.
//!
//! Key patterns stolen from Google ADK:
//! - **LoopAgent**: code-enforced verify→retry loops (no LLM willpower needed)
//! - **output_key**: typed state dict passed between steps via `{key}` substitution
//! - **escalate**: `CompleteWorkflowStep` with `passed` field controls loop termination
//! - **Sequential**: steps execute in order, each sees output of previous steps

use rusqlite::params;
use serde::{Deserialize, Serialize};
use tracing::info;

// ─── Types ──────────────────────────────────────────────────────────────

/// A workflow: a sequence of steps executed by agents, controlled by Rust code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    pub id: String,
    pub name: String,
    pub steps: Vec<WorkflowStep>,
    pub current_step: usize,
    pub status: WorkflowStatus,
    pub max_iterations: u32,
    pub current_iteration: u32,
    /// Shared state dict — Google's output_key pattern.
    /// Each step can write to a key; subsequent steps read via `{key}` substitution.
    pub state: serde_json::Value,
    pub created_at: String,
}

/// A single step in a workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStep {
    pub name: String,
    /// Which agent executes this step ("Nova", "Atlas", "Sentinel").
    pub agent: String,
    /// What to tell the agent. Supports `{key}` substitution from workflow state.
    pub instruction: String,
    /// If set, the step's result is saved to `state[output_key]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_key: Option<String>,
    pub step_type: StepType,
    pub status: StepStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
}

impl Default for WorkflowStep {
    fn default() -> Self {
        Self {
            name: String::new(),
            agent: String::new(),
            instruction: String::new(),
            output_key: None,
            step_type: StepType::Execute,
            status: StepStatus::Pending,
            result: None,
        }
    }
}

/// Type of workflow step.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum StepType {
    /// Agent does work, result saved to output_key.
    Execute,
    /// Agent verifies previous step's output. If `passed=false`, loop back.
    Verify,
    /// Conditional: check `state[condition_key] == expected_value`.
    /// If not, skip this step.
    Gate {
        condition_key: String,
        expected_value: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum StepStatus {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStatus {
    Running,
    Paused,
    Completed,
    Failed,
    MaxIterationsReached,
}

impl Default for Workflow {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            steps: Vec::new(),
            current_step: 0,
            status: WorkflowStatus::Running,
            max_iterations: 5,
            current_iteration: 0,
            state: serde_json::json!({}),
            created_at: chrono::Utc::now().to_rfc3339(),
        }
    }
}

// ─── Advance result ─────────────────────────────────────────────────────

/// What `advance_workflow` determined should happen next.
pub enum AdvanceResult {
    /// Route a message to the next agent.
    NextStep { agent: String, message: String },
    /// Workflow completed successfully.
    Completed(Workflow),
    /// Hit max iterations on a verify loop.
    MaxIterations(Workflow),
}

// ─── SQLite persistence ─────────────────────────────────────────────────

/// Create the workflows table. Called from BotMessageDb::open().
pub fn create_workflows_table(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS workflows (
            id                TEXT PRIMARY KEY,
            name              TEXT NOT NULL,
            steps_json        TEXT NOT NULL,
            current_step      INTEGER NOT NULL DEFAULT 0,
            status            TEXT NOT NULL DEFAULT 'running',
            max_iterations    INTEGER NOT NULL DEFAULT 5,
            current_iteration INTEGER NOT NULL DEFAULT 0,
            state_json        TEXT NOT NULL DEFAULT '{}',
            created_at        TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at        TEXT NOT NULL DEFAULT (datetime('now'))
        );",
    )
}

pub fn save_workflow(conn: &rusqlite::Connection, wf: &Workflow) -> anyhow::Result<()> {
    let steps_json = serde_json::to_string(&wf.steps)?;
    let state_json = serde_json::to_string(&wf.state)?;
    conn.execute(
        "INSERT OR REPLACE INTO workflows
         (id, name, steps_json, current_step, status, max_iterations,
          current_iteration, state_json, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, datetime('now'))",
        params![
            wf.id,
            wf.name,
            steps_json,
            wf.current_step,
            serde_json::to_string(&wf.status)?,
            wf.max_iterations,
            wf.current_iteration,
            state_json,
            wf.created_at,
        ],
    )?;
    Ok(())
}

pub fn load_workflow(conn: &rusqlite::Connection, id: &str) -> Result<Workflow, String> {
    conn.query_row(
        "SELECT id, name, steps_json, current_step, status, max_iterations,
                current_iteration, state_json, created_at
         FROM workflows WHERE id = ?1",
        params![id],
        |row| {
            let steps_json: String = row.get(2)?;
            let status_str: String = row.get(4)?;
            let state_json: String = row.get(7)?;
            Ok(Workflow {
                id: row.get(0)?,
                name: row.get(1)?,
                steps: serde_json::from_str(&steps_json).unwrap_or_default(),
                current_step: row.get(3)?,
                status: serde_json::from_str(&format!("\"{}\"", status_str))
                    .unwrap_or(WorkflowStatus::Running),
                max_iterations: row.get(5)?,
                current_iteration: row.get(6)?,
                state: serde_json::from_str(&state_json).unwrap_or(serde_json::json!({})),
                created_at: row.get(8)?,
            })
        },
    )
    .map_err(|e| format!("Workflow '{}' not found: {}", id, e))
}

pub fn get_active_workflows(conn: &rusqlite::Connection) -> Vec<Workflow> {
    let mut stmt = match conn.prepare(
        "SELECT id, name, steps_json, current_step, status, max_iterations,
                current_iteration, state_json, created_at
         FROM workflows WHERE status = '\"running\"' OR status = 'running'
         ORDER BY created_at DESC LIMIT 20",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    stmt.query_map([], |row| {
        let steps_json: String = row.get(2)?;
        let status_str: String = row.get(4)?;
        let state_json: String = row.get(7)?;
        Ok(Workflow {
            id: row.get(0)?,
            name: row.get(1)?,
            steps: serde_json::from_str(&steps_json).unwrap_or_default(),
            current_step: row.get(3)?,
            status: serde_json::from_str(&format!("\"{}\"", status_str))
                .unwrap_or(WorkflowStatus::Running),
            max_iterations: row.get(5)?,
            current_iteration: row.get(6)?,
            state: serde_json::from_str(&state_json).unwrap_or(serde_json::json!({})),
            created_at: row.get(8)?,
        })
    })
    .ok()
    .map(|rows| rows.filter_map(|r| r.ok()).collect())
    .unwrap_or_default()
}

// ─── Core engine: advance_workflow ──────────────────────────────────────

/// Advance a workflow after a step completes.
///
/// This is the heart of the engine. The RUST CODE decides what happens next:
/// - On verify failure → loop back to the last Execute step (LoopAgent pattern)
/// - On gate mismatch → skip the step
/// - On success → move to next step with `{key}` substitution
///
/// Returns what should happen next: route to an agent, or workflow is done.
pub fn advance_workflow(
    conn: &rusqlite::Connection,
    workflow_id: &str,
    step_result: &str,
    step_passed: bool,
    output_data: Option<&str>,
) -> Result<AdvanceResult, String> {
    let mut wf = load_workflow(conn, workflow_id)?;

    if wf.current_step >= wf.steps.len() {
        return Err(format!("Workflow {} has no more steps", workflow_id));
    }

    // Save result to current step
    let current = &mut wf.steps[wf.current_step];
    current.status = if step_passed {
        StepStatus::Done
    } else {
        StepStatus::Failed
    };
    current.result = Some(step_result.to_string());

    // Save to state via output_key (Google's output_key pattern)
    let output_key = current.output_key.clone();
    let current_type = current.step_type.clone();

    if let Some(key) = output_key {
        let value = output_data.unwrap_or(step_result);
        wf.state[key] = serde_json::Value::String(value.to_string());
    }

    // ── Verify failure → LoopAgent pattern ──────────────────────────
    if !step_passed && current_type == StepType::Verify {
        wf.current_iteration += 1;
        if wf.current_iteration >= wf.max_iterations {
            wf.status = WorkflowStatus::MaxIterationsReached;
            save_workflow(conn, &wf).map_err(|e| e.to_string())?;
            info!(
                "[workflow] {} hit max iterations ({}) — stopping",
                wf.id, wf.max_iterations
            );
            return Ok(AdvanceResult::MaxIterations(wf));
        }

        // Loop back to the last Execute step before this Verify step
        let redo_step = find_last_execute_before(wf.current_step, &wf.steps);
        wf.steps[redo_step].status = StepStatus::Pending;
        wf.steps[redo_step].result = None;
        wf.current_step = redo_step;
        save_workflow(conn, &wf).map_err(|e| e.to_string())?;

        let agent = wf.steps[redo_step].agent.clone();
        let original_instruction = &wf.steps[redo_step].instruction;
        let instruction = format!(
            "[WORKFLOW:{}] Iteration {}/{}: Verification FAILED.\n\
             Reason: {}\n\
             Current state: {}\n\
             Fix the issues and try again.\n\n\
             Original instruction: {}",
            wf.id,
            wf.current_iteration + 1,
            wf.max_iterations,
            step_result,
            format_state_compact(&wf.state),
            substitute_state_vars(original_instruction, &wf.state),
        );

        info!(
            "[workflow] {} verify failed — looping back to step {} ({}), iteration {}/{}",
            wf.id,
            redo_step,
            wf.steps[redo_step].name,
            wf.current_iteration + 1,
            wf.max_iterations
        );

        return Ok(AdvanceResult::NextStep {
            agent,
            message: instruction,
        });
    }

    // ── Move to next step ───────────────────────────────────────────
    wf.current_step += 1;

    // Skip any Gate steps whose conditions aren't met
    while wf.current_step < wf.steps.len() {
        if let StepType::Gate {
            ref condition_key,
            ref expected_value,
        } = wf.steps[wf.current_step].step_type
        {
            let actual = wf
                .state
                .get(condition_key)
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if actual != expected_value {
                info!(
                    "[workflow] {} skipping gate step {} ({}!={}) ",
                    wf.id, wf.steps[wf.current_step].name, actual, expected_value
                );
                wf.steps[wf.current_step].status = StepStatus::Skipped;
                wf.current_step += 1;
                continue;
            }
        }
        break;
    }

    // Check if we've reached the end
    if wf.current_step >= wf.steps.len() {
        wf.status = WorkflowStatus::Completed;
        save_workflow(conn, &wf).map_err(|e| e.to_string())?;
        info!("[workflow] {} completed successfully", wf.id);
        return Ok(AdvanceResult::Completed(wf));
    }

    // Start the next step
    wf.steps[wf.current_step].status = StepStatus::Running;
    save_workflow(conn, &wf).map_err(|e| e.to_string())?;

    let next = &wf.steps[wf.current_step];
    let instruction = substitute_state_vars(&next.instruction, &wf.state);
    let message = format!(
        "[WORKFLOW:{}] Step {}/{}: {}\n\n{}",
        wf.id,
        wf.current_step + 1,
        wf.steps.len(),
        next.name,
        instruction,
    );

    info!(
        "[workflow] {} advancing to step {}: {} (agent: {})",
        wf.id,
        wf.current_step + 1,
        next.name,
        next.agent
    );

    Ok(AdvanceResult::NextStep {
        agent: next.agent.clone(),
        message,
    })
}

// ─── Template substitution ──────────────────────────────────────────────

/// Substitute `{key}` placeholders in a string with values from workflow state.
/// `{key?}` is optional — missing values become empty string instead of being left as-is.
pub fn substitute_state_vars(template: &str, state: &serde_json::Value) -> String {
    let mut result = template.to_string();
    // Simple substitution — find {word} or {word?} patterns
    // Using a manual loop because we need two find() calls and may skip/continue
    #[allow(clippy::while_let_loop)]
    loop {
        let start = match result.find('{') {
            Some(i) => i,
            None => break,
        };
        let end = match result[start..].find('}') {
            Some(i) => start + i,
            None => break,
        };
        let inner = &result[start + 1..end];
        if inner.is_empty()
            || !inner
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '?')
        {
            // Not a valid placeholder — skip past this brace
            result = format!("{}{}", &result[..start + 1], &result[start + 1..]);
            // Prevent infinite loop by replacing the opening brace with a placeholder
            let before = &result[..start];
            let after = &result[start + 1..];
            result = format!("{}\x00{}", before, after);
            continue;
        }

        let optional = inner.ends_with('?');
        let key = if optional {
            &inner[..inner.len() - 1]
        } else {
            inner
        };

        let replacement = match state.get(key).and_then(|v| v.as_str()) {
            Some(val) => val.to_string(),
            None if optional => String::new(),
            None => {
                // Leave as-is — skip past
                let before = &result[..start];
                let after = &result[start + 1..];
                result = format!("{}\x00{}", before, after);
                continue;
            }
        };

        result = format!("{}{}{}", &result[..start], replacement, &result[end + 1..]);
    }
    // Restore any escaped braces
    result.replace('\x00', "{")
}

// ─── Helpers ────────────────────────────────────────────────────────────

/// Find the last Execute step before `index`.
fn find_last_execute_before(index: usize, steps: &[WorkflowStep]) -> usize {
    for i in (0..index).rev() {
        if steps[i].step_type == StepType::Execute {
            return i;
        }
    }
    0 // fallback to first step
}

/// Format workflow state compactly for inclusion in messages.
fn format_state_compact(state: &serde_json::Value) -> String {
    if let Some(obj) = state.as_object() {
        if obj.is_empty() {
            return "(empty)".to_string();
        }
        obj.iter()
            .map(|(k, v)| {
                let fallback = v.to_string();
                let val = v.as_str().unwrap_or(&fallback);
                let truncated: String = val.chars().take(100).collect();
                format!("  {}: {}", k, truncated)
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        "(empty)".to_string()
    }
}

/// Format a workflow into a human-readable status for the orchestrator.
pub fn format_workflow_status(wf: &Workflow) -> String {
    let mut lines = vec![format!(
        "Workflow '{}' [{}] — {:?}, iteration {}/{}",
        wf.name, wf.id, wf.status, wf.current_iteration, wf.max_iterations
    )];
    for (i, step) in wf.steps.iter().enumerate() {
        let marker = if i == wf.current_step && wf.status == WorkflowStatus::Running {
            "→"
        } else {
            " "
        };
        let status = match step.status {
            StepStatus::Pending => "pending",
            StepStatus::Running => "RUNNING",
            StepStatus::Done => "done",
            StepStatus::Failed => "FAILED",
            StepStatus::Skipped => "skipped",
        };
        lines.push(format!(
            "{} {}. [{}] {} ({}: {:?})",
            marker,
            i + 1,
            status,
            step.name,
            step.agent,
            step.step_type
        ));
    }
    lines.join("\n")
}

// ─── Predefined workflow templates ──────────────────────────────────────

/// Build → Verify → Report workflow (the most common multi-agent pattern).
pub fn build_verify_report(
    name: &str,
    task_description: &str,
    builder: &str,
    verifier: &str,
    reporter: &str,
    max_iterations: u32,
) -> Workflow {
    Workflow {
        id: format!("wf-{}", uuid::Uuid::new_v4()),
        name: name.to_string(),
        steps: vec![
            WorkflowStep {
                name: "Build".to_string(),
                agent: builder.to_string(),
                instruction: task_description.to_string(),
                output_key: Some("build_result".to_string()),
                step_type: StepType::Execute,
                ..Default::default()
            },
            WorkflowStep {
                name: "Verify".to_string(),
                agent: verifier.to_string(),
                instruction: "Verify the build result: {build_result}\n\
                    Run tests, check for issues. Report PASS or FAIL with details."
                    .to_string(),
                output_key: Some("verification".to_string()),
                step_type: StepType::Verify,
                ..Default::default()
            },
            WorkflowStep {
                name: "Report".to_string(),
                agent: reporter.to_string(),
                instruction:
                    "Report to the owner: Task '{build_result}' verification: {verification}"
                        .to_string(),
                output_key: None,
                step_type: StepType::Execute,
                ..Default::default()
            },
        ],
        max_iterations,
        ..Default::default()
    }
}

/// Build → Verify loop (no report step, for internal tasks).
pub fn build_verify_loop(
    name: &str,
    task_description: &str,
    builder: &str,
    verifier: &str,
    max_iterations: u32,
) -> Workflow {
    Workflow {
        id: format!("wf-{}", uuid::Uuid::new_v4()),
        name: name.to_string(),
        steps: vec![
            WorkflowStep {
                name: "Build".to_string(),
                agent: builder.to_string(),
                instruction: task_description.to_string(),
                output_key: Some("build_result".to_string()),
                step_type: StepType::Execute,
                ..Default::default()
            },
            WorkflowStep {
                name: "Verify".to_string(),
                agent: verifier.to_string(),
                instruction: "Verify: {build_result}\n\
                    Run tests and checks. Set passed=true if quality is acceptable."
                    .to_string(),
                output_key: Some("verification".to_string()),
                step_type: StepType::Verify,
                ..Default::default()
            },
        ],
        max_iterations,
        ..Default::default()
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        create_workflows_table(&conn).unwrap();
        conn
    }

    #[test]
    fn test_substitute_state_vars() {
        let state = serde_json::json!({
            "name": "Alice",
            "result": "PASS"
        });
        assert_eq!(
            substitute_state_vars("Hello {name}, status: {result}", &state),
            "Hello Alice, status: PASS"
        );
        // Optional missing key → empty string
        assert_eq!(
            substitute_state_vars("extra: {missing?}", &state),
            "extra: "
        );
        // Non-optional missing key → left as-is
        assert_eq!(
            substitute_state_vars("keep {unknown}", &state),
            "keep {unknown}"
        );
    }

    #[test]
    fn test_create_and_load_workflow() {
        let conn = test_conn();
        let wf = build_verify_report("Test WF", "Build something", "Nova", "Sentinel", "Atlas", 3);
        save_workflow(&conn, &wf).unwrap();

        let loaded = load_workflow(&conn, &wf.id).unwrap();
        assert_eq!(loaded.name, "Test WF");
        assert_eq!(loaded.steps.len(), 3);
        assert_eq!(loaded.max_iterations, 3);
        assert_eq!(loaded.status, WorkflowStatus::Running);
    }

    #[test]
    fn test_advance_happy_path() {
        let conn = test_conn();
        let mut wf = build_verify_report("Happy", "Build X", "Nova", "Sentinel", "Atlas", 3);
        wf.steps[0].status = StepStatus::Running;
        save_workflow(&conn, &wf).unwrap();

        // Step 1 (Build) completes
        let result = advance_workflow(&conn, &wf.id, "Built X successfully", true, None).unwrap();
        match result {
            AdvanceResult::NextStep { agent, message } => {
                assert_eq!(agent, "Sentinel");
                assert!(message.contains("Verify"));
                assert!(message.contains("Built X successfully")); // substituted
            }
            _ => panic!("Expected NextStep"),
        }

        // Step 2 (Verify) passes
        let result = advance_workflow(&conn, &wf.id, "All tests pass", true, None).unwrap();
        match result {
            AdvanceResult::NextStep { agent, message } => {
                assert_eq!(agent, "Atlas");
                assert!(message.contains("Report"));
            }
            _ => panic!("Expected NextStep"),
        }

        // Step 3 (Report) completes
        let result = advance_workflow(&conn, &wf.id, "Reported to owner", true, None).unwrap();
        match result {
            AdvanceResult::Completed(wf) => {
                assert_eq!(wf.status, WorkflowStatus::Completed);
            }
            _ => panic!("Expected Completed"),
        }
    }

    #[test]
    fn test_advance_verify_failure_loops_back() {
        let conn = test_conn();
        let mut wf = build_verify_loop("Loop", "Build Y", "Nova", "Sentinel", 3);
        wf.steps[0].status = StepStatus::Running;
        save_workflow(&conn, &wf).unwrap();

        // Build completes
        advance_workflow(&conn, &wf.id, "Built Y", true, None).unwrap();

        // Verify FAILS — should loop back to Build
        let result = advance_workflow(
            &conn,
            &wf.id,
            "Tests failed: missing error handling",
            false,
            None,
        )
        .unwrap();
        match result {
            AdvanceResult::NextStep { agent, message } => {
                assert_eq!(agent, "Nova");
                assert!(message.contains("FAILED"));
                assert!(message.contains("missing error handling"));
                assert!(message.contains("Iteration 2/3"));
            }
            _ => panic!("Expected NextStep (loop back)"),
        }

        // Check DB state
        let loaded = load_workflow(&conn, &wf.id).unwrap();
        assert_eq!(loaded.current_step, 0); // back to Build step
        assert_eq!(loaded.current_iteration, 1);
    }

    #[test]
    fn test_advance_max_iterations() {
        let conn = test_conn();
        let mut wf = build_verify_loop("Max", "Build Z", "Nova", "Sentinel", 2);
        wf.steps[0].status = StepStatus::Running;
        save_workflow(&conn, &wf).unwrap();

        // Build → Verify FAIL → Build → Verify FAIL → max iterations
        advance_workflow(&conn, &wf.id, "Built Z", true, None).unwrap();
        advance_workflow(&conn, &wf.id, "Fail 1", false, None).unwrap();
        advance_workflow(&conn, &wf.id, "Built Z v2", true, None).unwrap();

        let result = advance_workflow(&conn, &wf.id, "Fail 2", false, None).unwrap();
        match result {
            AdvanceResult::MaxIterations(wf) => {
                assert_eq!(wf.status, WorkflowStatus::MaxIterationsReached);
                assert_eq!(wf.current_iteration, 2);
            }
            _ => panic!("Expected MaxIterations"),
        }
    }

    #[test]
    fn test_gate_step_skip() {
        let conn = test_conn();
        let wf = Workflow {
            id: "wf-gate".to_string(),
            name: "Gate test".to_string(),
            steps: vec![
                WorkflowStep {
                    name: "Check".to_string(),
                    agent: "Nova".to_string(),
                    instruction: "Check things".to_string(),
                    output_key: Some("check_result".to_string()),
                    step_type: StepType::Execute,
                    status: StepStatus::Running,
                    ..Default::default()
                },
                WorkflowStep {
                    name: "Only if failed".to_string(),
                    agent: "Nova".to_string(),
                    instruction: "Fix things".to_string(),
                    output_key: None,
                    step_type: StepType::Gate {
                        condition_key: "check_result".to_string(),
                        expected_value: "FAIL".to_string(),
                    },
                    status: StepStatus::Pending,
                    ..Default::default()
                },
                WorkflowStep {
                    name: "Final".to_string(),
                    agent: "Atlas".to_string(),
                    instruction: "Report".to_string(),
                    output_key: None,
                    step_type: StepType::Execute,
                    status: StepStatus::Pending,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        save_workflow(&conn, &wf).unwrap();

        // Check passes with "PASS" — gate expects "FAIL", so gate is skipped
        let result = advance_workflow(&conn, &wf.id, "PASS", true, None).unwrap();
        match result {
            AdvanceResult::NextStep { agent, .. } => {
                assert_eq!(agent, "Atlas"); // Skipped the gate step, went to Final
            }
            _ => panic!("Expected NextStep to Atlas (gate skipped)"),
        }
    }

    #[test]
    fn test_output_key_state_passing() {
        let conn = test_conn();
        let mut wf = build_verify_report("State", "Build Q", "Nova", "Sentinel", "Atlas", 3);
        wf.steps[0].status = StepStatus::Running;
        save_workflow(&conn, &wf).unwrap();

        // Build step saves to output_key "build_result"
        advance_workflow(&conn, &wf.id, "Feature Q implemented", true, None).unwrap();

        // Check state was saved
        let loaded = load_workflow(&conn, &wf.id).unwrap();
        assert_eq!(
            loaded.state["build_result"].as_str().unwrap(),
            "Feature Q implemented"
        );
    }

    #[test]
    fn test_get_active_workflows() {
        let conn = test_conn();
        let wf1 = build_verify_loop("Active1", "A", "Nova", "Sentinel", 3);
        let mut wf2 = build_verify_loop("Done", "B", "Nova", "Sentinel", 3);
        wf2.status = WorkflowStatus::Completed;

        save_workflow(&conn, &wf1).unwrap();
        save_workflow(&conn, &wf2).unwrap();

        let active = get_active_workflows(&conn);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name, "Active1");
    }
}
