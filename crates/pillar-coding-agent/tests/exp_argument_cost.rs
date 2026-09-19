//! What an edit costs the model to *say*: the existing `edit` form must repeat
//! the text it replaces, while `exp_edit` sends a reference and the new text
//! (design §11 stage A: "旧方式との引数token比較").
//!
//! The comparison is deliberately conservative in the new form's favour: the
//! old form's `oldText` is the *minimal* window that is still unique in the
//! file, i.e. the cheapest `oldText` a model could possibly send. Every
//! `oldText` built here is validated against the real `edit` tool's schema, and
//! every `exp_edit` argument set against the experimental schema, so both
//! compared shapes are the tools' own.
//!
//! Numbers are printed for the baseline record (`cargo test -p
//! pillar-coding-agent --test exp_argument_cost -- --nocapture`). The
//! assertions are structural: the reference form never restates the replaced
//! text, so it must not need more argument bytes.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use pillar_agent::exp::{
    ExpEditRequest, ExpEditRequestItem, ExpLimits, ExpRange, ExpReadRequest, MemoryHost,
    OperationLedger, OwnerId, RefId, RefStore, exp_edit, exp_edit_parameters_json, exp_read,
    manual_clock,
};
use pillar_agent::tool_schema::validate_tool_arguments;
use pillar_coding_agent::core::tools::edit::edit_parameters_json;
use pillar_coding_agent::core::tools::read::read_parameters_json;
use serde_json::{Value, json};

/// A rough token estimate, labelled as an estimate on purpose (design §8.2:
/// measure with a tokenizer when one is available, otherwise say it is an
/// estimate). The assertions below use bytes, which are exact.
fn estimate_tokens(bytes: usize) -> usize {
    bytes.div_ceil(4)
}

struct Scenario {
    name: &'static str,
    file: String,
    /// `(first line, last line inclusive, replacement)`, 0-indexed.
    edits: Vec<(usize, usize, String)>,
}

fn lines(count: usize, body: impl Fn(usize) -> String) -> String {
    (0..count).map(body).collect::<Vec<_>>().join("\n") + "\n"
}

fn scenarios() -> Vec<Scenario> {
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
    let repeated_line = lines(40, |index| {
        if index % 6 == 3 {
            "    value += 1;".to_string()
        } else {
            format!("    step_{index}();")
        }
    });
    let two_regions = lines(60, |index| format!("    line_{index}();"));
    let block = lines(60, |index| format!("    old_{index}();"));
    let unicode = lines(20, |index| {
        format!("    // 日本語のコメント {index} — 説明")
    });

    vec![
        Scenario {
            name: "one_line_in_a_short_file",
            file: lines(8, |index| format!("line_{index}")),
            edits: vec![(3, 3, "LINE_THREE\n".to_string())],
        },
        Scenario {
            name: "one_line_inside_a_200_line_function",
            file: long_function,
            edits: vec![(120, 120, "    let target = 42;\n".to_string())],
        },
        Scenario {
            name: "one_of_six_repeated_lines",
            file: repeated_line,
            edits: vec![(9, 9, "    value += 2;".to_string())],
        },
        Scenario {
            name: "two_regions_in_one_call",
            file: two_regions,
            edits: vec![
                (5, 5, "    first_change();".to_string()),
                (45, 45, "    second_change();".to_string()),
            ],
        },
        Scenario {
            name: "thirty_line_block_replaced",
            file: block,
            edits: vec![(20, 49, lines(30, |index| format!("    new_{index}();")))],
        },
        Scenario {
            name: "multi_byte_comment",
            file: unicode,
            edits: vec![(
                7,
                7,
                "    // 日本語のコメントを書き換える — 説明\n".to_string(),
            )],
        },
    ]
}

/// The smallest line window around `first..=last` whose text occurs exactly
/// once in the file — the cheapest `oldText` the old form can use.
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

/// The arguments the existing `edit` tool would receive, validated against its
/// own schema.
fn old_arguments(file: &str, edits: &[(usize, usize, String)]) -> Value {
    let edits: Vec<Value> = edits
        .iter()
        .map(|(first, last, replacement)| {
            json!({
                "oldText": minimal_unique_old_text(file, *first, *last),
                "newText": replacement,
            })
        })
        .collect();
    let arguments = json!({"path": "f.txt", "edits": edits});
    validate_tool_arguments(&edit_parameters_json(), &arguments)
        .expect("the compared old-form arguments must match the edit tool's schema");
    arguments
}

fn bytes(value: &Value) -> usize {
    value.to_string().len()
}

/// The same window through the existing `read` tool: the old flow also has to
/// see the text it is going to restate, so its read call carries a range too.
/// Comparing against a whole-file read would flatter the reference form.
fn old_read_arguments(first: usize, last: usize) -> Value {
    let arguments = json!({
        "path": "f.txt",
        "offset": first + 1,
        "limit": last - first + 1,
    });
    validate_tool_arguments(&read_parameters_json(), &arguments)
        .expect("the compared old-form read arguments must match the read tool's schema");
    arguments
}

fn percent(old: usize, new: usize) -> i64 {
    if old == 0 {
        return 0;
    }
    (old as i64 - new as i64) * 100 / old as i64
}

/// One session's state, so the compared calls really run.
struct Session {
    host: Arc<MemoryHost>,
    refs: Arc<RefStore>,
    ledger: Arc<OperationLedger>,
    limits: ExpLimits,
    owner: OwnerId,
}

