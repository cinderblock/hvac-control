use super::*;
use chrono::NaiveDate;

const COOL: u32 = 2;
const HEAT: u32 = 1;
const MIN: u64 = 60_000;
const HOUR: u64 = 60 * MIN;

// 2026-07-15 is a Wednesday.
fn at(month: u32, day: u32, h: u32, m: u32) -> NaiveDateTime {
    NaiveDate::from_ymd_opt(2026, month, day).unwrap().and_hms_opt(h, m, 0).unwrap()
}

fn inputs(now: NaiveDateTime, outside: f64, high: f64, low: f64) -> Inputs {
    Inputs { now, outside_f: Some(outside), high_f: Some(high), low_f: Some(low) }
}

fn no_weather(now: NaiveDateTime) -> Inputs {
    Inputs { now, outside_f: None, high_f: None, low_f: None }
}

fn cool(temp: i32) -> Action {
    Action { power: true, mode: COOL, temp_f: Some(temp), fan: None, ramp_minutes: None }
}

fn rule(name: &str, when: When, then: Action) -> Rule {
    Rule { name: name.into(), enabled: true, when, then }
}

fn schedule(rules: Vec<Rule>) -> Schedule {
    Schedule { enabled: true, rules, ..Schedule::default() }
}

fn pick(s: &Schedule, i: &Inputs) -> Option<String> {
    evaluate(s, i, None).name
}

fn matches(when: When, i: &Inputs) -> bool {
    evaluate(&schedule(vec![rule("r", when, cool(75))]), i, None).rule.is_some()
}

fn settings(power: bool, mode: u32, temp_f: i32, fan: u32) -> Settings {
    Settings {
        power,
        mode,
        fan,
        temp: (temp_f - 64) as u32,
        turbo: false,
        eco: false,
        quiet: false,
        swing_v: false,
        swing_h: false,
        fahrenheit: true,
    }
}

#[test]
fn first_enabled_match_wins() {
    let mut s = schedule(vec![rule("a", When::default(), cool(75)), rule("b", When::default(), cool(76))]);
    assert_eq!(pick(&s, &no_weather(at(7, 15, 12, 0))).as_deref(), Some("a"));
    s.rules[0].enabled = false;
    assert_eq!(pick(&s, &no_weather(at(7, 15, 12, 0))).as_deref(), Some("b"));
}

#[test]
fn no_match_means_no_target() {
    let s = schedule(vec![rule("a", window("09:00", "10:00"), cool(75))]);
    assert_eq!(evaluate(&s, &no_weather(at(7, 15, 12, 0)), None), Decision::default());
}

#[test]
fn time_window_is_half_open() {
    let w = || window("09:00", "12:00");
    assert!(!matches(w(), &no_weather(at(7, 15, 8, 59))));
    assert!(matches(w(), &no_weather(at(7, 15, 9, 0))));
    assert!(matches(w(), &no_weather(at(7, 15, 11, 59))));
    assert!(!matches(w(), &no_weather(at(7, 15, 12, 0))));
}

#[test]
fn time_window_wraps_midnight() {
    let w = || window("22:00", "07:00");
    assert!(matches(w(), &no_weather(at(7, 15, 23, 0))));
    assert!(matches(w(), &no_weather(at(7, 15, 6, 59))));
    assert!(!matches(w(), &no_weather(at(7, 15, 7, 0))));
    assert!(!matches(w(), &no_weather(at(7, 15, 21, 59))));
}

#[test]
fn days_filter() {
    let weekdays = || When { days: vec![1, 2, 3, 4, 5], ..When::default() };
    assert!(matches(weekdays(), &no_weather(at(7, 15, 12, 0)))); // Wednesday
    assert!(!matches(weekdays(), &no_weather(at(7, 18, 12, 0)))); // Saturday
}

#[test]
fn overnight_window_belongs_to_its_start_day() {
    let friday_night = || When { days: vec![5], ..window("22:00", "07:00") };
    assert!(matches(friday_night(), &no_weather(at(7, 17, 23, 0)))); // Friday night
    assert!(matches(friday_night(), &no_weather(at(7, 18, 3, 0)))); // Saturday morning
    assert!(!matches(friday_night(), &no_weather(at(7, 17, 3, 0)))); // Friday morning
}

