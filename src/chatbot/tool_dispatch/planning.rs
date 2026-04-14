//! Tool dispatch — planning tools.

use tracing::info;

use crate::chatbot::engine::ChatbotConfig;

/// Save task checkpoint (CheckpointTask tool).
pub(super) async fn execute_checkpoint_task(
    config: &ChatbotConfig,
    task_id: &str,
    checkpoint: &str,
    status_note: &str,
) -> Result<Option<String>, String> {
    let db_path = config
        .shared_bot_messages_db
        .as_ref()
        .ok_or("No shared DB configured")?;

    let db = crate::chatbot::bot_messages::BotMessageDb::open(db_path)
        .map_err(|e| format!("DB error: {e}"))?;

    // Save checkpoint
    db.checkpoint_task(task_id, checkpoint)
        .map_err(|e| format!("Checkpoint failed: {e}"))?;

    info!("Checkpoint saved for task {}: {}", task_id, status_note);
    Ok(Some(format!(
        "Checkpoint saved for task {}. Status: {}",
        task_id, status_note
    )))
}

/// Load task state for resumption (ResumeTask tool).
pub(super) async fn execute_resume_task(
    config: &ChatbotConfig,
    task_id: &str,
) -> Result<Option<String>, String> {
    let db_path = config
        .shared_bot_messages_db
        .as_ref()
        .ok_or("No shared DB configured")?;

    let db = crate::chatbot::bot_messages::BotMessageDb::open(db_path)
        .map_err(|e| format!("DB error: {e}"))?;

    let task = db
        .get_task(task_id)
        .map_err(|e| format!("Query failed: {e}"))?
        .ok_or_else(|| format!("Task {} not found", task_id))?;

    let result = serde_json::json!({
        "id": task.id,
        "title": task.title,
        "status": task.status,
        "assigned_to": task.assigned_to,
        "context": task.context,
        "checkpoint": task.checkpoint_json,
        "error_log": task.error_log,
        "created_at": task.created_at,
        "started_at": task.started_at,
    });

    Ok(Some(
        serde_json::to_string_pretty(&result).unwrap_or_default(),
    ))
}

/// Create a structured plan (CreatePlan tool).
pub(super) async fn execute_create_plan(
    config: &ChatbotConfig,
    task_id: &str,
    steps_json: &str,
) -> Result<Option<String>, String> {
    let db_path = config
        .shared_bot_messages_db
        .as_ref()
        .ok_or("No shared DB configured")?;

    let db = crate::chatbot::bot_messages::BotMessageDb::open(db_path)
        .map_err(|e| format!("DB error: {e}"))?;

    let step_inputs: Vec<crate::chatbot::planner::PlanStepInput> =
        serde_json::from_str(steps_json).map_err(|e| format!("Invalid steps JSON: {e}"))?;

    if step_inputs.is_empty() {
        return Err("Plan must have at least one step".to_string());
    }

    let plan = crate::chatbot::planner::create_plan(&db.conn, task_id, &step_inputs)
        .map_err(|e| format!("Failed to create plan: {e}"))?;

    let summary = format!(
        "Plan created: {} ({} steps)\nSteps:\n{}",
        plan.id,
        plan.steps.len(),
        plan.steps
            .iter()
            .map(|s| format!(
                "  {}. {} [verify: {}]",
                s.index, s.description, s.verification
            ))
            .collect::<Vec<_>>()
            .join("\n"),
    );

    Ok(Some(summary))
}

