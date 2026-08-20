

pub struct CurveStepper {
    last_duty: Option<u32>,
    active_target: Option<u32>,
    transition_start_temp: i32,
    anchored: bool,
}

impl Default for CurveStepper {
    fn default() -> Self {
        Self::new()
    }
}

impl CurveStepper {
    pub fn new() -> Self {
        Self { last_duty: None, active_target: None, transition_start_temp: 0, anchored: false }
    }
    pub fn with_last_duty(duty: u32) -> Self {
        Self { last_duty: Some(duty), active_target: None, transition_start_temp: 0, anchored: false }
    }
    pub fn reset(&mut self) {
        self.last_duty = None;
        self.active_target = None;
        self.anchored = false;
    }
    pub fn note_applied(&mut self, duty: u32) {
        self.last_duty = Some(duty);
    }
    /// Last duty actually applied (or the seed), if any. Used to re-assert
    /// a converged duty after sleep/resume.
    pub fn current_duty(&self) -> Option<u32> {
        self.last_duty
    }
    pub fn next(&mut self, temp: i32, hysteresis_c: u32, rate_limit_up: u32, rate_limit_down: Option<u32>, full_points: &[[u32; 2]]) -> Option<u32> {
        if !self.anchored {
            self.transition_start_temp = temp;
            self.active_target = None;
            self.anchored = true;
        }
        let curve_target = calculate_duty_from_curve(temp, full_points);
        match self.active_target {
            None => {
                self.active_target = Some(curve_target);
                self.transition_start_temp = temp;
            }
            Some(current) if curve_target != current => {
                let should_apply = curve_target > current
                    || hysteresis_c == 0
                    || temp >= self.transition_start_temp
                    || temp <= self.transition_start_temp.saturating_sub(hysteresis_c as i32);
                if should_apply {
                    self.active_target = Some(curve_target);
                    self.transition_start_temp = temp;
                }
            }
            _ => {}
        }
        let tgt = self.active_target?;
        let next = match self.last_duty {
            Some(prev) => {
                let rate = if tgt >= prev {
                    rate_limit_up
                } else {
                    rate_limit_down.unwrap_or(rate_limit_up)
                };
                apply_rate_limit(prev, tgt, rate)
            }
            None => tgt,
        };
        if self.last_duty != Some(next) { Some(next) } else { None }
    }
}

pub fn calculate_duty_from_curve(temp: i32, full_points: &[[u32; 2]]) -> u32 {
    debug_assert!(full_points.len() >= 2, "full_points must have at least 2 elements (curve_full_points ensures this)");
    let temp = temp as f64;
    for w in full_points.windows(2) {
        let [p1, p2] = *w else { unreachable!("windows(2) always yields 2-element slices") };
        let (x1, y1) = (p1[0] as f64, p1[1] as f64);
        let (x2, y2) = (p2[0] as f64, p2[1] as f64);
        if temp <= x1 { return y1 as u32; }
        if temp <= x2 {
            if x2 == x1 { return y2 as u32; }
            let ratio = (temp - x1) / (x2 - x1);
            return (y1 + ratio * (y2 - y1)).round() as u32;
        }
    }
    // Safe default: max fan when no curve defined to prevent overheat
    100
}