#[test]
fn date_range_is_inclusive_and_wraps_new_year() {
    let range = |a: &str, b: &str| When { date_from: Some(a.into()), date_to: Some(b.into()), ..When::default() };
    assert!(matches(range("05-01", "09-30"), &no_weather(at(5, 1, 0, 0))));
    assert!(matches(range("05-01", "09-30"), &no_weather(at(9, 30, 23, 59))));
    assert!(!matches(range("05-01", "09-30"), &no_weather(at(10, 1, 0, 0))));
    assert!(matches(range("11-01", "02-28"), &no_weather(at(12, 31, 12, 0))));
    assert!(matches(range("11-01", "02-28"), &no_weather(at(1, 15, 12, 0))));
    assert!(!matches(range("11-01", "02-28"), &no_weather(at(7, 15, 12, 0))));
}

#[test]
fn weather_bounds() {
    let hot = || When { outside_min: Some(80.0), ..When::default() };
    let now = at(7, 15, 12, 0);
    assert!(matches(hot(), &inputs(now, 80.0, 90.0, 60.0)));
    assert!(!matches(hot(), &inputs(now, 79.9, 90.0, 60.0)));
    let forecast = || When { high_max: Some(70.0), low_min: Some(50.0), ..When::default() };
    assert!(matches(forecast(), &inputs(now, 60.0, 70.0, 50.0)));
    assert!(!matches(forecast(), &inputs(now, 60.0, 71.0, 50.0)));
    assert!(!matches(forecast(), &inputs(now, 60.0, 70.0, 49.0)));
}

#[test]
fn missing_weather_never_matches_a_weather_condition() {
    let hot = When { outside_min: Some(80.0), ..When::default() };
    assert!(!matches(hot, &no_weather(at(7, 15, 12, 0))));
}

#[test]
fn active_rule_keeps_matching_within_hysteresis() {
    let s = schedule(vec![rule("hot", When { outside_min: Some(80.0), ..When::default() }, cool(75))]);
    let i = inputs(at(7, 15, 12, 0), 79.5, 90.0, 60.0);
    assert_eq!(evaluate(&s, &i, None).rule, None);
    assert_eq!(evaluate(&s, &i, Some(0)).rule, Some(0));
    let i = inputs(at(7, 15, 12, 0), 78.9, 90.0, 60.0);
    assert_eq!(evaluate(&s, &i, Some(0)).rule, None);
}

#[test]
fn target_is_clamped_to_limits_and_says_so() {
    let s = schedule(vec![rule("a", When::default(), cool(90))]);
    let d = evaluate(&s, &no_weather(at(7, 15, 12, 0)), None);
    assert_eq!(d.target.unwrap().temp_f, Some(80));
    assert_eq!(d.reasons, ["90°F limited to 80°F"]);
}

#[test]
fn off_and_fan_targets_carry_no_setpoint() {
    let fan = Action { power: true, mode: 0, temp_f: Some(75), fan: None, ramp_minutes: None };
    let d = evaluate(&schedule(vec![rule("a", When::default(), fan)]), &no_weather(at(7, 15, 12, 0)), None);
    assert_eq!(d.target.unwrap().temp_f, None);
}

#[test]
fn reasons_explain_the_match() {
    let when = When { outside_min: Some(80.0), days: vec![3], ..window("09:00", "12:00") };
    let s = schedule(vec![rule("a", when, cool(75))]);
    let d = evaluate(&s, &inputs(at(7, 15, 10, 0), 82.4, 90.0, 60.0), None);
    assert_eq!(d.reasons, ["09:00–12:00", "Wed", "outside 82°F (≥ 80°F)"]);
}

#[test]
fn starter_schedule_is_valid_and_behaves() {
    let mut s = Schedule::default();
    assert!(!s.enabled);
    validate(&s).unwrap();
    s.enabled = true;
    let name = |i: &Inputs| pick(&s, i);
    assert_eq!(name(&inputs(at(7, 15, 10, 0), 75.0, 90.0, 60.0)).as_deref(), Some("Pre-cool"));
    assert_eq!(name(&inputs(at(7, 15, 15, 0), 88.0, 90.0, 60.0)).as_deref(), Some("Hot day"));
    assert_eq!(name(&inputs(at(7, 15, 23, 0), 70.0, 90.0, 60.0)).as_deref(), Some("Summer night"));
    assert_eq!(name(&inputs(at(1, 15, 23, 0), 40.0, 50.0, 35.0)).as_deref(), Some("Cold night"));
    assert_eq!(name(&inputs(at(1, 15, 12, 0), 50.0, 52.0, 40.0)).as_deref(), Some("Cold day"));
    assert_eq!(name(&inputs(at(4, 15, 12, 0), 65.0, 70.0, 50.0)).as_deref(), Some("Mild"));
    // Without weather the unit is left alone, not turned off.
    assert_eq!(name(&no_weather(at(1, 15, 12, 0))), None);
}

