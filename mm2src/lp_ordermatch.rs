/******************************************************************************
 * Copyright © 2014-2019 The SuperNET Developers.                             *
 *                                                                            *
 * See the AUTHORS, DEVELOPER-AGREEMENT and LICENSE files at                  *
 * the top-level directory of this distribution for the individual copyright  *
 * holder information and the developer policies on copyright and licensing.  *
 *                                                                            *
 * Unless otherwise agreed in a custom licensing agreement, no part of the    *
 * SuperNET software, including this file may be copied, modified, propagated *
 * or distributed except according to the terms contained in the LICENSE file *
 *                                                                            *
 * Removal or modification of this copyright notice is prohibited.            *
 *                                                                            *
 ******************************************************************************/

//
//  lp_ordermatch.rs
//  marketmaker
//
#![allow(uncommon_codepoints)]
#![cfg_attr(not(feature = "native"), allow(dead_code))]

use bigdecimal::BigDecimal;
use bitcrypto::sha256;
use coins::utxo::{compressed_pub_key_from_priv_raw, ChecksumType};
use coins::{lp_coinfind, lp_coinfindᵃ, BalanceUpdateEventHandler, MmCoinEnum};
use common::executor::{spawn, Timer};
use common::mm_ctx::{from_ctx, MmArc, MmWeak};
use common::mm_number::{from_dec_to_ratio, from_ratio_to_dec, Fraction, MmNumber};
use common::{bits256, block_on, json_dir_entries, new_uuid, now_ms, remove_file, write};
use futures::{compat::Future01CompatExt, lock::Mutex as AsyncMutex};
use gstuff::slurp;
use http::Response;
use keys::{Public, Signature};
use mm2_libp2p::{decode_signed, encode_and_sign, pub_sub_topic, TopicPrefix, TOPIC_SEPARATOR};
#[cfg(test)] use mocktopus::macros::*;
use num_rational::BigRational;
use num_traits::identities::Zero;
use primitives::hash::H256;
use rpc::v1::types::H256 as H256Json;
use serde_json::{self as json, Value as Json};
use std::collections::hash_map::{Entry, HashMap};
use std::collections::HashSet;
use std::fmt;
use std::fs::DirEntry;
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

use crate::mm2::{lp_network::{broadcast_p2p_msg, send_msgs_to_peers, subscribe_to_topic},
                 lp_swap::{check_balance_for_maker_swap, check_balance_for_taker_swap, get_locked_amount,
                           is_pubkey_banned, lp_atomic_locktime, run_maker_swap, run_taker_swap,
                           AtomicLocktimeVersion, MakerSwap, RunMakerSwapInput, RunTakerSwapInput,
                           SwapConfirmationsSettings, TakerSwap}};

pub const ORDERBOOK_PREFIX: TopicPrefix = "orbk";
const MIN_ORDER_KEEP_ALIVE_INTERVAL: u64 = 20;
const MAKER_ORDER_TIMEOUT: u64 = MIN_ORDER_KEEP_ALIVE_INTERVAL * 3;
const TAKER_ORDER_TIMEOUT: u64 = 30;
const ORDER_MATCH_TIMEOUT: u64 = 30;

impl From<(new_protocol::MakerOrderCreated, Vec<u8>, String, String)> for PricePingRequest {
    fn from(tuple: (new_protocol::MakerOrderCreated, Vec<u8>, String, String)) -> PricePingRequest {
        let (order, initial_message, pubsecp, peer_id) = tuple;
        PricePingRequest {
            method: "".to_string(),
            pubkey: "".to_string(),
            base: order.base,
            rel: order.rel,
            price: order.price.to_decimal(),
            price_rat: Some(order.price),
            price64: "".to_string(),
            timestamp: now_ms() / 1000,
            pubsecp,
            sig: "".to_string(),
            balance: order.max_volume.to_decimal(),
            balance_rat: Some(order.max_volume),
            uuid: Some(order.uuid.into()),
            peer_id,
            initial_message,
        }
    }
}

fn find_order_by_uuid_and_pubkey<'a>(
    orderbook: &'a mut HashMap<(String, String), HashMap<Uuid, PricePingRequest>>,
    uuid: &Uuid,
    from_pubkey: &str,
) -> Option<&'a mut PricePingRequest> {
    orderbook
        .values_mut()
        .flatten()
        .map(|(_, order)| order)
        .find(|order| order.uuid == Some(*uuid) && order.pubsecp == from_pubkey)
}

fn find_order_by_uuid<'a>(
    orderbook: &'a mut HashMap<(String, String), HashMap<Uuid, PricePingRequest>>,
    uuid: &Uuid,
) -> Option<&'a mut PricePingRequest> {
    orderbook
        .values_mut()
        .flatten()
        .map(|(_, order)| order)
        .find(|order| order.uuid == Some(*uuid))
}

async fn process_order_keep_alive(
    ctx: &MmArc,
    from_pubkey: &str,
    topic: &str,
    keep_alive: &new_protocol::MakerOrderKeepAlive,
) {
    let ordermatch_ctx = OrdermatchContext::from_ctx(&ctx).unwrap();
    let mut orderbook = ordermatch_ctx.orderbook.lock().await;
    let uuid = keep_alive.uuid.into();
    match find_order_by_uuid_and_pubkey(&mut orderbook, &uuid, from_pubkey) {
        Some(order) => order.timestamp = keep_alive.timestamp,
        None => match ordermatch_ctx.inactive_orders.lock().await.remove(&uuid) {
            Some(mut order) => {
                order.timestamp = keep_alive.timestamp;
                orderbook
                    .entry((order.base.clone(), order.rel.clone()))
                    .or_insert_with(HashMap::new)
                    .insert(uuid, order);
            },
            None => broadcast_repeat_order(ctx, topic.into(), keep_alive.uuid.into()),
        },
    }
}

async fn process_my_order_keep_alive(ctx: &MmArc, keep_alive: &new_protocol::MakerOrderKeepAlive, order: &MakerOrder) {
    let ordermatch_ctx = OrdermatchContext::from_ctx(&ctx).unwrap();
    let mut orderbook = ordermatch_ctx.orderbook.lock().await;

    let uuid = keep_alive.uuid.into();
    match find_order_by_uuid(&mut orderbook, &uuid) {
        Some(order) => order.timestamp = keep_alive.timestamp,
        None => {
            // avoid dead lock on orderbook as maker_order_created_p2p_notify also acquires it
            drop(orderbook);
            maker_order_created_p2p_notify(ctx.clone(), order).await;
        },
    }
}

async fn insert_or_update_order(ctx: &MmArc, req: PricePingRequest, uuid: Uuid) {
    let ordermatch_ctx = OrdermatchContext::from_ctx(&ctx).unwrap();
    let mut orderbook = ordermatch_ctx.orderbook.lock().await;
    match orderbook.entry((req.base.clone(), req.rel.clone())) {
        Entry::Vacant(pair_orders) => {
            if req.balance > 0.into() && req.price > 0.into() {
                let mut orders = HashMap::new();
                orders.insert(uuid, req);
                pair_orders.insert(orders);
            }
        },
        Entry::Occupied(mut pair_orders) => match pair_orders.get_mut().entry(uuid) {
            Entry::Vacant(order) => {
                if req.balance > 0.into() && req.price > 0.into() {
                    order.insert(req);
                }
            },
            Entry::Occupied(mut order) => {
                if req.balance > 0.into() {
                    order.insert(req);
                } else {
                    order.remove();
                }
            },
        },
    }
}

async fn delete_order(ctx: &MmArc, pubkey: &str, uuid: Uuid) {
    let ordermatch_ctx = OrdermatchContext::from_ctx(&ctx).unwrap();
    let mut orderbook = ordermatch_ctx.orderbook.lock().await;
    for (_, orders) in orderbook.iter_mut() {
        if let Entry::Occupied(order) = orders.entry(uuid) {
            if order.get().pubsecp == pubkey {
                order.remove();
            }
        }
    }
}

fn broadcast_repeat_order(ctx: &MmArc, topic: String, uuid: Uuid) {
    let msg = new_protocol::OrdermatchMessage::RepeatOrder(new_protocol::RepeatOrder { uuid: uuid.into() });
    broadcast_ordermatch_message(ctx, topic, msg);
}

async fn process_repeat_order(ctx: &MmArc, uuid: Uuid) {
    let ordermatch_ctx = OrdermatchContext::from_ctx(&ctx).unwrap();
    let my_maker_orders = ordermatch_ctx.my_maker_orders.lock().await;
    if let Some(order) = my_maker_orders.get(&uuid) {
        maker_order_created_p2p_notify(ctx.clone(), order).await;
    }
}

async fn delete_my_order(ctx: &MmArc, uuid: Uuid) {
    let ordermatch_ctx: Arc<OrdermatchContext> = OrdermatchContext::from_ctx(&ctx).unwrap();
    let mut orderbook = ordermatch_ctx.orderbook.lock().await;
    for (_, orders) in orderbook.iter_mut() {
        orders.remove(&uuid);
    }
}

pub async fn process_msg(ctx: MmArc, initial_topic: &str, from_peer: String, msg: &[u8]) {
    match decode_signed::<new_protocol::OrdermatchMessage>(msg) {
        Ok((message, _sig, pubkey)) => {
            println!("Got ordermatching message {:?}", message);
            match message {
                new_protocol::OrdermatchMessage::MakerOrderCreated(created_msg) => {
                    let req: PricePingRequest = (
                        created_msg,
                        msg.to_vec(),
                        hex::encode(pubkey.to_bytes().as_slice()),
                        from_peer,
                    )
                        .into();
                    let uuid = req.uuid.unwrap();
                    insert_or_update_order(&ctx, req, uuid).await;
                },
                new_protocol::OrdermatchMessage::MakerOrderKeepAlive(keep_alive) => {
                    process_order_keep_alive(&ctx, &pubkey.to_hex(), initial_topic, &keep_alive).await;
                },
                new_protocol::OrdermatchMessage::TakerRequest(taker_request) => {
                    let msg = TakerRequest::from_new_proto_and_pubkey(taker_request, pubkey.unprefixed().into());
                    process_taker_request(ctx, msg).await;
                },
                new_protocol::OrdermatchMessage::MakerReserved(maker_reserved) => {
                    let msg = MakerReserved::from_new_proto_and_pubkey(maker_reserved, pubkey.unprefixed().into());
                    process_maker_reserved(ctx, msg).await;
                },
                new_protocol::OrdermatchMessage::TakerConnect(taker_connect) => {
                    process_taker_connect(ctx, pubkey.unprefixed().into(), taker_connect.into()).await;
                },
                new_protocol::OrdermatchMessage::MakerConnected(maker_connected) => {
                    process_maker_connected(ctx, pubkey.unprefixed().into(), maker_connected.into()).await;
                },
                new_protocol::OrdermatchMessage::MakerOrderCancelled(cancelled_msg) => {
                    delete_order(&ctx, &pubkey.to_hex(), cancelled_msg.uuid.into()).await;
                },
                new_protocol::OrdermatchMessage::RepeatOrder(repeat_order) => {
                    process_repeat_order(&ctx, repeat_order.uuid.into()).await;
                },
                _ => unimplemented!(),
            }
        },
        Err(e) => println!("Error {} while decoding signed message", e),
    };
}

fn alb_ordered_pair(base: &str, rel: &str) -> String {
    let (first, second) = if base < rel { (base, rel) } else { (rel, base) };
    let mut res = first.to_owned();
    res.push(':');
    res.push_str(second);
    res
}

fn orderbook_topic(base: &str, rel: &str) -> String { pub_sub_topic(ORDERBOOK_PREFIX, &alb_ordered_pair(base, rel)) }

#[test]
fn test_alb_ordered_pair() {
    assert_eq!("BTC:KMD", alb_ordered_pair("KMD", "BTC"));
    assert_eq!("BTCH:KMD", alb_ordered_pair("KMD", "BTCH"));
    assert_eq!("KMD:QTUM", alb_ordered_pair("QTUM", "KMD"));
}

fn parse_orderbook_pair_from_topic(topic: &str) -> Option<(&str, &str)> {
    let mut split = topic.split(|maybe_sep| maybe_sep == TOPIC_SEPARATOR);
    match split.next() {
        Some(ORDERBOOK_PREFIX) => match split.next() {
            Some(maybe_pair) => {
                let colon = maybe_pair.find(|maybe_colon| maybe_colon == ':');
                match colon {
                    Some(index) => {
                        if index + 1 < maybe_pair.len() {
                            Some((&maybe_pair[..index], &maybe_pair[index + 1..]))
                        } else {
                            None
                        }
                    },
                    None => None,
                }
            },
            None => None,
        },
        _ => None,
    }
}

#[test]
fn test_parse_orderbook_pair_from_topic() {
    assert_eq!(Some(("BTC", "KMD")), parse_orderbook_pair_from_topic("orbk/BTC:KMD"));
    assert_eq!(None, parse_orderbook_pair_from_topic("orbk/BTC:"));
}

async fn maker_order_created_p2p_notify(ctx: MmArc, order: &MakerOrder) {
    let topic = orderbook_topic(&order.base, &order.rel);
    let message = new_protocol::MakerOrderCreated {
        uuid: order.uuid.into(),
        base: order.base.clone(),
        rel: order.rel.clone(),
        price: order.price.clone(),
        max_volume: order.max_base_vol.clone(),
        min_volume: order.min_base_vol.clone(),
        conf_settings: order.conf_settings.unwrap(),
    };

    let key_pair = ctx.secp256k1_key_pair.or(&&|| panic!());
    let to_broadcast = new_protocol::OrdermatchMessage::MakerOrderCreated(message.clone());
    let encoded_msg = encode_and_sign(&to_broadcast, &*key_pair.private().secret).unwrap();
    log!("Maker order created size "(encoded_msg.len()));
    let peer = ctx.peer_id.or(&&|| panic!()).clone();
    let price_ping_req: PricePingRequest =
        (message, encoded_msg.clone(), hex::encode(&**key_pair.public()), peer).into();
    let uuid = price_ping_req.uuid.unwrap();
    insert_or_update_order(&ctx, price_ping_req, uuid).await;
    broadcast_p2p_msg(&ctx, topic, encoded_msg);
}

