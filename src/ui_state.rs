//! Presentation-only meters/history. Never read or write audio worker state here.
#[derive(Clone, Copy, Debug)]
pub struct Meter {
    pub level_db: f32,
    pub hold_db: f32,
    hold_until: f64,
    last_time: f64,
}
impl Default for Meter {
    fn default() -> Self {
        Self {
            level_db: -60.0,
            hold_db: -60.0,
            hold_until: 0.0,
            last_time: 0.0,
        }
    }
}
impl Meter {
    pub fn update(&mut self, peak: f32, now: f64) {
        let db = if peak.is_finite() && peak > 0.0 {
            (20.0 * peak.log10()).clamp(-60.0, 12.0)
        } else {
            -60.0
        };
        let dt = (now - self.last_time).clamp(0.0, 0.25) as f32;
        self.level_db = db.max(self.level_db - dt * 24.0);
        if db >= self.hold_db {
            self.hold_db = db;
            self.hold_until = now + 1.5;
        } else if now > self.hold_until {
            self.hold_db = db.max(self.hold_db - dt * 12.0);
        }
        self.last_time = now;
    }
}

pub struct History<T> {
    undo: Vec<T>,
    redo: Vec<T>,
}
impl<T> Default for History<T> {
    fn default() -> Self {
        Self {
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }
}
impl<T> History<T> {
    pub fn push(&mut self, value: T) {
        if self.undo.len() == 64 {
            self.undo.remove(0);
        }
        self.undo.push(value);
        self.redo.clear();
    }
    pub fn undo(&mut self, current: T) -> Option<T> {
        let value = self.undo.pop()?;
        self.redo.push(current);
        Some(value)
    }
    pub fn redo(&mut self, current: T) -> Option<T> {
        let value = self.redo.pop()?;
        self.undo.push(current);
        Some(value)
    }
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn meter_holds_peaks_then_decays_without_changing_audio() {
        let mut m = Meter::default();
        m.update(0.5, 1.0);
        let peak = m.hold_db;
        m.update(0.0, 1.1);
        assert_eq!(m.hold_db, peak);
        assert!(m.level_db < peak);
        m.update(0.0, 3.0);
        assert!(m.hold_db < peak);
        m.update(f32::NAN, 3.1);
        assert!(m.level_db.is_finite());
    }
    #[test]
    fn history_is_bounded_and_new_edits_clear_redo() {
        let mut h = History::default();
        for i in 0..100 {
            h.push(i);
        }
        assert_eq!(h.undo.len(), 64);
        assert_eq!(h.undo(100), Some(99));
        assert_eq!(h.redo(99), Some(100));
        assert_eq!(h.undo(100), Some(99));
        h.push(99);
        assert!(!h.can_redo());
    }
}
