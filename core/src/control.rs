//! Codec for `t_control_value`, the packed u32 these units use for all settings.
//!
//! Each field is `value` followed by a low "changed" bit; a write must set the
//! changed bit on the fields it touches and clear it on all others.
//! Layout ported from github.com/deiger/AirCon `control_value.py`.

use serde::{Deserialize, Serialize};

/// Mask that keeps every field value and drops every "changed" bit.
const VALUES_ONLY: u32 = 2_868_817_502;

#[derive(Clone, Copy, Debug)]
struct Field {
    /// Bit position of the field's "changed" flag; the value sits just above it.
    shift: u32,
    /// Width of the value in bits.
    width: u32,
}

const FAN: Field = Field { shift: 0, width: 4 };
const POWER: Field = Field { shift: 5, width: 1 };
const MODE: Field = Field { shift: 8, width: 3 };
const TURBO: Field = Field { shift: 12, width: 1 };
const ECO: Field = Field { shift: 14, width: 1 };
const TEMP: Field = Field { shift: 16, width: 6 };
const SWING_V: Field = Field { shift: 24, width: 1 };
const SWING_H: Field = Field { shift: 26, width: 1 };
const QUIET: Field = Field { shift: 28, width: 1 };
const UNIT_F: Field = Field { shift: 30, width: 1 };

const POWER_FLAG: u32 = 1 << POWER.shift;
const POWER_VALUE: u32 = 1 << (POWER.shift + 1);

impl Field {
    fn get(self, c: u32) -> u32 {
        (c >> (self.shift + 1)) & ((1 << self.width) - 1)
    }