/// Update a plan step status (UpdatePlanStep tool).
pub(super) async fn execute_update_plan_step(
    config: &ChatbotConfig,
    plan_id: &str,
    step_index: usize,
    status: &str,
    result: Option<&str>,
) -> Result<Option<String>, String> {
    let db_path = config
        .shared_bot_messages_db
        .as_ref()
        .ok_or("No shared DB configured")?;

    let db = crate::chatbot::bot_messages::BotMessageDb::open(db_path)
        .map_err(|e| format!("DB error: {e}"))?;

    let mut plan = crate::chatbot::planner::get_plan(&db.conn, plan_id)
        .map_err(|e| format!("Failed to load plan: {e}"))?
        .ok_or_else(|| format!("Plan {} not found", plan_id))?;

    if step_index >= plan.steps.len() {
        return Err(format!(
            "Step index {} out of range (plan has {} steps)",
            step_index,
            plan.steps.len()
        ));
    }

    plan.steps[step_index].status = match status {
        "done" => crate::chatbot::planner::StepStatus::Done,
        "failed" => crate::chatbot::planner::StepStatus::Failed,
        "skipped" => crate::chatbot::planner::StepStatus::Skipped,
        _ => return Err(format!("Invalid status: {status}")),
    };
    plan.steps[step_index].result = result.map(|s| s.to_string());

    // Auto-advance: if all steps done, move to verifying
    if plan.all_steps_done() {
        plan.status = crate::chatbot::planner::PlanStatus::Done;
    } else if plan.has_failed_step() {
        plan.status = crate::chatbot::planner::PlanStatus::Verifying;
    }

    crate::chatbot::planner::update_plan(&db.conn, &plan)
        .map_err(|e| format!("Failed to update plan: {e}"))?;

    let next = plan.next_ready_step();
    let msg = format!(
        "Step {} marked as {}. Plan status: {}. {}",
        step_index,
        status,
        plan.status,
        if let Some(n) = next {
            format!("Next ready step: {}", n)
        } else {
            "No more steps pending.".to_string()
        },
    );
    Ok(Some(msg))
}

/// Revise a plan after failure (RevisePlan tool).
pub(super) async fn execute_revise_plan(
    config: &ChatbotConfig,
    plan_id: &str,
    revised_steps_json: &str,
    reason: &str,
) -> Result<Option<String>, String> {
    let db_path = config
        .shared_bot_messages_db
        .as_ref()
        .ok_or("No shared DB configured")?;

    let db = crate::chatbot::bot_messages::BotMessageDb::open(db_path)
        .map_err(|e| format!("DB error: {e}"))?;

    let mut plan = crate::chatbot::planner::get_plan(&db.conn, plan_id)
        .map_err(|e| format!("Failed to load plan: {e}"))?
        .ok_or_else(|| format!("Plan {} not found", plan_id))?;

    if plan.iteration >= plan.max_iterations {
        return Err(format!(
            "Plan {} has reached max revisions ({}). Consider a fundamentally different approach.",
            plan_id, plan.max_iterations
        ));
    }

    let step_inputs: Vec<crate::chatbot::planner::PlanStepInput> =
        serde_json::from_str(revised_steps_json)
            .map_err(|e| format!("Invalid revised steps JSON: {e}"))?;

    plan.steps = step_inputs
        .iter()
        .enumerate()
        .map(|(i, input)| crate::chatbot::planner::PlanStep {
            index: i,
            description: input.description.clone(),
            verification: input.verification.clone(),
            status: crate::chatbot::planner::StepStatus::Pending,
            result: None,
            depends_on: input.depends_on.clone(),
        })
        .collect();

    plan.iteration += 1;
    plan.current_step = 0;
    plan.status = crate::chatbot::planner::PlanStatus::Executing;

    crate::chatbot::planner::update_plan(&db.conn, &plan)
        .map_err(|e| format!("Failed to update plan: {e}"))?;

    Ok(Some(format!(
        "Plan {} revised (iteration {}). Reason: {}. {} new steps.",
        plan_id,
        plan.iteration,
        reason,
        plan.steps.len()
    )))
}

