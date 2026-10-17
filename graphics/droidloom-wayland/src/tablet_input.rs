//! Native stylus path; ABI 7 retains the existing touchscreen fallback.
use super::*;

pub(super) fn tilt_degrees(value: f64) -> i8 {
    if value.is_finite() {
        value.round().clamp(-90.0, 90.0) as i8
    } else {
        0
    }
}

impl App {
    pub(super) fn send_native_pen(&mut self, tool_id: u32, action: u8) -> bool {
        let Some((surface, tool_x, tool_y, tool_pressure, tool_tilt, tool_eraser, tool_buttons)) = self
            .tablet_tools
            .iter()
            .find(|tool| tool.tool_id == tool_id)
            .map(|t| (t.surface.clone(), t.x, t.y, t.pressure, t.tilt, t.eraser, t.buttons))
        else {
            return false;
        };
        let target = surface
            .as_ref()
            .and_then(|s| self.task_for_surface(s))
            .or(self.focused);
        let Some(object) = target else {
            return false;
        };
        let Some(task) = self
            .tasks
            .get(&object)
            .filter(|task| task.accepts_present())
        else {
            return false;
        };
        let Some(route) = task.input_route else {
            return false;
        };
        let surface = if let Some(s) = surface {
            s
        } else if let Some(s) = task.surface() {
            self.tablet_tool_mut(tool_id).surface = Some(s.clone());
            s.clone()
        } else {
            return false;
        };
        let scale = (task.applied_scale_120, FRACTIONAL_SCALE_DENOMINATOR);
        let position = self.surface_position(object, &surface, (tool_x, tool_y));
        let fixed = self.fixed_position(object, position);
        let pen = droidloom_input_protocol::Stylus {
            tool: tool_id,
            action,
            x_fixed: fixed.0,
            y_fixed: fixed.1,
            pressure: if matches!(action, 2 | 4 | 5) {
                0
            } else {
                tool_pressure
            },
            tilt_x: tool_tilt.0,
            tilt_y: tool_tilt.1,
            eraser: tool_eraser,
            buttons: tool_buttons,
        };
        let Ok(timestamp) = monotonic_timestamp_nanos() else {
            return false;
        };
        self.direct_input.set_blocked(self.clipboard.blocked());
        match self.direct_input.send_stylus(route, scale, timestamp, pen) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("Droidloom native pen: {error}");
                self.direct_input.disconnect();
                self.clear_input_tracking();
                false
            }
        }
    }

    pub(super) fn flush_native_pen(&mut self, tool_id: u32) {
        // Smithay 0.7 can swallow wl_touch.cancel when no touch frame is
        // pending. A pen entering the same application is a safe resync point:
        // cancel its current touch contacts before sending stylus input. The
        // cancellation and the pen records share one ordered connection, so
        // the pending pen frame stays valid while they drain.
        if self.tablet_tool_mut(tool_id).pending_down {
            self.text_input.note_touch();
            let surface = self.tablet_tool_mut(tool_id).surface.clone();
            if let Some(surface) = surface
                && let Some(object) = self.task_for_surface(&surface)
            {
                let _ = self.cancel_task_touch_contacts(object);
            }
        }

        let tool = self.tablet_tool_mut(tool_id);
        let down = std::mem::take(&mut tool.pending_down);
        let up = std::mem::take(&mut tool.pending_up);
        let out = std::mem::take(&mut tool.pending_proximity_out);
        let motion = std::mem::take(&mut tool.dirty_motion);
        let buttons = std::mem::take(&mut tool.dirty_buttons);
        if !tool.native_entered {
            if self.send_native_pen(tool_id, 0) {
                self.tablet_tool_mut(tool_id).native_entered = true;
            } else {
                if out {
                    self.tablet_tool_mut(tool_id).surface = None;
                }
                return;
            }
        }
        if down {
            if self.send_native_pen(tool_id, 3) {
                self.tablet_tool_mut(tool_id).is_down = true;
            }
        } else if motion && !up && !out {
            self.send_native_pen(tool_id, 1);
        }
        if buttons {
            self.send_native_pen(tool_id, 6);
        }
        if up {
            self.send_native_pen(tool_id, 4);
            self.tablet_tool_mut(tool_id).is_down = false;
        }
        if out {
            self.send_native_pen(tool_id, 2);
            let tool = self.tablet_tool_mut(tool_id);
            tool.surface = None;
            tool.native_entered = false;
            tool.is_down = false;
            tool.buttons = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::tilt_degrees;

    #[test]
    fn tablet_tilt_is_bounded_before_wire_encoding() {
        assert_eq!(tilt_degrees(30.4), 30);
        assert_eq!(tilt_degrees(-45.8), -46);
        assert_eq!(tilt_degrees(150.0), 90);
        assert_eq!(tilt_degrees(f64::NAN), 0);
    }
}
