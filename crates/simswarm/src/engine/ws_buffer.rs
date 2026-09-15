//! WebSocket frames become visible only at sim-tick boundaries.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::trace::WsRecord;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BufferedFrame {
    pub received_tick: u64,
    pub source_seq: Option<i64>,
    pub payload: Value,
    arrival: u64,
}

#[derive(Default)]
pub struct WsTickBuffer {
    by_tick: BTreeMap<u64, Vec<BufferedFrame>>,
    next_arrival: u64,
    last_source_seq: Option<i64>,
}

impl WsTickBuffer {
    pub fn push(&mut self, received_tick: u64, payload: Value) {
        let source_seq = payload
            .get("source_seq")
            .or_else(|| payload.get("outbox_seq"))
            .and_then(Value::as_i64);
        self.by_tick
            .entry(received_tick)
            .or_default()
            .push(BufferedFrame {
                received_tick,
                source_seq,
                payload,
                arrival: self.next_arrival,
            });
        self.next_arrival = self.next_arrival.saturating_add(1);
    }

    #[must_use]
    pub fn drain_through(&mut self, tick: u64) -> Vec<BufferedFrame> {
        let later = self.by_tick.split_off(&tick.saturating_add(1));
        let ready = std::mem::replace(&mut self.by_tick, later);
        let mut frames = ready.into_values().flatten().collect::<Vec<_>>();
        frames.sort_by_key(|frame| {
            (
                frame.received_tick,
                frame.source_seq.unwrap_or(i64::MIN),
                frame.arrival,
            )
        });
        frames
    }

    #[must_use]
    pub fn trace_records(&mut self, frames: &[BufferedFrame]) -> Vec<WsRecord> {
        let mut records = Vec::new();
        for frame in frames {
            let Some(source_seq) = frame.source_seq else {
                continue;
            };
            if let Some(last) = self.last_source_seq {
                let expected = last.saturating_add(1);
                if source_seq > expected {
                    records.push(WsRecord::Gap {
                        expected,
                        observed: source_seq,
                    });
                }
            }
            records.push(WsRecord::Event {
                source_seq,
                frame_type: frame
                    .payload
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned(),
            });
            self.last_source_seq = Some(source_seq);
        }
        records
    }

    #[must_use]
    pub fn recovery_snapshot(source_seq: i64, view_hash: impl Into<String>) -> WsRecord {
        WsRecord::Snapshot {
            source_seq,
            view_hash: view_hash.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn frames_wait_for_boundary_then_sort_by_sequence_and_arrival() {
        let mut buffer = WsTickBuffer::default();
        buffer.push(2, json!({"type":"price","outbox_seq":4}));
        buffer.push(1, json!({"type":"snapshot"}));
        buffer.push(2, json!({"type":"trade","outbox_seq":3}));
        assert!(buffer.drain_through(0).is_empty());
        let frames = buffer.drain_through(2);
        assert_eq!(
            frames
                .iter()
                .map(|frame| frame.source_seq)
                .collect::<Vec<_>>(),
            [None, Some(3), Some(4)]
        );
    }

    #[test]
    fn gaps_are_detected_and_snapshot_records_are_explicit() {
        let mut buffer = WsTickBuffer::default();
        buffer.push(1, json!({"type":"price","source_seq":3}));
        buffer.push(2, json!({"type":"price","source_seq":5}));
        let first = buffer.drain_through(1);
        assert_eq!(buffer.trace_records(&first).len(), 1);
        let second = buffer.drain_through(2);
        let records = buffer.trace_records(&second);
        assert!(matches!(
            records[0],
            WsRecord::Gap {
                expected: 4,
                observed: 5
            }
        ));
        assert_eq!(
            WsTickBuffer::recovery_snapshot(5, "hash"),
            WsRecord::Snapshot {
                source_seq: 5,
                view_hash: "hash".into()
            }
        );
        buffer.push(3, json!({"type":"snapshot"}));
        let unsequenced = buffer.drain_through(3);
        assert!(buffer.trace_records(&unsequenced).is_empty());
    }
}