/// Build a comprehensive orchestrator status report (OrchestratorStatus tool).
pub(super) async fn execute_orchestrator_status(
    config: &ChatbotConfig,
    task_id: Option<&str>,
) -> Result<Option<String>, String> {
    let db_path = config
        .shared_bot_messages_db
        .as_ref()
        .ok_or("No shared DB configured")?;
    let db = crate::chatbot::bot_messages::BotMessageDb::open(db_path)
        .map_err(|e| format!("DB error: {e}"))?;

    let mut report = String::new();

    // 1. Active tasks
    report.push_str("## Active Tasks\n");
    let tasks: Vec<crate::chatbot::bot_messages::Task> = if let Some(tid) = task_id {
        // Focus on a single task
        match db.get_task(tid) {
            Ok(Some(t)) => vec![t],
            Ok(None) => {
                report.push_str(&format!("Task '{}' not found.\n", tid));
                Vec::new()
            }
            Err(e) => {
                report.push_str(&format!("Error querying task: {e}\n"));
                Vec::new()
            }
        }
    } else {
        // All active tasks
        db.conn
            .prepare(
                "SELECT id, title, status, assigned_to, created_by, context, result, \
                 plan_id, checkpoint_json, priority, error_log, created_at, started_at \
                 FROM tasks WHERE status IN ('pending', 'in_progress', 'blocked') \
                 ORDER BY priority DESC, created_at DESC",
            )
            .and_then(|mut stmt| {
                let rows = stmt.query_map([], |row| {
                    Ok(crate::chatbot::bot_messages::Task {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        status: row.get(2)?,
                        assigned_to: row.get(3)?,
                        created_by: row.get(4)?,
                        context: row.get(5)?,
                        result: row.get(6)?,
                        plan_id: row.get(7)?,
                        checkpoint_json: row.get(8)?,
                        priority: row.get(9)?,
                        error_log: row.get(10)?,
                        created_at: row.get(11)?,
                        started_at: row.get(12)?,
                    })
                })?;
                Ok(rows.filter_map(|r| r.ok()).collect::<Vec<_>>())
            })
            .unwrap_or_default()
    };

    if tasks.is_empty() {
        report.push_str("No active tasks.\n");
    } else {
        for t in &tasks {
            let assigned = t.assigned_to.as_deref().unwrap_or("unassigned");
            let checkpoint = t
                .checkpoint_json
                .as_deref()
                .map(|c| {
                    let truncated = if c.len() > 80 { &c[..80] } else { c };
                    format!(" | checkpoint: {}", truncated)
                })
                .unwrap_or_default();
            report.push_str(&format!(
                "- [{}] {} (P{}) — {} → {}{}\n",
                t.status, t.title, t.priority, t.created_by, assigned, checkpoint
            ));
        }
    }

    // 2. Active plans
    report.push_str("\n## Active Plans\n");
    let plans: Vec<(String, String, i32, i32, String)> = db
        .conn
        .prepare(
            "SELECT id, task_id, current_step, \
             (SELECT COUNT(*) FROM json_each(steps_json)), status \
             FROM plans WHERE status NOT IN ('done', 'failed') \
             ORDER BY created_at DESC LIMIT 10",
        )
        .and_then(|mut stmt| {
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i32>(2)?,
                    row.get::<_, i32>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?;
            Ok(rows.filter_map(|r| r.ok()).collect::<Vec<_>>())
        })
        .unwrap_or_default();

    if plans.is_empty() {
        report.push_str("No active plans.\n");
    } else {
        for (plan_id, task_id_ref, current_step, total_steps, status) in &plans {
            report.push_str(&format!(
                "- Plan {} (task: {}) — step {}/{} [{}]\n",
                plan_id, task_id_ref, current_step, total_steps, status
            ));
        }
    }

    // 3. Pending handoffs
    report.push_str("\n## Pending Handoffs\n");
    let handoffs: Vec<(String, String, String, String)> = db
        .conn
        .prepare(
            "SELECT from_agent, to_agent, task_id, payload \
             FROM handoffs WHERE status = 'pending' ORDER BY id ASC",
        )
        .and_then(|mut stmt| {
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?;
            Ok(rows.filter_map(|r| r.ok()).collect::<Vec<_>>())
        })
        .unwrap_or_default();

    if handoffs.is_empty() {
        report.push_str("No pending handoffs.\n");
    } else {
        for (from, to, tid, payload) in &handoffs {
            let summary = if payload.len() > 80 {
                &payload[..80]
            } else {
                payload
            };
            report.push_str(&format!(
                "- {} → {} (task: {}): {}\n",
                from, to, tid, summary
            ));
        }
    }

    // 4. Pending consensus requests
    report.push_str("\n## Awaiting Consensus\n");
    let consensus: Vec<(i64, String, String, String, String)> = db
        .conn
        .prepare(
            "SELECT id, requesting_agent, action_type, description, required_approvers \
             FROM consensus_requests WHERE status = 'pending' ORDER BY id ASC",
        )
        .and_then(|mut stmt| {
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?;
            Ok(rows.filter_map(|r| r.ok()).collect::<Vec<_>>())
        })
        .unwrap_or_default();

    if consensus.is_empty() {
        report.push_str("No pending consensus requests.\n");
    } else {
        for (id, agent, action, desc, approvers) in &consensus {
            report.push_str(&format!(
                "- #{} [{}] {} requests '{}': {} (needs: {})\n",
                id,
                action,
                agent,
                desc,
                if desc.len() > 60 { &desc[..60] } else { desc },
                approvers
            ));
        }
    }

    // 5. Agent health (heartbeats)
    report.push_str("\n## Agent Status\n");
    let heartbeats: Vec<(String, String, String, Option<String>)> = db
        .conn
        .prepare(
            "SELECT bot_name, last_heartbeat, status, current_task \
             FROM heartbeats ORDER BY bot_name",
        )
        .and_then(|mut stmt| {
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?;
            Ok(rows.filter_map(|r| r.ok()).collect::<Vec<_>>())
        })
        .unwrap_or_default();

    if heartbeats.is_empty() {
        report.push_str("No heartbeat data.\n");
    } else {
        for (name, last_hb, status, current_task) in &heartbeats {
            let task_str = current_task.as_deref().unwrap_or("idle");
            report.push_str(&format!(
                "- {} [{}] heartbeat: {} | working on: {}\n",
                name, status, last_hb, task_str
            ));
        }
    }

    // 6. Recent activity (progress ledger, last 2 hours)
    report.push_str("\n## Recent Activity (last 2h)\n");
    let activity: Vec<(String, String, String, Option<String>)> = db
        .conn
        .prepare(
            "SELECT agent, action, created_at, detail \
             FROM progress_ledger \
             WHERE created_at > datetime('now', '-2 hours') \
             ORDER BY id DESC LIMIT 20",
        )
        .and_then(|mut stmt| {
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?;
            Ok(rows.filter_map(|r| r.ok()).collect::<Vec<_>>())
        })
        .unwrap_or_default();

    if activity.is_empty() {
        report.push_str("No recent activity.\n");
    } else {
        for (agent, action, ts, detail) in &activity {
            let ts_short = ts.get(11..16).unwrap_or(ts);
            let detail_str = detail.as_deref().unwrap_or("");
            report.push_str(&format!("  [{}] {} — {}", ts_short, agent, action));
            if !detail_str.is_empty() {
                report.push_str(&format!(": {}", detail_str));
            }
            report.push('\n');
        }
    }

    Ok(Some(report))
}

