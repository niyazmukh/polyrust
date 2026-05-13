//! Shadow-probe and live-trading binary. Wires all three WebSocket feeds
//! into RuntimeCore with anchor strike resolution, Gamma market discovery,
//! and gated live order submission.
//!
//! Mode switch: `POLY_ALLOW_LIVE_ORDERS=true` for live, omit/false for dry-run.
//! Live mode signs and submits to the CLOB via `POST /order`.
//! Dry-run connects all feeds but only logs signals.
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
use minirust::signing::{EXCHANGE_V2_NORMAL, OrderSigner, POLYGON_CHAIN_ID, SignInputs};
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
            Field {
                key: "live",
                value: &cfg.allow_live_orders,
            },
            Field {
                key: "slug_fmt",
                value: &launch.market_slug_fmt.as_str(),
            },
            Field {
                key: "binance_url",
                value: &launch.binance_ws_url.as_str(),
            },
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
    let mut poly_api_key = std::env::var("POLY_API_KEY").unwrap_or_default();
    let mut poly_api_secret = std::env::var("POLY_API_SECRET").unwrap_or_default();
    let mut poly_passphrase = std::env::var("POLY_PASSPHRASE").unwrap_or_default();
    let mut poly_address = std::env::var("POLY_ADDRESS").unwrap_or_default();
    let poly_pk = std::env::var("POLY_PK").unwrap_or_default();

    // Derive API credentials from the private key if direct creds are
    // missing. The Python bot does this via ClobClient.create_or_derive_api_creds().
    // Derivation is a one-time startup HTTP call — never on the hot path.
    if (poly_api_key.is_empty() || poly_api_secret.is_empty() || poly_passphrase.is_empty())
        && !poly_pk.is_empty()
    {
        match minirust::auth::derive_api_credentials(
            &poly_pk,
            137, // Polygon mainnet
            &launch.clob_url,
        )
        .await
        {
            Ok((key, secret, passphrase, eoa_address)) => {
                logline::log_event(Level::Warn, "api_credentials_derived_from_pk", &[]);
                poly_api_key = key;
                poly_api_secret = secret;
                poly_passphrase = passphrase;
                // The API key is associated with the EOA address (the
                // address that signed the derivation request). L2 auth
                // headers must use this address, not the proxy/funder.
                poly_address = eoa_address;
            }
            Err(e) => {
                eprintln!(
                    "FATAL: failed to derive API credentials from POLY_PK: {e}. \
                     Set POLY_API_KEY, POLY_API_SECRET, and POLY_PASSPHRASE directly."
                );
                std::process::exit(2);
            }
        }
    }

    // Fail fast if user WSS credentials are missing — even in dry-run.
    // BUY signals are gated on authenticated user WSS inventory truth;
    // without WSS auth, user_wss_trusted remains false and no signals fire.
    if poly_api_key.is_empty() || poly_api_secret.is_empty() || poly_passphrase.is_empty() {
        eprintln!(
            "FATAL: POLY_API_KEY, POLY_API_SECRET, and POLY_PASSPHRASE are required. \
             In shadow mode, BUY signals are gated on authenticated user WSS inventory truth. \
             Without WSS auth, user_wss_trusted remains false and no BUY signals will fire."
        );
        std::process::exit(2);
    }

    // 6. Live-submit infrastructure (only built when POLY_ALLOW_LIVE_ORDERS=true).
    //    Shadow mode skips both — no signer, no HTTP submitter.
    let auth_signer: Option<L2AuthSigner> = if cfg.allow_live_orders {
        L2AuthSigner::new(
            &poly_api_key,
            &poly_passphrase,
            &poly_api_secret,
            &poly_address,
        )
        .ok()
    } else {
        None
    };

    let submitter: Option<HttpSubmitter> = auth_signer
        .as_ref()
        .and_then(|auth| HttpSubmitter::new(&launch.clob_url, auth.clone()).ok());

    let order_signer: Option<OrderSigner> = if cfg.allow_live_orders && !poly_pk.is_empty() {
        match OrderSigner::new(
            &poly_pk,
            &poly_api_key,
            launch.poly_funder.as_deref().and_then(|f| f.parse().ok()),
            launch.poly_signature_kind,
            POLYGON_CHAIN_ID,
            EXCHANGE_V2_NORMAL,
        ) {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!(
                    "FATAL: invalid OrderSigner: {e} (check POLY_PK, POLY_FUNDER, POLY_SIGNATURE_KIND)"
                );
                std::process::exit(2);
            }
        }
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
        if poly_api_key.is_empty()
            || poly_api_secret.is_empty()
            || poly_passphrase.is_empty()
            || poly_address.is_empty()
        {
            eprintln!(
                "FATAL: live mode requires POLY_API_KEY, POLY_API_SECRET, POLY_PASSPHRASE, and POLY_ADDRESS for user WSS and L2 Auth"
            );
            std::process::exit(2);
        }
    }

    // 7. Signal ID counter for log correlation.
    let signal_id = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let sell_id = Arc::new(std::sync::atomic::AtomicU64::new(0));

    // Out-of-hotpath counters for periodic heartbeat — incremented with
    // Relaxed ordering so the binance callback pays no fence cost.
    let heartbeat_ticks: Arc<std::sync::atomic::AtomicU64> =
        Arc::new(std::sync::atomic::AtomicU64::new(0));
    let heartbeat_parse_errs: Arc<std::sync::atomic::AtomicU64> =
        Arc::new(std::sync::atomic::AtomicU64::new(0));
    let heartbeat_signals: Arc<std::sync::atomic::AtomicU64> =
        Arc::new(std::sync::atomic::AtomicU64::new(0));

    // 8. Market command channel for incremental WS subscribe on rotation.
    //    Traces to: market_ws.py:293-302 (incremental subscribe).
    //
    // Lock ordering invariant: when both `anchor` and `core` mutexes must be
    // held simultaneously, ALWAYS acquire `anchor` first, then `core`.
    // This order is observed in: binance callback, maint_task rotation.
    // Reversing it (core→anchor) will deadlock against these paths.
    let (market_tx, market_rx) = mpsc::unbounded_channel::<String>();

    // Log signer/maker addresses once at startup for cross-ref with POLY_ADDRESS.
    {
        let s_addr = order_signer
            .as_ref()
            .map(|s| minirust::signing::address_lower_hex(s.signer_address()))
            .unwrap_or_default();
        let m_addr = order_signer
            .as_ref()
            .map(|s| minirust::signing::address_lower_hex(s.maker_address()))
            .unwrap_or_default();
        logline::log_event(
            Level::Warn,
            "order_signer_addresses",
            &[
                Field {
                    key: "signer",
                    value: &s_addr,
                },
                Field {
                    key: "maker",
                    value: &m_addr,
                },
                Field {
                    key: "poly_address",
                    value: &poly_address,
                },
            ],
        );
    }

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
                            .map(|m| {
                                vec![
                                    m.yes_token.as_str().to_owned(),
                                    m.no_token.as_str().to_owned(),
                                ]
                            })
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
        let live = cfg.allow_live_orders;
        let sig_id = signal_id.clone();
        let sub = binance_sub;
        let sign = binance_sign;
        let hb_ticks = heartbeat_ticks.clone();
        let hb_parse_errs = heartbeat_parse_errs.clone();
        let hb_signals = heartbeat_signals.clone();
        tokio::spawn(async move {
            let binance_key = std::env::var("BINANCE_SBE_API_KEY").ok();
            feed::binance_feed_loop(&binance_url, binance_key.as_deref(), move |raw| {
                let ts = TsUs(now_us());
                // SBE @bestBidAsk provides exchange-origin eventTime.
                let ticker = match minirust::binance::parse_book_ticker(&raw) {
                    Ok(Some(t)) => t,
                    Ok(None) => return,
                    Err(e) => {
                        hb_parse_errs.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        logline::log_event(
                            Level::Debug,
                            "binance_parse_error",
                            &[Field {
                                key: "err",
                                value: &format!("{e:?}").as_str(),
                            }],
                        );
                        return;
                    }
                };
                hb_ticks.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
                            &[Field {
                                key: "strike",
                                value: &strike,
                            }],
                        );
                    }
                }

                // Signal decision + claim in one lock scope.
                // claim_entry() MUST be atomic with on_binance_sample():
                // has_entry_exposure_or_pending checks the claim so a second
                // Binance tick cannot slip in between intent production and
                // claim registration.
                // NOTE: dry-run does NOT claim — shadow mode must not create
                // fake pending exposure that blocks future same-token BUYs.
                let claimed = {
                    let mut c = match core.lock() {
                        Ok(c) => c,
                        Err(_) => return,
                    };

                    let market = match c.state_mut().market() {
                        Some(m) => m.clone(),
                        None => {
                            c.signal_mut().push(sample);
                            return;
                        }
                    };
                    let tte_us = market
                        .end_ts
                        .saturating_mul(1_000_000)
                        .saturating_sub(ts.micros());

                    match c.on_binance_sample(sample, ts, tte_us) {
                        Ok(Some(intent)) => {
                            let claim = if !live {
                                None
                            } else {
                                let policy = c.buy_submit_policy();
                                let claim_id = c.inventory_mut().claim_entry(
                                    intent.token.clone(),
                                    minirust::types::OrderSide::Buy,
                                    minirust::types::SharesAtoms(1),
                                    ts.micros(),
                                );
                                Some((policy, claim_id))
                            };
                            Some((intent, claim))
                        }
                        _ => None,
                    }
                };

                if let Some((intent, claim)) = claimed {
                    let id = sig_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let decide_us = now_us();
                    hb_signals.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if let Some((policy, claim_id)) = claim {
                        if let (Some(sub), Some(sign)) = (sub.as_ref(), sign.as_ref()) {
                            let sub = sub.clone();
                            let sign = sign.clone();
                            let core2 = core.clone();
                            let intent2 = intent.clone();
                            let claim_id2 = claim_id.clone();
                            let src_ts = sample.ts_us.micros();
                            let recv_ts = ts.micros();
                            tokio::spawn(async move {
                                let prepared = match runtime::prepare_buy_submit(
                                    &intent2,
                                    policy,
                                    &sign,
                                    SignInputs {
                                        salt: id,
                                        timestamp_ms: now_ms(),
                                    },
                                    claim_id2,
                                ) {
                                    Ok(p) => p,
                                    Err(e) => {
                                        logline::log_event(
                                            Level::Error,
                                            "buy_prepare_failed",
                                            &[Field {
                                                key: "error",
                                                value: &e.to_string().as_str(),
                                            }],
                                        );
                                        // Release the claim so this token is
                                        // unblocked for future BUYs.
                                        if let Ok(mut c) = core2.lock() {
                                            c.inventory_mut().release_claim(&claim_id);
                                        }
                                        return;
                                    }
                                };

                                let submit_us = now_us();
                                let outcome = sub.submit_order(&prepared.body).await;
                                let rtt_us = now_us() - submit_us;

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
                                let err_text = outcome.error_text().unwrap_or("-");
                                logline::log_event(
                                    Level::Warn,
                                    "buy_submit_outcome",
                                    &[
                                        Field {
                                            key: "signal_id",
                                            value: &(id as i64),
                                        },
                                        Field {
                                            key: "side",
                                            value: &intent2.side.as_str(),
                                        },
                                        Field {
                                            key: "token_id",
                                            value: &intent2.token.as_str(),
                                        },
                                        Field {
                                            key: "accepted",
                                            value: &accepted,
                                        },
                                        Field {
                                            key: "http_status",
                                            value: &(outcome.http_status() as i64),
                                        },
                                        Field {
                                            key: "error",
                                            value: &err_text,
                                        },
                                        Field {
                                            key: "src_ts_us",
                                            value: &src_ts,
                                        },
                                        Field {
                                            key: "recv_us",
                                            value: &recv_ts,
                                        },
                                        Field {
                                            key: "decide_us",
                                            value: &decide_us,
                                        },
                                        Field {
                                            key: "submit_us",
                                            value: &submit_us,
                                        },
                                        Field {
                                            key: "rtt_us",
                                            value: &rtt_us,
                                        },
                                    ],
                                );
                            });
                        }
                    } else {
                        // Dry-run: log the signal, no claim, no submit.
                        logline::log_event(
                            Level::Warn,
                            "shadow_buy_signal",
                            &[
                                Field {
                                    key: "signal_id",
                                    value: &(id as i64),
                                },
                                Field {
                                    key: "side",
                                    value: &intent.side.as_str(),
                                },
                                Field {
                                    key: "token_id",
                                    value: &intent.token.as_str(),
                                },
                                Field {
                                    key: "limit_ticks",
                                    value: &intent.limit.ticks(),
                                },
                                Field {
                                    key: "edge_ticks",
                                    value: &intent.edge_ticks,
                                },
                                Field {
                                    key: "src_ts_us",
                                    value: &sample.ts_us.micros(),
                                },
                                Field {
                                    key: "recv_us",
                                    value: &ts.micros(),
                                },
                                Field {
                                    key: "decide_us",
                                    value: &decide_us,
                                },
                            ],
                        );
                    }

                    // Off hot path: spawn a lightweight task to log the
                    // Polymarket token price at 1s intervals for 15s after
                    // the signal. Gives post-signal price trajectory for
                    // signal quality assessment.
                    {
                        let core3 = core.clone();
                        let tok = intent.token.clone();
                        tokio::spawn(async move {
                            for i in 1..=15u8 {
                                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                                let ask_ticks = core3
                                    .lock()
                                    .ok()
                                    .and_then(|mut c| {
                                        c.state_mut()
                                            .quote_for_token(&tok)
                                            .and_then(|q| q.ask)
                                            .map(|p| p.ticks())
                                    })
                                    .unwrap_or(0);
                                logline::log_event(
                                    Level::Info,
                                    "signal_price_after",
                                    &[
                                        Field {
                                            key: "signal_id",
                                            value: &(id as i64),
                                        },
                                        Field {
                                            key: "sec",
                                            value: &(i as i32),
                                        },
                                        Field {
                                            key: "ask_ticks",
                                            value: &ask_ticks,
                                        },
                                    ],
                                );
                            }
                        });
                    }
                }
            })
            .await;
        })
    };

    // --- User WS task ---
    // Trade events own inventory (WSS authority). Inventory is applied
    // only on CONFIRMED status (on-chain finality). The exit_task handles
    // selling once confirmed balance exists.
    let user_task = {
        let core = core.clone();
        let user_url = launch.poly_user_ws_url.clone();
        let api_key = poly_api_key.clone();
        let api_secret = poly_api_secret.clone();
        let passphrase = poly_passphrase.clone();
        tokio::spawn(async move {
            let core_disconnect = core.clone();
            feed::user_feed_loop(
                &user_url,
                &api_key,
                &api_secret,
                &passphrase,
                move |raw| {
                    let mut c = match core.lock() {
                        Ok(c) => c,
                        Err(_) => return,
                    };
                    let ts = now_us();
                    match c.apply_user_raw(&raw, ts) {
                        Ok(minirust::user::UserMessage::AuthError(ref msg)) => {
                            c.inventory_mut().set_user_wss_trusted(false);
                            logline::log_event(
                                Level::Error,
                                "user_wss_auth_error",
                                &[Field {
                                    key: "msg",
                                    value: &msg.as_str(),
                                }],
                            );
                        }
                        Ok(_) => {}
                        Err(e) => {
                            logline::log_event(
                                Level::Error,
                                "user_wss_parse_failed",
                                &[Field {
                                    key: "err",
                                    value: &format!("{e:?}").as_str(),
                                }],
                            );
                        }
                    }
                    // No immediate sell trigger here. The exit_task (50ms loop)
                    // handles selling once inventory is CONFIRMED on-chain.
                },
                {
                    let ca = core_disconnect.clone();
                    move || {
                        if let Ok(mut c) = ca.lock() {
                            c.inventory_mut().set_user_wss_trusted(true);
                            logline::log_event(Level::Warn, "user_wss_authenticated", &[]);
                        }
                    }
                },
                {
                    let cd = core_disconnect.clone();
                    move || {
                        if let Ok(mut c) = cd.lock() {
                            c.inventory_mut().set_user_wss_trusted(false);
                        }
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
            let mut tick = tokio::time::interval(std::time::Duration::from_micros(50_000));
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

                // Collect sellable plans under lock. Signing is done
                // outside the lock to avoid serializing against the
                // Binance BUY hot path.
                let plans: Vec<_> = {
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
                        .filter_map(|token| c.plan_sell_at_bid(&token))
                        .collect()
                };

                // Sign and fire sells outside the lock.
                for plan in plans {
                    let token = plan.token.clone();
                    let prepared = match plan.sign(
                        &sign,
                        SignInputs {
                            salt: sid.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                            timestamp_ms: now_ms(),
                        },
                    ) {
                        Ok(p) => p,
                        Err(e) => {
                            logline::log_event(
                                Level::Error,
                                "sell_prepare_failed",
                                &[Field {
                                    key: "error",
                                    value: &e.to_string().as_str(),
                                }],
                            );
                            continue;
                        }
                    };
                    let sub2 = sub.clone();
                    tokio::spawn(async move {
                        let outcome = sub2.submit_order(&prepared.body).await;
                        let err = outcome.error_text().unwrap_or("-");
                        logline::log_event(
                            Level::Warn,
                            "sell_submit_outcome",
                            &[
                                Field {
                                    key: "token_id",
                                    value: &token.as_str(),
                                },
                                Field {
                                    key: "accepted",
                                    value: &outcome.is_accepted(),
                                },
                                Field {
                                    key: "http_status",
                                    value: &(outcome.http_status() as i64),
                                },
                                Field {
                                    key: "error",
                                    value: &err,
                                },
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
        let maint_sub = submitter.clone();
        let hb_ticks = heartbeat_ticks.clone();
        let hb_parse_errs = heartbeat_parse_errs.clone();
        let hb_signals = heartbeat_signals.clone();
        tokio::spawn(async move {
            let mut periodic = tokio::time::interval(std::time::Duration::from_secs(10));
            periodic.tick().await; // skip initial burst
            let mut heartbeat_tick: u64 = 0;
            // Next rotation discovery deadline. Fires 5s before market end_ts.
            // Initialized to now (immediate first discovery).
            let mut rotation_deadline = tokio::time::Instant::now();

            loop {
                // Wait for either the periodic tick or the rotation deadline.
                let is_rotation = tokio::select! {
                    _ = periodic.tick() => false,
                    _ = tokio::time::sleep_until(rotation_deadline) => true,
                };

                // Periodic heartbeat — every 60 s (6 × 10 s).
                if !is_rotation {
                    heartbeat_tick = heartbeat_tick.wrapping_add(1);
                }
                if heartbeat_tick.is_multiple_of(6) && !is_rotation {
                    let ticks = hb_ticks.swap(0, std::sync::atomic::Ordering::Relaxed);
                    let parse_errs = hb_parse_errs.swap(0, std::sync::atomic::Ordering::Relaxed);
                    let signals = hb_signals.swap(0, std::sync::atomic::Ordering::Relaxed);
                    let (sellable, yes_bid, no_bid, trading) = {
                        let mut c = core.lock().unwrap();
                        let market = c.state_mut().market().cloned();
                        let sell = market
                            .as_ref()
                            .map(|m| {
                                [&m.yes_token, &m.no_token]
                                    .iter()
                                    .filter(|token| {
                                        c.inventory()
                                            .position(token)
                                            .is_some_and(|p| p.sellable.units() > 0)
                                    })
                                    .count()
                            })
                            .unwrap_or(0);
                        let yb = c
                            .state_mut()
                            .quote_for_side(minirust::types::OutcomeSide::Yes)
                            .and_then(|q| q.bid)
                            .map(|b| b.ticks())
                            .unwrap_or(0);
                        let nb = c
                            .state_mut()
                            .quote_for_side(minirust::types::OutcomeSide::No)
                            .and_then(|q| q.bid)
                            .map(|b| b.ticks())
                            .unwrap_or(0);
                        let active = c.state_mut().trading_active();
                        (sell, yb, nb, active)
                    };
                    logline::log_event(
                        Level::Info,
                        "heartbeat",
                        &[
                            Field {
                                key: "binance_ticks",
                                value: &(ticks as i64),
                            },
                            Field {
                                key: "parse_errs",
                                value: &(parse_errs as i64),
                            },
                            Field {
                                key: "signals_fired",
                                value: &(signals as i64),
                            },
                            Field {
                                key: "sellable_tokens",
                                value: &(sellable as i64),
                            },
                            Field {
                                key: "yes_bid_ticks",
                                value: &yes_bid,
                            },
                            Field {
                                key: "no_bid_ticks",
                                value: &no_bid,
                            },
                            Field {
                                key: "trading_active",
                                value: &trading,
                            },
                        ],
                    );
                }

                // 1. Expire unknown submits older than 30 s, and pending
                //    claims older than 60 s (defensive — the spawned submit
                //    task normally resolves the outcome in ≤ 2 s via HTTP
                //    timeout, but a task panic or cancellation would
                //    otherwise leave the Pending claim blocking same-token
                //    BUY forever).
                //    Traces to: _unknown_submit_expiry_loop in minimal_live_bot.py.
                {
                    let mut c = core.lock().unwrap();
                    let now = now_us();
                    let unknown_cutoff = now.saturating_sub(30_000_000);
                    let pending_cutoff = now.saturating_sub(60_000_000);
                    c.inventory_mut().expire_unknowns(unknown_cutoff);
                    c.inventory_mut().expire_pending(pending_cutoff);
                }

                // 2. Discover current/next market.
                // On rotation deadline, we know the next slug_ts = current end_ts.
                let next_slug_ts = {
                    let mut c = core.lock().unwrap();
                    c.state_mut().market().map(|m| m.end_ts)
                };
                let discovered = if is_rotation && let Some(nts) = next_slug_ts {
                    // Precise: query the exact next market by its known slug_ts.
                    match gamma.discover_for_ts(nts).await {
                        Some(ctx) => ctx,
                        None => match gamma.discover().await {
                            Some(ctx) => ctx,
                            None => continue,
                        },
                    }
                } else {
                    match gamma.discover().await {
                        Some(ctx) => ctx,
                        None => continue,
                    }
                };

                // 3. Rotate if new market found.
                let rotated = {
                    let mut c = core.lock().unwrap();
                    let current = c.state_mut().market();
                    let is_new = current.is_none_or(|m| {
                        m.condition_id != discovered.condition_id || m.slug != discovered.slug
                    });
                    if is_new {
                        let yes = discovered.yes_token.clone();
                        let no = discovered.no_token.clone();
                        logline::log_event(
                            Level::Warn,
                            "minirust_market_context",
                            &[
                                Field {
                                    key: "slug",
                                    value: &discovered.slug.as_str(),
                                },
                                Field {
                                    key: "condition_id",
                                    value: &discovered.condition_id.as_str(),
                                },
                                Field {
                                    key: "end_ts",
                                    value: &discovered.end_ts,
                                },
                            ],
                        );
                        // Schedule next rotation discovery 5s before this market ends.
                        let now_secs = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);
                        let secs_until_prefetch =
                            (discovered.end_ts - 5).saturating_sub(now_secs).max(1);
                        rotation_deadline = tokio::time::Instant::now()
                            + std::time::Duration::from_secs(secs_until_prefetch as u64);

                        c.inventory_mut().release_market_scope([&yes, &no]);
                        c.signal_mut().set_strike(0.0, true);
                        c.state_mut().set_market(discovered);
                        true
                    } else {
                        // Market unchanged — keep polling every 2s until we find the next one.
                        rotation_deadline =
                            tokio::time::Instant::now() + std::time::Duration::from_secs(2);
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
                            m.map(|m| m.yes_token.as_str().to_owned())
                                .unwrap_or_default(),
                            m.map(|m| m.no_token.as_str().to_owned())
                                .unwrap_or_default(),
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

                // Pre-warm HTTP connection pool every 10s so POST
                // never hits a cold/expired TLS connection.
                if let Some(ref sub) = maint_sub {
                    sub.warm_connection().await;
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
