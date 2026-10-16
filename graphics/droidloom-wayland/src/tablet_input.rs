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
        let Some(tool) = self
            .tablet_tools
            .iter()
            .find(|tool| tool.tool_id == tool_id)
        else {
            return false;
        };
        let Some(surface) = tool.surface.as_ref() else {
            return false;
        };
        let Some(object) = self.task_for_surface(surface) else {
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
        let scale = (task.applied_scale_120, FRACTIONAL_SCALE_DENOMINATOR);
        let position = self.surface_position(object, surface, (tool.x, tool.y));
        let fixed = self.fixed_position(object, position);
        let pen = droidloom_input_protocol::Stylus {
            tool: tool_id,
            action,
            x_fixed: fixed.0,
            y_fixed: fixed.1,
            pressure: if matches!(action, 2 | 4 | 5) {
                0
            } else {
                tool.pressure
            },
            tilt_x: tool.tilt.0,
            tilt_y: tool.tilt.1,
            eraser: tool.eraser,
            buttons: tool.buttons,
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