/// Get the progress audit trail for a task (GetProgress tool).
pub(super) async fn execute_get_progress(
    config: &ChatbotConfig,
    task_id: &str,
) -> Result<Option<String>, String> {
    let db_path = config
        .shared_bot_messages_db
        .as_ref()
        .ok_or("No shared DB configured")?;
    let db = crate::chatbot::bot_messages::BotMessageDb::open(db_path)
        .map_err(|e| format!("DB error: {e}"))?;
    let entries = db
        .get_task_progress(task_id)
        .map_err(|e| format!("Query error: {e}"))?;
    if entries.is_empty() {
        return Ok(Some(format!("No progress entries for task '{}'.", task_id)));
    }
    let mut lines = vec![format!("Progress for task '{}':", task_id)];
    for e in &entries {
        let ts = e.created_at.get(..16).unwrap_or(&e.created_at);
        lines.push(format!(
            "  [{}] {} — {} {}",
            ts,
            e.agent,
            e.action,
            e.detail.as_deref().unwrap_or("")
        ));
    }
    Ok(Some(lines.join("\n")))
}

// ─── Workflow tools ─────────────────────────────────────────────────────

/// Start a code-enforced workflow (StartWorkflow tool).
pub(super) async fn execute_start_workflow(
    config: &ChatbotConfig,
    name: &str,
    steps_json: &str,
    max_iterations: Option<u32>,
) -> Result<Option<String>, String> {
    let db_path = config
        .shared_bot_messages_db
        .as_ref()
        .ok_or("No shared DB configured")?;

    // Parse step definitions from JSON
    let step_defs: Vec<crate::chatbot::workflow::WorkflowStep> =
        serde_json::from_str(steps_json).map_err(|e| format!("Invalid steps JSON: {e}"))?;

    if step_defs.is_empty() {
        return Err("Workflow must have at least one step".to_string());
    }

    // Build workflow
    let mut wf = crate::chatbot::workflow::Workflow {
        id: format!("wf-{}", uuid::Uuid::new_v4()),
        name: name.to_string(),
        steps: step_defs,
        max_iterations: max_iterations.unwrap_or(5),
        ..Default::default()
    };

    // Set first step to Running
    wf.steps[0].status = crate::chatbot::workflow::StepStatus::Running;

    let db = crate::chatbot::bot_messages::BotMessageDb::open(db_path)
        .map_err(|e| format!("DB error: {e}"))?;

    crate::chatbot::workflow::save_workflow(&db.conn, &wf)
        .map_err(|e| format!("Save failed: {e}"))?;

    // Inject the first step's instruction to the assigned agent via bot_messages
    let first_step = &wf.steps[0];
    let instruction =
        crate::chatbot::workflow::substitute_state_vars(&first_step.instruction, &wf.state);
    let message = format!(
        "[WORKFLOW:{}] Step 1/{}: {}\n\n{}",
        wf.id,
        wf.steps.len(),
        first_step.name,
        instruction,
    );

    let _ = db.insert_typed(
        &config.bot_name,
        Some(&first_step.agent),
        &message,
        crate::chatbot::bot_messages::message_type::CHAT,
        None,
        None,
    );

    info!(
        "[workflow] Started '{}' ({}): {} steps, first → {}",
        wf.name,
        wf.id,
        wf.steps.len(),
        first_step.agent
    );

    Ok(Some(format!(
        "Workflow '{}' started (id: {}). {} steps, first step '{}' sent to {}.\n\
         The Rust engine will control the flow — agents just call complete_workflow_step when done.",
        wf.name,
        wf.id,
        wf.steps.len(),
        first_step.name,
        first_step.agent,
    )))
}

