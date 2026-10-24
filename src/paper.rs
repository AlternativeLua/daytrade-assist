use crate::candle::Candle;
use crate::strategy::Side;

#[derive(Clone, Copy, Debug)]
pub struct Trade {
    pub side: Side,
    pub entry: f64,
    pub expiry_slot: i64,
    pub stake: f64,
    pub payout: f64,
    /// Martingale step the stake was sized at (0 = base stake).
    pub step: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Win,
    Loss,
    Tie,
}

#[derive(Clone, Copy, Debug)]
pub struct Settlement {
    pub trade: Trade,
    pub exit: f64,
    pub outcome: Outcome,
    pub pnl: f64,
}

/// Stake sizing after losses: multiply the stake by `mult` after each loss, for
/// at most `steps` losses in a row, then go back to the base stake. A win also
/// resets it. `steps` 0 keeps the stake fixed.
#[derive(Clone, Copy, Debug)]
pub struct Martingale {
    pub steps: u32,
    pub mult: f64,
}

/// Fixed-expiry paper account paying the way OTC binary platforms do.
/// A win returns `stake * payout`, a loss forfeits the stake, a tie refunds it.
pub struct Paper {
    pub start_balance: f64,
    pub balance: f64,
    pub base_stake: f64,
    pub payout: f64,
    pub martingale: Martingale,
    pub step: u32,
    pub open: Option<Trade>,
    pub wins: u32,
    pub losses: u32,
    pub ties: u32,
    pub staked: f64,
    pub max_stake: f64,
    peak: f64,
    pub max_drawdown: f64,
}

impl Paper {
    pub fn new(balance: f64, stake: f64, payout: f64, martingale: Martingale) -> Self {
        Self {
            start_balance: balance,
            balance,
            base_stake: stake,
            payout,
            martingale,
            step: 0,
            open: None,
            wins: 0,
            losses: 0,
            ties: 0,
            staked: 0.0,
            max_stake: 0.0,
            peak: balance,
            max_drawdown: 0.0,
        }
    }

    /// Stake the next trade would use.
    pub fn next_stake(&self) -> f64 {
        self.base_stake * self.martingale.mult.powi(self.step as i32)
    }

    pub fn set_martingale(&mut self, m: Martingale) {
        self.martingale = m;
        if self.step > m.steps {
            self.step = 0;
        }
    }

    /// Opens a trade that expires when `expiry_slot`'s candle closes, unless
    /// one is already open or the balance can't cover the stake.
    pub fn open(&mut self, side: Side, entry: f64, expiry_slot: i64) -> bool {
        let stake = self.next_stake();
        if self.open.is_some() || self.balance < stake {
            return false;
        }
        self.staked += stake;
        self.max_stake = self.max_stake.max(stake);
        self.open = Some(Trade {
            side,
            entry,
            expiry_slot,
            stake,
            payout: self.payout,
            step: self.step,
        });
        true
    }

    pub fn settle(&mut self, closed: &Candle) -> Option<Settlement> {
        let trade = self.open.filter(|t| closed.slot >= t.expiry_slot)?;
        self.open = None;
        let exit = closed.close;
        let outcome = if exit == trade.entry {
            Outcome::Tie
        } else if (exit > trade.entry) == (trade.side == Side::Call) {
            Outcome::Win
        } else {
            Outcome::Loss
        };
        let pnl = match outcome {
            Outcome::Win => {
                self.wins += 1;
                self.step = 0;
                trade.stake * trade.payout
            }
            Outcome::Loss => {
                self.losses += 1;
                self.step = if trade.step < self.martingale.steps { trade.step + 1 } else { 0 };
                -trade.stake
            }
            Outcome::Tie => {
                self.ties += 1;
                0.0
            }
        };
        self.balance += pnl;
        self.peak = self.peak.max(self.balance);
        self.max_drawdown = self.max_drawdown.max(self.peak - self.balance);
        Some(Settlement {
            trade,
            exit,
            outcome,
            pnl,
        })
    }

    pub fn decided(&self) -> u32 {
        self.wins + self.losses
    }

    pub fn win_rate(&self) -> f64 {
        if self.decided() == 0 { 0.0 } else { self.wins as f64 / self.decided() as f64 }
    }

    /// Win rate needed to break even at this payout.
    pub fn breakeven(&self) -> f64 {
        1.0 / (1.0 + self.payout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn closing_at(minute: i64, close: f64) -> Candle {
        Candle {
            slot: minute,
            open: close,
            high: close,
            low: close,
            close,
            late: None,
            ticks: 1,
        }
    }

    const FLAT: Martingale = Martingale { steps: 0, mult: 2.0 };

    #[test]
    fn settles_binary_payouts() {
        let mut p = Paper::new(100.0, 10.0, 0.8, FLAT);
        assert!(p.open(Side::Call, 1.0, 5));
        assert!(!p.open(Side::Put, 1.0, 5));
        assert!(p.settle(&closing_at(4, 2.0)).is_none());
        let s = p.settle(&closing_at(5, 1.1)).unwrap();
        assert_eq!(s.outcome, Outcome::Win);
        assert!((p.balance - 108.0).abs() < 1e-9);

        p.open(Side::Call, 1.0, 6);
        assert_eq!(p.settle(&closing_at(6, 0.9)).unwrap().outcome, Outcome::Loss);
        p.open(Side::Put, 1.0, 7);
        assert_eq!(p.settle(&closing_at(7, 1.0)).unwrap().outcome, Outcome::Tie);
        assert!((p.balance - 98.0).abs() < 1e-9);
        assert!((p.max_drawdown - 10.0).abs() < 1e-9);
        assert!((p.breakeven() - 1.0 / 1.8).abs() < 1e-12);
    }

    #[test]
    fn doubles_after_losses_then_resets() {
        let mut p = Paper::new(10_000.0, 10.0, 0.85, Martingale { steps: 4, mult: 2.0 });
        let mut stakes = Vec::new();
        // Six losses in a row: 10, 20, 40, 80, 160, then back to 10.
        for slot in 0..6 {
            p.open(Side::Call, 1.0, slot);
            stakes.push(p.open.unwrap().stake);
            p.settle(&closing_at(slot, 0.9));
        }
        assert_eq!(stakes, vec![10.0, 20.0, 40.0, 80.0, 160.0, 10.0]);
        // A win resets; a tie keeps the step.
        p.open(Side::Call, 1.0, 6);
        p.settle(&closing_at(6, 1.0));
        assert_eq!(p.next_stake(), 20.0);
        p.open(Side::Call, 1.0, 7);
        p.settle(&closing_at(7, 1.1));
        assert_eq!(p.next_stake(), 10.0);
        assert!((p.balance - (10_000.0 - 310.0 - 10.0 + 17.0)).abs() < 1e-9);
        assert_eq!(p.max_stake, 160.0);
    }

    #[test]
    fn trade_keeps_the_payout_it_opened_with() {
        let mut p = Paper::new(100.0, 10.0, 0.9, FLAT);
        p.open(Side::Put, 1.0, 1);
        p.payout = 0.5;
        assert!((p.settle(&closing_at(1, 0.9)).unwrap().pnl - 9.0).abs() < 1e-9);
    }
}
