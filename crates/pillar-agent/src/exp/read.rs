//! `exp_read`: show one byte range of a resource and issue a reference for it
//! (design §4.1, §4.2).
//!
//! Line numbers *address* the range the caller wants to see; the reference is
//! issued for the equivalent byte range, and that byte range is what an edit
//! later carries. A reference is issued only for content that was fully
//! delivered: an omitted function body or a truncated huge line must not
//! silently widen the editable area (design §4.2).

use serde::{Deserialize, Serialize};

use super::ExpLimits;
use super::error::ExpError;
use super::refs::{ByteRange, OwnerId, RefId, RefRecord, RefStore};
use super::store::{ConditionalStore, Guarantee, Revision, digest64};

/// Which lines to show, 1-indexed (display addressing only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpRange {
    pub start_line: usize,
    pub line_count: usize,
}

/// An `exp_read` call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpReadRequest {
    pub path: String,
    pub range: ExpRange,
}

/// Whether everything the caller asked for is in `text` (design §3: complete
/// delivery is not the same as a complete scan).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    Complete,
    Partial,
}

/// Why a reference was not issued.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WithheldReason {
    /// The target is not valid UTF-8, so a byte range cannot be trusted to
    /// fall on character boundaries and no editable reference is issued
    /// (design §4.2).
    NotUtf8,
    /// The range did not fit the per-call budget; read a smaller range.
    BudgetExceeded,
}

/// The result of an `exp_read`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpReadResponse {
    pub path: String,
    /// 1-indexed first delivered line.
    pub start_line: usize,
    /// Delivered line count (may be short of the request when partial).
    pub line_count: usize,
    /// Lines in the whole file.
    pub total_lines: usize,
    pub text: String,
    pub delivery_state: DeliveryState,
    /// The reference to hand to an edit, when one was issued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<RefId>,
    /// The revision the reference was issued against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<Revision>,
    /// Whether `reference` may be used for a strict edit. False for a partial
    /// delivery, a non-UTF-8 target, or a host that cannot promise a strict
    /// publication (design §4.3).
    pub editable: bool,
    pub guarantee: Guarantee,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub withheld: Option<WithheldReason>,
    /// One-line repair when something was withheld.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair: Option<String>,
}

/// Read a range and issue a reference for it.
pub async fn exp_read(
    host: &dyn ConditionalStore,
    refs: &RefStore,
    limits: &ExpLimits,
    owner: &OwnerId,
    request: &ExpReadRequest,
) -> Result<ExpReadResponse, ExpError> {
    if request.range.line_count == 0 {
        return Err(ExpError::invalid_request(
            "range.lineCount must be at least 1",
        ));
    }

    let resource = host.resolve(&request.path).await?;
    let snapshot = host.snapshot(&resource).await?;
    let guarantee = host.guarantee();

    let spans = line_spans(&snapshot.bytes);
    let total_lines = spans.len();
    if total_lines == 0 {
        return Err(ExpError::invalid_request(format!(
            "{} is empty; there is nothing to read",
            request.path
        ))
        .with_repair("do not read an empty file"));
    }
    let first = request.range.start_line;
    if first == 0 || first > total_lines {
        return Err(ExpError::invalid_request(format!(
            "range.startLine {} is beyond the file ({} lines)",
            first, total_lines
        ))
        .with_repair(format!(
            "use a startLine between 1 and {total_lines}",
            total_lines = total_lines
        )));
    }

    let last = first
        .saturating_add(request.range.line_count)
        .saturating_sub(1)
        .min(total_lines);
    let requested_lines = last - first + 1;
    let utf8 = std::str::from_utf8(&snapshot.bytes).ok();

    // Deliver as many whole lines as both budgets allow; never truncate
    // silently mid-range (design §8.2).
    let max_lines = requested_lines.min(limits.max_read_lines);
    let mut text = String::new();
    let mut delivered = 0usize;
    for index in first - 1..last {
        if delivered >= max_lines {
            break;
        }
        let (start, end) = spans[index];
        let line = match utf8 {
            Some(text_all) => &text_all[start..end],
            None => "",
        };
        let extra = if utf8.is_some() {
            line.len()
        } else {
            end - start
        };
        if !text.is_empty() && text.len() + extra > limits.max_read_bytes {
            break;
        }
        if utf8.is_some() {
            text.push_str(line);
        }
        delivered += 1;
    }

    let complete = delivered == requested_lines;
    let byte_range = if delivered == 0 {
        ByteRange::new(spans[first - 1].0, spans[first - 1].0)
    } else {
        ByteRange::new(spans[first - 1].0, spans[first - 1 + delivered - 1].1)
    };

    let withheld = if utf8.is_none() {
        Some(WithheldReason::NotUtf8)
    } else if !complete {
        Some(WithheldReason::BudgetExceeded)
    } else {
        None
    };
    let repair = match withheld {
        Some(WithheldReason::NotUtf8) => Some(
            "the target is not text this tool can edit; use bash or read it as bytes".to_string(),
        ),
        Some(WithheldReason::BudgetExceeded) => Some(format!(
            "read at most {} lines or {} bytes per call: ask for a smaller range",
            limits.max_read_lines, limits.max_read_bytes
        )),
        None => None,
    };

    let mut response = ExpReadResponse {
        path: request.path.clone(),
        start_line: first,
        line_count: delivered,
        total_lines,
        text,
        delivery_state: if complete {
            DeliveryState::Complete
        } else {
            DeliveryState::Partial
        },
        reference: None,
        revision: Some(snapshot.revision),
        editable: false,
        guarantee,
        withheld,
        repair,
    };

    if !complete {
        return Ok(response);
    }

    // A partial range on a non-UTF-8 target is still delivered as a note (the
    // bytes are shown reversibly through `from_utf8_lossy` by the caller); the
    // reference is what is withheld.
    if utf8.is_none() {
        response.text =
            String::from_utf8_lossy(&snapshot.bytes[byte_range.start..byte_range.end]).into_owned();
        return Ok(response);
    }

    let range_digest = digest64(&snapshot.bytes[byte_range.start..byte_range.end]);
    let record = RefRecord::new(
        owner,
        &request.path,
        &resource,
        snapshot.revision,
        byte_range,
        range_digest,
        guarantee == Guarantee::Strict,
    );
    let reference = refs.issue(record, limits.max_ref_ttl_ms)?;
    response.editable = guarantee == Guarantee::Strict;
    response.reference = Some(reference);
    Ok(response)
}

/// Half-open byte spans, one per line, each including its terminator.
///
/// A file that does not end with a newline has a final span without one, and
/// an empty file has none — the new contract says so explicitly instead of
/// inheriting the phantom trailing line of `split('\n')`.
fn line_spans(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = 0usize;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            spans.push((start, index + 1));
            start = index + 1;
        }
    }
    if start < bytes.len() {
        spans.push((start, bytes.len()));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_keep_terminators_and_drop_the_phantom_tail() {
        assert_eq!(line_spans(b""), Vec::new());
        assert_eq!(line_spans(b"one\n"), vec![(0, 4)]);
        assert_eq!(line_spans(b"one\ntwo"), vec![(0, 4), (4, 7)]);
        assert_eq!(line_spans(b"\n\n"), vec![(0, 1), (1, 2)]);
    }
}