#[test]
fn schedule_json_uses_the_ui_field_names() {
    let s = Schedule::default();
    let v = serde_json::to_value(&s).unwrap();
    assert_eq!(v["hold"], serde_json::json!({ "kind": "boundary", "maxHours": 4.0 }));
    assert_eq!(v["limits"]["minF"], 64);
    assert_eq!(v["rules"][0]["when"]["highMin"], 85.0);
    let back: Schedule = serde_json::from_value(v).unwrap();
    assert_eq!(back, s);
}

#[test]
fn validate_rejects_bad_input() {
    let bad = |edit: fn(&mut Schedule)| {
        let mut s = schedule(vec![rule("a", When::default(), cool(75))]);
        edit(&mut s);
        validate(&s).is_err()
    };
    assert!(!bad(|_| {}));
    assert!(bad(|s| s.limits.min_f = 60));
    assert!(bad(|s| s.limits = Limits { min_f: 80, max_f: 70 }));
    assert!(bad(|s| s.hold = Hold::Fixed { hours: 0.0 }));
    assert!(bad(|s| s.rules[0].name = " ".into()));
    assert!(bad(|s| s.rules[0].when.days = vec![7]));
    assert!(bad(|s| s.rules[0].when.from = Some("09:00".into())));
    assert!(bad(|s| s.rules[0].when = window("09:00", "09:00")));
    assert!(bad(|s| s.rules[0].when = window("25:00", "09:00")));
    assert!(bad(|s| s.rules[0].when.date_from = Some("13-01".into())));
    assert!(bad(|s| s.rules[0].then.mode = 5));
    assert!(bad(|s| s.rules[0].then.fan = Some(3)));
    assert!(bad(|s| s.rules[0].then.temp_f = Some(63)));
}

#[test]
fn action_applies_only_what_it_manages() {
    let mut current = settings(true, HEAT, 70, 7);
    current.swing_v = true;
    let want = cool(75).apply(&current);
    assert_eq!((want.mode, want.temp, want.fan), (COOL, 11, 7));
    assert!(want.swing_v);
    let off = Action { power: false, mode: COOL, temp_f: Some(75), fan: Some(8), ramp_minutes: None };
    assert_eq!(off.apply(&current), Settings { power: false, ..current });
    assert!(off.matches(&settings(false, HEAT, 70, 7)));
}

#[test]
fn action_converts_for_a_unit_in_celsius() {
    let current = Settings { fahrenheit: false, temp: 20, ..settings(true, COOL, 70, 0) };
    assert_eq!(cool(77).apply(&current).temp, 25);
}

/// Two always-matching rules, driven through the tracker.
struct Rig {
    schedule: Schedule,
    decision: Decision,
    tracker: Tracker,
}

impl Rig {
    fn new(hold: Hold) -> Self {
        let mut schedule = schedule(vec![rule("a", When::default(), cool(75)), rule("b", When::default(), cool(78))]);
        schedule.hold = hold;
        let decision = evaluate(&schedule, &no_weather(at(7, 15, 12, 0)), None);
        Self { schedule, decision, tracker: Tracker::default() }
    }

    fn switch_to_b(&mut self) {
        self.schedule.rules[0].enabled = false;
        self.decision = evaluate(&self.schedule, &no_weather(at(7, 15, 12, 0)), None);
    }

    fn step(&mut self, now: u64, unit: Settings) -> Option<Settings> {
        self.tracker.step(now, &self.schedule, &self.decision, &unit)
    }
}

const BOUNDARY: Hold = Hold::Boundary { max_hours: None };

#[test]
fn asserts_target_then_goes_quiet_once_confirmed() {
    let mut r = Rig::new(BOUNDARY);
    let want = r.step(0, settings(true, HEAT, 70, 0)).unwrap();
    assert_eq!((want.mode, want.temp), (COOL, 11));
    assert_eq!(r.step(10 * MIN, want), None);
    assert_eq!(r.step(60 * MIN, want), None);
}

#[test]
fn resends_until_the_unit_confirms() {
    let mut r = Rig::new(BOUNDARY);
    let start = settings(true, HEAT, 70, 0);
    assert!(r.step(0, start).is_some());
    assert_eq!(r.step(MIN, start), None);
    assert!(r.step(2 * MIN, start).is_some());
}

