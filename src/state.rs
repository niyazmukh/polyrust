//! Minimal runtime state for active market context and latest quotes.
//!
//! Inventory is intentionally absent. User-WSS trades belong to `inventory`.

use std::collections::HashMap;

use crate::types::{ConditionId, OutcomeSide, PriceTick, TokenId, TsUs};

const BOOK_DEPTH_LEVELS: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BookLevel {
    pub price: PriceTick,
    pub size_atoms: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BookDepth {
    levels: [Option<BookLevel>; BOOK_DEPTH_LEVELS],
}

impl BookDepth {
    pub const fn empty() -> Self {
        Self {
            levels: [None; BOOK_DEPTH_LEVELS],
        }
    }

    pub fn from_levels(levels: impl IntoIterator<Item = BookLevel>, want_bid: bool) -> Self {
        let mut depth = Self::empty();
        for level in levels {
            if level.size_atoms > 0 {
                depth.insert(level, want_bid);
            }
        }
        depth
    }

    pub(crate) fn best_price(self) -> Option<PriceTick> {
        self.levels[0].map(|level| level.price)
    }

    fn cutoff_for_buy_cents(self, target_cents: i64) -> Option<PriceTick> {
        if target_cents <= 0 {
            return None;
        }
        let target_microcents = i128::from(target_cents) * 1_000_000;
        let mut total = 0i128;
        for level in self.levels.into_iter().flatten() {
            total += i128::from(level.price.ticks()) * i128::from(level.size_atoms);
            if total >= target_microcents {
                return Some(level.price);
            }
        }
        None
    }

    fn insert(&mut self, level: BookLevel, want_bid: bool) {
        if let Some(existing) = self
            .levels
            .iter_mut()
            .flatten()
            .find(|existing| existing.price == level.price)
        {
            existing.size_atoms = existing.size_atoms.saturating_add(level.size_atoms);
            return;
        }
        for idx in 0..BOOK_DEPTH_LEVELS {
            let should_insert = match self.levels[idx] {
                None => true,
                Some(existing) if want_bid => level.price > existing.price,
                Some(existing) => level.price < existing.price,
            };
            if should_insert {
                for shift in ((idx + 1)..BOOK_DEPTH_LEVELS).rev() {
                    self.levels[shift] = self.levels[shift - 1];
                }
                self.levels[idx] = Some(level);
                return;
            }
        }
    }
}

impl Default for BookDepth {
    fn default() -> Self {
        Self::empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarketContext {
    pub slug: String,
    pub condition_id: ConditionId,
    pub yes_token: TokenId,
    pub no_token: TokenId,
    pub end_ts: i64,
    pub slug_ts: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quote {
    pub bid: Option<PriceTick>,
    pub ask: Option<PriceTick>,
    pub ask_depth: BookDepth,
    pub tick: PriceTick,
    pub ts_us: TsUs,
}

impl Quote {
    pub fn buy_cutoff_for_cents(self, target_cents: i64) -> Option<PriceTick> {
        self.ask_depth
            .cutoff_for_buy_cents(target_cents)
            .or(self.ask)
    }
}

pub struct RuntimeState {
    market: Option<MarketContext>,
    quotes: HashMap<TokenId, Quote>,
    trading_active: bool,
}

impl RuntimeState {
    pub fn new() -> Self {
        Self {
            market: None,
            quotes: HashMap::new(),
            trading_active: false,
        }
    }

    pub fn set_market(&mut self, market: MarketContext) {
        self.market = Some(market);
        self.quotes.clear();
        self.trading_active = true;
    }

    pub fn market(&self) -> Option<&MarketContext> {
        self.market.as_ref()
    }

    pub fn trading_active(&self) -> bool {
        self.trading_active
    }

    pub fn mark_market_inactive(&mut self) {
        self.trading_active = false;
        self.quotes.clear();
    }

    pub fn token_for_side(&self, side: OutcomeSide) -> Option<&TokenId> {
        let market = self.market.as_ref()?;
        match side {
            OutcomeSide::Yes => Some(&market.yes_token),
            OutcomeSide::No => Some(&market.no_token),
        }
    }

    pub fn side_for_token(&self, token: &TokenId) -> Option<OutcomeSide> {
        let market = self.market.as_ref()?;
        if token == &market.yes_token {
            Some(OutcomeSide::Yes)
        } else if token == &market.no_token {
            Some(OutcomeSide::No)
        } else {
            None
        }
    }

    pub fn update_quote(
        &mut self,
        token: TokenId,
        bid: Option<PriceTick>,
        ask: Option<PriceTick>,
        tick: PriceTick,
        ts_us: TsUs,
    ) -> bool {
        self.update_quote_with_depth(token, bid, ask, BookDepth::empty(), tick, ts_us)
    }

    pub fn update_quote_with_depth(
        &mut self,
        token: TokenId,
        bid: Option<PriceTick>,
        ask: Option<PriceTick>,
        ask_depth: BookDepth,
        tick: PriceTick,
        ts_us: TsUs,
    ) -> bool {
        if !self.trading_active || self.side_for_token(&token).is_none() {
            return false;
        }
        self.quotes.insert(
            token,
            Quote {
                bid,
                ask,
                ask_depth,
                tick,
                ts_us,
            },
        );
        true
    }

    pub fn quote_for_side(&self, side: OutcomeSide) -> Option<&Quote> {
        let token = self.token_for_side(side)?;
        self.quotes.get(token)
    }

    pub fn quote_for_token(&self, token: &TokenId) -> Option<&Quote> {
        self.side_for_token(token)?;
        self.quotes.get(token)
    }
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self::new()
    }
}
