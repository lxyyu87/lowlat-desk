/// Lowers the target bitrate when loss or RTT rises, and creeps back up when
/// the path stays quiet.
#[derive(Debug, Clone)]
pub struct BitrateControl {
    target: u32,
    min: u32,
    max: u32,
    quiet: u32,
}

impl BitrateControl {
    pub fn new(max_bps: u32) -> Self {
        let max = max_bps.max(200_000);
        Self {
            target: max,
            min: 200_000,
            max,
            quiet: 0,
        }
    }

    pub fn target(&self) -> u32 {
        self.target
    }

    /// `loss_permille` is lost packets per 1000. Returns the new target.
    pub fn observe(&mut self, loss_permille: u32, rtt_ms: u32) -> u32 {
        let congested = loss_permille >= 30 || rtt_ms >= 200;
        if congested {
            self.target = (u64::from(self.target) * 7 / 10).max(u64::from(self.min)) as u32;
            self.quiet = 0;
        } else if loss_permille < 10 && rtt_ms < 120 {
            self.quiet = self.quiet.saturating_add(1);
            if self.quiet >= 3 {
                self.target = (u64::from(self.target) * 11 / 10).min(u64::from(self.max)) as u32;
                self.quiet = 0;
            }
        } else {
            self.quiet = 0;
        }
        self.target
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loss_drops_bitrate_and_a_quiet_path_recovers() {
        let mut control = BitrateControl::new(2_000_000);
        let dropped = control.observe(80, 40);
        assert!(dropped < 2_000_000);
        assert!(control.observe(0, 30) < 2_000_000);
        control.observe(0, 30);
        let recovered = control.observe(0, 30);
        assert!(recovered > dropped);
    }
}
