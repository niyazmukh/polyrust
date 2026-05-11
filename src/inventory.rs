//! WSS-authoritative inventory and UNKNOWN submit matching.
//!
//! This module is intentionally small. It keeps only state that protects a
//! live-risk invariant:
//!
//! * user WSS trades are inventory truth;
//! * duplicate trade lifecycle events must not double-count inventory;
//! * ambiguous HTTP submits must remain matchable by later WSS trades;
//! * BUY exposure checks must include WSS-owned inventory and active
//!   pending/UNKNOWN entry submits.
//!
//! There are no reservations, no SELL balance locks, no cooldowns, and no
//! settled-inventory ledger.

use std::collections::{HashMap, HashSet};

use crate::types::{OrderId, OrderSide, PriceTick, Shares2, SharesAtoms, TokenId, TradeId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitIntent {
    Entry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitStatus {
    Pending,
    Accepted,
    Unknown,
    ExpiredUnknown,
}

impl SubmitStatus {
    fn blocks_entry(self) -> bool {
        matches!(self, SubmitStatus::Pending | SubmitStatus::Accepted | SubmitStatus::Unknown)
    }

    fn matchable(self) -> bool {
        matches!(
            self,
            SubmitStatus::Pending
                | SubmitStatus::Accepted
                | SubmitStatus::Unknown
                | SubmitStatus::ExpiredUnknown
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SubmitId(String);

impl SubmitId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingSubmit {
    pub id: SubmitId,
    pub intent: SubmitIntent,
    pub token: TokenId,
    pub side: OrderSide,
    pub size_atoms: SharesAtoms,
    pub order_id: Option<OrderId>,
    pub status: SubmitStatus,
    pub created_ts_us: i64,
    pub updated_ts_us: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TradeStatus {
    Matched,
    Mined,
    Confirmed,
    Failed,
    Retrying,
    Other,
}

impl TradeStatus {
    pub fn from_venue(s: &str) -> Self {
        match s.trim().to_ascii_uppercase().as_str() {
            "MATCHED" => Self::Matched,
            "MINED" => Self::Mined,
            "CONFIRMED" => Self::Confirmed,
            "FAILED" => Self::Failed,
            "RETRYING" => Self::Retrying,
            _ => Self::Other,
        }
    }

    fn inventory_applying(self) -> bool {
        matches!(self, TradeStatus::Matched | TradeStatus::Confirmed)
    }

    fn terminal(self) -> bool {
        matches!(self, TradeStatus::Confirmed | TradeStatus::Failed)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserTrade {
    pub trade_id: TradeId,
    pub token: TokenId,
    pub taker_order_id: Option<OrderId>,
    pub side: OrderSide,
    pub size_atoms: SharesAtoms,
    pub price: PriceTick,
    pub status: TradeStatus,
    pub ts_us: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TradeState {
    pub trade_id: TradeId,
    pub token: TokenId,
    pub side: OrderSide,
    pub status: TradeStatus,
    pub lifecycle: Vec<TradeStatus>,
    pub applied: bool,
    pub finalized: bool,
    pub matched_submit: Option<SubmitId>,
}

#[derive(Clone, Debug)]
struct TradeRecord {
    token: TokenId,
    side: OrderSide,
    size_atoms: SharesAtoms,
    lifecycle: Vec<TradeStatus>,
    applied: bool,
    finalized: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PositionView {
    pub owned_atoms: SharesAtoms,
    pub sellable: Shares2,
}

#[derive(Clone, Debug, Default)]
pub struct Inventory {
    user_wss_trusted: bool,
    next_submit_id: u64,
    owned_by_token: HashMap<TokenId, i64>,
    trades: HashMap<TradeId, TradeRecord>,
    pending: HashMap<SubmitId, PendingSubmit>,
}

impl Inventory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_user_wss_trusted(&mut self, trusted: bool) {
        self.user_wss_trusted = trusted;
    }

    pub fn is_user_wss_trusted(&self) -> bool {
        self.user_wss_trusted
    }

    pub fn register_submit(
        &mut self,
        intent: SubmitIntent,
        token: TokenId,
        side: OrderSide,
        size_atoms: SharesAtoms,
        now_ts_us: i64,
    ) -> SubmitId {
        self.next_submit_id = self.next_submit_id.saturating_add(1);
        let id = SubmitId(format!("s{}", self.next_submit_id));
        let pending = PendingSubmit {
            id: id.clone(),
            intent,
            token,
            side,
            size_atoms,
            order_id: None,
            status: SubmitStatus::Pending,
            created_ts_us: now_ts_us,
            updated_ts_us: now_ts_us,
        };
        self.pending.insert(id.clone(), pending);
        id
    }

    pub fn mark_submit_accepted(&mut self, id: &SubmitId, order_id: OrderId, now_ts_us: i64) {
        if let Some(p) = self.pending.get_mut(id) {
            p.order_id = Some(order_id);
            p.status = SubmitStatus::Accepted;
            p.updated_ts_us = now_ts_us;
        }
    }

    pub fn mark_submit_unknown(&mut self, id: &SubmitId, now_ts_us: i64) {
        if let Some(p) = self.pending.get_mut(id) {
            p.status = SubmitStatus::Unknown;
            p.updated_ts_us = now_ts_us;
        }
    }

    /// Synchronous BUY claim: register a Pending entry submit before the
    /// async spawn that does signing + HTTP. This closes the duplicate
    /// race where intent is produced under lock but `register_submit`
    /// happened inside the spawned task after lock release.
    pub fn claim_entry(
        &mut self,
        token: TokenId,
        side: OrderSide,
        size_atoms: SharesAtoms,
        now_ts_us: i64,
    ) -> SubmitId {
        self.register_submit(SubmitIntent::Entry, token, side, size_atoms, now_ts_us)
    }

    /// Release a claim on rejection/failure before HTTP submit. Removes
    /// the pending entry so it no longer blocks same-token BUY.
    pub fn release_claim(&mut self, id: &SubmitId) {
        self.pending.remove(id);
    }

    pub fn expire_unknowns(&mut self, older_than_ts_us: i64) {
        for p in self.pending.values_mut() {
            if p.status == SubmitStatus::Unknown && p.updated_ts_us <= older_than_ts_us {
                p.status = SubmitStatus::ExpiredUnknown;
            }
        }
    }

    pub fn apply_user_trade(&mut self, trade: UserTrade) -> TradeState {
        let matched_submit = self.match_pending_submit(&trade);

        // WSS is now authoritative — remove matched Entry pending submits.
        // They have served their purpose (blocking duplicate BUYs between
        // claim and WSS confirmation). Inventory is truth.
        if let Some(ref id) = matched_submit
            && self.pending.get(id).is_some_and(|p| p.intent == SubmitIntent::Entry)
        {
            self.pending.remove(id);
        }

        let record = self
            .trades
            .entry(trade.trade_id.clone())
            .or_insert_with(|| TradeRecord {
                token: trade.token.clone(),
                side: trade.side,
                size_atoms: trade.size_atoms,
                lifecycle: Vec::with_capacity(3),
                applied: false,
                finalized: false,
            });

        if !record.lifecycle.contains(&trade.status) {
            record.lifecycle.push(trade.status);
        }

        if trade.status.inventory_applying() && !record.applied {
            apply_inventory_delta(
                &mut self.owned_by_token,
                &record.token,
                record.side,
                record.size_atoms,
            );
            record.applied = true;
        }
        if trade.status.terminal() {
            record.finalized = true;
        }

        TradeState {
            trade_id: trade.trade_id,
            token: record.token.clone(),
            side: record.side,
            status: trade.status,
            lifecycle: record.lifecycle.clone(),
            applied: record.applied,
            finalized: record.finalized,
            matched_submit,
        }
    }

    pub fn sellable(&self, token: &TokenId) -> Shares2 {
        let atoms = self.owned_by_token.get(token).copied().unwrap_or(0).max(0);
        // 0.01 share = 10_000 atoms.
        Shares2::new_unchecked(atoms / 10_000)
    }

    pub fn owned_atoms(&self, token: &TokenId) -> SharesAtoms {
        SharesAtoms(self.owned_by_token.get(token).copied().unwrap_or(0).max(0))
    }

    pub fn position(&self, token: &TokenId) -> Option<PositionView> {
        let owned = self.owned_atoms(token);
        if owned.atoms() <= 0 {
            return None;
        }
        Some(PositionView {
            owned_atoms: owned,
            sellable: self.sellable(token),
        })
    }

    pub fn has_entry_exposure_or_pending(&self, token: &TokenId) -> bool {
        if !self.user_wss_trusted {
            return true;
        }
        self.owned_atoms(token).atoms() > 0
            || self.pending.values().any(|p| {
                p.intent == SubmitIntent::Entry
                    && p.token == *token
                    && p.status.blocks_entry()
            })
    }

    pub fn open_position_count<'a>(&self, scope: impl IntoIterator<Item = &'a TokenId>) -> usize {
        let scope: HashSet<&TokenId> = scope.into_iter().collect();
        self.owned_by_token
            .iter()
            .filter(|(token, atoms)| **atoms > 0 && (scope.is_empty() || scope.contains(token)))
            .map(|(token, _)| token)
            .chain(
                self.pending
                    .values()
                    .filter(|p| {
                        p.intent == SubmitIntent::Entry
                            && p.status.blocks_entry()
                            && (scope.is_empty() || scope.contains(&p.token))
                    })
                    .map(|p| &p.token),
            )
            .collect::<HashSet<_>>()
            .len()
    }

    pub fn release_market_scope<'a>(&mut self, active_tokens: impl IntoIterator<Item = &'a TokenId>) {
        let active: HashSet<TokenId> = active_tokens.into_iter().cloned().collect();
        self.owned_by_token.retain(|token, _| active.contains(token));
        self.pending.retain(|_, p| active.contains(&p.token));
    }

    pub fn pending(&self, id: &SubmitId) -> Option<&PendingSubmit> {
        self.pending.get(id)
    }

    fn match_pending_submit(&mut self, trade: &UserTrade) -> Option<SubmitId> {
        let by_order = trade.taker_order_id.as_ref().and_then(|oid| {
            self.pending
                .iter()
                .find(|(_, p)| p.order_id.as_ref() == Some(oid) && p.status.matchable())
                .map(|(id, _)| id.clone())
        });
        let id = by_order.or_else(|| {
            self.pending
                .iter()
                .find(|(_, p)| {
                    p.status.matchable()
                        && p.token == trade.token
                        && p.side == trade.side
                })
                .map(|(id, _)| id.clone())
        });
        if let Some(id) = id.as_ref()
            && let Some(p) = self.pending.get_mut(id)
        {
            p.status = SubmitStatus::Accepted;
            if p.order_id.is_none() {
                p.order_id = trade.taker_order_id.clone();
            }
            p.updated_ts_us = trade.ts_us;
        }
        id
    }
}

fn apply_inventory_delta(
    owned: &mut HashMap<TokenId, i64>,
    token: &TokenId,
    side: OrderSide,
    size: SharesAtoms,
) {
    let entry = owned.entry(token.clone()).or_insert(0);
    match side {
        OrderSide::Buy => *entry = entry.saturating_add(size.atoms()),
        OrderSide::Sell => *entry = entry.saturating_sub(size.atoms()).max(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(n: &str) -> TokenId {
        TokenId::new(n)
    }

    fn order(n: &str) -> OrderId {
        OrderId::new(n)
    }

    fn trade(n: &str, token: TokenId, side: OrderSide, status: TradeStatus) -> UserTrade {
        UserTrade {
            trade_id: TradeId::new(n),
            token,
            taker_order_id: Some(order("0xorder")),
            side,
            size_atoms: SharesAtoms(1_416_664),
            price: PriceTick::checked(59).unwrap(),
            status,
            ts_us: 100,
        }
    }

    #[test]
    fn matched_applies_inventory_once_and_confirmed_only_finalizes() {
        let mut inv = Inventory::new();
        let t = token("asset");
        let first = inv.apply_user_trade(trade("tr1", t.clone(), OrderSide::Buy, TradeStatus::Matched));
        assert!(first.applied);
        assert_eq!(inv.owned_atoms(&t), SharesAtoms(1_416_664));
        assert_eq!(inv.sellable(&t), Shares2::new_unchecked(141));

        let confirmed =
            inv.apply_user_trade(trade("tr1", t.clone(), OrderSide::Buy, TradeStatus::Confirmed));
        assert!(confirmed.finalized);
        assert_eq!(inv.owned_atoms(&t), SharesAtoms(1_416_664));
        assert_eq!(confirmed.lifecycle, vec![TradeStatus::Matched, TradeStatus::Confirmed]);
    }

    #[test]
    fn confirmed_without_matched_still_recovers_inventory_once() {
        let mut inv = Inventory::new();
        let t = token("asset");
        let state = inv.apply_user_trade(trade("tr1", t.clone(), OrderSide::Buy, TradeStatus::Confirmed));
        assert!(state.applied);
        assert!(state.finalized);
        assert_eq!(inv.owned_atoms(&t), SharesAtoms(1_416_664));
    }

    #[test]
    fn sell_reduces_inventory_and_clamps_underflow() {
        let mut inv = Inventory::new();
        let t = token("asset");
        inv.apply_user_trade(trade("buy", t.clone(), OrderSide::Buy, TradeStatus::Matched));
        let sell = UserTrade {
            trade_id: TradeId::new("sell"),
            token: t.clone(),
            taker_order_id: Some(order("0xsell")),
            side: OrderSide::Sell,
            size_atoms: SharesAtoms(2_000_000),
            price: PriceTick::checked(60).unwrap(),
            status: TradeStatus::Matched,
            ts_us: 200,
        };
        inv.apply_user_trade(sell);
        assert_eq!(inv.owned_atoms(&t), SharesAtoms(0));
        assert_eq!(inv.sellable(&t), Shares2::new_unchecked(0));
    }

    #[test]
    fn unknown_submit_matches_late_wss_trade_and_blocks_until_expired() {
        let mut inv = Inventory::new();
        inv.set_user_wss_trusted(true);
        let t = token("asset");
        let id = inv.register_submit(
            SubmitIntent::Entry,
            t.clone(),
            OrderSide::Buy,
            SharesAtoms(1_000_000),
            10,
        );
        inv.mark_submit_unknown(&id, 20);
        assert!(inv.has_entry_exposure_or_pending(&t));
        inv.expire_unknowns(20);
        assert!(!inv.has_entry_exposure_or_pending(&t));

        let state = inv.apply_user_trade(UserTrade {
            trade_id: TradeId::new("late"),
            token: t.clone(),
            taker_order_id: None,
            side: OrderSide::Buy,
            size_atoms: SharesAtoms(1_000_000),
            price: PriceTick::checked(50).unwrap(),
            status: TradeStatus::Matched,
            ts_us: 30,
        });
        assert_eq!(state.matched_submit, Some(id.clone()));
        assert_eq!(inv.pending(&id), None); // Entry pending removed — WSS is authority
        assert_eq!(inv.owned_atoms(&t), SharesAtoms(1_000_000));
    }

    #[test]
    fn release_market_scope_drops_old_tokens() {
        let mut inv = Inventory::new();
        let old = token("old");
        let new = token("new");
        inv.apply_user_trade(trade("old-trade", old.clone(), OrderSide::Buy, TradeStatus::Matched));
        inv.register_submit(
            SubmitIntent::Entry,
            old.clone(),
            OrderSide::Buy,
            SharesAtoms(1_000_000),
            1,
        );
        inv.release_market_scope([&new]);
        assert_eq!(inv.owned_atoms(&old), SharesAtoms(0));
        assert_eq!(inv.open_position_count([&old, &new]), 0);
    }
}