#[test]
fn stale_readback_of_the_old_state_is_not_a_manual_change() {
    let mut r = Rig::new(BOUNDARY);
    let start = settings(true, HEAT, 70, 0);
    let want = r.step(0, start).unwrap();
    assert_eq!(r.step(20_000, want), None); // optimistic update
    assert_eq!(r.step(30_000, start), None); // poll raced the write
    assert!(r.tracker.hold().is_none());
    assert!(r.step(2 * MIN, start).is_some());
}

#[test]
fn manual_change_holds_until_the_rule_changes() {
    let mut r = Rig::new(BOUNDARY);
    let want = r.step(0, settings(true, HEAT, 70, 0)).unwrap();
    r.step(MIN, want);
    let manual = settings(true, COOL, 72, 0);
    assert_eq!(r.step(HOUR, manual), None);
    assert!(r.tracker.hold().is_some());
    assert_eq!(r.step(5 * HOUR, manual), None);
    r.switch_to_b();
    let want = r.step(5 * HOUR + MIN, manual).unwrap();
    assert!(r.tracker.hold().is_none());
    assert_eq!(want.temp, 14);
}

#[test]
fn manual_change_inside_the_grace_period_is_still_noticed() {
    let mut r = Rig::new(BOUNDARY);
    let want = r.step(0, settings(true, HEAT, 70, 0)).unwrap();
    r.step(10_000, want);
    assert_eq!(r.step(20_000, settings(true, COOL, 72, 0)), None);
    assert!(r.tracker.hold().is_some());
}

#[test]
fn hold_cap_expires_even_if_the_rule_does_not_change() {
    let mut r = Rig::new(Hold::Boundary { max_hours: Some(4.0) });
    let want = r.step(0, settings(true, HEAT, 70, 0)).unwrap();
    r.step(MIN, want);
    let manual = settings(true, COOL, 72, 0);
    r.step(HOUR, manual);
    assert_eq!(r.step(5 * HOUR - 1, manual), None);
    assert!(r.step(5 * HOUR, manual).is_some());
}

#[test]
fn fixed_hold_ignores_rule_changes() {
    let mut r = Rig::new(Hold::Fixed { hours: 2.0 });
    let want = r.step(0, settings(true, HEAT, 70, 0)).unwrap();
    r.step(MIN, want);
    let manual = settings(true, COOL, 72, 0);
    r.step(HOUR, manual);
    r.switch_to_b();
    assert_eq!(r.step(2 * HOUR, manual), None);
    assert!(r.step(3 * HOUR, manual).is_some());
}

#[test]
fn resume_hold_lasts_until_resumed() {
    let mut r = Rig::new(Hold::Resume);
    let want = r.step(0, settings(true, HEAT, 70, 0)).unwrap();
    r.step(MIN, want);
    let manual = settings(true, COOL, 72, 0);
    r.step(HOUR, manual);
    r.switch_to_b();
    assert_eq!(r.step(1000 * HOUR, manual), None);
    r.tracker.resume();
    assert!(r.step(1000 * HOUR + 1, manual).is_some());
}

#[test]
fn changes_to_unmanaged_fields_do_not_hold() {
    let mut r = Rig::new(BOUNDARY);
    let want = r.step(0, settings(true, HEAT, 70, 0)).unwrap();
    r.step(MIN, want);
    r.step(HOUR, Settings { swing_v: true, ..want });
    assert!(r.tracker.hold().is_none());
}

#[test]
fn earlier_command_arriving_after_a_rule_change_is_not_manual() {
    let mut r = Rig::new(BOUNDARY);
    let start = settings(true, HEAT, 70, 0);
    let first = r.step(0, start).unwrap();
    r.switch_to_b();
    assert!(r.step(10_000, start).is_some());
    assert_eq!(r.step(20_000, first), None); // the first command lands, then the second
    assert!(r.tracker.hold().is_none());
}

#[test]
fn disabled_schedule_sends_nothing_and_clears_a_hold() {
    let mut r = Rig::new(BOUNDARY);
    let want = r.step(0, settings(true, HEAT, 70, 0)).unwrap();
    r.step(MIN, want);
    r.step(HOUR, settings(true, COOL, 72, 0));
    assert!(r.tracker.hold().is_some());
    r.schedule.enabled = false;
    assert_eq!(r.step(2 * HOUR, settings(true, HEAT, 70, 0)), None);
    assert!(r.tracker.hold().is_none());
}

