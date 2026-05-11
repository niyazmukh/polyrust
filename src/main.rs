// Delivered by DeepSeek.
//! Shadow-probe and live-trading binary. Wires all three WebSocket feeds
//! into RuntimeCore with anchor strike resolution, Gamma market discovery,
//! and gated live order submission.
//!
//! Shadow mode:  set `MINIMAL_DRY_RUN_ORDERS=true`.  Signals are logged;
//!               no orders reach the venue.
//! Live mode:    set `POLY_ALLOW_LIVE_ORDERS=true`.  BUY intent decisions
//!               are signed and submitted to the CLOB via `POST /order`.
//!               Requires POLY_PK + POLY_API_KEY / POLY_API_SECRET /
//!               POLY_PASSPHRASE / POLY_ADDRESS.
//!
//! Traces to:
//!   shadow_signal_probe.py:main          (shadow probe wiring)
//!   minimal_live_bot.py:run_supervised    (multi-feed orchestration)
//!   bot_orchestrator.py:on_binance_tick   (buy submit fire)

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use minirust::anchor::AnchorBuffer;
use minirust::auth::L2AuthSigner;
use minirust::config::{self, Config, LaunchConfig};
use minirust::feed;
use minirust::gamma::GammaClient;
use minirust::logline::{self, Field, Level};
use minirust::runtime::{self, RuntimeCore};
use minirust::signing::{
    OrderSigner, SignInputs, EXCHANGE_V2_NORMAL, POLYGON_CHAIN_ID,
};
use minirust::submit::HttpSubmitter;
use minirust::types::TsUs;
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    // Start the non-blocking background logger immediately.
    logline::init_background_logger();

    // 1. Load .env.poly (idempotent — only sets missing vars).
    let _ = config::load_env_file(".env.poly");

    // 2. Parse configuration.
    let cfg = Config::from_env().expect("FATAL: invalid config");

    // Safety gate: refuse to start without explicit mode.
    if !cfg.dry_run_orders && !cfg.allow_live_orders {
        eprintln!(
            "FATAL: set MINIMAL_DRY_RUN_ORDERS=true for shadow mode, \
             or POLY_ALLOW_LIVE_ORDERS=true for live trading"
        );
        std::process::exit(2);
    }

    let log_level = std::env::var("MINIRUST_LOG_LEVEL")
        .ok()
        .and_then(|s| match s.to_ascii_uppercase().as_str() {
            "DEBUG" => Some(Level::Debug),
            "INFO" => Some(Level::Info),
            "WARNING" => Some(Level::Warn),
            "ERROR" => Some(Level::Error),
            _ => None,
        })
        .unwrap_or(Level::Warn);
    logline::set_level(log_level);

    let launch = LaunchConfig::from_env().expect("FATAL: invalid launch config");

    logline::log_event(
        Level::Warn,
        "minirust_start",
        &[
            Field { key: "dry_run", value: &cfg.dry_run_orders },
            Field { key: "live", value: &cfg.allow_live_orders },
            Field { key: "slug_fmt", value: &launch.market_slug_fmt.as_str() },
            Field { key: "binance_url", value: &launch.binance_ws_url.as_str() },
        ],
    );

    // 3. Build shared runtime state.
    let core = Arc::new(Mutex::new(
        RuntimeCore::new(&cfg).expect("FATAL: failed to build runtime core"),
    ));
    let anchor = Arc::new(Mutex::new(AnchorBuffer::new()));

    // 4. Build Gamma client for dynamic market discovery.
    let gamma = GammaClient::new(
        &launch.clob_url,
        &launch.gamma_url,
        &launch.market_slug_fmt,
        launch.market_window_s,
    );

    // 5. User WS credentials.
    let poly_api_key = std::env::var("POLY_API_KEY").unwrap_or_default();
    let poly_api_secret = std::env::var("POLY_API_SECRET").unwrap_or_default();
    let poly_passphrase = std::env::var("POLY_PASSPHRASE").unwrap_or_default();
    let poly_address = std::env::var("POLY_ADDRESS").unwrap_or_default();
    let poly_pk = std::env::var("POLY_PK").unwrap_or_default();

    // 6. Live-submit infrastructure (only built when POLY_ALLOW_LIVE_ORDERS=true).
    //    Shadow mode skips both — no signer, no HTTP submitter.
    let auth_signer: Option<L2AuthSigner> = if cfg.allow_live_orders {
        L2AuthSigner::new(&poly_api_key, &poly_passphrase, &poly_api_secret, &poly_address).ok()
    } else {
        None
    };

    let submitter: Option<HttpSubmitter> = auth_signer
        .as_ref()
        .and_then(|auth| HttpSubmitter::new(&launch.clob_url, auth.clone()).ok());

    let order_signer: Option<OrderSigner> = if cfg.allow_live_orders && !poly_pk.is_empty() {
        OrderSigner::new(
            &poly_pk,
            &poly_api_key,
            launch.poly_funder.as_deref().and_then(|f| f.parse().ok()),
            launch.poly_signature_kind,
            POLYGON_CHAIN_ID,
            EXCHANGE_V2_NORMAL,
        )
        .ok()
    } else {
        None
    };

    if cfg.allow_live_orders {
        if submitter.is_none() {
            eprintln!(
                "FATAL: live mode requires valid L2 auth \
                 (POLY_API_KEY / POLY_API_SECRET / POLY_PASSPHRASE / POLY_ADDRESS)"
            );
            std::process::exit(2);
        }
        if order_signer.is_none() {
            eprintln!("FATAL: live mode requires valid OrderSigner (POLY_PK)");
            std::process::exit(2);
        }
        if poly_api_key.is_empty() || poly_api_secret.is_empty() || poly_passphrase.is_empty() || poly_address.is_empty() {
            eprintln!(
                "FATAL: live mode requires POLY_API_KEY, POLY_API_SECRET, POLY_PASSPHRASE, and POLY_ADDRESS for user WSS and L2 Auth"
            );
            std::process::exit(2);
        }
        if let Some(sub) = &submitter
            && let Err(e) = sub.verify_flat_start().await
        {
            logline::log_event(
                Level::Error,
                "ensure_flat_start_failed",
                &[Field { key: "reason", value: &e }],
            );
            std::process::exit(3);
        }
    }

    // 7. Signal ID counter for log correlation.
    let signal_id = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let sell_id = Arc::new(std::sync::atomic::AtomicU64::new(0));

    // 8. Market command channel for incremental WS subscribe on rotation.
    //    Traces to: market_ws.py:293-302 (incremental subscribe).
    //
    // Lock ordering invariant: when both `anchor` and `core` mutexes must be
    // held simultaneously, ALWAYS acquire `anchor` first, then `core`.
    // This order is observed in: binance callback, maint_task rotation.
    // Reversing it (core→anchor) will deadlock against these paths.
    let (market_tx, market_rx) = mpsc::unbounded_channel::<String>();

    logline::log_event(Level::Warn, "minirust_initialized", &[]);

    // ==================================================================
    // Spawn feed tasks
    // ==================================================================

    // --- Market WS task ---
    let market_task = {
        let core = core.clone();
        let market_url = launch.poly_market_ws_url.clone();
        tokio::spawn(async move {
            feed::market_feed_loop(
                &market_url,
                {
                    let core = core.clone();
                    move || {
                        core.lock()
                            .ok()
                            .and_then(|mut c| c.state_mut().market().cloned())
                            .map(|m| vec![
                                m.yes_token.as_str().to_owned(),
                                m.no_token.as_str().to_owned(),
                            ])
                            .unwrap_or_default()
                    }
                },
                move |raw| {
                    if let Ok(mut c) = core.lock() {
                        let ts = TsUs(now_us());
                        let _ = c.apply_market_raw(&raw, ts);
                    }
                },
                || {},
                market_rx,
            )
            .await;
        })
    };

    // --- Binance WS task ---
    // Clone submit infrastructure before moving into the spawned task.
    let binance_sub = submitter.clone();
    let binance_sign = order_signer.clone();
    let binance_task = {
        let core = core.clone();
        let anchor = anchor.clone();
        let binance_url = launch.binance_ws_url.clone();
        let dry_run = cfg.dry_run_orders;
        let sig_id = signal_id.clone();
        let sub = binance_sub;
        let sign = binance_sign;
        tokio::spawn(async move {
            feed::binance_feed_loop(&binance_url, move |raw| {
                let ts = TsUs(now_us());
                // Parse the book-ticker frame.
                let ticker = match minirust::binance::parse_book_ticker(&raw) {
                    Ok(Some(t)) => t,
                    _ => return,
                };
                let sample = match ticker.sample() {
                    Some(s) => s,
                    None => return,
                };

                // Push to anchor buffer and try resolve.
                // Lock order: anchor → core (see invariant at §8).
                {
                    let mut a = anchor.lock().unwrap();
                    a.push(
                        sample.ts_us.micros(),
                        sample.bid,
                        sample.ask,
                        sample.bid_qty,
                        sample.ask_qty,
                    );
                    if let minirust::anchor::AnchorResult::Resolved(strike) =
                        a.try_resolve(ts.micros())
                        && let Ok(mut c) = core.lock()
                    {
                        c.signal_mut().set_strike(strike, true);
                        logline::log_event(
                            Level::Warn,
                            "shadow_anchor_resolved",
                            &[Field { key: "strike", value: &strike }],
                        );
                    }
                }

                // Try signal decision.
                let intent = {
                    let mut c = match core.lock() {
                        Ok(c) => c,
                        Err(_) => return,
                    };

                    // Compute TTE from the current market.
                    let market = match c.state_mut().market() {
                        Some(m) => m.clone(),
                        None => {
                            // No market set yet — push sample and move on.
                            c.signal_mut().push(sample);
                            return;
                        }
                    };
                    let tte_us = market.end_ts.saturating_mul(1_000_000)
                        .saturating_sub(ts.micros());

                    match c.on_binance_sample(sample, ts, tte_us) {
                        Ok(Some(intent)) => Some(intent),
                        _ => None,
                    }
                };

                // Log the buy signal and optionally submit.
                if let Some(intent) = intent {
                    let id = sig_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if dry_run {
                        logline::log_event(
                            Level::Warn,
                            "shadow_buy_signal",
                            &[
                                Field { key: "signal_id", value: &(id as i64) },
                                Field { key: "side", value: &intent.side.as_str() },
                                Field { key: "token_id", value: &intent.token.as_str() },
                                Field { key: "limit_ticks", value: &intent.limit.ticks() },
                                Field { key: "edge_ticks", value: &intent.edge_ticks },
                            ],
                        );
                    } else {
                        // Live mode: fire-and-forget submit via spawn.
                        // P0-2 fix: claim entry BEFORE spawn under the same
                        // lock scope that will produce the intent check, so a
                        // second Binance tick cannot pass
                        // has_entry_exposure_or_pending between lock release
                        // and the old register_submit inside the spawn.
                        if let (Some(sub), Some(sign)) = (sub.as_ref(), sign.as_ref()) {
                            let ts_us = ts.micros();
                            let (claim_id, policy) = {
                                let mut c = match core.lock() {
                                    Ok(c) => c,
                                    Err(_) => return,
                                };
                                let policy = c.buy_submit_policy();
                                // Use SharesAtoms(1) as placeholder — claim
                                // blocks by token+Entry, not by size.
                                let claim = c.inventory_mut().claim_entry(
                                    intent.token.clone(),
                                    minirust::types::OrderSide::Buy,
                                    minirust::types::SharesAtoms(1),
                                    ts_us,
                                );
                                (claim, policy)
                            };
                            let sub = sub.clone();
                            let sign = sign.clone();
                            let core2 = core.clone();
                            let intent2 = intent.clone();
                            let claim_id2 = claim_id.clone();
                            tokio::spawn(async move {
                                let prepared = match runtime::prepare_buy_submit(
                                    &intent2,
                                    policy,
                                    &sign,
                                    SignInputs { salt: id, timestamp_ms: now_ms() },
                                    claim_id2,
                                ) {
                                    Ok(p) => p,
                                    Err(e) => {
                                        logline::log_event(
                                            Level::Error,
                                            "buy_prepare_failed",
                                            &[Field { key: "error", value: &e.to_string().as_str() }],
                                        );
                                        // Release the claim so this token is
                                        // unblocked for future BUYs.
                                        if let Ok(mut c) = core2.lock() {
                                            c.inventory_mut().release_claim(&claim_id);
                                        }
                                        return;
                                    }
                                };

                                let outcome = sub.submit_order(&prepared.body).await;

                                {
                                    let mut c = match core2.lock() {
                                        Ok(c) => c,
                                        Err(_) => return,
                                    };
                                    runtime::record_buy_submit_outcome(
                                        c.inventory_mut(),
                                        &prepared.submit_id,
                                        &outcome,
                                        now_us(),
                                    );
                                }

                                let accepted = outcome.is_accepted();
                                logline::log_event(
                                    Level::Warn,
                                    "buy_submit_outcome",
                                    &[
                                        Field { key: "signal_id", value: &(id as i64) },
                                        Field { key: "side", value: &intent2.side.as_str() },
                                        Field { key: "token_id", value: &intent2.token.as_str() },
                                        Field { key: "accepted", value: &accepted },
                                        Field { key: "http_status", value: &(outcome.http_status() as i64) },
                                    ],
                                );
                            });
                        }
                    }
                }
            })
            .await;
        })
    };

    // --- User WS task ---
    // Trade events own inventory (WSS authority). On BUY fill confirmation,
    // immediately check for sellable inventory and fire SELL at bid.
    // Traces to: minimal_live_bot.py user event → tracker.on_trade_event
    //   → exit evaluation.
    let user_task = {
        let core = core.clone();
        let user_url = launch.poly_user_ws_url.clone();
        let api_key = poly_api_key.clone();
        let api_secret = poly_api_secret.clone();
        let passphrase = poly_passphrase.clone();
        let sub = submitter.clone();
        let sign = order_signer.clone();
        let sid = sell_id.clone();
        tokio::spawn(async move {
            if api_key.is_empty() || api_secret.is_empty() {
                logline::log_event(
                    Level::Warn,
                    "user_feed_skipped",
                    &[Field { key: "reason", value: &"missing POLY_API_KEY or POLY_API_SECRET" }],
                );
                return;
            }
            feed::user_feed_loop(
                &user_url,
                &api_key,
                &api_secret,
                &passphrase,
                move |raw| {
                    let sell_target = {
                        let mut c = match core.lock() {
                            Ok(c) => c,
                            Err(_) => return,
                        };
                        let ts = now_us();
                        match c.apply_user_raw(&raw, ts) {
                            Ok(minirust::user::UserMessage::AuthSuccess) => {
                                c.inventory_mut().set_user_wss_trusted(true);
                                logline::log_event(Level::Info, "user_wss_auth_success", &[]);
                            }
                            Ok(minirust::user::UserMessage::AuthError(msg)) => {
                                c.inventory_mut().set_user_wss_trusted(false);
                                logline::log_event(Level::Error, "user_wss_auth_error", &[Field { key: "msg", value: &msg }]);
                            }
                            Err(e) => {
                                let err_msg = format!("{e:?}");
                                c.inventory_mut().set_user_wss_trusted(false);
                                logline::log_event(Level::Error, "user_wss_parse_failed", &[Field { key: "err", value: &err_msg }]);
                            }
                            Ok(_) => {}
                        }

                        // Check sellable inventory after trade update.
                        // WSS authority: trade events own inventory.
                        // If a BUY just filled, sell immediately at bid.
                        let tokens: Vec<_> = c
                            .state_mut()
                            .market()
                            .map(|m| vec![m.yes_token.clone(), m.no_token.clone()])
                            .unwrap_or_default();
                        tokens
                            .into_iter()
                            .find_map(|token| {
                                let pos = c.inventory_mut().position(&token)?;
                                if pos.sellable.units() > 0 {
                                    Some(token)
                                } else {
                                    None
                                }
                            })
                    };

                    // Fire sell outside the lock.
                    if let (Some(sub), Some(sign), Some(token)) =
                        (sub.as_ref(), sign.as_ref(), sell_target)
                    {
                        let sub2 = sub.clone();
                        let sign2 = sign.clone();
                        let core2 = core.clone();
                        let sid_val =
                            sid.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        tokio::spawn(async move {
                            let prepared = {
                                let mut c = match core2.lock() {
                                    Ok(c) => c,
                                    Err(_) => return,
                                };
                                match c.prepare_sell_at_bid(
                                    &token,
                                    &sign2,
                                    SignInputs {
                                        salt: sid_val,
                                        timestamp_ms: now_ms(),
                                    },
                                    now_us(),
                                ) {
                                    Ok(Some(p)) => p,
                                    _ => return,
                                }
                            };
                            let outcome = sub2.submit_order(&prepared.body).await;
                            {
                                let mut c = match core2.lock() {
                                    Ok(c) => c,
                                    Err(_) => return,
                                };
                                runtime::record_sell_submit_outcome(
                                    c.inventory_mut(),
                                    &prepared.submit_id,
                                    &outcome,
                                    now_us(),
                                );
                            }
                            logline::log_event(
                                Level::Warn,
                                "sell_trade_trigger",
                                &[
                                    Field { key: "token_id", value: &token.as_str() },
                                    Field {
                                        key: "accepted",
                                        value: &outcome.is_accepted(),
                                    },
                                ],
                            );
                        });
                    }
                },
            )
            .await;
        })
    };

    // --- Exit task: periodic SELL at executable bid, no gating ---
    // Traces to: minimal_live_bot.py:309-312 (_exit_loop),
    //   bot_orchestrator.py:464-538 (evaluate_exit).
    // Principle: SELL is not locally over-gated. FAK rejection is cheap.
    // No reservations, balance locks, cooldowns, or in-flight blockers.
    let exit_task = {
        let core = core.clone();
        let sub = submitter.clone();
        let sign = order_signer.clone();
        let sid = sell_id.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(
                std::time::Duration::from_micros(50_000),
            );
            tick.tick().await;
            loop {
                tick.tick().await;
                let sub = match sub.as_ref() {
                    Some(s) => s.clone(),
                    None => continue,
                };
                let sign = match sign.as_ref() {
                    Some(s) => s.clone(),
                    None => continue,
                };

                // Collect sellable positions under lock.
                let sells: Vec<_> = {
                    let mut c = match core.lock() {
                        Ok(c) => c,
                        Err(_) => continue,
                    };
                    let tokens: Vec<_> = c
                        .state_mut()
                        .market()
                        .map(|m| vec![m.yes_token.clone(), m.no_token.clone()])
                        .unwrap_or_default();
                    tokens
                        .into_iter()
                        .filter_map(|token| {
                            let pos = c.inventory_mut().position(&token)?;
                            if pos.sellable.units() <= 0 {
                                return None;
                            }
                            let prepared = c.prepare_sell_at_bid(
                                &token,
                                &sign,
                                SignInputs {
                                    salt: sid.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                                    timestamp_ms: now_ms(),
                                },
                                now_us(),
                            )
                            .ok()??;
                            Some((prepared, token))
                        })
                        .collect()
                };

                // Fire sells outside the lock via spawn.
                for (prepared, token) in sells {
                    let sub2 = sub.clone();
                    let core2 = core.clone();
                    tokio::spawn(async move {
                        let outcome = sub2.submit_order(&prepared.body).await;
                        {
                            let mut c = match core2.lock() {
                                Ok(c) => c,
                                Err(_) => return,
                            };
                            runtime::record_sell_submit_outcome(
                                c.inventory_mut(),
                                &prepared.submit_id,
                                &outcome,
                                now_us(),
                            );
                        }
                        logline::log_event(
                            Level::Warn,
                            "sell_submit_outcome",
                            &[
                                Field { key: "token_id", value: &token.as_str() },
                                Field { key: "accepted", value: &outcome.is_accepted() },
                                Field {
                                    key: "http_status",
                                    value: &(outcome.http_status() as i64),
                                },
                            ],
                        );
                    });
                }
            }
        })
    };

    // --- Force-exit task: close positions near market expiry ---
    // Traces to: bot_orchestrator.py force_exit_tte_us check in evaluate_exit.
    // Fires sells with no limit (marketable at any bid) when TTE < 5s.
    let force_exit_task = {
        let core = core.clone();
        let sub = submitter.clone();
        let sign = order_signer.clone();
        let sid = sell_id.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(
                std::time::Duration::from_millis(500),
            );
            tick.tick().await;
            loop {
                tick.tick().await;
                let (sub, sign) = match (sub.as_ref(), sign.as_ref()) {
                    (Some(s), Some(g)) => (s.clone(), g.clone()),
                    _ => continue,
                };

                let force_sells: Vec<_> = {
                    let mut c = match core.lock() {
                        Ok(c) => c,
                        Err(_) => continue,
                    };
                    let market = match c.state_mut().market() {
                        Some(m) => m.clone(),
                        None => continue,
                    };
                    let tte_us = market
                        .end_ts
                        .saturating_mul(1_000_000)
                        .saturating_sub(now_us());
                    // Fire force sells when TTE < 5 s.
                    if tte_us > 5_000_000 {
                        continue;
                    }

                    let tokens = [market.yes_token.clone(), market.no_token.clone()];
                    tokens
                        .into_iter()
                        .filter_map(|token| {
                            let pos = c.inventory_mut().position(&token)?;
                            let size_atoms = pos.owned_atoms;
                            if size_atoms.atoms() <= 0 {
                                return None;
                            }
                            let prepared = c.prepare_sell_for_size_at_bid(
                                &token,
                                size_atoms,
                                &sign,
                                SignInputs {
                                    salt: sid.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                                    timestamp_ms: now_ms(),
                                },
                                now_us(),
                            )
                            .ok()??;
                            Some((prepared, token))
                        })
                        .collect()
                };

                for (prepared, token) in force_sells {
                    let sub2 = sub.clone();
                    let core2 = core.clone();
                    tokio::spawn(async move {
                        let outcome = sub2.submit_order(&prepared.body).await;
                        {
                            let mut c = match core2.lock() {
                                Ok(c) => c,
                                Err(_) => return,
                            };
                            runtime::record_sell_submit_outcome(
                                c.inventory_mut(),
                                &prepared.submit_id,
                                &outcome,
                                now_us(),
                            );
                        }
                        logline::log_event(
                            Level::Warn,
                            "force_sell_outcome",
                            &[
                                Field { key: "token_id", value: &token.as_str() },
                                Field { key: "accepted", value: &outcome.is_accepted() },
                            ],
                        );
                    });
                }
            }
        })
    };

    // --- Maintenance task (market discovery + rotation, every 10 s) ---
    // Traces to: market_ws.py:284-302 (reconcile),
    //   bot_orchestrator.py:542-571 (_apply_market_context on rotation),
    //   bot_orchestrator.py _unknown_submit_expiry_loop (expire_unknowns).
    let maint_task = {
        let core = core.clone();
        let anchor = anchor.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(
                std::time::Duration::from_secs(10),
            );
            interval.tick().await; // skip initial burst
            loop {
                interval.tick().await;

                // 1. Expire unknown submits older than 30 s.
                //    Traces to: _unknown_submit_expiry_loop in minimal_live_bot.py.
                {
                    let mut c = core.lock().unwrap();
                    let cutoff = now_us().saturating_sub(30_000_000);
                    c.inventory_mut().expire_unknowns(cutoff);
                }

                // 2. Discover current market.
                let discovered = match gamma.discover().await {
                    Some(ctx) => ctx,
                    None => continue,
                };

                // 3. Rotate if new market found.
                let rotated = {
                    let mut c = core.lock().unwrap();
                    let current = c.state_mut().market();
                    let is_new = current.is_none_or(|m| {
                        m.condition_id != discovered.condition_id
                            || m.slug != discovered.slug
                    });
                    if is_new {
                        // P0-4: fail-closed if old tokens have nonzero inventory.
                        // Dropping nonzero inventory blindly could leave unhedged
                        // positions that the bot doesn't know about.
                        let can_rotate = if let Some(old_market) = c.state_mut().market() {
                            let old_yes = old_market.yes_token.clone();
                            let old_no = old_market.no_token.clone();
                            let yes_atoms = c.inventory_mut().owned_atoms(&old_yes).atoms();
                            let no_atoms = c.inventory_mut().owned_atoms(&old_no).atoms();
                            if yes_atoms > 0 || no_atoms > 0 {
                                logline::log_event(
                                    Level::Error,
                                    "rotation_blocked_nonzero_inventory",
                                    &[
                                        Field { key: "yes_atoms", value: &yes_atoms },
                                        Field { key: "no_atoms", value: &no_atoms },
                                    ],
                                );
                                false
                            } else {
                                true
                            }
                        } else {
                            true
                        };

                        if can_rotate {
                            let yes = discovered.yes_token.clone();
                            let no = discovered.no_token.clone();
                            logline::log_event(
                                Level::Warn,
                                "minirust_market_context",
                                &[
                                    Field { key: "slug", value: &discovered.slug.as_str() },
                                    Field { key: "condition_id", value: &discovered.condition_id.as_str() },
                                    Field { key: "end_ts", value: &discovered.end_ts },
                                ],
                            );
                            c.inventory_mut().release_market_scope([&yes, &no]);
                            // P0-4: clear signal strike so the bot does not
                            // trade using the previous market's strike until
                            // the anchor resolves a new one.
                            c.signal_mut().set_strike(0.0, false);
                            c.state_mut().set_market(discovered);
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                };

                // 4. Reset anchor + send incremental WS subscribe on rotation.
                // Lock order: anchor → core (see invariant at §8).
                if rotated {
                    let mut a = anchor.lock().unwrap();
                    let (slug_ts, yes_tok, no_tok) = {
                        let mut c = core.lock().unwrap();
                        let m = c.state_mut().market();
                        (
                            m.map(|m| m.slug_ts).unwrap_or(0),
                            m.map(|m| m.yes_token.as_str().to_owned()).unwrap_or_default(),
                            m.map(|m| m.no_token.as_str().to_owned()).unwrap_or_default(),
                        )
                    };
                    a.set_pending(slug_ts);

                    // Send incremental subscribe to the live market WS.
                    // Traces to: market_ws.py:293-302.
                    if !yes_tok.is_empty() && !no_tok.is_empty() {
                        let frame = serde_json::json!({
                            "operation": "subscribe",
                            "assets_ids": [yes_tok, no_tok],
                            "custom_feature_enabled": true,
                        });
                        let _ = market_tx.send(frame.to_string());
                    }
                }
            }
        })
    };

    // ==================================================================
    // Wait for any task to exit (mirrors Python asyncio.FIRST_EXCEPTION).
    // ==================================================================
    tokio::select! {
        r = market_task => {
            logline::log_event(Level::Error, "market_task_exited", &[]);
            if let Err(e) = r { panic!("market task panicked: {e}"); }
        }
        r = binance_task => {
            logline::log_event(Level::Error, "binance_task_exited", &[]);
            if let Err(e) = r { panic!("binance task panicked: {e}"); }
        }
        r = user_task => {
            logline::log_event(Level::Error, "user_task_exited", &[]);
            if let Err(e) = r { panic!("user task panicked: {e}"); }
        }
        r = exit_task => {
            logline::log_event(Level::Error, "exit_task_exited", &[]);
            match r {
                Ok(()) => {}
                Err(e) => panic!("exit task panicked: {e}"),
            }
        }
        r = force_exit_task => {
            logline::log_event(Level::Error, "force_exit_task_exited", &[]);
            match r {
                Ok(()) => {}
                Err(e) => panic!("force-exit task panicked: {e}"),
            }
        }
        r = maint_task => {
            logline::log_event(Level::Error, "maint_task_exited", &[]);
            match r {
                Ok(()) => {}
                Err(e) => panic!("maintenance task panicked: {e}"),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Time helpers
// ---------------------------------------------------------------------------

fn now_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// Unix timestamp in milliseconds — used as the signed-body `timestamp` field.
/// Traces to: Python `int(time.time() * 1000)` in FastOrderSubmitter / DryRunOrderSubmitter.
fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
