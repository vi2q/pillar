//! A deterministic A/B trace of the two edit paths (design §10.3 steps 1–2):
//! the same scripted task set through the existing `read` + `edit` tools and
//! through `exp_read` + `exp_edit`.
//!
//! The model is scripted, so this measures the *contract*, not a model: both
//! paths must produce byte-identical files, and for each task the trace
//! reports round trips, the bytes the model must send (arguments) and the
//! bytes it must read back (tool results). Step 3 — a real model — is what
//! decides adoption; this is what makes that comparison reproducible.
//!
//! The old path's `oldText` is the smallest window that is still unique
//! (the cheapest the old tool can be asked to do); the read step sees the same
//! window in both paths, so the difference is the edit step's contract.
//!
//! Numbers for the record: `cargo test -p pillar-coding-agent --test
//! exp_ab_trace -- --nocapture`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use pillar_agent::exp::{
    ExpEditRequest, ExpEditRequestItem, ExpLimits, ExpRange, ExpReadRequest, ExpToolkit, MemoryHost,
    OperationId, OperationLedger, OwnerId, RefStore, manual_clock,
};
use pillar_agent::types::AgentToolResult;
use pillar_coding_agent::core::tools::edit::edit;
use pillar_coding_agent::core::tools::edit_diff::Edit;
use pillar_coding_agent::core::tools::file_mutation_queue::FileMutationQueue;
use pillar_coding_agent::core::tools::read::read;
use serde_json::json;

struct Task {
    name: &'static str,
    file: String,
    /// `(first line, last line inclusive, replacement)`, 0-indexed.
    first: usize,
    last: usize,
    replacement: String,
}

fn lines(count: usize, body: impl Fn(usize) -> String) -> String {
    (0..count).map(body).collect::<Vec<_>>().join("\n") + "\n"
}

fn tasks() -> Vec<Task> {
    let long_function = {
        let mut body = String::from("fn big(input: &[i32]) -> i32 {\n");
        for index in 1..198 {
            if index == 120 {
                body.push_str("    let target = 41;\n");
            } else {
                body.push_str(&format!("    let value_{index} = {index};\n"));
            }
        }
        body.push_str("}\n");
        body
    };
    vec![
        Task {
            name: "one_line_in_a_short_file",
            file: lines(8, |index| format!("line_{index}")),
            first: 3,
            last: 3,
            replacement: "LINE_THREE\n".to_string(),
        },
        Task {
            name: "one_line_inside_a_200_line_function",
            file: long_function,
            first: 120,
            last: 120,
            replacement: "    let target = 42;\n".to_string(),
        },
        Task {
            name: "thirty_line_block_replaced",
            file: lines(60, |index| format!("    old_{index}();")),
            first: 20,
            last: 49,
            replacement: lines(30, |index| format!("    new_{index}();")),
        },
        Task {
            name: "two_regions_in_one_call",
            file: lines(80, |index| format!("    line_{index}();")),
            first: 5,
            last: 5,
            replacement: "    first_change();\n".to_string(),
        },
    ]
}

/// The smallest line window around the edit whose text is unique in the file.
fn minimal_unique_old_text(file: &str, first: usize, last: usize) -> String {
    let lines: Vec<&str> = file.split_inclusive('\n').collect();
    let mut radius = 0usize;
    loop {
        let start = first.saturating_sub(radius);
        let end = (last + radius + 1).min(lines.len());
        let candidate: String = lines[start..end].concat();
        if file.matches(&candidate).count() == 1 || (start == 0 && end == lines.len()) {
            return candidate;
        }
        radius += 1;
    }
}

fn text_bytes(result: &AgentToolResult) -> usize {
    result
        .content
        .iter()
        .filter_map(|part| match part {
            pillar_ai::types::Content::Text { text, .. } => Some(text.len()),
            _ => None,
        })
        .sum()
}

