//! History transfer state machine.
//!
//! Kept separate from Bluetooth I/O so that window filtering, completion
//! handling and duplicate suppression can be tested with captured frames.

use crate::protocol::{decode_history_message, parse_control_frame, ControlFrame, Measurement};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationKind {
    Command,
    Data,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRecord {
    pub timestamp: u64,
    pub measurement: Measurement,
}

#[derive(Debug, Clone)]
pub struct Transfer {
    start_minutes: u32,
    end_minutes: u32,
    anchor: u64,
    records: Vec<HistoryRecord>,
    messages_received: u16,
    expected: Option<u16>,
    acknowledged: bool,
    saw_response: bool,
    completed: bool,
    timed_out: bool,
    interrupted: bool,
}

#[derive(Debug, Default)]
pub struct TransferSummary {
    pub records: Vec<HistoryRecord>,
    pub expected: Option<u16>,
    pub received: u16,
    pub complete: bool,
    pub timed_out: bool,
    pub interrupted: bool,
    pub saw_response: bool,
    pub acknowledged: bool,
}

impl Transfer {
    /// `request_started` is anchored to the start of the minute: the device
    /// reports whole minutes back, so this keeps repeated downloads of the same
    /// history reproducible.
    pub fn new(start_minutes: u32, end_minutes: u32, request_started: u64) -> Self {
        let anchor = request_started - (request_started % 60);
        Self {
            start_minutes,
            end_minutes,
            anchor,
            records: Vec::new(),
            messages_received: 0,
            expected: None,
            acknowledged: false,
            saw_response: false,
            completed: false,
            timed_out: false,
            interrupted: false,
        }
    }

    pub fn observe(&mut self, kind: NotificationKind, value: &[u8]) {
        self.saw_response = true;
        match kind {
            NotificationKind::Command => match parse_control_frame(value) {
                Some(ControlFrame::Ack) => self.acknowledged = true,
                Some(ControlFrame::Complete { expected_messages }) => {
                    self.expected = Some(expected_messages);
                    self.completed = true;
                }
                None => {}
            },
            NotificationKind::Data => {
                self.messages_received = self.messages_received.saturating_add(1);
                let Some((minutes_back, batch)) = decode_history_message(value) else {
                    return;
                };
                for record in batch {
                    let age = i64::from(minutes_back) - record.offset as i64;
                    // The device overshoots the requested end by up to the
                    // batch size and can include a slot older than start.
                    // Negative ages are malformed and are skipped, not clamped.
                    if age < i64::from(self.end_minutes) || age > i64::from(self.start_minutes) {
                        continue;
                    }
                    let timestamp = self.anchor.saturating_sub(age as u64 * 60);
                    self.records.push(HistoryRecord {
                        timestamp,
                        measurement: record.measurement,
                    });
                }
            }
        }
    }

    pub fn on_idle(&mut self) {
        if !self.completed {
            self.timed_out = true;
        }
    }

    pub fn on_total_timeout(&mut self) {
        if !self.completed {
            self.timed_out = true;
        }
    }

    pub fn on_interrupt(&mut self) {
        self.interrupted = true;
    }

    pub fn completed(&self) -> bool {
        self.completed
    }

    pub fn saw_response(&self) -> bool {
        self.saw_response
    }

    pub fn is_done(&self) -> bool {
        self.completed
            && self
                .expected
                .is_some_and(|expected| self.messages_received >= expected)
    }

    fn is_complete(&self) -> bool {
        self.completed
            && self
                .expected
                .is_some_and(|expected| self.messages_received >= expected)
    }

    pub fn finish(&mut self) -> TransferSummary {
        let mut records = std::mem::take(&mut self.records);
        records.sort_by_key(|record| record.timestamp);
        records.dedup_by_key(|record| record.timestamp);
        TransferSummary {
            records,
            expected: self.expected,
            received: self.messages_received,
            complete: self.is_complete(),
            timed_out: self.timed_out,
            interrupted: self.interrupted,
            saw_response: self.saw_response,
            acknowledged: self.acknowledged,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        assert_eq!(s.len() % 2, 0);
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn capture_data() -> Vec<Vec<u8>> {
        vec![
            hex("00150371e70375cf0371e70375cf0371e60375ce"),
            hex("000f0375cf0375cf0375ce0375cf0375ce0375cf"),
            hex("00090375cf0375cf0375d10375d00375d00375cf"),
            hex("00030375cf0375cf0375ceffffffffffffffffffff"),
        ]
    }

    #[test]
    fn filters_records_to_the_requested_window() {
        let mut transfer = Transfer::new(21, 2, 1_700_000_030);
        for data in capture_data() {
            transfer.observe(NotificationKind::Data, &data);
        }
        let summary = transfer.finish();

        // Captured batches cover ages 21..16, 15..10, 9..4 and 3..1. The
        // device overshoots end=2 by including age 1, which must be dropped:
        // 6 + 6 + 6 + 2 = 20 rows.
        assert_eq!(summary.records.len(), 20);
        assert!(summary.records.iter().all(|record| {
            let age = (1_700_000_030 - (1_700_000_030 % 60) - record.timestamp) / 60;
            (2..=21).contains(&age)
        }));
    }

    #[test]
    fn skips_negative_ages_instead_of_clamping_them_to_now() {
        // minutes_back 0 with a valid record in slot 1 would be one minute in
        // the future; it must not be emitted as a duplicate of the current row.
        let mut transfer = Transfer::new(10, 0, 1_700_000_030);
        transfer.observe(
            NotificationKind::Data,
            &hex("0000ffffffff0375cf0375cfffffffffffffffff"),
        );
        let summary = transfer.finish();
        assert!(summary.records.is_empty());
    }

    #[test]
    fn deduplicates_overlapping_batches() {
        let mut transfer = Transfer::new(21, 2, 1_700_000_030);
        for data in capture_data() {
            transfer.observe(NotificationKind::Data, &data);
            // A retransmitted/overlapping batch must not add duplicate rows.
            transfer.observe(NotificationKind::Data, &data);
        }
        let summary = transfer.finish();
        assert_eq!(summary.records.len(), 20);
        assert_eq!(summary.received, 8);
    }

    #[test]
    fn completes_only_after_the_expected_message_count() {
        let mut transfer = Transfer::new(60, 0, 1_700_000_030);
        transfer.observe(
            NotificationKind::Command,
            &hex("3301000000000000000000000000000000000032"),
        );
        assert!(transfer.saw_response());

        for data in capture_data() {
            transfer.observe(NotificationKind::Data, &data);
        }
        assert!(!transfer.completed());

        transfer.observe(
            NotificationKind::Command,
            &hex("ee010004000000000000000000000000000000eb"),
        );
        assert!(transfer.completed());
        assert!(transfer.is_done());

        let summary = transfer.finish();
        assert!(summary.complete);
        assert_eq!(summary.expected, Some(4));
        assert_eq!(summary.received, 4);
    }

    #[test]
    fn reports_idle_and_total_timeouts_as_incomplete() {
        let mut transfer = Transfer::new(60, 0, 1_700_000_030);
        transfer.observe(NotificationKind::Data, &capture_data()[0]);
        transfer.on_idle();
        let summary = transfer.finish();
        assert!(summary.timed_out);
        assert!(!summary.complete);
        assert_eq!(summary.received, 1);
    }

    #[test]
    fn interrupted_keeps_partial_records() {
        let mut transfer = Transfer::new(60, 0, 1_700_000_030);
        transfer.observe(NotificationKind::Data, &capture_data()[0]);
        transfer.on_interrupt();
        let summary = transfer.finish();
        assert!(summary.interrupted);
        assert!(!summary.complete);
        assert_eq!(summary.records.len(), 6);
    }
}