pub fn apply_rate_limit(current: u32, target: u32, max_change: u32) -> u32 {
    // A zero step would stall the ramp forever (every call returns the same
    // value, so the caller keeps resubmitting and the fan never moves).
    // Treat 0 as "no rate limit": jump straight to the target.
    if max_change == 0 {
        return target;
    }
    if target > current {
        current.saturating_add(max_change).min(target)
    } else {
        current.saturating_sub(max_change).max(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types;

    #[test]
    fn calculate_duty_from_curve_below_min() {
        let points = types::curve_full_points(&[[30, 0], [45, 20], [60, 40], [75, 80], [85, 100]]);
        assert_eq!(calculate_duty_from_curve(20, &points), 0);
    }

    #[test]
    fn calculate_duty_from_curve_at_point() {
        let points = types::curve_full_points(&[[30, 0], [45, 20], [60, 40], [75, 80], [85, 100]]);
        assert_eq!(calculate_duty_from_curve(60, &points), 40);
    }

    #[test]
    fn calculate_duty_from_curve_interpolated() {
        let points = types::curve_full_points(&[[30, 0], [45, 20], [60, 40], [75, 80], [85, 100]]);
        assert_eq!(calculate_duty_from_curve(67, &points), 59);
    }

    #[test]
    fn calculate_duty_from_curve_above_max() {
        let points = types::curve_full_points(&[[30, 0], [45, 20], [60, 40], [75, 80], [85, 100]]);
        assert_eq!(calculate_duty_from_curve(90, &points), 100);
    }

    #[test]
    fn calculate_duty_from_curve_empty_uses_full() {
        let points = types::curve_full_points(&[]);
        // The fallback spans 0°C..110°C, so 50°C sits at 50/110 of the ramp.
        assert_eq!(calculate_duty_from_curve(50, &points), 45);
        assert_eq!(calculate_duty_from_curve(110, &points), 100);
    }

    #[test]
    fn apply_rate_limit_up() {
        assert_eq!(apply_rate_limit(40, 60, 10), 50);
    }

    #[test]
    fn apply_rate_limit_down() {
        assert_eq!(apply_rate_limit(60, 40, 10), 50);
    }

    #[test]
    fn apply_rate_limit_already_at_target() {
        assert_eq!(apply_rate_limit(50, 50, 10), 50);
    }

    #[test]
    fn apply_rate_limit_clamps_to_target() {
        assert_eq!(apply_rate_limit(40, 100, 50), 90);
    }

    #[test]
    fn apply_rate_limit_saturate_sub() {
        assert_eq!(apply_rate_limit(5, 0, 10), 0);
    }

    #[test]
    fn apply_rate_limit_zero_step_jumps_to_target() {
        assert_eq!(apply_rate_limit(40, 60, 0), 60);
        assert_eq!(apply_rate_limit(60, 40, 0), 40);
        assert_eq!(apply_rate_limit(40, 40, 0), 40);
    }

    #[test]
    fn curve_stepper_first_return() {
        let mut stepper = CurveStepper::new();
        let points = [[30, 0], [45, 20], [60, 40], [75, 80], [85, 100]];
        let full = types::curve_full_points(&points);
        let result = stepper.next(50, 2, 10, None, &full);
        assert_eq!(result, Some(27));
    }

    #[test]
    fn curve_stepper_no_change_returns_none() {
        let mut stepper = CurveStepper::new();
        let points = [[30, 0], [45, 20], [60, 40], [75, 80], [85, 100]];
        let full = types::curve_full_points(&points);
        let first = stepper.next(60, 2, 100, None, &full);
        assert_eq!(first, Some(40));
        stepper.note_applied(40);
        let result = stepper.next(60, 2, 100, None, &full);
        assert_eq!(result, None);
    }

    #[test]
    fn curve_stepper_rate_limited() {
        let mut stepper = CurveStepper::new();
        let points = [[30, 0], [45, 20], [60, 40], [75, 80], [85, 100]];
        let full = types::curve_full_points(&points);
        let first = stepper.next(85, 2, 5, None, &full);
        assert_eq!(first, Some(100));
        stepper.note_applied(100);
        let result = stepper.next(30, 2, 5, None, &full);
        assert_eq!(result, Some(95));
    }

    #[test]
    fn curve_stepper_hysteresis_suppresses_small_drop() {
        let mut stepper = CurveStepper::new();
        let points = [[30, 0], [45, 20], [60, 40], [75, 80], [85, 100]];
        let full = types::curve_full_points(&points);
        let first = stepper.next(70, 5, 100, None, &full);
        assert_eq!(first, Some(67));
        stepper.note_applied(67);
        let result = stepper.next(68, 5, 100, None, &full);
        assert_eq!(result, None);
    }

    #[test]
    fn curve_stepper_hysteresis_allows_large_drop() {
        let mut stepper = CurveStepper::new();
        let points = [[30, 0], [45, 20], [60, 40], [75, 80], [85, 100]];
        let full = types::curve_full_points(&points);
        let first = stepper.next(70, 5, 100, None, &full);
        assert_eq!(first, Some(67));
        stepper.note_applied(67);
        let result = stepper.next(60, 5, 100, None, &full);
        assert_eq!(result, Some(40));
    }

    #[test]
    fn curve_stepper_hysteresis_zero_always_triggers() {
        let mut stepper = CurveStepper::new();
        let points = [[30, 0], [45, 20], [60, 40], [75, 80], [85, 100]];
        let full = types::curve_full_points(&points);
        let first = stepper.next(70, 0, 100, None, &full);
        assert_eq!(first, Some(67));
        stepper.note_applied(67);
        let result = stepper.next(69, 0, 100, None, &full);
        assert_eq!(result, Some(64));
    }

    #[test]
    fn curve_stepper_rise_always_triggers() {
        let mut stepper = CurveStepper::new();
        let points = [[30, 0], [45, 20], [60, 40], [75, 80], [85, 100]];
        let full = types::curve_full_points(&points);
        let first = stepper.next(60, 5, 100, None, &full);
        assert_eq!(first, Some(40));
        stepper.note_applied(40);
        let result = stepper.next(62, 5, 100, None, &full);
        assert_eq!(result, Some(45));
    }

    #[test]
    fn curve_stepper_with_last_duty_restores_state() {
        let mut stepper = CurveStepper::with_last_duty(50);
        let points = [[30, 0], [45, 20], [60, 40], [75, 80], [85, 100]];
        let full = types::curve_full_points(&points);
        let result = stepper.next(60, 2, 10, None, &full);
        assert_eq!(result, Some(40));
    }

    #[test]
    fn curve_stepper_reset_clears_state() {
        let mut stepper = CurveStepper::new();
        let points = [[30, 0], [45, 20], [60, 40], [75, 80], [85, 100]];
        let full = types::curve_full_points(&points);
        let first = stepper.next(85, 2, 100, None, &full);
        assert_eq!(first, Some(100));
        stepper.note_applied(100);
        stepper.reset();
        let result = stepper.next(85, 2, 100, None, &full);
        assert_eq!(result, Some(100));
    }

    #[test]
    fn curve_stepper_separate_down_rate() {
        let mut stepper = CurveStepper::new();
        let points = [[30, 0], [45, 20], [60, 40], [75, 80], [85, 100]];
        let full = types::curve_full_points(&points);
        let first = stepper.next(85, 0, 100, Some(3), &full);
        assert_eq!(first, Some(100));
        stepper.note_applied(100);
        let result = stepper.next(30, 0, 100, Some(3), &full);
        assert_eq!(result, Some(97));
    }
}
