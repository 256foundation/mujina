//! I2C transaction assembly.

use crate::capture::{I2cEvent, I2cEventType};
use std::collections::VecDeque;

/// I2C transaction
#[derive(Debug, Clone)]
pub struct I2cTransaction {
    pub start_time: f64,
    pub address: u8,
    pub is_read: bool,
    pub data: Vec<u8>,
    /// Register address from write phase (for restart-based reads)
    pub register: Option<u8>,
    /// Whether all bytes were ACKed (false if any NAK occurred)
    pub all_acked: bool,
}

/// I2C transaction assembly state
#[derive(Debug, Clone)]
enum I2cState {
    /// Waiting for START condition
    Idle,
    /// Got START, waiting for address
    WaitingForAddress { start_time: f64 },
    /// Got address, collecting data
    CollectingData {
        start_time: f64,
        address: u8,
        is_read: bool,
        data: Vec<u8>,
        all_acks: bool,
        /// Register from write phase (for restart-based reads)
        register: Option<u8>,
    },
    /// Got restart during write, waiting for read address
    RestartingForRead {
        start_time: f64,
        write_address: u8,
        write_data: Vec<u8>,
        restart_time: f64,
        all_acks: bool,
    },
}

/// I2C transaction assembler
pub struct I2cAssembler {
    state: I2cState,
    transactions: VecDeque<I2cTransaction>,
}

impl I2cAssembler {
    pub fn new() -> Self {
        Self {
            state: I2cState::Idle,
            transactions: VecDeque::new(),
        }
    }

    /// Process an I2C event
    pub fn process(&mut self, event: &I2cEvent) {
        match &mut self.state {
            I2cState::Idle => {
                if event.event_type == I2cEventType::Start {
                    self.state = I2cState::WaitingForAddress {
                        start_time: event.timestamp,
                    };
                }
            }
            I2cState::WaitingForAddress { start_time } => match event.event_type {
                I2cEventType::Address => {
                    if let Some(addr) = event.address {
                        self.state = I2cState::CollectingData {
                            start_time: *start_time,
                            address: addr,
                            is_read: event.read,
                            data: Vec::new(),
                            all_acks: event.ack,
                            register: None,
                        };
                    } else {
                        // Invalid address, go back to idle
                        self.state = I2cState::Idle;
                    }
                }
                I2cEventType::Stop => {
                    // Unexpected stop, go back to idle
                    self.state = I2cState::Idle;
                }
                _ => {}
            },
            I2cState::CollectingData {
                start_time,
                address,
                is_read,
                data,
                all_acks,
                register,
            } => match event.event_type {
                I2cEventType::Data => {
                    if let Some(byte) = event.data {
                        data.push(byte);
                        // For reads, ignore NAK on last byte (master NAKs to signal end)
                        // For writes, track all NAKs as they indicate errors
                        if !*is_read {
                            *all_acks = *all_acks && event.ack;
                        }
                    }
                }
                I2cEventType::Stop => {
                    // Transaction complete
                    self.transactions.push_back(I2cTransaction {
                        start_time: *start_time,
                        address: *address,
                        is_read: *is_read,
                        data: data.clone(),
                        register: *register,
                        all_acked: *all_acks,
                    });
                    self.state = I2cState::Idle;
                }
                I2cEventType::Start => {
                    // Repeated start - check if this is a register select for read
                    if !*is_read && data.len() == 1 {
                        // This might be a register select for read-after-write
                        // Don't save yet - wait to see if next is a read to same address
                        self.state = I2cState::RestartingForRead {
                            start_time: *start_time,
                            write_address: *address,
                            write_data: data.clone(),
                            restart_time: event.timestamp,
                            all_acks: *all_acks,
                        };
                    } else {
                        // Normal restart - save current transaction if it has data
                        if !data.is_empty() {
                            self.transactions.push_back(I2cTransaction {
                                start_time: *start_time,
                                address: *address,
                                is_read: *is_read,
                                data: data.clone(),
                                register: *register,
                                all_acked: *all_acks,
                            });
                        }
                        // Start new transaction
                        self.state = I2cState::WaitingForAddress {
                            start_time: event.timestamp,
                        };
                    }
                }
                _ => {}
            },
            I2cState::RestartingForRead {
                start_time,
                write_address,
                write_data,
                restart_time,
                all_acks,
            } => match event.event_type {
                I2cEventType::Address => {
                    if let Some(addr) = event.address {
                        if addr == *write_address && event.read {
                            // This is the expected read address after restart
                            // Continue collecting read data with register from write
                            self.state = I2cState::CollectingData {
                                start_time: *start_time,
                                address: addr,
                                is_read: true,
                                data: Vec::new(),
                                all_acks: event.ack,
                                register: Some(write_data[0]),
                            };
                        } else {
                            // Different address or write - save write as separate transaction
                            self.transactions.push_back(I2cTransaction {
                                start_time: *start_time,
                                address: *write_address,
                                is_read: false,
                                data: write_data.clone(),
                                register: None,
                                all_acked: *all_acks,
                            });
                            // Start new transaction
                            self.state = I2cState::CollectingData {
                                start_time: *restart_time,
                                address: addr,
                                is_read: event.read,
                                data: Vec::new(),
                                all_acks: event.ack,
                                register: None,
                            };
                        }
                    } else {
                        // Invalid address, save write and go idle
                        self.transactions.push_back(I2cTransaction {
                            start_time: *start_time,
                            address: *write_address,
                            is_read: false,
                            data: write_data.clone(),
                            register: None,
                            all_acked: *all_acks,
                        });
                        self.state = I2cState::Idle;
                    }
                }
                I2cEventType::Stop => {
                    // Unexpected stop - save write transaction
                    self.transactions.push_back(I2cTransaction {
                        start_time: *start_time,
                        address: *write_address,
                        is_read: false,
                        data: write_data.clone(),
                        register: None,
                        all_acked: *all_acks,
                    });
                    self.state = I2cState::Idle;
                }
                _ => {}
            },
        }
    }