fn maker_order_updated_p2p_notify(ctx: MmArc, _order: &MakerOrder) {
    spawn(async move {
        if let Err(e) = broadcast_my_maker_orders(&ctx).await {
            ctx.log
                .log("", &[&"broadcast_my_maker_orders"], &format!("error {}", e));
        };
    });
}

async fn maker_order_cancelled_p2p_notify(ctx: MmArc, order: &MakerOrder) {
    let message = new_protocol::OrdermatchMessage::MakerOrderCancelled(new_protocol::MakerOrderCancelled {
        uuid: order.uuid.into(),
    });
    delete_my_order(&ctx, order.uuid).await;
    println!("maker_order_cancelled_p2p_notify called, message {:?}", message);
    broadcast_ordermatch_message(&ctx, orderbook_topic(&order.base, &order.rel), message);
}

pub async fn handle_peer_subscribed(ctx: MmArc, peer: &str, topic: &str) {
    let pair = match parse_orderbook_pair_from_topic(topic) {
        Some(p) => p,
        None => return,
    };
    let ordermatch_ctx = unwrap!(OrdermatchContext::from_ctx(&ctx));
    let orderbook = ordermatch_ctx.orderbook.lock().await;
    let mut messages = vec![];
    if let Some(orders) = orderbook.get(&(pair.0.to_owned(), pair.1.to_owned())) {
        for (_, order) in orders.iter() {
            messages.push((topic.to_owned(), order.initial_message.clone()));
        }
    }

    if let Some(orders) = orderbook.get(&(pair.1.to_owned(), pair.0.to_owned())) {
        for (_, order) in orders.iter() {
            messages.push((topic.to_owned(), order.initial_message.clone()));
        }
    }
    if !messages.is_empty() {
        let peers = vec![peer.to_owned()];
        send_msgs_to_peers(&ctx, messages, peers);
    }
}

pub struct BalanceUpdateOrdermatchHandler {
    ctx: MmArc,
}

impl BalanceUpdateOrdermatchHandler {
    pub fn new(ctx: MmArc) -> Self { BalanceUpdateOrdermatchHandler { ctx } }
}

impl BalanceUpdateEventHandler for BalanceUpdateOrdermatchHandler {
    fn balance_updated(&self, ticker: &str, new_balance: &BigDecimal) {
        let new_balance = MmNumber::from(new_balance.clone());
        let ordermatch_ctx = unwrap!(OrdermatchContext::from_ctx(&self.ctx));
        let mut maker_orders = block_on(ordermatch_ctx.my_maker_orders.lock());
        *maker_orders = maker_orders
            .drain()
            .filter_map(|(uuid, mut order)| {
                if order.base == *ticker {
                    if new_balance < MmNumber::from(MIN_TRADING_VOL) {
                        order.max_base_vol = 0.into();
                        block_on(maker_order_cancelled_p2p_notify(self.ctx.clone(), &order));
                        None
                    } else if new_balance < order.max_base_vol {
                        order.max_base_vol = new_balance.clone();
                        maker_order_updated_p2p_notify(self.ctx.clone(), &order);
                        Some((uuid, order))
                    } else {
                        Some((uuid, order))
                    }
                } else {
                    Some((uuid, order))
                }
            })
            .collect();
    }
}

#[cfg(test)]
#[cfg(feature = "native")]
#[path = "ordermatch_tests.rs"]
mod ordermatch_tests;

