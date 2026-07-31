//! The Dell C6522QT RS-232 command set. Dell publishes an *RS232 External Control
//! Application* with a documented frame + opcode table; this models the commands we use
//! (power, input select) and encodes the wire frame.
//!
//! NOTE: the exact opcode bytes must be confirmed against Dell's C6522QT RS232 manual
//! (#21). The frame *shape* below follows Dell's large-format-monitor
//! convention (`header, id, category, opcode, len, data..., checksum`); the opcode
//! constants are placeholders until verified.

use castaway_core::DisplayInput;

/// A high-level display command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DellCommand {
    /// Power the panel on.
    PowerOn,
    /// Power the panel off (standby).
    PowerOff,
    /// Select an input source.
    SelectInput(DisplayInput),
}

/// The monitor id byte for a single directly-connected panel (broadcast/default `0x01`).
pub const MONITOR_ID: u8 = 0x01;
const HEADER: u8 = 0xA6;
const CATEGORY_POWER: u8 = 0x01;
const CATEGORY_INPUT: u8 = 0x02;

// Placeholder opcodes — verify against the Dell manual (#21).
const OP_POWER: u8 = 0x18;
const OP_INPUT: u8 = 0xAC;

const INPUT_HDMI1: u8 = 0x11;
const INPUT_HDMI2: u8 = 0x12;
const INPUT_DP: u8 = 0x0D;
const INPUT_USBC: u8 = 0x19;

impl DellCommand {
    fn category_opcode_data(self) -> (u8, u8, u8) {
        match self {
            DellCommand::PowerOn => (CATEGORY_POWER, OP_POWER, 0x01),
            DellCommand::PowerOff => (CATEGORY_POWER, OP_POWER, 0x00),
            DellCommand::SelectInput(input) => {
                let code = match input {
                    DisplayInput::Hdmi1 => INPUT_HDMI1,
                    DisplayInput::Hdmi2 => INPUT_HDMI2,
                    DisplayInput::DisplayPort => INPUT_DP,
                    DisplayInput::UsbC => INPUT_USBC,
                    // DisplayInput is #[non_exhaustive]; default a future input to HDMI1.
                    _ => INPUT_HDMI1,
                };
                (CATEGORY_INPUT, OP_INPUT, code)
            }
        }
    }

    /// Encode the command into its RS-232 frame (with trailing XOR checksum).
    #[must_use]
    pub fn encode(self) -> Vec<u8> {
        let (category, opcode, data) = self.category_opcode_data();
        // header, monitor-id, category, opcode, data-len, data
        let mut frame = vec![HEADER, MONITOR_ID, category, opcode, 0x01, data];
        let checksum = frame.iter().fold(0u8, |acc, &b| acc ^ b);
        frame.push(checksum);
        frame
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn power_on_frame_has_header_and_checksum() {
        let f = DellCommand::PowerOn.encode();
        assert_eq!(f[0], HEADER);
        assert_eq!(f[1], MONITOR_ID);
        let body = &f[..f.len() - 1];
        let expected = body.iter().fold(0u8, |a, &b| a ^ b);
        assert_eq!(*f.last().unwrap(), expected, "checksum must be XOR of body");
    }

    #[test]
    fn input_select_encodes_per_source() {
        let hdmi = DellCommand::SelectInput(DisplayInput::Hdmi1).encode();
        let usbc = DellCommand::SelectInput(DisplayInput::UsbC).encode();
        assert_ne!(hdmi, usbc);
        assert_eq!(hdmi[5], INPUT_HDMI1);
        assert_eq!(usbc[5], INPUT_USBC);
    }
}