    /// Get next completed transaction
    pub fn next_transaction(&mut self) -> Option<I2cTransaction> {
        self.transactions.pop_front()
    }

    /// Flush any pending transaction
    pub fn flush(&mut self) {
        // If we're in the middle of collecting data, treat it as incomplete
        if let I2cState::CollectingData {
            start_time,
            address,
            is_read,
            data,
            all_acks,
            register,
            ..
        } = &self.state
        {
            if !data.is_empty() {
                self.transactions.push_back(I2cTransaction {
                    start_time: *start_time,
                    address: *address,
                    is_read: *is_read,
                    data: data.clone(),
                    register: *register,
                    all_acked: *all_acks,
                });
            }
        }
        self.state = I2cState::Idle;
    }
}

/// Group related I2C transactions (e.g., register write followed by read)
#[derive(Debug, Clone)]
pub struct I2cOperation {
    pub start_time: f64,
    pub address: u8,
    pub register: Option<u8>,
    pub write_data: Option<Vec<u8>>,
    pub read_data: Option<Vec<u8>>,
    /// Whether the operation was NAKed (any byte not acknowledged)
    pub was_naked: bool,
}

/// Maximum time gap (in seconds) between transactions to consider them related
const MAX_TRANSACTION_GAP: f64 = 0.010; // 10ms