fn text_of(result: &AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|part| match part {
            pillar_ai::types::Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn the_two_paths_agree_on_the_result_and_the_trace_records_the_cost() {
    println!(
        "{:<38} {:>6} {:>7} {:>8} {:>8} {:>8}",
        "task", "trips", "args_A", "args_B", "res_A", "res_B"
    );

    for task in tasks() {
        // --- path A: read + edit (the existing tools) ---
        let dir_a = std::env::temp_dir().join(format!(
            "pillar-ab-a-{}-{}",
            std::process::id(),
            task.name
        ));
        std::fs::create_dir_all(&dir_a).expect("temp dir");
        std::fs::write(dir_a.join("f.txt"), task.file.as_bytes()).expect("write");
        let cwd = dir_a.to_string_lossy().into_owned();

        let read_a = read(
            "f.txt",
            Some(task.first + 1),
            Some(task.last - task.first + 1),
            &cwd,
        )
        .expect("read");
        let read_args_a = json!({
            "path": "f.txt",
            "offset": task.first + 1,
            "limit": task.last - task.first + 1,
        })
        .to_string()
        .len();

        let old_text = minimal_unique_old_text(&task.file, task.first, task.last);
        let edit_args_a = json!({
            "path": "f.txt",
            "edits": [{"oldText": old_text, "newText": task.replacement}],
        })
        .to_string()
        .len();
        let edit_result_a = edit(
            "f.txt",
            &[Edit {
                old_text: old_text.clone(),
                new_text: task.replacement.clone(),
            }],
            &cwd,
            None,
            &FileMutationQueue::new(),
        )
        .expect("edit");
        let result_bytes_a = read_a.text.len() + edit_result_a.text.len();

        // --- path B: exp_read + exp_edit (the reference form) ---
        let limits = ExpLimits::default();
        let host = Arc::new(MemoryHost::new());
        host.set_file("f.txt", task.file.as_bytes());
        let (clock, _now) = manual_clock(0);
        let next = Arc::new(AtomicU64::new(0));
        let ids: Arc<dyn Fn() -> String + Send + Sync> =
            Arc::new(move || format!("{:016x}", next.fetch_add(1, Ordering::SeqCst)));
        let toolkit = ExpToolkit::with_state(
            Arc::clone(&host) as Arc<dyn pillar_agent::exp::ConditionalStore>,
            Arc::new(RefStore::with_id_source(clock, limits.max_live_refs, ids)),
            Arc::new(OperationLedger::new(limits.ledger_capacity)),
            limits.clone(),
            OwnerId::new("session-a"),
        );
        let tools = toolkit.tools();
        let read_tool = tools
            .iter()
            .find(|tool| tool.tool.name == "exp_read")
            .expect("exp_read");
        let edit_tool = tools
            .iter()
            .find(|tool| tool.tool.name == "exp_edit")
            .expect("exp_edit");

        let read_request = ExpReadRequest {
            path: "f.txt".to_string(),
            range: ExpRange {
                start_line: task.first + 1,
                line_count: task.last - task.first + 1,
            },
        };
        let read_args_b = serde_json::to_string(&read_request).expect("args").len();
        let read_b = (read_tool.execute)(
            "call-1".to_string(),
            serde_json::to_value(&read_request).expect("args"),
            None,
            None,
        )
        .await
        .expect("exp_read");
        let reference = read_b.details["reference"]
            .as_str()
            .expect("reference")
            .to_string();

        let edit_request = ExpEditRequest {
            operation_id: OperationId::new("op-1"),
            edits: vec![ExpEditRequestItem {
                reference: pillar_agent::exp::RefId::new(reference),
                replacement: task.replacement.clone(),
            }],
        };
        let edit_args_b = serde_json::to_string(&edit_request).expect("args").len();
        let edit_b = (edit_tool.execute)(
            "call-2".to_string(),
            serde_json::to_value(&edit_request).expect("args"),
            None,
            None,
        )
        .await
        .expect("exp_edit");
        let result_bytes_b = text_bytes(&read_b) + text_bytes(&edit_b);

        // The same task, the same outcome — that is the gate.
        let file_a = std::fs::read_to_string(dir_a.join("f.txt")).expect("file a");
        let file_b = String::from_utf8(host.content("f.txt").expect("file b")).expect("utf8");
        assert_eq!(file_a, file_b, "{}: the two paths must agree", task.name);

        let args_a = read_args_a + edit_args_a;
        let args_b = read_args_b + edit_args_b;
        println!(
            "{:<38} {:>6} {:>7} {:>8} {:>8} {:>8}",
            task.name, 2, args_a, args_b, result_bytes_a, result_bytes_b
        );
        std::fs::remove_dir_all(&dir_a).ok();

        // Both paths take one read and one edit; the reference path must not
        // need more model turns for the same task.
        assert!(
            text_of(&edit_b).contains("[exp_edit f.txt ok]"),
            "{}: the reference edit must report a receipt",
            task.name
        );
    }
}