    fn set(self, c: u32, v: u32) -> u32 {
        let mask = ((1u32 << (self.width + 1)) - 1) << self.shift;
        let v = v & ((1 << self.width) - 1);
        (c & !mask) | (((v << 1) | 1) << self.shift)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub power: bool,
    /// 0 fan, 1 heat, 2 cool, 3 dry, 4 auto.
    pub mode: u32,
    /// 0 auto, 5 lower, 6 low, 7 medium, 8 high, 9 higher.
    pub fan: u32,
    /// Raw setpoint field.
    pub temp: u32,
    pub turbo: bool,
    pub eco: bool,
    pub quiet: bool,
    pub swing_v: bool,
    pub swing_h: bool,
    pub fahrenheit: bool,
}

pub fn decode(c: u32) -> Settings {
    Settings {
        power: POWER.get(c) == 1,
        mode: MODE.get(c),
        fan: FAN.get(c),
        temp: TEMP.get(c),
        turbo: TURBO.get(c) == 1,
        eco: ECO.get(c) == 1,
        quiet: QUIET.get(c) == 1,
        swing_v: SWING_V.get(c) == 1,
        swing_h: SWING_H.get(c) == 1,
        fahrenheit: UNIT_F.get(c) == 1,
    }
}

/// Builds the value to write: `current` with every field that differs in `want` updated.
pub fn encode_changes(current: u32, want: &Settings) -> u32 {
    let have = decode(current);
    let mut c = current & VALUES_ONLY;
    let fields: [(Field, u32, u32); 10] = [
        (POWER, have.power as u32, want.power as u32),
        (MODE, have.mode, want.mode),
        (FAN, have.fan, want.fan),
        (TEMP, have.temp, want.temp),
        (TURBO, have.turbo as u32, want.turbo as u32),
        (ECO, have.eco as u32, want.eco as u32),
        (QUIET, have.quiet as u32, want.quiet as u32),
        (SWING_V, have.swing_v as u32, want.swing_v as u32),
        (SWING_H, have.swing_h as u32, want.swing_h as u32),
        (UNIT_F, have.fahrenheit as u32, want.fahrenheit as u32),
    ];
    for (field, old, new) in fields {
        if old != new {
            c = field.set(c, new);
        }
    }
    c
}

/// Drops the "changed" flags, giving the steady-state value the unit would report.
pub fn values_only(c: u32) -> u32 {
    c & VALUES_ONLY
}

/// Folds a queued write into a newer one: `next`'s values, flagging what either one changes.
pub fn merge_flags(queued: u32, next: u32) -> u32 {
    next | (queued & !VALUES_ONLY)
}

/// True for a write that switches power along with something else. The unit acts on the power
/// flag alone and drops the rest, so these have to go out separately.
pub fn mixes_power(c: u32) -> bool {
    let flags = c & !VALUES_ONLY;
    flags & POWER_FLAG != 0 && flags != POWER_FLAG
}

/// Splits a write that `mixes_power` into the other changes, then the power switch. In that order
/// so the unit starts up on the new settings.
pub fn split_power(current: u32, next: u32) -> Option<(u32, u32)> {
    if !mixes_power(next) {
        return None;
    }
    let power = POWER_VALUE | POWER_FLAG;
    let settings = (next & !power) | (current & POWER_VALUE);
    Some((settings, values_only(next) | POWER_FLAG))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Value reported by the cloud for our unit.
    const SAMPLE: u32 = 0x8295_0240;

    #[test]
    fn decodes_sample() {
        let s = decode(SAMPLE);
        assert!(s.power);
        assert_eq!(s.mode, 1);
        assert_eq!(s.fan, 0);
        assert_eq!(s.temp, 10);
        assert!(s.swing_v);
        assert!(s.fahrenheit);
    }

    #[test]
    fn matches_aircon_setters() {
        // set_temp(c, 24) == (c & ~(127 << 16)) | (((24 << 1) | 1) << 16)
        let c = values_only(SAMPLE);
        assert_eq!(TEMP.set(c, 24), (c & !(127 << 16)) | (((24 << 1) | 1) << 16));
        // set_power(c, OFF) == (c & ~(3 << 5)) | (((0 << 1) | 1) << 5)
        assert_eq!(POWER.set(c, 0), (c & !(3 << 5)) | (1 << 5));
        // set_fan_speed(c, HIGH=8) == (c & ~31) | ((8 << 1) | 1)
        assert_eq!(FAN.set(c, 8), (c & !31) | 17);
    }

    #[test]
    fn merged_write_keeps_both_changes() {
        let current = decode(SAMPLE);
        let fan = encode_changes(SAMPLE, &Settings { fan: 8, ..current });
        let temp = encode_changes(values_only(fan), &Settings { fan: 8, temp: 12, ..current });
        let merged = merge_flags(fan, temp);
        assert_eq!(merged, TEMP.set(FAN.set(values_only(SAMPLE), 8), 12));
    }

    #[test]
    fn power_goes_out_after_the_other_changes() {
        let off = values_only(SAMPLE) & !POWER_VALUE;
        let want = Settings { power: true, temp: 8, ..decode(off) };
        let next = encode_changes(off, &want);
        assert!(mixes_power(next));

        let (settings, power) = split_power(off, next).unwrap();
        assert_eq!(decode(settings), Settings { power: false, ..want });
        assert_eq!(settings & !VALUES_ONLY, 1 << TEMP.shift);
        assert_eq!(decode(power), want);
        assert_eq!(power & !VALUES_ONLY, POWER_FLAG);
    }

    #[test]
    fn single_changes_need_no_split() {
        let current = decode(SAMPLE);
        let temp = encode_changes(SAMPLE, &Settings { temp: 12, ..current });
        let power = encode_changes(SAMPLE, &Settings { power: false, ..current });
        assert_eq!(split_power(SAMPLE, temp), None);
        assert_eq!(split_power(SAMPLE, power), None);
    }

    #[test]
    fn encode_only_flags_changed_fields() {
        let mut want = decode(SAMPLE);
        want.temp = 22;
        let c = encode_changes(SAMPLE, &want);
        assert_eq!(decode(c), want);
        assert_eq!(c & !VALUES_ONLY, 1 << TEMP.shift);
    }
}
