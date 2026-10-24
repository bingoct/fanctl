//! The `ipmitool exec` batch protocol, shared by reads and writes.
//!
//! `ipmitool exec /dev/stdin` runs a whole list of raw commands in one process.
//! That is the only way to avoid paying ~90 ms of process startup per command:
//! on a TYAN S8030 the cost is ~40 ms once plus ~17 ms per BMC transaction.
//!
//! Two properties make the output usable:
//!
//! * a failed command prints nothing to stdout and the run continues, so the
//!   output alone cannot say *which* command failed;
//! * `raw 0x06 0x01` (Get Device ID) always prints exactly 15 bytes.
//!
//! So every command is preceded by that separator and the output is read as a
//! list of **slots**: slot `i` holds whatever followed the `i`-th separator. An
//! empty slot is a hole, never a shift - the reading after it still belongs to
//! its own command. This module owns the shape of that protocol so the reader
//! ([`crate::sensors::ipmi`]) and the writer ([`crate::actuator`]) cannot drift
//! apart.

/// `raw 0x06 0x01` is Get Device ID: it always prints 15 bytes, which makes it
/// a separator the BMC cannot skip.
pub(crate) const SEPARATOR_COMMAND: &str = "raw 0x06 0x01";

/// Number of bytes a Get Device ID response prints.
const SEPARATOR_BYTES: usize = 15;

/// Where `ipmitool exec` reads its command list from.
pub(crate) const SCRIPT_PATH: &str = "/dev/stdin";

/// Builds a command list with one separator line before every command.
///
/// Used for both directions: the read path builds its script once at startup,
/// the write path builds one per duty cycle change.
pub(crate) fn script<I, S>(commands: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = String::new();
    for command in commands {
        out.push_str(SEPARATOR_COMMAND);
        out.push('\n');
        out.push_str(command.as_ref());
        out.push('\n');
    }
    out
}

/// Splits a batched output into one entry per separator-delimited slot.
///
/// `slots[i]` is the first non-empty line after the `i`-th separator, trimmed;
/// `None` means that command printed nothing, i.e. it failed. Output before the
/// first separator is ignored, and anything past `expected` slots is ignored
/// too - a caller that asked for fewer commands must not read someone else's
/// answer.
pub(crate) fn split_slots(text: &str, expected: usize) -> Vec<Option<&str>> {
    let mut slots: Vec<Option<&str>> = vec![None; expected];
    let mut current: Option<usize> = None;

    for line in text.lines() {
        if byte_count(line) == SEPARATOR_BYTES {
            current = Some(match current {
                Some(index) => index + 1,
                None => 0,
            });
            continue;
        }

        // Anything before the first separator is not an answer we asked for.
        let Some(index) = current else {
            continue;
        };
        if index >= expected || slots[index].is_some() {
            continue;
        }

        let trimmed = line.trim();
        if !trimmed.is_empty() {
            slots[index] = Some(trimmed);
        }
    }

    slots
}

/// Which commands are confirmed by their slot holding a response.
///
/// A `raw 0x2e 0x44 ...` PWM write answers with a fixed three byte reply, so
/// "the slot is not empty" is the success signal. `false` means the command
/// printed nothing and the BMC refused it.
pub(crate) fn parse_acks(text: &str, expected: usize) -> Vec<bool> {
    split_slots(text, expected)
        .into_iter()
        .map(|slot| slot.is_some())
        .collect()
}

/// Number of whitespace-separated tokens on a line.
///
/// Walked as an iterator rather than collected: this runs per line, per cycle,
/// and counting is all that is needed to tell a separator from a payload.
fn byte_count(line: &str) -> usize {
    let mut tokens = line.split_whitespace();
    if tokens.next().is_none() {
        return 0;
    }
    if tokens.next().is_some() {
        2 + tokens.count()
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_acks, script, split_slots};

    const SEP: &str = " 20 81 06 02 02 bf fd 19 00 5e 1f 00 00 00 00";

    #[test]
    fn every_slot_is_filled_when_every_command_answers() {
        let text = format!("{SEP}\n 14 c0 c0 00\n{SEP}\n 1b c0 c0 00\n");
        assert_eq!(
            split_slots(&text, 2),
            vec![Some("14 c0 c0 00"), Some("1b c0 c0 00")]
        );
    }

    #[test]
    fn a_failed_command_leaves_a_hole_instead_of_shifting() {
        // Three commands, the middle one printed nothing: the third answer must
        // still land in slot 2, not slide into slot 1.
        let text = format!("{SEP}\n 14 c0 c0 00\n{SEP}\n{SEP}\n 16 c0 c0 00\n");
        assert_eq!(
            split_slots(&text, 3),
            vec![Some("14 c0 c0 00"), None, Some("16 c0 c0 00")]
        );
    }

    #[test]
    fn slots_past_the_expected_count_are_ignored() {
        let text = format!("{SEP}\n 14 c0 c0 00\n{SEP}\n 15 c0 c0 00\n");
        assert_eq!(split_slots(&text, 1), vec![Some("14 c0 c0 00")]);
    }

    #[test]
    fn empty_output_leaves_every_slot_empty() {
        assert_eq!(split_slots("", 3), vec![None, None, None]);
    }

    #[test]
    fn a_write_ack_counts_as_confirmation() {
        let text = format!("{SEP}\n fd 19 00\n{SEP}\n{SEP}\n fd 19 00\n");
        assert_eq!(parse_acks(&text, 3), vec![true, false, true]);
    }

    #[test]
    fn the_script_puts_a_separator_before_every_command() {
        let written = script(["raw 0x2e 0x01 0x28", "raw 0x2e 0x02 0x1e"]);
        assert_eq!(
            written,
            "raw 0x06 0x01\nraw 0x2e 0x01 0x28\nraw 0x06 0x01\nraw 0x2e 0x02 0x1e\n"
        );
    }
}