/// Group I2C transactions into logical operations
pub fn group_transactions(transactions: &[I2cTransaction]) -> Vec<I2cOperation> {
    let mut operations = Vec::new();
    let mut i = 0;

    while i < transactions.len() {
        let t1 = &transactions[i];

        // Check if this is a register write followed by read pattern
        if !t1.is_read && t1.data.len() >= 1 && i + 1 < transactions.len() {
            let t2 = &transactions[i + 1];
            let time_gap = t2.start_time - t1.start_time;

            if t2.is_read && t2.address == t1.address && time_gap <= MAX_TRANSACTION_GAP {
                let command = t1.data[0];

                // Don't group command-only writes like CLEAR_FAULTS (0x03) with subsequent reads
                // These are data-less commands that don't expect a read response
                if t1.data.len() == 1 && command == 0x03 {
                    // Skip grouping for CLEAR_FAULTS
                } else {
                    // Valid register read pattern: write register address, then read data
                    operations.push(I2cOperation {
                        start_time: t1.start_time,
                        address: t1.address,
                        register: Some(command),
                        write_data: if t1.data.len() > 1 {
                            Some(t1.data[1..].to_vec())
                        } else {
                            None
                        },
                        read_data: Some(t2.data.clone()),
                        was_naked: !t1.all_acked || !t2.all_acked,
                    });
                    i += 2;
                    continue;
                }
            }
        }

        // Single transaction
        let (register, write_data) = if !t1.is_read && !t1.data.is_empty() {
            // For writes, first byte is command/register, rest is data
            let cmd = t1.data[0];
            let data = if t1.data.len() > 1 {
                Some(t1.data[1..].to_vec()) // Data after command
            } else {
                None // Command-only (like CLEAR_FAULTS)
            };
            (Some(cmd), data)
        } else if !t1.data.is_empty() {
            // For reads, include all data
            (Some(t1.data[0]), None)
        } else {
            (None, None)
        };

        operations.push(I2cOperation {
            start_time: t1.start_time,
            address: t1.address,
            register,
            write_data,
            read_data: if t1.is_read {
                Some(t1.data.clone())
            } else {
                None
            },
            was_naked: !t1.all_acked,
        });
        i += 1;
    }

    operations
}

