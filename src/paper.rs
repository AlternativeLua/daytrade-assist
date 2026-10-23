use crate::candle::Candle;
use crate::strategy::Side;

#[derive(Clone, Copy, Debug)]
pub struct Trade {
    pub side: Side,
    pub entry: f64,
    pub expiry_slot: i64,
    pub stake: f64,
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

/// Fixed-payout, fixed-expiry paper account (the way OTC binary platforms pay).
/// A win returns `stake * payout`, a loss forfeits the stake, a tie refunds it.
pub struct Paper {
    pub start_balance: f64,
    pub balance: f64,
    pub stake: f64,
    pub payout: f64,
    pub open: Option<Trade>,
    pub wins: u32,
    pub losses: u32,
    pub ties: u32,
    peak: f64,
    pub max_drawdown: f64,
}

impl Paper {
    pub fn new(balance: f64, stake: f64, payout: f64) -> Self {
        Self {
            start_balance: balance,
            balance,
            stake,
            payout,
            open: None,
            wins: 0,
            losses: 0,
            ties: 0,
            peak: balance,
            max_drawdown: 0.0,
        }
    }

    /// Opens a trade that expires when `expiry_slot`'s candle closes.
    pub fn open(&mut self, side: Side, entry: f64, expiry_slot: i64) -> bool {
        if self.open.is_some() || self.balance < self.stake {
            return false;
        }
        self.open = Some(Trade {
            side,
            entry,
            expiry_slot,
            stake: self.stake,
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
                trade.stake * self.payout
            }
            Outcome::Loss => {
                self.losses += 1;
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

    #[test]
    fn settles_binary_payouts() {
        let mut p = Paper::new(100.0, 10.0, 0.8);
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
}
