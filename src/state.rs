//! Minimal runtime state for active market context and latest quotes.
//!
//! Inventory is intentionally absent. User-WSS trades belong to `inventory`.

use std::collections::HashMap;

use crate::types::{ConditionId, OutcomeSide, PriceTick, TokenId, TsUs};

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
    pub tick: PriceTick,
    pub ts_us: TsUs,
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
        if !self.trading_active || self.side_for_token(&token).is_none() {
            return false;
        }
        self.quotes.insert(
            token,
            Quote {
                bid,
                ask,
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