#[test]
fn change_with_no_active_rule_starts_no_hold() {
    let mut r = Rig::new(BOUNDARY);
    r.decision = Decision::default();
    r.step(0, settings(true, HEAT, 70, 0));
    r.step(MIN, settings(true, COOL, 72, 0));
    assert!(r.tracker.hold().is_none());
}

#[test]
fn time_window_can_end_at_midnight() {
    let w = || window("19:00", "00:00");
    assert!(!matches(w(), &no_weather(at(7, 15, 18, 59))));
    assert!(matches(w(), &no_weather(at(7, 15, 19, 0))));
    assert!(matches(w(), &no_weather(at(7, 15, 23, 59))));
    assert!(!matches(w(), &no_weather(at(7, 15, 0, 0))));
}

fn ramp_rule(to: i32, minutes: u32) -> Schedule {
    schedule(vec![rule("r", When::default(), Action { ramp_minutes: Some(minutes), ..cool(to) })])
}

fn ramp_step(t: &mut Tracker, s: &Schedule, now: u64, current: &Settings) -> Option<i32> {
    let d = evaluate(s, &no_weather(at(7, 15, 12, 0)), None);
    t.step(now, s, &d, current).map(|w| temp_f(&w))
}

#[test]
fn ramp_steps_one_degree_at_a_time_and_lands_on_time() {
    let s = ramp_rule(72, 40);
    let mut t = Tracker::default();
    let mut current = settings(true, COOL, 76, 0);

    assert_eq!(ramp_step(&mut t, &s, 0, &current), None);
    assert_eq!(t.ramp().map(|r| (r.from_f, r.to_f)), Some((76, 72)));
    assert_eq!(ramp_step(&mut t, &s, 9 * MIN, &current), None);

    let mut sent = Vec::new();
    for minute in 10..=40 {
        if let Some(temp) = ramp_step(&mut t, &s, minute * MIN, &current) {
            sent.push((minute, temp));
            current = settings(true, COOL, temp, 0);
        }
    }
    assert_eq!(sent, vec![(10, 75), (20, 74), (30, 73), (40, 72)]);
    assert!(t.hold().is_none());
    assert_eq!(ramp_step(&mut t, &s, 41 * MIN, &current), None);
    assert!(t.ramp().is_none());
}

#[test]
fn ramp_keeps_its_rule_through_the_last_step() {
    let s = ramp_rule(75, 10);
    let mut t = Tracker::default();
    let current = settings(true, COOL, 76, 0);
    ramp_step(&mut t, &s, 0, &current);
    assert_eq!(ramp_step(&mut t, &s, 10 * MIN, &current), Some(75));
    assert!(t.ramp().is_some(), "the final step is still a ramp step");
}

#[test]
fn ramp_does_not_apply_when_the_unit_is_off_or_in_another_mode() {
    let s = ramp_rule(72, 60);
    for current in [settings(false, COOL, 76, 0), settings(true, HEAT, 76, 0)] {
        let mut t = Tracker::default();
        assert_eq!(ramp_step(&mut t, &s, 0, &current), Some(72));
        assert!(t.ramp().is_none());
    }
}

#[test]
fn manual_change_during_a_ramp_holds_and_ends_it() {
    let s = ramp_rule(72, 40);
    let mut t = Tracker::default();
    ramp_step(&mut t, &s, 0, &settings(true, COOL, 76, 0));

    assert_eq!(ramp_step(&mut t, &s, 5 * MIN, &settings(true, COOL, 79, 0)), None);
    assert!(t.hold().is_some());
    assert!(t.ramp().is_none());
}

#[test]
fn next_change_finds_the_rule_boundary() {
    let s = schedule(vec![rule("day", window("08:30", "19:00"), cool(75))]);
    let start = at(7, 15, 18, 0);
    let ahead = |m: u32| no_weather(start + chrono::Duration::minutes(m as i64));
    assert_eq!(next_change(&s, Some(0), 240, ahead), Some(60));
    assert_eq!(next_change(&s, Some(0), 30, ahead), None);
}

#[test]
fn quiet_defaults_to_all_for_schedules_saved_before_it_existed() {
    let mut value = serde_json::to_value(Schedule::default()).unwrap();
    value.as_object_mut().unwrap().remove("quiet");
    assert_eq!(serde_json::from_value::<Schedule>(value).unwrap().quiet, Quiet::All);
}
