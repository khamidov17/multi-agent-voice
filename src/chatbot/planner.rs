//! Planning layer — structured plan creation, execution, and revision.
//!
//! For non-trivial tasks, agents generate a plan with steps and verification
//! criteria, execute step-by-step, and revise if verification fails.
//!
//! Plans are stored in the shared bot_messages.db so all agents can see them.

use serde::{Deserialize, Serialize};
use tracing::info;

/// Status of a plan.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum PlanStatus {
    Planning,
    Reviewing,
    Approved,
    Executing,
    Verifying,
    Done,
    Failed,
}

impl std::fmt::Display for PlanStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanStatus::Planning => write!(f, "planning"),
            PlanStatus::Reviewing => write!(f, "reviewing"),
            PlanStatus::Approved => write!(f, "approved"),
            PlanStatus::Executing => write!(f, "executing"),
            PlanStatus::Verifying => write!(f, "verifying"),
            PlanStatus::Done => write!(f, "done"),
            PlanStatus::Failed => write!(f, "failed"),
        }
    }
}

/// Status of a single plan step.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum StepStatus {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

/// A single step in a plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    pub index: usize,
    pub description: String,
    pub verification: String,
    pub status: StepStatus,
    pub result: Option<String>,
    pub depends_on: Vec<usize>,
}

/// Input for creating/revising plan steps (from MCP tool calls).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStepInput {
    pub description: String,
    pub verification: String,
    #[serde(default)]
    pub depends_on: Vec<usize>,
}

/// A full plan with steps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub id: String,
    pub task_id: String,
    pub steps: Vec<PlanStep>,
    pub current_step: usize,
    pub status: PlanStatus,
    pub iteration: u32,
    pub max_iterations: u32,
    pub created_at: String,
    pub updated_at: String,
}

impl Plan {
    /// Check if all steps are done.
    pub fn all_steps_done(&self) -> bool {
        self.steps
            .iter()
            .all(|s| matches!(s.status, StepStatus::Done | StepStatus::Skipped))
    }

    /// Check if any step failed.
    pub fn has_failed_step(&self) -> bool {
        self.steps
            .iter()
            .any(|s| matches!(s.status, StepStatus::Failed))
    }

    /// Get the next pending step (respecting dependencies).
    pub fn next_ready_step(&self) -> Option<usize> {
        for step in &self.steps {
            if !matches!(step.status, StepStatus::Pending) {
                continue;
            }
            // Check all dependencies are done
            let deps_met = step.depends_on.iter().all(|&dep| {
                self.steps
                    .get(dep)
                    .map(|s| matches!(s.status, StepStatus::Done))
                    .unwrap_or(true)
            });
            if deps_met {
                return Some(step.index);
            }
        }
        None
    }
}

// ─── Database operations ─────────────────────────────────────────────────

/// Create a new plan in the shared database.
pub fn create_plan(
    conn: &rusqlite::Connection,
    task_id: &str,
    step_inputs: &[PlanStepInput],
) -> anyhow::Result<Plan> {
    let plan_id = format!("plan-{}", uuid_v4());

    let steps: Vec<PlanStep> = step_inputs
        .iter()
        .enumerate()
        .map(|(i, input)| PlanStep {
            index: i,
            description: input.description.clone(),
            verification: input.verification.clone(),
            status: StepStatus::Pending,
            result: None,
            depends_on: input.depends_on.clone(),
        })
        .collect();

    let steps_json = serde_json::to_string(&steps)?;

    conn.execute(
        "INSERT INTO plans (id, task_id, steps_json, status)
         VALUES (?1, ?2, ?3, 'executing')",
        rusqlite::params![plan_id, task_id, steps_json],
    )?;

    info!(
        "Created plan {} with {} steps for task {}",
        plan_id,
        steps.len(),
        task_id
    );

    Ok(Plan {
        id: plan_id,
        task_id: task_id.to_string(),
        steps,
        current_step: 0,
        status: PlanStatus::Executing,
        iteration: 0,
        max_iterations: 3,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
    })
}

/// Load a plan from the database.
pub fn get_plan(conn: &rusqlite::Connection, plan_id: &str) -> anyhow::Result<Option<Plan>> {
    let result = conn.query_row(
        "SELECT id, task_id, steps_json, current_step, status, iteration, max_iterations,
                created_at, updated_at
         FROM plans WHERE id = ?1",
        rusqlite::params![plan_id],
        |row| {
            let steps_json: String = row.get(2)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                steps_json,
                row.get::<_, usize>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, u32>(5)?,
                row.get::<_, u32>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
            ))
        },
    );

    match result {
        Ok((
            id,
            task_id,
            steps_json,
            current_step,
            status_str,
            iteration,
            max_iterations,
            created_at,
            updated_at,
        )) => {
            let steps: Vec<PlanStep> = serde_json::from_str(&steps_json).unwrap_or_default();
            let status = match status_str.as_str() {
                "planning" => PlanStatus::Planning,
                "reviewing" => PlanStatus::Reviewing,
                "approved" => PlanStatus::Approved,
                "executing" => PlanStatus::Executing,
                "verifying" => PlanStatus::Verifying,
                "done" => PlanStatus::Done,
                "failed" => PlanStatus::Failed,
                _ => PlanStatus::Planning,
            };
            Ok(Some(Plan {
                id,
                task_id,
                steps,
                current_step,
                status,
                iteration,
                max_iterations,
                created_at,
                updated_at,
            }))
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Save plan state back to database.
pub fn update_plan(conn: &rusqlite::Connection, plan: &Plan) -> anyhow::Result<()> {
    let steps_json = serde_json::to_string(&plan.steps)?;
    conn.execute(
        "UPDATE plans SET steps_json = ?1, current_step = ?2, status = ?3,
                          iteration = ?4, updated_at = datetime('now')
         WHERE id = ?5",
        rusqlite::params![
            steps_json,
            plan.current_step,
            plan.status.to_string(),
            plan.iteration,
            plan.id,
        ],
    )?;
    Ok(())
}

/// Get the active plan for a task.
pub fn get_active_plan_for_task(
    conn: &rusqlite::Connection,
    task_id: &str,
) -> anyhow::Result<Option<Plan>> {
    let plan_id: Option<String> = conn
        .query_row(
            "SELECT id FROM plans WHERE task_id = ?1 AND status NOT IN ('done', 'failed')
             ORDER BY created_at DESC LIMIT 1",
            rusqlite::params![task_id],
            |row| row.get(0),
        )
        .ok();

    match plan_id {
        Some(id) => get_plan(conn, &id),
        None => Ok(None),
    }
}

/// Simple UUID v4 generator (no external dependency).
fn uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let random = now ^ (std::process::id() as u128) ^ 0xdeadbeef;
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        (random >> 96) as u32,
        (random >> 80) as u16,
        (random >> 64) as u16 & 0x0fff,
        ((random >> 48) as u16 & 0x3fff) | 0x8000,
        random as u64 & 0xffffffffffff,
    )
}