/// Report completion of a workflow step (CompleteWorkflowStep tool).
/// The Rust engine decides what happens next.
pub(super) async fn execute_complete_workflow_step(
    config: &ChatbotConfig,
    workflow_id: &str,
    result: &str,
    passed: bool,
    output_data: Option<&str>,
) -> Result<Option<String>, String> {
    let db_path = config
        .shared_bot_messages_db
        .as_ref()
        .ok_or("No shared DB configured")?;

    let db = crate::chatbot::bot_messages::BotMessageDb::open(db_path)
        .map_err(|e| format!("DB error: {e}"))?;

    // Advance the workflow — the Rust code decides what happens next
    let advance_result = crate::chatbot::workflow::advance_workflow(
        &db.conn,
        workflow_id,
        result,
        passed,
        output_data,
    )?;

    match advance_result {
        crate::chatbot::workflow::AdvanceResult::NextStep { agent, message } => {
            // Route the next instruction to the target agent via bot_messages
            let _ = db.insert_typed(
                &config.bot_name,
                Some(&agent),
                &message,
                crate::chatbot::bot_messages::message_type::CHAT,
                None,
                None,
            );

            // Wake the target agent
            crate::chatbot::event_bus::global_event_bus().wake(&agent);

            Ok(Some(format!(
                "Step complete. Next step routed to {} by the workflow engine.\n\
                 You can stop now — the engine handles the flow.",
                agent,
            )))
        }
        crate::chatbot::workflow::AdvanceResult::Completed(wf) => {
            // Notify all agents that the workflow finished
            let _ = db.insert_typed(
                &config.bot_name,
                None, // broadcast
                &format!(
                    "[WORKFLOW:{}] COMPLETED: '{}' finished successfully.\nFinal state: {}",
                    wf.id,
                    wf.name,
                    serde_json::to_string_pretty(&wf.state).unwrap_or_default(),
                ),
                crate::chatbot::bot_messages::message_type::STATUS,
                None,
                None,
            );

            Ok(Some(format!(
                "Workflow '{}' COMPLETED successfully after {} steps.",
                wf.name,
                wf.steps.len(),
            )))
        }
        crate::chatbot::workflow::AdvanceResult::MaxIterations(wf) => {
            // Notify about max iterations
            let _ = db.insert_typed(
                &config.bot_name,
                None,
                &format!(
                    "[WORKFLOW:{}] MAX ITERATIONS: '{}' hit {} iterations without passing verification.\n\
                     Last result: {}",
                    wf.id,
                    wf.name,
                    wf.max_iterations,
                    result,
                ),
                crate::chatbot::bot_messages::message_type::STATUS,
                None,
                None,
            );

            Ok(Some(format!(
                "Workflow '{}' hit MAX ITERATIONS ({}). Verification never passed. Escalate to owner.",
                wf.name, wf.max_iterations,
            )))
        }
    }
}