/// PMBus-aware transaction parsing that respects I2C START/STOP boundaries
pub fn group_pmbus_transactions(transactions: &[I2cTransaction]) -> Vec<I2cOperation> {
    let mut operations = Vec::new();
    let mut i = 0;

    while i < transactions.len() {
        let t1 = &transactions[i];

        // Check for PMBus retry pattern: failed register select followed by successful read
        if !t1.is_read && t1.data.len() == 1 && i + 2 < transactions.len() {
            let t2 = &transactions[i + 1];
            let t3 = &transactions[i + 2];

            // Pattern: write reg → (incomplete/failed) → write reg → read data
            if !t2.is_read
                && t2.data.len() == 1
                && t3.is_read
                && t1.address == t2.address
                && t2.address == t3.address
                && t1.data[0] == t2.data[0]
            {
                // Same register command

                let command = t1.data[0];
                let time_gap1 = t2.start_time - t1.start_time;
                let time_gap2 = t3.start_time - t2.start_time;

                // Both gaps should be small for retry pattern
                if time_gap1 <= MAX_TRANSACTION_GAP && time_gap2 <= MAX_TRANSACTION_GAP {
                    // Handle PMBus read response format
                    let actual_data = if t3.data.len() > 1 {
                        let length = t3.data[0] as usize;
                        if length + 1 == t3.data.len() && length > 0 {
                            // PMBus block read: [length, data...]
                            Some(t3.data[1..].to_vec())
                        } else {
                            // Word/byte read
                            Some(t3.data.clone())
                        }
                    } else {
                        Some(t3.data.clone())
                    };

                    operations.push(I2cOperation {
                        start_time: t1.start_time,
                        address: t1.address,
                        register: Some(command),
                        write_data: None,
                        read_data: actual_data,
                        was_naked: !t1.all_acked || !t2.all_acked || !t3.all_acked,
                    });
                    i += 3; // Skip all three transactions
                    continue;
                }
            }
        }

        // Handle PMBus register read pattern: register select write + read response
        if !t1.is_read && t1.data.len() == 1 && i + 1 < transactions.len() {
            let t2 = &transactions[i + 1];
            let time_gap = t2.start_time - t1.start_time;

            // Check for register read pattern within timing window
            // This includes both normal patterns and restart-separated patterns
            if t2.is_read && t2.address == t1.address && time_gap <= MAX_TRANSACTION_GAP {
                let command = t1.data[0];

                // Handle PMBus read response formats
                let actual_data = if t2.data.len() > 1 {
                    let length = t2.data[0] as usize;
                    if length + 1 == t2.data.len() && length > 0 {
                        // PMBus block read: [length, data...]
                        Some(t2.data[1..].to_vec())
                    } else {
                        // Word/byte read or non-standard format
                        Some(t2.data.clone())
                    }
                } else {
                    // Single byte or empty read
                    if !t2.data.is_empty() {
                        Some(t2.data.clone())
                    } else {
                        None
                    }
                };

                operations.push(I2cOperation {
                    start_time: t1.start_time,
                    address: t1.address,
                    register: Some(command),
                    write_data: None,
                    read_data: actual_data,
                    was_naked: !t1.all_acked || !t2.all_acked,
                });
                i += 2;
                continue;
            }
        }

        // Handle complete PMBus write transaction (respects START/STOP boundaries)
        if !t1.is_read {
            let (command, write_data) = if !t1.data.is_empty() {
                let cmd = t1.data[0];
                let data = if t1.data.len() > 1 {
                    Some(t1.data[1..].to_vec()) // Command + data
                } else {
                    None // Command-only (data-less)
                };
                (Some(cmd), data)
            } else {
                (None, None) // Empty write
            };

            operations.push(I2cOperation {
                start_time: t1.start_time,
                address: t1.address,
                register: command,
                write_data,
                read_data: None,
                was_naked: !t1.all_acked,
            });
            i += 1;
            continue;
        }

        // Handle standalone read (unusual in PMBus but possible)
        // This also handles restart-based reads that were combined by the assembler
        if t1.is_read {
            // Handle PMBus read response formats if this is a restart-combined read
            let actual_data = if t1.register.is_some() && t1.data.len() > 1 {
                let length = t1.data[0] as usize;
                if length + 1 == t1.data.len() && length > 0 {
                    // PMBus block read: [length, data...]
                    Some(t1.data[1..].to_vec())
                } else {
                    // Word/byte read or non-standard format
                    Some(t1.data.clone())
                }
            } else {
                Some(t1.data.clone())
            };

            operations.push(I2cOperation {
                start_time: t1.start_time,
                address: t1.address,
                register: t1.register, // Use register from restart-combined transaction if present
                write_data: None,
                read_data: actual_data,
                was_naked: !t1.all_acked,
            });
            i += 1;
            continue;
        }

        // Empty transaction
        operations.push(I2cOperation {
            start_time: t1.start_time,
            address: t1.address,
            register: None,
            write_data: None,
            read_data: None,
            was_naked: !t1.all_acked,
        });
        i += 1;
    }

    operations
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_transaction(
        start_time: f64,
        address: u8,
        is_read: bool,
        data: Vec<u8>,
    ) -> I2cTransaction {
        I2cTransaction {
            start_time,
            address,
            is_read,
            data,
            register: None,
            all_acked: true, // Default to all ACKed for tests
        }
    }

    #[test]
    fn test_timing_constraint_prevents_grouping() {
        let transactions = vec![
            create_test_transaction(1.0, 0x24, false, vec![0x79]), // Write STATUS_WORD register
            create_test_transaction(1.020, 0x24, true, vec![0x00, 0x42]), // Read 20ms later
        ];

        let operations = group_transactions(&transactions);
        assert_eq!(operations.len(), 2); // Should be two separate operations due to timing gap
        assert!(operations[0].read_data.is_none()); // First should be write-only
        assert!(operations[1].read_data.is_some()); // Second should be read-only
    }

    #[test]
    fn test_clear_faults_not_grouped() {
        let transactions = vec![
            create_test_transaction(1.0, 0x24, false, vec![0x03]), // CLEAR_FAULTS write
            create_test_transaction(1.005, 0x24, true, vec![0x00, 0x42]), // Read 5ms later
        ];

        let operations = group_transactions(&transactions);
        assert_eq!(operations.len(), 2); // Should be two separate operations

        // First operation should be CLEAR_FAULTS write-only
        assert_eq!(operations[0].register, Some(0x03));
        assert!(operations[0].read_data.is_none());
        assert!(operations[0].write_data.is_none()); // Data-less

        // Second operation should be separate read
        assert!(operations[1].read_data.is_some());
    }

    #[test]
    fn test_proper_register_read_grouping() {
        let transactions = vec![
            create_test_transaction(1.0, 0x24, false, vec![0x79]), // Write STATUS_WORD register
            create_test_transaction(1.002, 0x24, true, vec![0x00, 0x42]), // Read 2ms later
        ];

        let operations = group_transactions(&transactions);
        assert_eq!(operations.len(), 1); // Should be grouped into one operation

        let op = &operations[0];
        assert_eq!(op.register, Some(0x79)); // STATUS_WORD
        assert!(op.write_data.is_none()); // No write data for register select
        assert_eq!(op.read_data, Some(vec![0x00, 0x42])); // Read response
    }

    #[test]
    fn test_register_write_with_data_then_read() {
        let transactions = vec![
            create_test_transaction(1.0, 0x24, false, vec![0x21, 0x66, 0x02]), // VOUT_COMMAND write
            create_test_transaction(1.003, 0x24, true, vec![0x66, 0x02]),      // Read back
        ];

        let operations = group_transactions(&transactions);
        assert_eq!(operations.len(), 1); // Should be grouped

        let op = &operations[0];
        assert_eq!(op.register, Some(0x21)); // VOUT_COMMAND
        assert_eq!(op.write_data, Some(vec![0x66, 0x02])); // Write data
        assert_eq!(op.read_data, Some(vec![0x66, 0x02])); // Read data
    }

    #[test]
    fn test_different_addresses_not_grouped() {
        let transactions = vec![
            create_test_transaction(1.0, 0x24, false, vec![0x79]), // TPS546
            create_test_transaction(1.002, 0x4C, true, vec![0x27]), // EMC2101
        ];

        let operations = group_transactions(&transactions);
        assert_eq!(operations.len(), 2); // Different addresses should not be grouped
    }

    #[test]
    fn test_single_write_operation() {
        let transactions = vec![
            create_test_transaction(1.0, 0x24, false, vec![0x01, 0x80]), // OPERATION ON
        ];

        let operations = group_transactions(&transactions);
        assert_eq!(operations.len(), 1);

        let op = &operations[0];
        assert_eq!(op.register, Some(0x01));
        assert_eq!(op.write_data, Some(vec![0x80])); // Just the data, not the command
        assert!(op.read_data.is_none());
    }

    #[test]
    fn test_single_read_operation() {
        let transactions = vec![
            create_test_transaction(1.0, 0x24, true, vec![0x42, 0x00]), // STATUS read response
        ];

        let operations = group_transactions(&transactions);
        assert_eq!(operations.len(), 1);

        let op = &operations[0];
        assert_eq!(op.register, Some(0x42)); // First byte treated as register for read
        assert!(op.write_data.is_none());
        assert_eq!(op.read_data, Some(vec![0x42, 0x00]));
    }

    #[test]
    fn test_pmbus_block_read_parsing() {
        let transactions = vec![
            create_test_transaction(1.0, 0x24, false, vec![0x99]), // MFR_ID register write
            create_test_transaction(1.002, 0x24, true, vec![0x03, 0x54, 0x49, 0x00]), // Block read: length=3, data=[54,49,00]
        ];

        let operations = group_pmbus_transactions(&transactions);
        assert_eq!(operations.len(), 1); // Should be grouped into one operation

        let op = &operations[0];
        assert_eq!(op.register, Some(0x99)); // MFR_ID
        assert!(op.write_data.is_none()); // No write data for register select
        assert_eq!(op.read_data, Some(vec![0x54, 0x49, 0x00])); // Actual data without length byte
    }

    #[test]
    fn test_pmbus_clear_faults_write_only() {
        let transactions = vec![
            create_test_transaction(1.0, 0x24, false, vec![0x03]), // CLEAR_FAULTS write
        ];

        let operations = group_pmbus_transactions(&transactions);
        assert_eq!(operations.len(), 1);

        let op = &operations[0];
        assert_eq!(op.register, Some(0x03)); // CLEAR_FAULTS
        assert!(op.write_data.is_none()); // Data-less command
        assert!(op.read_data.is_none()); // Write-only
    }

    #[test]
    fn test_pmbus_register_write_with_data() {
        let transactions = vec![
            create_test_transaction(1.0, 0x24, false, vec![0x21, 0x66, 0x02]), // VOUT_COMMAND write
        ];

        let operations = group_pmbus_transactions(&transactions);
        assert_eq!(operations.len(), 1);

        let op = &operations[0];
        assert_eq!(op.register, Some(0x21)); // VOUT_COMMAND
        assert_eq!(op.write_data, Some(vec![0x66, 0x02])); // Write data
        assert!(op.read_data.is_none());
    }

    #[test]
    fn test_pmbus_word_read() {
        let transactions = vec![
            create_test_transaction(1.0, 0x24, false, vec![0x79]), // STATUS_WORD register write
            create_test_transaction(1.002, 0x24, true, vec![0x00, 0x42]), // Word read: 2 bytes
        ];

        let operations = group_pmbus_transactions(&transactions);
        assert_eq!(operations.len(), 1);

        let op = &operations[0];
        assert_eq!(op.register, Some(0x79)); // STATUS_WORD
        assert!(op.write_data.is_none());
        assert_eq!(op.read_data, Some(vec![0x00, 0x42])); // Raw word data
    }

    #[test]
    fn test_i2c_restart_pattern_assembler() {
        let mut assembler = I2cAssembler::new();

        // Simulate I2C restart pattern: START → W@0x24 → 0x9A → RESTART → R@0x24 → data → STOP
        assembler.process(&I2cEvent {
            event_type: I2cEventType::Start,
            timestamp: 1.0,
            address: None,
            data: None,
            ack: false,
            read: false,
        });

        assembler.process(&I2cEvent {
            event_type: I2cEventType::Address,
            timestamp: 1.001,
            address: Some(0x24),
            data: None,
            ack: true,
            read: false,
        });

        assembler.process(&I2cEvent {
            event_type: I2cEventType::Data,
            timestamp: 1.002,
            address: None,
            data: Some(0x9A),
            ack: true,
            read: false,
        });

        // RESTART condition
        assembler.process(&I2cEvent {
            event_type: I2cEventType::Start,
            timestamp: 1.003,
            address: None,
            data: None,
            ack: false,
            read: false,
        });

        assembler.process(&I2cEvent {
            event_type: I2cEventType::Address,
            timestamp: 1.004,
            address: Some(0x24),
            data: None,
            ack: true,
            read: true,
        });

        assembler.process(&I2cEvent {
            event_type: I2cEventType::Data,
            timestamp: 1.005,
            address: None,
            data: Some(0x03),
            ack: true,
            read: false,
        });

        assembler.process(&I2cEvent {
            event_type: I2cEventType::Data,
            timestamp: 1.006,
            address: None,
            data: Some(0x00),
            ack: true,
            read: false,
        });

        assembler.process(&I2cEvent {
            event_type: I2cEventType::Data,
            timestamp: 1.007,
            address: None,
            data: Some(0x00),
            ack: false,
            read: false,
        });

        assembler.process(&I2cEvent {
            event_type: I2cEventType::Stop,
            timestamp: 1.008,
            address: None,
            data: None,
            ack: false,
            read: false,
        });

        // Should have a single combined transaction
        let transaction = assembler
            .next_transaction()
            .expect("Should have transaction");
        assert_eq!(transaction.address, 0x24);
        assert_eq!(transaction.is_read, true);
        assert_eq!(transaction.register, Some(0x9A)); // Register from write phase
        assert_eq!(transaction.data, vec![0x03, 0x00, 0x00]); // Read data
        assert!(assembler.next_transaction().is_none());
    }

    #[test]
    fn test_i2c_restart_pattern_different_address() {
        let mut assembler = I2cAssembler::new();

        // Simulate restart with different address: should be two separate transactions
        assembler.process(&I2cEvent {
            event_type: I2cEventType::Start,
            timestamp: 1.0,
            address: None,
            data: None,
            ack: false,
            read: false,
        });

        assembler.process(&I2cEvent {
            event_type: I2cEventType::Address,
            timestamp: 1.001,
            address: Some(0x24),
            data: None,
            ack: true,
            read: false,
        });

        assembler.process(&I2cEvent {
            event_type: I2cEventType::Data,
            timestamp: 1.002,
            address: None,
            data: Some(0x9A),
            ack: true,
            read: false,
        });

        // RESTART to different address
        assembler.process(&I2cEvent {
            event_type: I2cEventType::Start,
            timestamp: 1.003,
            address: None,
            data: None,
            ack: false,
            read: false,
        });

        assembler.process(&I2cEvent {
            event_type: I2cEventType::Address,
            timestamp: 1.004,
            address: Some(0x4C), // Different address
            data: None,
            ack: true,
            read: true,
        });

        assembler.process(&I2cEvent {
            event_type: I2cEventType::Data,
            timestamp: 1.005,
            address: None,
            data: Some(0x42),
            ack: false,
            read: false,
        });

        assembler.process(&I2cEvent {
            event_type: I2cEventType::Stop,
            timestamp: 1.006,
            address: None,
            data: None,
            ack: false,
            read: false,
        });

        // Should have two separate transactions
        let t1 = assembler
            .next_transaction()
            .expect("Should have first transaction");
        assert_eq!(t1.address, 0x24);
        assert_eq!(t1.is_read, false);
        assert_eq!(t1.data, vec![0x9A]);
        assert_eq!(t1.register, None);

        let t2 = assembler
            .next_transaction()
            .expect("Should have second transaction");
        assert_eq!(t2.address, 0x4C);
        assert_eq!(t2.is_read, true);
        assert_eq!(t2.data, vec![0x42]);
        assert_eq!(t2.register, None);

        assert!(assembler.next_transaction().is_none());
    }

    #[test]
    fn test_i2c_restart_multi_byte_write() {
        let mut assembler = I2cAssembler::new();

        // Restart with multi-byte write should be treated as normal restart
        assembler.process(&I2cEvent {
            event_type: I2cEventType::Start,
            timestamp: 1.0,
            address: None,
            data: None,
            ack: false,
            read: false,
        });

        assembler.process(&I2cEvent {
            event_type: I2cEventType::Address,
            timestamp: 1.001,
            address: Some(0x24),
            data: None,
            ack: true,
            read: false,
        });

        assembler.process(&I2cEvent {
            event_type: I2cEventType::Data,
            timestamp: 1.002,
            address: None,
            data: Some(0x21),
            ack: true,
            read: false,
        });

        assembler.process(&I2cEvent {
            event_type: I2cEventType::Data,
            timestamp: 1.003,
            address: None,
            data: Some(0x66),
            ack: true,
            read: false,
        });

        // RESTART - should save write as separate transaction
        assembler.process(&I2cEvent {
            event_type: I2cEventType::Start,
            timestamp: 1.004,
            address: None,
            data: None,
            ack: false,
            read: false,
        });

        assembler.process(&I2cEvent {
            event_type: I2cEventType::Address,
            timestamp: 1.005,
            address: Some(0x24),
            data: None,
            ack: true,
            read: true,
        });

        assembler.process(&I2cEvent {
            event_type: I2cEventType::Data,
            timestamp: 1.006,
            address: None,
            data: Some(0x42),
            ack: false,
            read: false,
        });

        assembler.process(&I2cEvent {
            event_type: I2cEventType::Stop,
            timestamp: 1.007,
            address: None,
            data: None,
            ack: false,
            read: false,
        });

        // Should have two separate transactions
        let t1 = assembler
            .next_transaction()
            .expect("Should have first transaction");
        assert_eq!(t1.address, 0x24);
        assert_eq!(t1.is_read, false);
        assert_eq!(t1.data, vec![0x21, 0x66]);
        assert_eq!(t1.register, None);

        let t2 = assembler
            .next_transaction()
            .expect("Should have second transaction");
        assert_eq!(t2.address, 0x24);
        assert_eq!(t2.is_read, true);
        assert_eq!(t2.data, vec![0x42]);
        assert_eq!(t2.register, None);

        assert!(assembler.next_transaction().is_none());
    }
}