impl Session {
    fn new(file: &str) -> Self {
        let limits = ExpLimits::default();
        let host = Arc::new(MemoryHost::new());
        host.set_file("f.txt", file.as_bytes());
        let (clock, _now) = manual_clock(0);
        let next = Arc::new(AtomicU64::new(0));
        let ids: Arc<dyn Fn() -> String + Send + Sync> =
            Arc::new(move || format!("ref-{}", next.fetch_add(1, Ordering::SeqCst)));
        Self {
            host,
            refs: Arc::new(RefStore::with_id_source(clock, limits.max_live_refs, ids)),
            ledger: Arc::new(OperationLedger::new(limits.ledger_capacity)),
            limits,
            owner: OwnerId::new("session-a"),
        }
    }

    fn read_request(first: usize, last: usize) -> ExpReadRequest {
        ExpReadRequest {
            path: "f.txt".to_string(),
            range: ExpRange {
                start_line: first + 1,
                line_count: last - first + 1,
            },
        }
    }

    async fn read(&self, first: usize, last: usize) -> RefId {
        let response = exp_read(
            self.host.as_ref(),
            &self.refs,
            &self.limits,
            &self.owner,
            &Self::read_request(first, last),
        )
        .await
        .expect("read");
        response
            .reference
            .expect("a complete UTF-8 range yields a reference")
    }

    async fn edit(&self, operation: &str, reference: RefId, replacement: &str) {
        exp_edit(
            self.host.as_ref(),
            &self.refs,
            &self.ledger,
            &self.limits,
            &self.owner,
            &ExpEditRequest {
                operation_id: pillar_agent::exp::OperationId::new(operation),
                edits: vec![ExpEditRequestItem {
                    reference,
                    replacement: replacement.to_string(),
                }],
            },
        )
        .await
        .expect("the compared new-form call must succeed");
    }
}

#[tokio::test]
async fn the_reference_form_sends_fewer_argument_bytes_than_repeating_the_text() {
    println!(
        "{:<36} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
        "scenario", "replaced", "old_tok", "new_tok", "cyc_old", "cyc_new", "saved"
    );
    let (mut old_total, mut new_total, mut old_cycle_total, mut new_cycle_total) =
        (0usize, 0usize, 0usize, 0usize);

    for scenario in scenarios() {
        let old = old_arguments(&scenario.file, &scenario.edits);
        let replaced_text_bytes: usize = scenario
            .edits
            .iter()
            .map(|(first, last, _)| minimal_unique_old_text(&scenario.file, *first, *last).len())
            .sum();
        let session = Session::new(&scenario.file);

        let mut old_read_bytes = 0usize;
        let mut new_read_bytes = 0usize;
        let mut new_edits: Vec<Value> = Vec::new();
        for (index, (first, last, replacement)) in scenario.edits.iter().enumerate() {
            old_read_bytes += bytes(&old_read_arguments(*first, *last));
            new_read_bytes += bytes(
                &serde_json::to_value(Session::read_request(*first, *last)).expect("request"),
            );
            let reference = session.read(*first, *last).await;
            new_edits.push(json!({"ref": reference.as_str(), "replacement": replacement}));
            session
                .edit(&format!("op-{index}"), reference, replacement)
                .await;
        }
        let new = json!({"operationId": "op-0", "edits": new_edits});
        validate_tool_arguments(&exp_edit_parameters_json(), &new)
            .expect("the compared new-form arguments must match the experimental schema");

        let old_bytes = bytes(&old);
        let new_bytes = bytes(&new);
        let old_cycle = old_bytes + old_read_bytes;
        let new_cycle = new_bytes + new_read_bytes;
        old_total += old_bytes;
        new_total += new_bytes;
        old_cycle_total += old_cycle;
        new_cycle_total += new_cycle;
        let saved = percent(old_cycle, new_cycle);
        println!(
            "{:<36} {:>8} {:>8} {:>8} {:>8} {:>8} {:>7}",
            scenario.name,
            replaced_text_bytes,
            estimate_tokens(old_bytes),
            estimate_tokens(new_bytes),
            estimate_tokens(old_cycle),
            estimate_tokens(new_cycle),
            format!("{}%", saved)
        );

        // The reference form pays a fixed overhead per edit (an opaque id, the
        // operation id, the JSON envelope). Below roughly that many bytes of
        // replaced text it cannot win, and this comparison does not pretend
        // otherwise; above it, it must.
        const REFERENCE_OVERHEAD_BYTES: usize = 64;
        if replaced_text_bytes > REFERENCE_OVERHEAD_BYTES {
            assert!(
                new_bytes < old_bytes,
                "{}: replacing {replaced_text_bytes} bytes must not need more arguments \
                 ({new_bytes} vs {old_bytes})",
                scenario.name
            );
        }
    }

    println!(
        "{:<36} {:>8} {:>8} {:>8} {:>8} {:>8} {:>7}",
        "total",
        "",
        estimate_tokens(old_total),
        estimate_tokens(new_total),
        estimate_tokens(old_cycle_total),
        estimate_tokens(new_cycle_total),
        format!("{}%", percent(old_cycle_total, new_cycle_total))
    );
    assert!(
        new_total < old_total,
        "the aggregate edit arguments must shrink ({new_total} vs {old_total})"
    );
    assert!(
        new_cycle_total < old_cycle_total,
        "the aggregate read-then-edit arguments must shrink ({new_cycle_total} vs {old_cycle_total})"
    );
}