const MIN_TRADING_VOL: &str = "0.00777";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum TakerAction {
    Buy,
    Sell,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct OrderConfirmationsSettings {
    pub base_confs: u64,
    pub base_nota: bool,
    pub rel_confs: u64,
    pub rel_nota: bool,
}

impl OrderConfirmationsSettings {
    pub fn reversed(&self) -> OrderConfirmationsSettings {
        OrderConfirmationsSettings {
            base_confs: self.rel_confs,
            base_nota: self.rel_nota,
            rel_confs: self.base_confs,
            rel_nota: self.base_nota,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TakerRequest {
    base: String,
    rel: String,
    base_amount: BigDecimal,
    base_amount_rat: Option<BigRational>,
    rel_amount: BigDecimal,
    rel_amount_rat: Option<BigRational>,
    action: TakerAction,
    uuid: Uuid,
    method: String,
    sender_pubkey: H256Json,
    dest_pub_key: H256Json,
    #[serde(default)]
    match_by: MatchBy,
    conf_settings: Option<OrderConfirmationsSettings>,
}

impl TakerRequest {
    fn from_new_proto_and_pubkey(message: new_protocol::TakerRequest, sender_pubkey: H256Json) -> Self {
        TakerRequest {
            base: message.base,
            rel: message.rel,
            base_amount: message.base_amount.to_decimal(),
            base_amount_rat: Some(message.base_amount.into()),
            rel_amount: message.rel_amount.to_decimal(),
            rel_amount_rat: Some(message.rel_amount.into()),
            action: message.action,
            uuid: message.uuid.into(),
            method: "".to_string(),
            sender_pubkey,
            dest_pub_key: Default::default(),
            match_by: message.match_by.into(),
            conf_settings: Some(message.conf_settings),
        }
    }
}

impl Into<new_protocol::OrdermatchMessage> for TakerRequest {
    fn into(self) -> new_protocol::OrdermatchMessage {
        new_protocol::OrdermatchMessage::TakerRequest(new_protocol::TakerRequest {
            base_amount: self.get_base_amount(),
            rel_amount: self.get_rel_amount(),
            base: self.base,
            rel: self.rel,
            action: self.action,
            uuid: self.uuid.into(),
            match_by: self.match_by.into(),
            conf_settings: self.conf_settings.unwrap(),
        })
    }
}

impl TakerRequest {
    fn get_base_amount(&self) -> MmNumber {
        match &self.base_amount_rat {
            Some(r) => r.clone().into(),
            None => self.base_amount.clone().into(),
        }
    }

    fn get_rel_amount(&self) -> MmNumber {
        match &self.rel_amount_rat {
            Some(r) => r.clone().into(),
            None => self.rel_amount.clone().into(),
        }
    }
}

struct TakerRequestBuilder {
    base: String,
    rel: String,
    base_amount: MmNumber,
    rel_amount: MmNumber,
    sender_pubkey: H256Json,
    action: TakerAction,
    match_by: MatchBy,
    conf_settings: Option<OrderConfirmationsSettings>,
}

impl Default for TakerRequestBuilder {
    fn default() -> Self {
        TakerRequestBuilder {
            base: "".into(),
            rel: "".into(),
            base_amount: 0.into(),
            rel_amount: 0.into(),
            sender_pubkey: H256Json::default(),
            action: TakerAction::Buy,
            match_by: MatchBy::Any,
            conf_settings: None,
        }
    }
}

enum TakerRequestBuildError {
    BaseCoinEmpty,
    RelCoinEmpty,
    BaseEqualRel,
    /// Base amount too low with threshold
    BaseAmountTooLow {
        actual: MmNumber,
        threshold: MmNumber,
    },
    /// Rel amount too low with threshold
    RelAmountTooLow {
        actual: MmNumber,
        threshold: MmNumber,
    },
    SenderPubkeyIsZero,
    ConfsSettingsNotSet,
}

impl fmt::Display for TakerRequestBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TakerRequestBuildError::BaseCoinEmpty => write!(f, "Base coin can not be empty"),
            TakerRequestBuildError::RelCoinEmpty => write!(f, "Rel coin can not be empty"),
            TakerRequestBuildError::BaseEqualRel => write!(f, "Rel coin can not be same as base"),
            TakerRequestBuildError::BaseAmountTooLow { actual, threshold } => write!(
                f,
                "Base amount {} is too low, required: {}",
                actual.to_decimal(),
                threshold.to_decimal()
            ),
            TakerRequestBuildError::RelAmountTooLow { actual, threshold } => write!(
                f,
                "Rel amount {} is too low, required: {}",
                actual.to_decimal(),
                threshold.to_decimal()
            ),
            TakerRequestBuildError::SenderPubkeyIsZero => write!(f, "Sender pubkey can not be zero"),
            TakerRequestBuildError::ConfsSettingsNotSet => write!(f, "Confirmation settings must be set"),
        }
    }
}

impl TakerRequestBuilder {
    fn with_base_coin(mut self, ticker: String) -> Self {
        self.base = ticker;
        self
    }

    fn with_rel_coin(mut self, ticker: String) -> Self {
        self.rel = ticker;
        self
    }

    fn with_base_amount(mut self, vol: MmNumber) -> Self {
        self.base_amount = vol;
        self
    }

    fn with_rel_amount(mut self, vol: MmNumber) -> Self {
        self.rel_amount = vol;
        self
    }

    fn with_action(mut self, action: TakerAction) -> Self {
        self.action = action;
        self
    }

    fn with_match_by(mut self, match_by: MatchBy) -> Self {
        self.match_by = match_by;
        self
    }

    fn with_conf_settings(mut self, settings: OrderConfirmationsSettings) -> Self {
        self.conf_settings = Some(settings);
        self
    }

    fn with_sender_pubkey(mut self, sender_pubkey: H256Json) -> Self {
        self.sender_pubkey = sender_pubkey;
        self
    }

    /// Validate fields and build
    fn build(self) -> Result<TakerRequest, TakerRequestBuildError> {
        let min_vol = MmNumber::from(MIN_TRADING_VOL.parse::<BigDecimal>().unwrap());

        if self.base.is_empty() {
            return Err(TakerRequestBuildError::BaseCoinEmpty);
        }

        if self.rel.is_empty() {
            return Err(TakerRequestBuildError::RelCoinEmpty);
        }

        if self.base == self.rel {
            return Err(TakerRequestBuildError::BaseEqualRel);
        }

        if self.base_amount < min_vol {
            return Err(TakerRequestBuildError::BaseAmountTooLow {
                actual: self.base_amount,
                threshold: min_vol,
            });
        }

        if self.rel_amount < min_vol {
            return Err(TakerRequestBuildError::RelAmountTooLow {
                actual: self.rel_amount,
                threshold: min_vol,
            });
        }

        if self.sender_pubkey == H256Json::default() {
            return Err(TakerRequestBuildError::SenderPubkeyIsZero);
        }

        if self.conf_settings.is_none() {
            return Err(TakerRequestBuildError::ConfsSettingsNotSet);
        }

        Ok(TakerRequest {
            base: self.base,
            rel: self.rel,
            base_amount: self.base_amount.to_decimal(),
            base_amount_rat: Some(self.base_amount.into()),
            rel_amount: self.rel_amount.to_decimal(),
            rel_amount_rat: Some(self.rel_amount.into()),
            action: self.action,
            uuid: new_uuid(),
            method: "request".to_string(),
            sender_pubkey: self.sender_pubkey,
            dest_pub_key: Default::default(),
            match_by: self.match_by,
            conf_settings: self.conf_settings,
        })
    }

    #[cfg(test)]
    /// skip validation for tests
    fn build_unchecked(self) -> TakerRequest {
        TakerRequest {
            base: self.base,
            rel: self.rel,
            base_amount: self.base_amount.to_decimal(),
            base_amount_rat: Some(self.base_amount.into()),
            rel_amount: self.rel_amount.to_decimal(),
            rel_amount_rat: Some(self.rel_amount.into()),
            action: self.action,
            uuid: new_uuid(),
            method: "request".to_string(),
            sender_pubkey: self.sender_pubkey,
            dest_pub_key: Default::default(),
            match_by: self.match_by,
            conf_settings: self.conf_settings,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", content = "data")]
pub enum MatchBy {
    Any,
    Orders(HashSet<Uuid>),
    Pubkeys(HashSet<H256Json>),
}

impl Default for MatchBy {
    fn default() -> Self { MatchBy::Any }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data")]
enum OrderType {
    FillOrKill,
    GoodTillCancelled,
}

impl Default for OrderType {
    fn default() -> Self { OrderType::GoodTillCancelled }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct TakerOrder {
    created_at: u64,
    request: TakerRequest,
    matches: HashMap<Uuid, TakerMatch>,
    order_type: OrderType,
}

/// Result of match_reserved function
#[derive(Debug, PartialEq)]
enum MatchReservedResult {
    /// Order and reserved message matched,
    Matched,
    /// Order and reserved didn't match
    NotMatched,
}

impl TakerOrder {
    fn is_cancellable(&self) -> bool { self.matches.is_empty() }

    fn match_reserved(&self, reserved: &MakerReserved) -> MatchReservedResult {
        match &self.request.match_by {
            MatchBy::Any => (),
            MatchBy::Orders(uuids) => {
                if !uuids.contains(&reserved.maker_order_uuid) {
                    return MatchReservedResult::NotMatched;
                }
            },
            MatchBy::Pubkeys(pubkeys) => {
                if !pubkeys.contains(&reserved.sender_pubkey) {
                    return MatchReservedResult::NotMatched;
                }
            },
        }

        let my_base_amount: MmNumber = self.request.get_base_amount();
        let my_rel_amount: MmNumber = self.request.get_rel_amount();
        let other_base_amount: MmNumber = reserved.get_base_amount();
        let other_rel_amount: MmNumber = reserved.get_rel_amount();

        match self.request.action {
            TakerAction::Buy => {
                if self.request.base == reserved.base
                    && self.request.rel == reserved.rel
                    && my_base_amount == other_base_amount
                    && other_rel_amount <= my_rel_amount
                {
                    MatchReservedResult::Matched
                } else {
                    MatchReservedResult::NotMatched
                }
            },
            TakerAction::Sell => {
                if self.request.base == reserved.rel
                    && self.request.rel == reserved.base
                    && my_base_amount == other_rel_amount
                    && my_rel_amount <= other_base_amount
                {
                    MatchReservedResult::Matched
                } else {
                    MatchReservedResult::NotMatched
                }
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
/// Market maker order
/// The "action" is missing here because it's easier to always consider maker order as "sell"
/// So upon ordermatch with request we have only 2 combinations "sell":"sell" and "sell":"buy"
/// Adding "action" to maker order will just double possible combinations making order match more complex.
pub struct MakerOrder {
    pub max_base_vol: MmNumber,
    pub min_base_vol: MmNumber,
    pub price: MmNumber,
    pub created_at: u64,
    pub base: String,
    pub rel: String,
    matches: HashMap<Uuid, MakerMatch>,
    started_swaps: Vec<Uuid>,
    uuid: Uuid,
    conf_settings: Option<OrderConfirmationsSettings>,
}

struct MakerOrderBuilder {
    max_base_vol: MmNumber,
    min_base_vol: MmNumber,
    price: MmNumber,
    base: String,
    rel: String,
    conf_settings: Option<OrderConfirmationsSettings>,
}

impl Default for MakerOrderBuilder {
    fn default() -> MakerOrderBuilder {
        MakerOrderBuilder {
            base: "".into(),
            rel: "".into(),
            max_base_vol: 0.into(),
            min_base_vol: 0.into(),
            price: 0.into(),
            conf_settings: None,
        }
    }
}

enum MakerOrderBuildError {
    BaseCoinEmpty,
    RelCoinEmpty,
    BaseEqualRel,
    /// Max base vol too low with threshold
    MaxBaseVolTooLow {
        actual: MmNumber,
        threshold: MmNumber,
    },
    /// Min base vol too low with threshold
    MinBaseVolTooLow {
        actual: MmNumber,
        threshold: MmNumber,
    },
    /// Price too low with threshold
    PriceTooLow {
        actual: MmNumber,
        threshold: MmNumber,
    },
    /// Rel vol too low with threshold
    RelVolTooLow {
        actual: MmNumber,
        threshold: MmNumber,
    },
    ConfSettingsNotSet,
}

impl fmt::Display for MakerOrderBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MakerOrderBuildError::BaseCoinEmpty => write!(f, "Base coin can not be empty"),
            MakerOrderBuildError::RelCoinEmpty => write!(f, "Rel coin can not be empty"),
            MakerOrderBuildError::BaseEqualRel => write!(f, "Rel coin can not be same as base"),
            MakerOrderBuildError::MaxBaseVolTooLow { actual, threshold } => write!(
                f,
                "Max base vol {} is too low, required: {}",
                actual.to_decimal(),
                threshold.to_decimal()
            ),
            MakerOrderBuildError::MinBaseVolTooLow { actual, threshold } => write!(
                f,
                "Min base vol {} is too low, required: {}",
                actual.to_decimal(),
                threshold.to_decimal()
            ),
            MakerOrderBuildError::PriceTooLow { actual, threshold } => write!(
                f,
                "Price {} is too low, required: {}",
                actual.to_decimal(),
                threshold.to_decimal()
            ),
            MakerOrderBuildError::RelVolTooLow { actual, threshold } => write!(
                f,
                "Max rel vol {} is too low, required: {}",
                actual.to_decimal(),
                threshold.to_decimal()
            ),
            MakerOrderBuildError::ConfSettingsNotSet => write!(f, "Confirmation settings must be set"),
        }
    }
}

impl MakerOrderBuilder {
    fn with_base_coin(mut self, ticker: String) -> Self {
        self.base = ticker;
        self
    }

    fn with_rel_coin(mut self, ticker: String) -> Self {
        self.rel = ticker;
        self
    }

    fn with_max_base_vol(mut self, vol: MmNumber) -> Self {
        self.max_base_vol = vol;
        self
    }

    fn with_price(mut self, price: MmNumber) -> Self {
        self.price = price;
        self
    }

    fn with_conf_settings(mut self, conf_settings: OrderConfirmationsSettings) -> Self {
        self.conf_settings = Some(conf_settings);
        self
    }

    /// Validate fields and build
    fn build(self) -> Result<MakerOrder, MakerOrderBuildError> {
        let min_price = MmNumber::from(BigRational::new(1.into(), 100_000_000.into()));
        let min_vol = MmNumber::from(MIN_TRADING_VOL.parse::<BigDecimal>().unwrap());
        let zero: MmNumber = 0.into();

        if self.base.is_empty() {
            return Err(MakerOrderBuildError::BaseCoinEmpty);
        }

        if self.rel.is_empty() {
            return Err(MakerOrderBuildError::RelCoinEmpty);
        }

        if self.base == self.rel {
            return Err(MakerOrderBuildError::BaseEqualRel);
        }

        if self.max_base_vol < min_vol {
            return Err(MakerOrderBuildError::MaxBaseVolTooLow {
                actual: self.max_base_vol,
                threshold: min_vol,
            });
        }

        if self.price < min_price {
            return Err(MakerOrderBuildError::PriceTooLow {
                actual: self.price,
                threshold: min_price,
            });
        }

        let rel_vol = &self.max_base_vol * &self.price;
        if rel_vol < min_vol {
            return Err(MakerOrderBuildError::RelVolTooLow {
                actual: rel_vol,
                threshold: min_vol,
            });
        }

        if self.min_base_vol < zero {
            return Err(MakerOrderBuildError::MinBaseVolTooLow {
                actual: self.min_base_vol,
                threshold: zero,
            });
        }

        if self.conf_settings.is_none() {
            return Err(MakerOrderBuildError::ConfSettingsNotSet);
        }

        Ok(MakerOrder {
            base: self.base,
            rel: self.rel,
            created_at: now_ms(),
            max_base_vol: self.max_base_vol,
            min_base_vol: self.min_base_vol,
            price: self.price,
            matches: HashMap::new(),
            started_swaps: Vec::new(),
            uuid: new_uuid(),
            conf_settings: self.conf_settings,
        })
    }
}

#[allow(dead_code)]
fn zero_rat() -> BigRational { BigRational::zero() }

impl MakerOrder {
    fn available_amount(&self) -> MmNumber {
        let reserved: MmNumber = self.matches.iter().fold(
            MmNumber::from(BigRational::from_integer(0.into())),
            |reserved, (_, order_match)| reserved + order_match.reserved.get_base_amount(),
        );
        &self.max_base_vol - &reserved
    }

    fn is_cancellable(&self) -> bool { !self.has_ongoing_matches() }

    fn has_ongoing_matches(&self) -> bool {
        for (_, order_match) in self.matches.iter() {
            // if there's at least 1 ongoing match the order is not cancellable
            if order_match.connected.is_none() && order_match.connect.is_none() {
                return true;
            }
        }
        false
    }
}

impl Into<MakerOrder> for TakerOrder {
    fn into(self) -> MakerOrder {
        match self.request.action {
            TakerAction::Sell => MakerOrder {
                price: (self.request.get_rel_amount() / self.request.get_base_amount()),
                max_base_vol: self.request.get_base_amount(),
                min_base_vol: 0.into(),
                created_at: now_ms(),
                base: self.request.base,
                rel: self.request.rel,
                matches: HashMap::new(),
                started_swaps: Vec::new(),
                uuid: self.request.uuid,
                conf_settings: self.request.conf_settings,
            },
            // The "buy" taker order is recreated with reversed pair as Maker order is always considered as "sell"
            TakerAction::Buy => MakerOrder {
                price: (self.request.get_base_amount() / self.request.get_rel_amount()),
                max_base_vol: self.request.get_rel_amount(),
                min_base_vol: 0.into(),
                created_at: now_ms(),
                base: self.request.rel,
                rel: self.request.base,
                matches: HashMap::new(),
                started_swaps: Vec::new(),
                uuid: self.request.uuid,
                conf_settings: self.request.conf_settings.map(|s| s.reversed()),
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TakerConnect {
    taker_order_uuid: Uuid,
    maker_order_uuid: Uuid,
    method: String,
    sender_pubkey: H256Json,
    dest_pub_key: H256Json,
}

impl From<new_protocol::TakerConnect> for TakerConnect {
    fn from(message: new_protocol::TakerConnect) -> TakerConnect {
        TakerConnect {
            taker_order_uuid: message.taker_order_uuid.into(),
            maker_order_uuid: message.maker_order_uuid.into(),
            method: "".to_string(),
            sender_pubkey: Default::default(),
            dest_pub_key: Default::default(),
        }
    }
}

impl Into<new_protocol::OrdermatchMessage> for TakerConnect {
    fn into(self) -> new_protocol::OrdermatchMessage {
        new_protocol::OrdermatchMessage::TakerConnect(new_protocol::TakerConnect {
            taker_order_uuid: self.taker_order_uuid.into(),
            maker_order_uuid: self.maker_order_uuid.into(),
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(test, derive(Default))]
pub struct MakerReserved {
    base: String,
    rel: String,
    base_amount: BigDecimal,
    base_amount_rat: Option<BigRational>,
    rel_amount: BigDecimal,
    rel_amount_rat: Option<BigRational>,
    taker_order_uuid: Uuid,
    maker_order_uuid: Uuid,
    method: String,
    sender_pubkey: H256Json,
    dest_pub_key: H256Json,
    conf_settings: Option<OrderConfirmationsSettings>,
}

impl MakerReserved {
    fn get_base_amount(&self) -> MmNumber {
        match &self.base_amount_rat {
            Some(r) => r.clone().into(),
            None => self.base_amount.clone().into(),
        }
    }

    fn get_rel_amount(&self) -> MmNumber {
        match &self.rel_amount_rat {
            Some(r) => r.clone().into(),
            None => self.rel_amount.clone().into(),
        }
    }
}

impl MakerReserved {
    fn from_new_proto_and_pubkey(message: new_protocol::MakerReserved, sender_pubkey: H256Json) -> Self {
        MakerReserved {
            base: message.base,
            rel: message.rel,
            base_amount: message.base_amount.to_decimal(),
            rel_amount: message.rel_amount.to_decimal(),
            base_amount_rat: Some(message.base_amount.into()),
            rel_amount_rat: Some(message.rel_amount.into()),
            taker_order_uuid: message.taker_order_uuid.into(),
            maker_order_uuid: message.maker_order_uuid.into(),
            method: "".to_string(),
            sender_pubkey,
            dest_pub_key: Default::default(),
            conf_settings: Some(message.conf_settings),
        }
    }
}

impl Into<new_protocol::OrdermatchMessage> for MakerReserved {
    fn into(self) -> new_protocol::OrdermatchMessage {
        new_protocol::OrdermatchMessage::MakerReserved(new_protocol::MakerReserved {
            base_amount: self.get_base_amount(),
            rel_amount: self.get_rel_amount(),
            base: self.base,
            rel: self.rel,
            taker_order_uuid: self.taker_order_uuid.into(),
            maker_order_uuid: self.maker_order_uuid.into(),
            conf_settings: self.conf_settings.unwrap(),
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MakerConnected {
    taker_order_uuid: Uuid,
    maker_order_uuid: Uuid,
    method: String,
    sender_pubkey: H256Json,
    dest_pub_key: H256Json,
}

impl From<new_protocol::MakerConnected> for MakerConnected {
    fn from(message: new_protocol::MakerConnected) -> MakerConnected {
        MakerConnected {
            taker_order_uuid: message.taker_order_uuid.into(),
            maker_order_uuid: message.maker_order_uuid.into(),
            method: "".to_string(),
            sender_pubkey: Default::default(),
            dest_pub_key: Default::default(),
        }
    }
}

impl Into<new_protocol::OrdermatchMessage> for MakerConnected {
    fn into(self) -> new_protocol::OrdermatchMessage {
        new_protocol::OrdermatchMessage::MakerConnected(new_protocol::MakerConnected {
            taker_order_uuid: self.taker_order_uuid.into(),
            maker_order_uuid: self.maker_order_uuid.into(),
        })
    }
}

pub trait OrdermatchEventHandler {
    fn maker_order_created(&self, order: &MakerOrder);

    fn maker_order_updated(&self, order: &MakerOrder);

    fn maker_order_cancelled(&self, order: &MakerOrder);
}

async fn broadcast_maker_keep_alives(ctx: &MmArc) {
    let ordermatch_ctx: Arc<OrdermatchContext> = OrdermatchContext::from_ctx(&ctx).unwrap();
    let mut my_maker_orders = ordermatch_ctx.my_maker_orders.lock().await;
    for order in my_maker_orders.values_mut() {
        let msg = new_protocol::MakerOrderKeepAlive {
            uuid: order.uuid.into(),
            timestamp: now_ms() / 1000,
        };
        process_my_order_keep_alive(ctx, &msg, order).await;
        let topic = orderbook_topic(&order.base, &order.rel);
        broadcast_ordermatch_message(ctx, topic, msg);
    }
}

fn broadcast_ordermatch_message<T: Into<new_protocol::OrdermatchMessage>>(ctx: &MmArc, topic: String, msg: T) {
    let msg = msg.into();
    let key_pair = ctx.secp256k1_key_pair.or(&&|| panic!());
    let encoded_msg = encode_and_sign(&msg, &*key_pair.private().secret).unwrap();
    broadcast_p2p_msg(ctx, topic, encoded_msg);
}

struct OrdermatchContext {
    pub my_maker_orders: AsyncMutex<HashMap<Uuid, MakerOrder>>,
    pub my_taker_orders: AsyncMutex<HashMap<Uuid, TakerOrder>>,
    pub my_cancelled_orders: AsyncMutex<HashMap<Uuid, MakerOrder>>,
    /// A map from (base, rel)
    pub orderbook: AsyncMutex<HashMap<(String, String), HashMap<Uuid, PricePingRequest>>>,
    pub inactive_orders: AsyncMutex<HashMap<Uuid, PricePingRequest>>,
}

impl OrdermatchContext {
    /// Obtains a reference to this crate context, creating it if necessary.
    fn from_ctx(ctx: &MmArc) -> Result<Arc<OrdermatchContext>, String> {
        Ok(try_s!(from_ctx(&ctx.ordermatch_ctx, move || {
            Ok(OrdermatchContext {
                my_taker_orders: AsyncMutex::new(HashMap::default()),
                my_maker_orders: AsyncMutex::new(HashMap::default()),
                my_cancelled_orders: AsyncMutex::new(HashMap::default()),
                orderbook: AsyncMutex::new(HashMap::default()),
                inactive_orders: AsyncMutex::new(HashMap::default()),
            })
        })))
    }

    /// Obtains a reference to this crate context, creating it if necessary.
    #[allow(dead_code)]
    fn from_ctx_weak(ctx_weak: &MmWeak) -> Result<Arc<OrdermatchContext>, String> {
        let ctx = try_s!(MmArc::from_weak(ctx_weak).ok_or("Context expired"));
        Self::from_ctx(&ctx)
    }
}

#[cfg_attr(test, mockable)]
fn lp_connect_start_bob(ctx: MmArc, maker_match: MakerMatch, maker_order: MakerOrder) {
    spawn(async move {
        // aka "maker_loop"
        let taker_coin = match lp_coinfindᵃ(&ctx, &maker_match.reserved.rel).await {
            Ok(Some(c)) => c,
            Ok(None) => {
                log!("Coin " (maker_match.reserved.rel) " is not found/enabled");
                return;
            },
            Err(e) => {
                log!("!lp_coinfind(" (maker_match.reserved.rel) "): " (e));
                return;
            },
        };

        let maker_coin = match lp_coinfindᵃ(&ctx, &maker_match.reserved.base).await {
            Ok(Some(c)) => c,
            Ok(None) => {
                log!("Coin " (maker_match.reserved.base) " is not found/enabled");
                return;
            },
            Err(e) => {
                log!("!lp_coinfind(" (maker_match.reserved.base) "): " (e));
                return;
            },
        };
        let mut alice = bits256::default();
        alice.bytes = maker_match.request.sender_pubkey.0;
        let maker_amount = maker_match.reserved.get_base_amount().into();
        let taker_amount = maker_match.reserved.get_rel_amount().into();
        let privkey = &ctx.secp256k1_key_pair().private().secret;
        let my_persistent_pub = unwrap!(compressed_pub_key_from_priv_raw(&privkey[..], ChecksumType::DSHA256));
        let uuid = maker_match.request.uuid.to_string();
        let my_conf_settings = choose_maker_confs_and_notas(
            maker_order.conf_settings,
            &maker_match.request,
            &maker_coin,
            &taker_coin,
        );
        // detect atomic lock time version implicitly by conf_settings existence in taker request
        let atomic_locktime_v = match maker_match.request.conf_settings {
            Some(_) => {
                let other_conf_settings =
                    choose_taker_confs_and_notas(&maker_match.request, &maker_match.reserved, &maker_coin, &taker_coin);
                AtomicLocktimeVersion::V2 {
                    my_conf_settings,
                    other_conf_settings,
                }
            },
            None => AtomicLocktimeVersion::V1,
        };
        let lock_time = lp_atomic_locktime(maker_coin.ticker(), taker_coin.ticker(), atomic_locktime_v);
        log!("Entering the maker_swap_loop " (maker_coin.ticker()) "/" (taker_coin.ticker()) " with uuid: " (uuid));
        let maker_swap = MakerSwap::new(
            ctx.clone(),
            alice,
            maker_amount,
            taker_amount,
            my_persistent_pub,
            uuid,
            my_conf_settings,
            maker_coin,
            taker_coin,
            lock_time,
        );
        run_maker_swap(RunMakerSwapInput::StartNew(maker_swap), ctx).await;
    });
}

fn lp_connected_alice(ctx: MmArc, taker_request: TakerRequest, taker_match: TakerMatch) {
    spawn(async move {
        // aka "taker_loop"
        let mut maker = bits256::default();
        maker.bytes = taker_match.reserved.sender_pubkey.0;
        let taker_coin = match lp_coinfindᵃ(&ctx, &taker_match.reserved.rel).await {
            Ok(Some(c)) => c,
            Ok(None) => {
                log!("Coin " (taker_match.reserved.rel) " is not found/enabled");
                return;
            },
            Err(e) => {
                log!("!lp_coinfind(" (taker_match.reserved.rel) "): " (e));
                return;
            },
        };

        let maker_coin = match lp_coinfindᵃ(&ctx, &taker_match.reserved.base).await {
            Ok(Some(c)) => c,
            Ok(None) => {
                log!("Coin " (taker_match.reserved.base) " is not found/enabled");
                return;
            },
            Err(e) => {
                log!("!lp_coinfind(" (taker_match.reserved.base) "): " (e));
                return;
            },
        };

        let privkey = &ctx.secp256k1_key_pair().private().secret;
        let my_persistent_pub = unwrap!(compressed_pub_key_from_priv_raw(&privkey[..], ChecksumType::DSHA256));
        let maker_amount = taker_match.reserved.get_base_amount().into();
        let taker_amount = taker_match.reserved.get_rel_amount().into();
        let uuid = taker_match.reserved.taker_order_uuid.to_string();

        let my_conf_settings =
            choose_taker_confs_and_notas(&taker_request, &taker_match.reserved, &maker_coin, &taker_coin);
        // detect atomic lock time version implicitly by conf_settings existence in maker reserved
        let atomic_locktime_v = match taker_match.reserved.conf_settings {
            Some(_) => {
                let other_conf_settings = choose_maker_confs_and_notas(
                    taker_match.reserved.conf_settings,
                    &taker_request,
                    &maker_coin,
                    &taker_coin,
                );
                AtomicLocktimeVersion::V2 {
                    my_conf_settings,
                    other_conf_settings,
                }
            },
            None => AtomicLocktimeVersion::V1,
        };
        let locktime = lp_atomic_locktime(maker_coin.ticker(), taker_coin.ticker(), atomic_locktime_v);
        log!("Entering the taker_swap_loop " (maker_coin.ticker()) "/" (taker_coin.ticker())  " with uuid: " (uuid));
        let taker_swap = TakerSwap::new(
            ctx.clone(),
            maker,
            maker_amount,
            taker_amount,
            my_persistent_pub,
            uuid,
            my_conf_settings,
            maker_coin,
            taker_coin,
            locktime,
        );
        run_taker_swap(RunTakerSwapInput::StartNew(taker_swap), ctx).await
    });
}

pub async fn lp_ordermatch_loop(ctx: MmArc) {
    let mut last_price_broadcast = now_ms();

    loop {
        if ctx.is_stopping() {
            break;
        }
        let ordermatch_ctx = unwrap!(OrdermatchContext::from_ctx(&ctx));
        {
            let mut my_taker_orders = ordermatch_ctx.my_taker_orders.lock().await;
            let mut my_maker_orders = ordermatch_ctx.my_maker_orders.lock().await;
            let _my_cancelled_orders = ordermatch_ctx.my_cancelled_orders.lock().await;
            // transform the timed out and unmatched GTC taker orders to maker
            *my_taker_orders = my_taker_orders
                .drain()
                .filter_map(|(uuid, order)| {
                    if order.created_at + TAKER_ORDER_TIMEOUT * 1000 < now_ms() {
                        delete_my_taker_order(&ctx, &order);
                        if order.matches.is_empty() && order.order_type == OrderType::GoodTillCancelled {
                            let maker_order: MakerOrder = order.into();
                            spawn({
                                let maker_order = maker_order.clone();
                                let ctx = ctx.clone();
                                async move {
                                    maker_order_created_p2p_notify(ctx, &maker_order).await;
                                }
                            });
                            my_maker_orders.insert(uuid, maker_order);
                        }
                        None
                    } else {
                        Some((uuid, order))
                    }
                })
                .collect();
            // remove timed out unfinished matches to unlock the reserved amount
            my_maker_orders.iter_mut().for_each(|(_, order)| {
                order.matches = order
                    .matches
                    .drain()
                    .filter(|(_, order_match)| {
                        order_match.last_updated + ORDER_MATCH_TIMEOUT * 1000 > now_ms()
                            || order_match.connected.is_some()
                    })
                    .collect();
                save_my_maker_order(&ctx, order);
            });
            let mut cancelled = vec![];
            *my_maker_orders = my_maker_orders
                .drain()
                .filter_map(|(uuid, order)| {
                    let min_amount: BigDecimal = MIN_TRADING_VOL.parse().unwrap();
                    let min_amount: MmNumber = min_amount.into();
                    if order.available_amount() <= min_amount && !order.has_ongoing_matches() {
                        cancelled.push(order);
                        None
                    } else {
                        Some((uuid, order))
                    }
                })
                .collect();
            for order in cancelled {
                maker_order_cancelled_p2p_notify(ctx.clone(), &order).await;
            }
        }

        if now_ms() > last_price_broadcast + MIN_ORDER_KEEP_ALIVE_INTERVAL * 1000 {
            /*
            if let Err(e) = broadcast_my_maker_orders(&ctx).await {
                ctx.log
                    .log("", &[&"broadcast_my_maker_orders"], &format!("error {}", e));
            }
            */
            last_price_broadcast = now_ms();
            broadcast_maker_keep_alives(&ctx).await;
        }

        {
            // remove "timed out" orders from orderbook
            // ones that didn't receive an update for 30 seconds or more
            // store them in inactive orders temporary to avoid RepeatOrder request to network in case we start
            // receiving keep alive again
            let mut orderbook = ordermatch_ctx.orderbook.lock().await;
            let mut inactive = ordermatch_ctx.inactive_orders.lock().await;
            *orderbook = orderbook
                .drain()
                .filter_map(|((base, rel), mut pair_orderbook)| {
                    pair_orderbook = pair_orderbook
                        .drain()
                        .filter_map(|(pubkey, order)| {
                            if now_ms() / 1000 > order.timestamp + MAKER_ORDER_TIMEOUT {
                                inactive.insert(pubkey, order);
                                None
                            } else {
                                Some((pubkey, order))
                            }
                        })
                        .collect();
                    if pair_orderbook.is_empty() {
                        None
                    } else {
                        Some(((base, rel), pair_orderbook))
                    }
                })
                .collect();
        }

        Timer::sleep(0.777).await;
    }
}

async fn process_maker_reserved(ctx: MmArc, reserved_msg: MakerReserved) {
    let ordermatch_ctx = unwrap!(OrdermatchContext::from_ctx(&ctx));
    let our_public_id = unwrap!(ctx.public_id());

    if is_pubkey_banned(&ctx, &reserved_msg.sender_pubkey) {
        log!("Sender pubkey " [reserved_msg.sender_pubkey] " is banned");
        return;
    }

    let mut my_taker_orders = ordermatch_ctx.my_taker_orders.lock().await;
    let my_order = match my_taker_orders.entry(reserved_msg.taker_order_uuid) {
        Entry::Vacant(_) => {
            log!("Our node doesn't have the order with uuid "(
                reserved_msg.taker_order_uuid
            ));
            return;
        },
        Entry::Occupied(entry) => entry.into_mut(),
    };

    // send "connect" message if reserved message targets our pubkey AND
    // reserved amounts match our order AND order is NOT reserved by someone else (empty matches)
    if my_order.match_reserved(&reserved_msg) == MatchReservedResult::Matched && my_order.matches.is_empty() {
        let connect = TakerConnect {
            sender_pubkey: H256Json::from(our_public_id.bytes),
            dest_pub_key: reserved_msg.sender_pubkey.clone(),
            method: "connect".into(),
            taker_order_uuid: reserved_msg.taker_order_uuid,
            maker_order_uuid: reserved_msg.maker_order_uuid,
        };
        let topic = orderbook_topic(&my_order.request.base, &my_order.request.rel);
        broadcast_ordermatch_message(&ctx, topic, connect.clone());
        let taker_match = TakerMatch {
            reserved: reserved_msg,
            connect,
            connected: None,
            last_updated: now_ms(),
        };
        my_order
            .matches
            .insert(taker_match.reserved.maker_order_uuid, taker_match);
        save_my_taker_order(&ctx, &my_order);
    }
}

async fn process_maker_connected(ctx: MmArc, from_pubkey: H256Json, connected: MakerConnected) {
    let ordermatch_ctx = unwrap!(OrdermatchContext::from_ctx(&ctx));
    let _our_public_id = unwrap!(ctx.public_id());

    let mut my_taker_orders = ordermatch_ctx.my_taker_orders.lock().await;
    let my_order_entry = match my_taker_orders.entry(connected.taker_order_uuid) {
        Entry::Occupied(e) => e,
        Entry::Vacant(_) => {
            log!("Our node doesn't have the order with uuid "(connected.taker_order_uuid));
            return;
        },
    };
    let order_match = match my_order_entry.get().matches.get(&connected.maker_order_uuid) {
        Some(o) => o,
        None => {
            log!("Our node doesn't have the match with uuid "(connected.maker_order_uuid));
            return;
        },
    };

    if order_match.reserved.sender_pubkey != from_pubkey {
        log!("Connected message sender pubkey != reserved message sender pubkey");
        return;
    }
    // alice
    lp_connected_alice(ctx.clone(), my_order_entry.get().request.clone(), order_match.clone());
    // remove the matched order immediately
    delete_my_taker_order(&ctx, &my_order_entry.get());
    my_order_entry.remove();
}

async fn process_taker_request(ctx: MmArc, taker_request: TakerRequest) {
    if is_pubkey_banned(&ctx, &taker_request.sender_pubkey) {
        log!("Sender pubkey " [taker_request.sender_pubkey] " is banned");
        return;
    }

    let our_public_id = unwrap!(ctx.public_id());
    if our_public_id.bytes == taker_request.dest_pub_key.0 {
        log!("Skip the request originating from our pubkey");
        return;
    }

    println!("Processing request {:?}", taker_request);
    let ordermatch_ctx = unwrap!(OrdermatchContext::from_ctx(&ctx));
    let mut my_orders = ordermatch_ctx.my_maker_orders.lock().await;

    for (uuid, order) in my_orders.iter_mut() {
        if let OrderMatchResult::Matched((base_amount, rel_amount)) = match_order_and_request(order, &taker_request) {
            let base_coin = match lp_coinfind(&ctx, &order.base) {
                Ok(Some(c)) => c,
                _ => return, // attempt to match with deactivated coin
            };
            let rel_coin = match lp_coinfind(&ctx, &order.rel) {
                Ok(Some(c)) => c,
                _ => return, // attempt to match with deactivated coin
            };

            if !order.matches.contains_key(&taker_request.uuid) {
                let reserved = MakerReserved {
                    dest_pub_key: taker_request.sender_pubkey.clone(),
                    sender_pubkey: our_public_id.bytes.into(),
                    base: order.base.clone(),
                    base_amount: base_amount.clone().into(),
                    base_amount_rat: Some(base_amount.into()),
                    rel_amount: rel_amount.clone().into(),
                    rel_amount_rat: Some(rel_amount.into()),
                    rel: order.rel.clone(),
                    method: "reserved".into(),
                    taker_order_uuid: taker_request.uuid,
                    maker_order_uuid: *uuid,
                    conf_settings: order.conf_settings.or_else(|| {
                        Some(OrderConfirmationsSettings {
                            base_confs: base_coin.required_confirmations(),
                            base_nota: base_coin.requires_notarization(),
                            rel_confs: rel_coin.required_confirmations(),
                            rel_nota: rel_coin.requires_notarization(),
                        })
                    }),
                };
                let topic = orderbook_topic(&order.base, &order.rel);
                println!("Request matched sending reserved {:?}", reserved);
                broadcast_ordermatch_message(&ctx, topic, reserved.clone());
                let maker_match = MakerMatch {
                    request: taker_request,
                    reserved,
                    connect: None,
                    connected: None,
                    last_updated: now_ms(),
                };
                order.matches.insert(maker_match.request.uuid, maker_match);
                save_my_maker_order(&ctx, &order);
            }
            return;
        }
    }
}

async fn process_taker_connect(ctx: MmArc, sender_pubkey: H256Json, connect_msg: TakerConnect) {
    let ordermatch_ctx = unwrap!(OrdermatchContext::from_ctx(&ctx));
    let our_public_id = unwrap!(ctx.public_id());

    let mut maker_orders = ordermatch_ctx.my_maker_orders.lock().await;
    let my_order = match maker_orders.get_mut(&connect_msg.maker_order_uuid) {
        Some(o) => o,
        None => {
            log!("Our node doesn't have the order with uuid "(
                connect_msg.maker_order_uuid
            ));
            return;
        },
    };
    let order_match = match my_order.matches.get_mut(&connect_msg.taker_order_uuid) {
        Some(o) => o,
        None => {
            log!("Our node doesn't have the match with uuid "(
                connect_msg.taker_order_uuid
            ));
            return;
        },
    };
    if order_match.request.sender_pubkey != sender_pubkey {
        log!("Connect message sender pubkey != request message sender pubkey");
        return;
    }

    if order_match.connected.is_none() && order_match.connect.is_none() {
        let connected = MakerConnected {
            sender_pubkey: our_public_id.bytes.into(),
            dest_pub_key: connect_msg.sender_pubkey.clone(),
            taker_order_uuid: connect_msg.taker_order_uuid,
            maker_order_uuid: connect_msg.maker_order_uuid,
            method: "connected".into(),
        };
        let topic = orderbook_topic(&my_order.base, &my_order.rel);
        broadcast_ordermatch_message(&ctx, topic, connected.clone());
        order_match.connect = Some(connect_msg);
        order_match.connected = Some(connected);
        my_order.started_swaps.push(order_match.request.uuid);
        lp_connect_start_bob(ctx.clone(), order_match.clone(), my_order.clone());
        save_my_maker_order(&ctx, &my_order);
    }
}

pub fn lp_trade_command(ctx: MmArc, json: Json) {
    let method = json["method"].as_str();
    match method {
        Some("reserved") => {
            let reserved_msg: MakerReserved = match json::from_value(json) {
                Ok(r) => r,
                Err(_) => return,
            };
            block_on(process_maker_reserved(ctx, reserved_msg));
        },
        Some("connected") => {
            let connected: MakerConnected = match json::from_value(json.clone()) {
                Ok(c) => c,
                Err(_) => return,
            };
            block_on(process_maker_connected(ctx, connected.sender_pubkey.clone(), connected));
        },
        // bob
        Some("request") => {
            let taker_request: TakerRequest = match json::from_value(json.clone()) {
                Ok(r) => r,
                Err(_) => return,
            };
            block_on(process_taker_request(ctx, taker_request));
        },
        Some("connect") => {
            let connect_msg: TakerConnect = match json::from_value(json.clone()) {
                Ok(m) => m,
                Err(_) => return,
            };
            block_on(process_taker_connect(
                ctx,
                connect_msg.sender_pubkey.clone(),
                connect_msg,
            ));
        },
        _ => (),
    }
}

#[derive(Deserialize, Debug)]
pub struct AutoBuyInput {
    base: String,
    rel: String,
    price: MmNumber,
    volume: MmNumber,
    timeout: Option<u32>,
    /// Not used. Deprecated.
    duration: Option<u32>,
    // TODO: remove this field on API refactoring, method should be separated from params
    method: String,
    gui: Option<String>,
    #[serde(rename = "destpubkey")]
    #[serde(default)]
    dest_pub_key: H256Json,
    #[serde(default)]
    match_by: MatchBy,
    #[serde(default)]
    order_type: OrderType,
    base_confs: Option<u64>,
    base_nota: Option<bool>,
    rel_confs: Option<u64>,
    rel_nota: Option<bool>,
}

pub async fn buy(ctx: MmArc, req: Json) -> Result<Response<Vec<u8>>, String> {
    let input: AutoBuyInput = try_s!(json::from_value(req));
    if input.base == input.rel {
        return ERR!("Base and rel must be different coins");
    }
    let rel_coin = try_s!(lp_coinfindᵃ(&ctx, &input.rel).await);
    let rel_coin = try_s!(rel_coin.ok_or("Rel coin is not found or inactive"));
    let base_coin = try_s!(lp_coinfindᵃ(&ctx, &input.base).await);
    let base_coin: MmCoinEnum = try_s!(base_coin.ok_or("Base coin is not found or inactive"));
    let my_amount = &input.volume * &input.price;
    try_s!(check_balance_for_taker_swap(&ctx, &rel_coin, &base_coin, my_amount, None).await);
    try_s!(base_coin.can_i_spend_other_payment().compat().await);
    let res = try_s!(lp_auto_buy(&ctx, &base_coin, &rel_coin, input).await).into_bytes();
    Ok(try_s!(Response::builder().body(res)))
}

pub async fn sell(ctx: MmArc, req: Json) -> Result<Response<Vec<u8>>, String> {
    let input: AutoBuyInput = try_s!(json::from_value(req));
    if input.base == input.rel {
        return ERR!("Base and rel must be different coins");
    }
    let base_coin = try_s!(lp_coinfindᵃ(&ctx, &input.base).await);
    let base_coin = try_s!(base_coin.ok_or("Base coin is not found or inactive"));
    let rel_coin = try_s!(lp_coinfindᵃ(&ctx, &input.rel).await);
    let rel_coin = try_s!(rel_coin.ok_or("Rel coin is not found or inactive"));
    try_s!(check_balance_for_taker_swap(&ctx, &base_coin, &rel_coin, input.volume.clone(), None).await);
    try_s!(rel_coin.can_i_spend_other_payment().compat().await);
    let res = try_s!(lp_auto_buy(&ctx, &base_coin, &rel_coin, input).await).into_bytes();
    Ok(try_s!(Response::builder().body(res)))
}

/// Created when maker order is matched with taker request
#[derive(Clone, Debug, Deserialize, Serialize)]
struct MakerMatch {
    request: TakerRequest,
    reserved: MakerReserved,
    connect: Option<TakerConnect>,
    connected: Option<MakerConnected>,
    last_updated: u64,
}

/// Created upon taker request broadcast
#[derive(Clone, Debug, Deserialize, Serialize)]
struct TakerMatch {
    reserved: MakerReserved,
    connect: TakerConnect,
    connected: Option<MakerConnected>,
    last_updated: u64,
}

pub async fn lp_auto_buy(
    ctx: &MmArc,
    base_coin: &MmCoinEnum,
    rel_coin: &MmCoinEnum,
    input: AutoBuyInput,
) -> Result<String, String> {
    if input.price < MmNumber::from(BigRational::new(1.into(), 100_000_000.into())) {
        return ERR!("Price is too low, minimum is 0.00000001");
    }

    let action = match Some(input.method.as_ref()) {
        Some("buy") => TakerAction::Buy,
        Some("sell") => TakerAction::Sell,
        _ => return ERR!("Auto buy must be called only from buy/sell RPC methods"),
    };
    let topic = orderbook_topic(&input.base, &input.rel);
    subscribe_to_topic(ctx, topic.clone()).await;
    let ordermatch_ctx = try_s!(OrdermatchContext::from_ctx(&ctx));
    let mut my_taker_orders = ordermatch_ctx.my_taker_orders.lock().await;
    let our_public_id = try_s!(ctx.public_id());
    let rel_volume = &input.volume * &input.price;
    let conf_settings = OrderConfirmationsSettings {
        base_confs: input.base_confs.unwrap_or_else(|| base_coin.required_confirmations()),
        base_nota: input.base_nota.unwrap_or_else(|| base_coin.requires_notarization()),
        rel_confs: input.rel_confs.unwrap_or_else(|| rel_coin.required_confirmations()),
        rel_nota: input.rel_nota.unwrap_or_else(|| rel_coin.requires_notarization()),
    };
    let request_builder = TakerRequestBuilder::default()
        .with_base_coin(input.base)
        .with_rel_coin(input.rel)
        .with_base_amount(input.volume)
        .with_rel_amount(rel_volume)
        .with_action(action)
        .with_match_by(input.match_by)
        .with_conf_settings(conf_settings)
        .with_sender_pubkey(H256Json::from(our_public_id.bytes));
    let request = try_s!(request_builder.build());
    broadcast_ordermatch_message(&ctx, topic, request.clone());
    let result = json!({ "result": request }).to_string();
    let order = TakerOrder {
        created_at: now_ms(),
        matches: HashMap::new(),
        request,
        order_type: input.order_type,
    };
    save_my_taker_order(ctx, &order);
    my_taker_orders.insert(order.request.uuid, order);
    drop(my_taker_orders);
    Ok(result)
}

fn price_ping_sig_hash(timestamp: u32, pubsecp: &[u8], pubkey: &[u8], base: &[u8], rel: &[u8], price64: u64) -> H256 {
    let mut input = vec![];
    input.extend_from_slice(&timestamp.to_le_bytes());
    input.extend_from_slice(pubsecp);
    input.extend_from_slice(pubkey);
    input.extend_from_slice(base);
    input.extend_from_slice(rel);
    input.extend_from_slice(&price64.to_le_bytes());
    sha256(&input)
}

#[derive(Debug, Deserialize, Serialize)]
struct PricePingRequest {
    method: String,
    pubkey: String,
    base: String,
    rel: String,
    price: BigDecimal,
    price_rat: Option<MmNumber>,
    price64: String,
    timestamp: u64,
    pubsecp: String,
    sig: String,
    // TODO rename, it's called "balance", but it's actual meaning is max available volume to trade
    #[serde(rename = "bal")]
    balance: BigDecimal,
    balance_rat: Option<MmNumber>,
    uuid: Option<Uuid>,
    peer_id: String,
    initial_message: Vec<u8>,
}

impl PricePingRequest {
    fn new(ctx: &MmArc, order: &MakerOrder, balance: BigDecimal) -> Result<PricePingRequest, String> {
        let public_id = try_s!(ctx.public_id());
        // not used anywhere
        let price64 = 0;
        let timestamp = now_ms() / 1000;
        let sig_hash = price_ping_sig_hash(
            timestamp as u32,
            &**ctx.secp256k1_key_pair().public(),
            &public_id.bytes,
            order.base.as_bytes(),
            order.rel.as_bytes(),
            price64,
        );

        let sig = try_s!(ctx.secp256k1_key_pair().private().sign(&sig_hash));

        let available_amount: BigRational = order.available_amount().into();
        let min_amount = BigRational::new(777.into(), 100_000.into());
        let max_volume = if available_amount > min_amount {
            let my_balance = from_dec_to_ratio(balance);
            if available_amount <= my_balance && available_amount > BigRational::from_integer(0.into()) {
                available_amount
            } else {
                my_balance
            }
        } else {
            BigRational::from_integer(0.into())
        };

        Ok(PricePingRequest {
            method: "postprice".into(),
            pubkey: hex::encode(&public_id.bytes),
            base: order.base.clone(),
            rel: order.rel.clone(),
            price64: price64.to_string(),
            price: order.price.to_decimal(),
            price_rat: Some(order.price.clone()),
            timestamp,
            pubsecp: hex::encode(&**ctx.secp256k1_key_pair().public()),
            sig: hex::encode(&*sig),
            balance: from_ratio_to_dec(&max_volume),
            balance_rat: Some(max_volume.into()),
            uuid: Some(order.uuid),
            peer_id: ctx.peer_id.or(&|| panic!()).clone(),
            initial_message: vec![],
        })
    }
}

pub async fn lp_post_price_recv(ctx: &MmArc, req: Json) -> Result<(), String> {
    let req: PricePingRequest = try_s!(json::from_value(req));
    let signature: Signature = try_s!(req.sig.parse());
    let pubkey_bytes = try_s!(hex::decode(&req.pubsecp));
    // return success response to avoid excessive logging of
    // RPC error response: lp_ordermatch:852] sender pubkey 03eb26aab2e22fd2507042d1c472b3f973d629d295d391faf7b68ac5b85197ec80 is banned""
    // messages
    if is_pubkey_banned(ctx, &H256Json::from(&pubkey_bytes[1..])) {
        return ERR!("sender pubkey {} is banned", req.pubsecp);
    }
    let pub_secp = try_s!(Public::from_slice(&pubkey_bytes));
    let pubkey = try_s!(hex::decode(&req.pubkey));
    let sig_hash = price_ping_sig_hash(
        req.timestamp as u32,
        &*pub_secp,
        &pubkey,
        req.base.as_bytes(),
        req.rel.as_bytes(),
        try_s!(req.price64.parse()),
    );
    let sig_check = try_s!(pub_secp.verify(&sig_hash, &signature));
    if sig_check {
        // identify the order by first 16 bytes of node pubkey to keep backwards-compatibility
        // TODO remove this when all nodes are updated
        let mut bytes = [0; 16];
        bytes.copy_from_slice(&pubkey[..16]);
        let uuid = req.uuid.unwrap_or_else(|| Uuid::from_bytes(bytes));
        let ordermatch_ctx: Arc<OrdermatchContext> = try_s!(OrdermatchContext::from_ctx(ctx));
        let mut orderbook = ordermatch_ctx.orderbook.lock().await;
        match orderbook.entry((req.base.clone(), req.rel.clone())) {
            Entry::Vacant(pair_orders) => {
                if req.balance > 0.into() && req.price > 0.into() {
                    let mut orders = HashMap::new();
                    orders.insert(uuid, req);
                    pair_orders.insert(orders);
                }
            },
            Entry::Occupied(mut pair_orders) => match pair_orders.get_mut().entry(uuid) {
                Entry::Vacant(order) => {
                    if req.balance > 0.into() && req.price > 0.into() {
                        order.insert(req);
                    }
                },
                Entry::Occupied(mut order) => {
                    if req.balance > 0.into() {
                        order.insert(req);
                    } else {
                        order.remove();
                    }
                },
            },
        }
        Ok(())
    } else {
        ERR!("price ping invalid signature")
    }
}

fn lp_send_price_ping(req: &PricePingRequest, ctx: &MmArc) -> Result<(), String> {
    let req_string = try_s!(json::to_string(req));

    // TODO this is required to process the set price message on our own node, it's the easiest way now
    //      there might be a better way of doing this so we should consider refactoring
    let req_value = try_s!(json::to_value(req));
    let ctxʹ = ctx.clone();
    spawn(async move {
        let rc = lp_post_price_recv(&ctxʹ, req_value).await;
        if let Err(err) = rc {
            log!("!lp_post_price_recv: "(err))
        }
    });

    ctx.broadcast_p2p_msg(orderbook_topic(&req.base, &req.rel), req_string.into_bytes());
    Ok(())
}

fn one() -> u8 { 1 }

fn get_true() -> bool { true }

#[derive(Deserialize)]
struct SetPriceReq {
    base: String,
    rel: String,
    price: MmNumber,
    #[serde(default)]
    max: bool,
    #[allow(dead_code)]
    #[serde(default = "one")]
    broadcast: u8,
    #[serde(default)]
    volume: MmNumber,
    #[serde(default = "get_true")]
    cancel_previous: bool,
    base_confs: Option<u64>,
    base_nota: Option<bool>,
    rel_confs: Option<u64>,
    rel_nota: Option<bool>,
}

pub async fn set_price(ctx: MmArc, req: Json) -> Result<Response<Vec<u8>>, String> {
    let req: SetPriceReq = try_s!(json::from_value(req));

    let base_coin: MmCoinEnum = match try_s!(lp_coinfindᵃ(&ctx, &req.base).await) {
        Some(coin) => coin,
        None => return ERR!("Base coin {} is not found", req.base),
    };

    let rel_coin: MmCoinEnum = match try_s!(lp_coinfindᵃ(&ctx, &req.rel).await) {
        Some(coin) => coin,
        None => return ERR!("Rel coin {} is not found", req.rel),
    };

    let my_balance = try_s!(base_coin.my_balance().compat().await);
    let volume = if req.max {
        // use entire balance deducting the locked amount and trade fee if it's paid with base coin,
        // skipping "check_balance_for_maker_swap"
        let trade_fee = try_s!(base_coin.get_trade_fee().compat().await);
        let mut vol = MmNumber::from(my_balance) - get_locked_amount(&ctx, base_coin.ticker(), &trade_fee);
        if trade_fee.coin == base_coin.ticker() {
            vol = vol - trade_fee.amount;
        }
        vol
    } else {
        try_s!(check_balance_for_maker_swap(&ctx, &base_coin, req.volume.clone(), None).await);
        req.volume.clone()
    };
    try_s!(rel_coin.can_i_spend_other_payment().compat().await);

    let ordermatch_ctx = try_s!(OrdermatchContext::from_ctx(&ctx));
    let conf_settings = OrderConfirmationsSettings {
        base_confs: req.base_confs.unwrap_or_else(|| base_coin.required_confirmations()),
        base_nota: req.base_nota.unwrap_or_else(|| base_coin.requires_notarization()),
        rel_confs: req.rel_confs.unwrap_or_else(|| rel_coin.required_confirmations()),
        rel_nota: req.rel_nota.unwrap_or_else(|| rel_coin.requires_notarization()),
    };
    let builder = MakerOrderBuilder::default()
        .with_base_coin(req.base)
        .with_rel_coin(req.rel)
        .with_max_base_vol(volume)
        .with_price(req.price)
        .with_conf_settings(conf_settings);

    let new_order = try_s!(builder.build());
    subscribe_to_topic(&ctx, orderbook_topic(&new_order.base, &new_order.rel)).await;
    maker_order_created_p2p_notify(ctx.clone(), &new_order).await;
    let mut my_orders = ordermatch_ctx.my_maker_orders.lock().await;
    if req.cancel_previous {
        let mut cancelled = vec![];
        // remove the previous orders if there're some to allow multiple setprice call per pair
        // it's common use case now as `autoprice` doesn't work with new ordermatching and
        // MM2 users request the coins price from aggregators by their own scripts issuing
        // repetitive setprice calls with new price
        *my_orders = my_orders
            .drain()
            .filter_map(|(uuid, order)| {
                let to_delete = order.base == new_order.base && order.rel == new_order.rel;
                if to_delete {
                    delete_my_maker_order(&ctx, &order);
                    cancelled.push(order);
                    None
                } else {
                    Some((uuid, order))
                }
            })
            .collect();
        for order in cancelled {
            maker_order_cancelled_p2p_notify(ctx.clone(), &order).await;
        }
    }
    let res = try_s!(json::to_vec(&json!({ "result": new_order })));
    my_orders.insert(new_order.uuid, new_order);
    Ok(try_s!(Response::builder().body(res)))
}

pub async fn broadcast_my_maker_orders(ctx: &MmArc) -> Result<(), String> {
    let ordermatch_ctx = try_s!(OrdermatchContext::from_ctx(ctx));
    let my_orders = ordermatch_ctx.my_maker_orders.lock().await.clone();
    for (_, order) in my_orders {
        let base_coin = match try_s!(lp_coinfindᵃ(ctx, &order.base).await) {
            Some(coin) => coin,
            None => {
                ctx.log.log(
                    "",
                    &[&"broadcast_my_maker_orders", &order.base, &order.rel],
                    "base coin is not active yet",
                );
                continue;
            },
        };

        let _rel_coin = match try_s!(lp_coinfindᵃ(ctx, &order.rel).await) {
            Some(coin) => coin,
            None => {
                ctx.log.log(
                    "",
                    &[&"broadcast_my_maker_orders", &order.base, &order.rel],
                    "rel coin is not active yet",
                );
                continue;
            },
        };

        let balance = match base_coin.my_balance().compat().await {
            Ok(b) => b,
            Err(e) => {
                ctx.log.log(
                    "",
                    &[&"broadcast_my_maker_orders", &order.base, &order.rel],
                    &format!("failed to get balance of base coin: {}", e),
                );
                continue;
            },
        };

        if balance >= MIN_TRADING_VOL.parse().unwrap() {
            let ping = match PricePingRequest::new(ctx, &order, balance) {
                Ok(p) => p,
                Err(e) => {
                    ctx.log.log(
                        "",
                        &[&"broadcast_my_maker_orders", &order.base, &order.rel],
                        &format!("ping request creation failed {}", e),
                    );
                    continue;
                },
            };

            if let Err(e) = lp_send_price_ping(&ping, ctx) {
                ctx.log.log(
                    "",
                    &[&"broadcast_my_maker_orders", &order.base, &order.rel],
                    &format!("ping request send failed {}", e),
                );
                continue;
            }
        } else {
            // cancel the order if available balance is lower than MIN_TRADING_VOL
            let order = ordermatch_ctx.my_maker_orders.lock().await.remove(&order.uuid);
            if let Some(mut order) = order {
                // TODO cancelling means setting volume to 0 as of now, should refactor
                order.max_base_vol = 0.into();
                maker_order_cancelled_p2p_notify(ctx.clone(), &order).await;
            }
        }
    }

    Ok(())
}

/// Result of match_order_and_request function
#[derive(Debug, PartialEq)]
enum OrderMatchResult {
    /// Order and request matched, contains base and rel resulting amounts
    Matched((MmNumber, MmNumber)),
    /// Orders didn't match
    NotMatched,
}

/// Attempts to match the Maker's order and Taker's request
fn match_order_and_request(maker: &MakerOrder, taker: &TakerRequest) -> OrderMatchResult {
    let taker_base_amount: MmNumber = taker.get_base_amount();
    let taker_rel_amount: MmNumber = taker.get_rel_amount();

    match taker.action {
        TakerAction::Buy => {
            if maker.base == taker.base
                && maker.rel == taker.rel
                && taker_base_amount <= maker.available_amount()
                && taker_base_amount >= maker.min_base_vol
            {
                let taker_price = &taker_rel_amount / &taker_base_amount;
                if taker_price >= maker.price {
                    OrderMatchResult::Matched((taker_base_amount.clone(), &taker_base_amount * &maker.price))
                } else {
                    OrderMatchResult::NotMatched
                }
            } else {
                OrderMatchResult::NotMatched
            }
        },
        TakerAction::Sell => {
            if maker.base == taker.rel
                && maker.rel == taker.base
                && taker_rel_amount <= maker.available_amount()
                && taker_rel_amount >= maker.min_base_vol
            {
                let taker_price = &taker_base_amount / &taker_rel_amount;
                if taker_price >= maker.price {
                    OrderMatchResult::Matched((&taker_base_amount / &maker.price, taker_base_amount))
                } else {
                    OrderMatchResult::NotMatched
                }
            } else {
                OrderMatchResult::NotMatched
            }
        },
    }
}

#[derive(Deserialize)]
struct OrderStatusReq {
    uuid: Uuid,
}

pub async fn order_status(ctx: MmArc, req: Json) -> Result<Response<Vec<u8>>, String> {
    let req: OrderStatusReq = try_s!(json::from_value(req));

    let ordermatch_ctx = try_s!(OrdermatchContext::from_ctx(&ctx));
    let maker_orders = ordermatch_ctx.my_maker_orders.lock().await;
    if let Some(order) = maker_orders.get(&req.uuid) {
        let res = json!({
            "type": "Maker",
            "order": MakerOrderForRpc::from(order),
        });
        return Response::builder()
            .body(json::to_vec(&res).unwrap())
            .map_err(|e| ERRL!("{}", e));
    }

    let taker_orders = ordermatch_ctx.my_taker_orders.lock().await;
    if let Some(order) = taker_orders.get(&req.uuid) {
        let res = json!({
            "type": "Taker",
            "order": TakerOrderForRpc::from(order),
        });
        return Response::builder()
            .body(json::to_vec(&res).unwrap())
            .map_err(|e| ERRL!("{}", e));
    }

    let res = json!({
        "error": format!("Order with uuid {} is not found", req.uuid),
    });
    Response::builder()
        .status(404)
        .body(json::to_vec(&res).unwrap())
        .map_err(|e| ERRL!("{}", e))
}

#[derive(Deserialize)]
struct CancelOrderReq {
    uuid: Uuid,
}

pub async fn cancel_order(ctx: MmArc, req: Json) -> Result<Response<Vec<u8>>, String> {
    let req: CancelOrderReq = try_s!(json::from_value(req));

    let ordermatch_ctx = try_s!(OrdermatchContext::from_ctx(&ctx));
    let mut maker_orders = ordermatch_ctx.my_maker_orders.lock().await;
    match maker_orders.entry(req.uuid) {
        Entry::Occupied(order) => {
            if !order.get().is_cancellable() {
                return ERR!("Order {} is being matched now, can't cancel", req.uuid);
            }
            let _cancelled_orders = ordermatch_ctx.my_cancelled_orders.lock().await;
            let order = order.remove();
            maker_order_cancelled_p2p_notify(ctx, &order).await;
            let res = json!({
                "result": "success"
            });
            return Response::builder()
                .body(json::to_vec(&res).unwrap())
                .map_err(|e| ERRL!("{}", e));
        },
        // look for taker order with provided uuid
        Entry::Vacant(_) => (),
    }

    let mut taker_orders = ordermatch_ctx.my_taker_orders.lock().await;
    match taker_orders.entry(req.uuid) {
        Entry::Occupied(order) => {
            if !order.get().is_cancellable() {
                return ERR!("Order {} is being matched now, can't cancel", req.uuid);
            }
            let order = order.remove();
            delete_my_taker_order(&ctx, &order);
            let res = json!({
                "result": "success"
            });
            return Response::builder()
                .body(json::to_vec(&res).unwrap())
                .map_err(|e| ERRL!("{}", e));
        },
        // error is returned
        Entry::Vacant(_) => (),
    }

    let res = json!({
        "error": format!("Order with uuid {} is not found", req.uuid),
    });
    Response::builder()
        .status(404)
        .body(json::to_vec(&res).unwrap())
        .map_err(|e| ERRL!("{}", e))
}

#[derive(Serialize)]
struct MakerOrderForRpc<'a> {
    #[serde(flatten)]
    order: &'a MakerOrder,
    cancellable: bool,
    available_amount: BigDecimal,
}

impl<'a> From<&'a MakerOrder> for MakerOrderForRpc<'a> {
    fn from(order: &'a MakerOrder) -> MakerOrderForRpc {
        MakerOrderForRpc {
            order,
            cancellable: order.is_cancellable(),
            available_amount: order.available_amount().into(),
        }
    }
}

#[derive(Serialize)]
struct TakerOrderForRpc<'a> {
    #[serde(flatten)]
    order: &'a TakerOrder,
    cancellable: bool,
}

impl<'a> From<&'a TakerOrder> for TakerOrderForRpc<'a> {
    fn from(order: &'a TakerOrder) -> TakerOrderForRpc {
        TakerOrderForRpc {
            order,
            cancellable: order.is_cancellable(),
        }
    }
}

pub async fn my_orders(ctx: MmArc) -> Result<Response<Vec<u8>>, String> {
    let ordermatch_ctx = try_s!(OrdermatchContext::from_ctx(&ctx));
    let maker_orders = ordermatch_ctx.my_maker_orders.lock().await;
    let taker_orders = ordermatch_ctx.my_taker_orders.lock().await;
    let maker_orders_for_rpc: HashMap<_, _> = maker_orders
        .iter()
        .map(|(uuid, order)| (uuid, MakerOrderForRpc::from(order)))
        .collect();
    let taker_orders_for_rpc: HashMap<_, _> = taker_orders
        .iter()
        .map(|(uuid, order)| (uuid, TakerOrderForRpc::from(order)))
        .collect();
    let res = json!({
        "result": {
            "maker_orders": maker_orders_for_rpc,
            "taker_orders": taker_orders_for_rpc,
        }
    });
    Response::builder()
        .body(json::to_vec(&res).unwrap())
        .map_err(|e| ERRL!("{}", e))
}

pub fn my_maker_orders_dir(ctx: &MmArc) -> PathBuf { ctx.dbdir().join("ORDERS").join("MY").join("MAKER") }

fn my_taker_orders_dir(ctx: &MmArc) -> PathBuf { ctx.dbdir().join("ORDERS").join("MY").join("TAKER") }

fn my_maker_order_file_path(ctx: &MmArc, uuid: &Uuid) -> PathBuf {
    my_maker_orders_dir(ctx).join(format!("{}.json", uuid))
}

fn my_taker_order_file_path(ctx: &MmArc, uuid: &Uuid) -> PathBuf {
    my_taker_orders_dir(ctx).join(format!("{}.json", uuid))
}

fn save_my_maker_order(ctx: &MmArc, order: &MakerOrder) {
    let path = my_maker_order_file_path(ctx, &order.uuid);
    let content = unwrap!(json::to_vec(order));
    unwrap!(write(&path, &content));
}

fn save_my_taker_order(ctx: &MmArc, order: &TakerOrder) {
    let path = my_taker_order_file_path(ctx, &order.request.uuid);
    let content = unwrap!(json::to_vec(order));
    unwrap!(write(&path, &content));
}

#[cfg_attr(test, mockable)]
fn delete_my_maker_order(ctx: &MmArc, order: &MakerOrder) {
    let path = my_maker_order_file_path(ctx, &order.uuid);
    match remove_file(&path) {
        Ok(_) => (),
        Err(e) => log!("Warning, could not remove order file " (path.display()) ", error " (e)),
    }
}

#[cfg_attr(test, mockable)]
fn delete_my_taker_order(ctx: &MmArc, order: &TakerOrder) {
    let path = my_taker_order_file_path(ctx, &order.request.uuid);
    match remove_file(&path) {
        Ok(_) => (),
        Err(e) => log!("Warning, could not remove order file " (path.display()) ", error " (e)),
    }
}

pub async fn orders_kick_start(ctx: &MmArc) -> Result<HashSet<String>, String> {
    let mut coins = HashSet::new();
    let ordermatch_ctx = try_s!(OrdermatchContext::from_ctx(ctx));
    let mut maker_orders = ordermatch_ctx.my_maker_orders.lock().await;
    let maker_entries = try_s!(json_dir_entries(&my_maker_orders_dir(&ctx)));

    maker_entries.iter().for_each(|entry| {
        if let Ok(order) = json::from_slice::<MakerOrder>(&slurp(&entry.path())) {
            coins.insert(order.base.clone());
            coins.insert(order.rel.clone());
            maker_orders.insert(order.uuid, order);
        }
    });

    let mut taker_orders = ordermatch_ctx.my_taker_orders.lock().await;
    let taker_entries: Vec<DirEntry> = try_s!(json_dir_entries(&my_taker_orders_dir(&ctx)));

    taker_entries.iter().for_each(|entry| {
        if let Ok(order) = json::from_slice::<TakerOrder>(&slurp(&entry.path())) {
            coins.insert(order.request.base.clone());
            coins.insert(order.request.rel.clone());
            taker_orders.insert(order.request.uuid, order);
        }
    });
    Ok(coins)
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum CancelBy {
    /// All orders of current node
    All,
    /// All orders of specific pair
    Pair { base: String, rel: String },
    /// All orders using the coin ticker as base or rel
    Coin { ticker: String },
}

pub async fn cancel_orders_by(ctx: &MmArc, cancel_by: CancelBy) -> Result<(Vec<Uuid>, Vec<Uuid>), String> {
    let mut cancelled = vec![];
    let mut currently_matching = vec![];

    let ordermatch_ctx = try_s!(OrdermatchContext::from_ctx(ctx));
    let mut maker_orders = ordermatch_ctx.my_maker_orders.lock().await;
    let mut taker_orders = ordermatch_ctx.my_taker_orders.lock().await;
    let mut my_cancelled_orders = ordermatch_ctx.my_cancelled_orders.lock().await;

    macro_rules! cancel_maker_if_true {
        ($e: expr, $uuid: ident, $order: ident) => {
            if $e {
                if $order.is_cancellable() {
                    delete_my_maker_order(&ctx, &$order);
                    my_cancelled_orders.insert($uuid, $order);
                    cancelled.push($uuid);
                    None
                } else {
                    currently_matching.push($uuid);
                    Some(($uuid, $order))
                }
            } else {
                Some(($uuid, $order))
            }
        };
    }

    macro_rules! cancel_taker_if_true {
        ($e: expr, $uuid: ident, $order: ident) => {
            if $e {
                if $order.is_cancellable() {
                    delete_my_taker_order(&ctx, &$order);
                    cancelled.push($uuid);
                    None
                } else {
                    currently_matching.push($uuid);
                    Some(($uuid, $order))
                }
            } else {
                Some(($uuid, $order))
            }
        };
    }

    match cancel_by {
        CancelBy::All => {
            *maker_orders = maker_orders
                .drain()
                .filter_map(|(uuid, order)| cancel_maker_if_true!(true, uuid, order))
                .collect();
            *taker_orders = taker_orders
                .drain()
                .filter_map(|(uuid, order)| cancel_taker_if_true!(true, uuid, order))
                .collect();
        },
        CancelBy::Pair { base, rel } => {
            *maker_orders = maker_orders
                .drain()
                .filter_map(|(uuid, order)| cancel_maker_if_true!(order.base == base && order.rel == rel, uuid, order))
                .collect();
            *taker_orders = taker_orders
                .drain()
                .filter_map(|(uuid, order)| {
                    cancel_taker_if_true!(order.request.base == base && order.request.rel == rel, uuid, order)
                })
                .collect();
        },
        CancelBy::Coin { ticker } => {
            *maker_orders = maker_orders
                .drain()
                .filter_map(|(uuid, order)| {
                    cancel_maker_if_true!(order.base == ticker || order.rel == ticker, uuid, order)
                })
                .collect();
            *taker_orders = taker_orders
                .drain()
                .filter_map(|(uuid, order)| {
                    cancel_taker_if_true!(order.request.base == ticker || order.request.rel == ticker, uuid, order)
                })
                .collect();
        },
    };
    Ok((cancelled, currently_matching))
}

pub async fn cancel_all_orders(ctx: MmArc, req: Json) -> Result<Response<Vec<u8>>, String> {
    let cancel_by: CancelBy = try_s!(json::from_value(req["cancel_by"].clone()));

    let (cancelled, currently_matching) = try_s!(cancel_orders_by(&ctx, cancel_by).await);

    let res = json!({
        "result": {
            "cancelled": cancelled,
            "currently_matching": currently_matching,
        }
    });
    Response::builder()
        .body(json::to_vec(&res).unwrap())
        .map_err(|e| ERRL!("{}", e))
}

#[derive(Serialize)]
pub struct OrderbookEntry {
    coin: String,
    address: String,
    price: BigDecimal,
    price_rat: BigRational,
    price_fraction: Fraction,
    #[serde(rename = "maxvolume")]
    max_volume: BigDecimal,
    max_volume_rat: BigRational,
    max_volume_fraction: Fraction,
    pubkey: String,
    age: i64,
    zcredits: u64,
    uuid: Uuid,
    is_mine: bool,
}

#[derive(Serialize)]
pub struct OrderbookResponse {
    #[serde(rename = "askdepth")]
    ask_depth: u32,
    asks: Vec<OrderbookEntry>,
    base: String,
    #[serde(rename = "biddepth")]
    bid_depth: u32,
    bids: Vec<OrderbookEntry>,
    netid: u16,
    #[serde(rename = "numasks")]
    num_asks: usize,
    #[serde(rename = "numbids")]
    num_bids: usize,
    rel: String,
    timestamp: u64,
}

#[derive(Deserialize)]
struct OrderbookReq {
    base: String,
    rel: String,
}

pub async fn orderbook(ctx: MmArc, req: Json) -> Result<Response<Vec<u8>>, String> {
    let req: OrderbookReq = try_s!(json::from_value(req));
    if req.base == req.rel {
        return ERR!("Base and rel must be different coins");
    }
    let rel_coin = try_s!(lp_coinfindᵃ(&ctx, &req.rel).await);
    let rel_coin = try_s!(rel_coin.ok_or("Rel coin is not found or inactive"));
    let base_coin = try_s!(lp_coinfindᵃ(&ctx, &req.base).await);
    let base_coin: MmCoinEnum = try_s!(base_coin.ok_or("Base coin is not found or inactive"));
    subscribe_to_topic(&ctx, orderbook_topic(&req.base, &req.rel)).await;
    Timer::sleep(3.).await;
    let ordermatch_ctx: Arc<OrdermatchContext> = try_s!(OrdermatchContext::from_ctx(&ctx));
    let orderbook = ordermatch_ctx.orderbook.lock().await;
    let my_pubsecp = hex::encode(&**ctx.secp256k1_key_pair().public());
    let asks = match orderbook.get(&(req.base.clone(), req.rel.clone())) {
        Some(asks) => {
            let mut orderbook_entries = vec![];
            for (uuid, ask) in asks.iter() {
                log!("Ask size {}"(std::mem::size_of_val(ask)));
                orderbook_entries.push(OrderbookEntry {
                    coin: req.base.clone(),
                    address: try_s!(base_coin.address_from_pubkey_str(&ask.pubsecp)),
                    price: ask.price.clone(),
                    price_rat: ask
                        .price_rat
                        .as_ref()
                        .map(|p| p.to_ratio())
                        .unwrap_or_else(|| from_dec_to_ratio(ask.price.clone())),
                    price_fraction: ask
                        .price_rat
                        .as_ref()
                        .map(|p| p.to_fraction())
                        .unwrap_or_else(|| ask.price.clone().into()),
                    max_volume: ask.balance.clone(),
                    max_volume_rat: ask
                        .balance_rat
                        .as_ref()
                        .map(|p| p.to_ratio())
                        .unwrap_or_else(|| from_dec_to_ratio(ask.balance.clone())),
                    max_volume_fraction: ask
                        .balance_rat
                        .as_ref()
                        .map(|p| p.to_fraction())
                        .unwrap_or_else(|| ask.balance.clone().into()),
                    pubkey: ask.pubkey.clone(),
                    age: (now_ms() as i64 / 1000) - ask.timestamp as i64,
                    zcredits: 0,
                    uuid: *uuid,
                    is_mine: my_pubsecp == ask.pubsecp,
                })
            }
            orderbook_entries
        },
        None => vec![],
    };
    let bids = match orderbook.get(&(req.rel.clone(), req.base.clone())) {
        Some(asks) => {
            let mut orderbook_entries = vec![];
            for (uuid, ask) in asks.iter() {
                log!("Ask size {}"(std::mem::size_of_val(ask)));
                let price_mm = MmNumber::from(1i32)
                    / ask
                        .price_rat
                        .clone()
                        .unwrap_or_else(|| from_dec_to_ratio(ask.price.clone()).into());
                orderbook_entries.push(OrderbookEntry {
                    coin: req.rel.clone(),
                    address: try_s!(rel_coin.address_from_pubkey_str(&ask.pubsecp)),
                    // NB: 1/x can not be represented as a decimal and introduces a rounding error
                    // cf. https://github.com/KomodoPlatform/atomicDEX-API/issues/495#issuecomment-516365682
                    price: BigDecimal::from(1) / &ask.price,
                    price_rat: price_mm.to_ratio(),
                    price_fraction: price_mm.to_fraction(),
                    max_volume: ask.balance.clone(),
                    max_volume_rat: ask
                        .balance_rat
                        .as_ref()
                        .map(|p| p.to_ratio())
                        .unwrap_or_else(|| from_dec_to_ratio(ask.balance.clone())),
                    max_volume_fraction: ask
                        .balance_rat
                        .as_ref()
                        .map(|p| p.to_fraction())
                        .unwrap_or_else(|| from_dec_to_ratio(ask.balance.clone()).into()),
                    pubkey: ask.pubkey.clone(),
                    age: (now_ms() as i64 / 1000) - ask.timestamp as i64,
                    zcredits: 0,
                    uuid: *uuid,
                    is_mine: my_pubsecp == ask.pubsecp,
                })
            }
            orderbook_entries
        },
        None => vec![],
    };
    let response = OrderbookResponse {
        num_asks: asks.len(),
        num_bids: bids.len(),
        ask_depth: 0,
        asks,
        base: req.base,
        bid_depth: 0,
        bids,
        netid: ctx.netid(),
        rel: req.rel,
        timestamp: now_ms() / 1000,
    };
    let responseʲ = try_s!(json::to_vec(&response));
    Ok(try_s!(Response::builder().body(responseʲ)))
}

pub fn migrate_saved_orders(ctx: &MmArc) -> Result<(), String> {
    let taker_entries: Vec<DirEntry> = try_s!(json_dir_entries(&my_taker_orders_dir(&ctx)));
    taker_entries.iter().for_each(|entry| {
        if let Ok(mut order) = json::from_slice::<TakerOrder>(&slurp(&entry.path())) {
            if order.request.base_amount_rat.is_none() {
                order.request.base_amount_rat = Some(from_dec_to_ratio(order.request.base_amount.clone()));
            }
            if order.request.rel_amount_rat.is_none() {
                order.request.rel_amount_rat = Some(from_dec_to_ratio(order.request.rel_amount.clone()));
            }
            save_my_taker_order(ctx, &order)
        }
    });
    Ok(())
}

fn choose_maker_confs_and_notas(
    maker_confs: Option<OrderConfirmationsSettings>,
    taker_req: &TakerRequest,
    maker_coin: &MmCoinEnum,
    taker_coin: &MmCoinEnum,
) -> SwapConfirmationsSettings {
    let maker_settings = maker_confs.unwrap_or(OrderConfirmationsSettings {
        base_confs: maker_coin.required_confirmations(),
        base_nota: maker_coin.requires_notarization(),
        rel_confs: taker_coin.required_confirmations(),
        rel_nota: taker_coin.requires_notarization(),
    });

    let (maker_coin_confs, maker_coin_nota, taker_coin_confs, taker_coin_nota) = match taker_req.conf_settings {
        Some(taker_settings) => match taker_req.action {
            TakerAction::Sell => {
                let maker_coin_confs = if taker_settings.rel_confs < maker_settings.base_confs {
                    taker_settings.rel_confs
                } else {
                    maker_settings.base_confs
                };
                let maker_coin_nota = if !taker_settings.rel_nota {
                    taker_settings.rel_nota
                } else {
                    maker_settings.base_nota
                };
                (
                    maker_coin_confs,
                    maker_coin_nota,
                    maker_settings.rel_confs,
                    maker_settings.rel_nota,
                )
            },
            TakerAction::Buy => {
                let maker_coin_confs = if taker_settings.base_confs < maker_settings.base_confs {
                    taker_settings.base_confs
                } else {
                    maker_settings.base_confs
                };
                let maker_coin_nota = if !taker_settings.base_nota {
                    taker_settings.base_nota
                } else {
                    maker_settings.base_nota
                };
                (
                    maker_coin_confs,
                    maker_coin_nota,
                    maker_settings.rel_confs,
                    maker_settings.rel_nota,
                )
            },
        },
        None => (
            maker_settings.base_confs,
            maker_settings.base_nota,
            maker_settings.rel_confs,
            maker_settings.rel_nota,
        ),
    };

    SwapConfirmationsSettings {
        maker_coin_confs,
        maker_coin_nota,
        taker_coin_confs,
        taker_coin_nota,
    }
}

fn choose_taker_confs_and_notas(
    taker_req: &TakerRequest,
    maker_reserved: &MakerReserved,
    maker_coin: &MmCoinEnum,
    taker_coin: &MmCoinEnum,
) -> SwapConfirmationsSettings {
    let (mut taker_coin_confs, mut taker_coin_nota, maker_coin_confs, maker_coin_nota) = match taker_req.action {
        TakerAction::Buy => match taker_req.conf_settings {
            Some(s) => (s.rel_confs, s.rel_nota, s.base_confs, s.base_nota),
            None => (
                taker_coin.required_confirmations(),
                taker_coin.requires_notarization(),
                maker_coin.required_confirmations(),
                maker_coin.requires_notarization(),
            ),
        },
        TakerAction::Sell => match taker_req.conf_settings {
            Some(s) => (s.base_confs, s.base_nota, s.rel_confs, s.rel_nota),
            None => (
                taker_coin.required_confirmations(),
                taker_coin.requires_notarization(),
                maker_coin.required_confirmations(),
                maker_coin.requires_notarization(),
            ),
        },
    };
    if let Some(settings_from_maker) = maker_reserved.conf_settings {
        if settings_from_maker.rel_confs < taker_coin_confs {
            taker_coin_confs = settings_from_maker.rel_confs;
        }
        if !settings_from_maker.rel_nota {
            taker_coin_nota = settings_from_maker.rel_nota;
        }
    }
    SwapConfirmationsSettings {
        maker_coin_confs,
        maker_coin_nota,
        taker_coin_confs,
        taker_coin_nota,
    }
}

mod new_protocol {
    use super::{MatchBy as SuperMatchBy, TakerAction};
    use crate::mm2::lp_ordermatch::OrderConfirmationsSettings;
    use common::mm_number::MmNumber;
    use compact_uuid::CompactUuid;
    use std::collections::HashSet;

    #[derive(Debug, Deserialize, Serialize)]
    #[allow(clippy::large_enum_variant)]
    pub enum OrdermatchMessage {
        MakerOrderCreated(MakerOrderCreated),
        MakerOrderUpdated(MakerOrderUpdated),
        MakerOrderKeepAlive(MakerOrderKeepAlive),
        MakerOrderCancelled(MakerOrderCancelled),
        TakerRequest(TakerRequest),
        MakerReserved(MakerReserved),
        TakerConnect(TakerConnect),
        MakerConnected(MakerConnected),
        RepeatOrder(RepeatOrder),
    }

    impl From<MakerOrderKeepAlive> for OrdermatchMessage {
        fn from(keep_alive: MakerOrderKeepAlive) -> Self { OrdermatchMessage::MakerOrderKeepAlive(keep_alive) }
    }

    /// MsgPack compact representation does not work with tagged enums (encoding works, but decoding fails)
    /// This is untagged representation also using compact Uuid representation
    #[derive(Clone, Debug, Deserialize, Serialize)]
    pub enum MatchBy {
        Any,
        Orders(HashSet<CompactUuid>),
        Pubkeys(HashSet<[u8; 32]>),
    }

    impl From<SuperMatchBy> for MatchBy {
        fn from(match_by: SuperMatchBy) -> MatchBy {
            match match_by {
                SuperMatchBy::Any => MatchBy::Any,
                SuperMatchBy::Orders(uuids) => MatchBy::Orders(uuids.into_iter().map(|uuid| uuid.into()).collect()),
                SuperMatchBy::Pubkeys(pubkeys) => {
                    MatchBy::Pubkeys(pubkeys.into_iter().map(|pubkey| pubkey.0).collect())
                },
            }
        }
    }

    impl Into<SuperMatchBy> for MatchBy {
        fn into(self) -> SuperMatchBy {
            match self {
                MatchBy::Any => SuperMatchBy::Any,
                MatchBy::Orders(uuids) => SuperMatchBy::Orders(uuids.into_iter().map(|uuid| uuid.into()).collect()),
                MatchBy::Pubkeys(pubkeys) => {
                    SuperMatchBy::Pubkeys(pubkeys.into_iter().map(|pubkey| pubkey.into()).collect())
                },
            }
        }
    }

    mod compact_uuid {
        use serde::{Deserialize, Deserializer, Serialize, Serializer};
        use std::str::FromStr;
        use uuid::Uuid;

        /// Default MsgPack encoded UUID length is 38 bytes (seems like it is encoded as string)
        /// This wrapper is encoded to raw 16 bytes representation
        /// Derives all traits of wrapped value
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct CompactUuid(Uuid);

        impl From<Uuid> for CompactUuid {
            fn from(uuid: Uuid) -> Self { CompactUuid(uuid) }
        }

        impl Into<Uuid> for CompactUuid {
            fn into(self) -> Uuid { self.0 }
        }

        impl FromStr for CompactUuid {
            type Err = uuid::parser::ParseError;

            fn from_str(str: &str) -> Result<Self, Self::Err> {
                let uuid = Uuid::parse_str(str)?;
                Ok(uuid.into())
            }
        }

        impl Serialize for CompactUuid {
            fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                s.serialize_bytes(self.0.as_bytes())
            }
        }

        impl<'de> Deserialize<'de> for CompactUuid {
            fn deserialize<D>(d: D) -> Result<CompactUuid, D::Error>
            where
                D: Deserializer<'de>,
            {
                let bytes: &[u8] = Deserialize::deserialize(d)?;
                let uuid = Uuid::from_slice(bytes)
                    .map_err(|e| serde::de::Error::custom(format!("Uuid::from_slice error {}", e)))?;
                Ok(uuid.into())
            }
        }
    }

    #[derive(Clone, Debug, Deserialize, Serialize)]
    pub struct MakerOrderCreated {
        pub uuid: CompactUuid,
        pub base: String,
        pub rel: String,
        pub price: MmNumber,
        pub max_volume: MmNumber,
        pub min_volume: MmNumber,
        pub conf_settings: OrderConfirmationsSettings,
    }

    #[derive(Debug, Deserialize, Serialize)]
    pub struct MakerOrderKeepAlive {
        pub uuid: CompactUuid,
        pub timestamp: u64,
    }

    #[derive(Debug, Deserialize, Serialize)]
    pub struct MakerOrderCancelled {
        pub uuid: CompactUuid,
    }

    #[derive(Debug, Deserialize, Serialize)]
    pub struct RepeatOrder {
        pub uuid: CompactUuid,
    }

    #[derive(Debug, Deserialize, Serialize)]
    pub struct MakerOrderUpdated {
        uuid: CompactUuid,
        new_price: Option<MmNumber>,
        new_max_volume: Option<MmNumber>,
        new_min_volume: Option<MmNumber>,
    }

    #[derive(Clone, Debug, Deserialize, Serialize)]
    pub struct TakerRequest {
        pub base: String,
        pub rel: String,
        pub base_amount: MmNumber,
        pub rel_amount: MmNumber,
        pub action: TakerAction,
        pub uuid: CompactUuid,
        pub match_by: MatchBy,
        pub conf_settings: OrderConfirmationsSettings,
    }

    #[derive(Clone, Debug, Deserialize, Serialize)]
    pub struct MakerReserved {
        pub base: String,
        pub rel: String,
        pub base_amount: MmNumber,
        pub rel_amount: MmNumber,
        pub taker_order_uuid: CompactUuid,
        pub maker_order_uuid: CompactUuid,
        pub conf_settings: OrderConfirmationsSettings,
    }

    #[derive(Clone, Debug, Deserialize, Serialize)]
    pub struct TakerConnect {
        pub taker_order_uuid: CompactUuid,
        pub maker_order_uuid: CompactUuid,
    }

    #[derive(Clone, Debug, Deserialize, Serialize)]
    pub struct MakerConnected {
        pub taker_order_uuid: CompactUuid,
        pub maker_order_uuid: CompactUuid,
    }
}
