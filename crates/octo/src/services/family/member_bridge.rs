//! Members sign in to Octo with their one account password; Octo signs in to Navidrome for
//! them with a hidden password they never see. This reads the sign-in a Subsonic request
//! carries, checks it against the account password, and rewrites the request to the hidden
//! password.
//! It also keeps the failure throttle and repairs a hidden password Navidrome stopped taking.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use chrono::{DateTime, TimeDelta, Utc};
use octo_core::common::Clock;
use octo_core::common::dotnet::eq_ignore_case;
use octo_core::family::codes::{constant_time_eq, hidden_password, subsonic_token};
use octo_subsonic::subsonic_request_parser::{
    BodyReading, RequestParts, RequestReading, StringValuesCollection, escape_data_string, json_object,
    read_request, unescape_data_string,
};
use parking_lot::Mutex;

use tracing::{info, warn};

use super::family_store::FamilyStore;
use super::navidrome_admin::NavidromeAdmin;
use super::vault::FamilyVault;
use crate::services::admin::sign_in_throttle::trim_oldest;

/// Every key a Subsonic sign-in can use.
pub const SIGN_IN_KEYS: [&str; 6] = ["u", "p", "t", "s", "apiKey", "jwt"];

fn is_sign_in_key(key: &str) -> bool {
    SIGN_IN_KEYS.iter().any(|k| eq_ignore_case(k, key))
}

fn media_type(content_type: Option<&str>) -> String {
    content_type
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

fn is_url_encoded(content_type: Option<&str>) -> bool {
    media_type(content_type) == "application/x-www-form-urlencoded"
}

fn is_json(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|c| c.contains("application/json"))
}

/// The sign-in a request carries, from its query, a form body (url-encoded or multipart) and
/// a JSON body, each key matched ignoring case.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SignIn {
    pub users: Vec<String>,
    pub passwords: Vec<String>,
    pub tokens: Vec<String>,
    pub salts: Vec<String>,
    pub api_keys: Vec<String>,
    pub jwts: Vec<String>,
    /// Some sign-in key sits in a multipart body, which Octo cannot rewrite.
    pub in_multipart: bool,
    /// `u` came in a url-encoded body and not in the query.
    pub user_in_body_only: bool,
}

impl SignIn {
    /// The sign-in of a request with this query and body, read as [`read_request`] reads it.
    pub fn read(query: Option<&str>, content_type: Option<&str>, body: &[u8]) -> SignIn {
        let reading = read_request(&RequestParts {
            query,
            content_type,
            content_length: Some(body.len() as u64),
            body,
        });
        SignIn::of(&reading, content_type)
    }

    /// The sign-in a request read once carries.
    pub fn of(reading: &RequestReading, content_type: Option<&str>) -> SignIn {
        let mut sign_in = SignIn::default();
        sign_in.take(&reading.query);
        let user_in_query = reading.query.get("u").is_some();
        match &reading.body {
            BodyReading::Form(form) if is_url_encoded(content_type) => {
                sign_in.user_in_body_only = !user_in_query && form.get("u").is_some();
                sign_in.take(form);
            }
            BodyReading::Form(form) => {
                sign_in.in_multipart = form.iter().any(|(key, _)| is_sign_in_key(key));
                sign_in.take(form);
            }
            BodyReading::Json(_) => {
                if let Some(values) = reading.body_values() {
                    sign_in.take(&values);
                }
            }
            BodyReading::Nothing | BodyReading::Unreadable => {}
        }
        sign_in
    }

    fn take(&mut self, values: &StringValuesCollection) {
        let add = |key: &str, into: &mut Vec<String>| {
            if let Some(found) = values.get(key) {
                into.extend(
                    found
                        .iter()
                        .map(|v| v.trim().to_string())
                        .filter(|v| !v.is_empty()),
                );
            }
        };
        add("u", &mut self.users);
        add("p", &mut self.passwords);
        add("t", &mut self.tokens);
        add("s", &mut self.salts);
        add("apiKey", &mut self.api_keys);
        add("jwt", &mut self.jwts);
    }

    /// The names in `u`, each once (ignoring case).
    pub fn named_users(&self) -> Vec<&str> {
        distinct(&self.users)
    }
}

fn distinct(values: &[String]) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for value in values {
        if !out.iter().any(|known| eq_ignore_case(known, value)) {
            out.push(value);
        }
    }
    out
}

fn distinct_exact(values: &[String]) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for value in values {
        if !out.contains(&value.as_str()) {
            out.push(value);
        }
    }
    out
}

/// What checking a member's sign-in came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The member's account password, or one of their web player sessions, signed this
    /// request.
    Verified,
    /// A sign-in was there and it was wrong.
    Refused,
    /// No password, token or web token at all.
    Missing,
}

/// Checks a member's sign-in against their account password (`password`, None when they have
/// none): `p` (plain or `enc:` hex) equal to it, or `t` equal to `md5(password + s)`, or a web
/// token Octo signed for one of their web player sessions (`web_session`). Every comparison
/// takes the same time wherever the values differ.
pub fn verify(password: Option<&str>, sign_in: &SignIn, web_session: bool) -> Verdict {
    let passwords = distinct_exact(&sign_in.passwords);
    if !passwords.is_empty() {
        if passwords.len() > 1 {
            return Verdict::Refused;
        }
        let mut typed = passwords[0].to_string();
        if let Some(encoded) = typed.strip_prefix("enc:")
            && let Ok(bytes) = hex::decode(encoded)
        {
            typed = String::from_utf8_lossy(&bytes).into_owned();
        }
        let right = password.is_some_and(|password| constant_time_eq(typed.as_bytes(), password.as_bytes()));
        return if right {
            Verdict::Verified
        } else {
            Verdict::Refused
        };
    }
    let tokens = distinct_exact(&sign_in.tokens);
    let salts = distinct_exact(&sign_in.salts);
    if !tokens.is_empty() || !salts.is_empty() {
        if tokens.len() != 1 || salts.len() != 1 {
            return Verdict::Refused;
        }
        let token = tokens[0].to_ascii_lowercase();
        let right = password.is_some_and(|password| {
            constant_time_eq(subsonic_token(password, salts[0]).as_bytes(), token.as_bytes())
        });
        return if right {
            Verdict::Verified
        } else {
            Verdict::Refused
        };
    }
    if web_session {
        return Verdict::Verified;
    }
    if !sign_in.jwts.is_empty() {
        return Verdict::Refused;
    }
    Verdict::Missing
}

/// Whether one `name=value` segment of a query or form is a sign-in key.
fn segment_is_sign_in(segment: &str) -> bool {
    let name = segment.split_once('=').map_or(segment, |(name, _)| name);
    is_sign_in_key(&unescape_data_string(&name.replace('+', " ")))
}

/// `encoded` (a query or url-encoded body) without its sign-in keys. Every other segment stays
/// exactly as written, repeated keys and their order included.
pub fn strip_sign_in(encoded: &str) -> String {
    encoded
        .split('&')
        .filter(|segment| !segment.is_empty() && !segment_is_sign_in(segment))
        .collect::<Vec<_>>()
        .join("&")
}

/// `u`, `t` and `s` for `user` with a new salt, url-encoded.
pub fn signed_as(user: &str, hidden: &str) -> String {
    let salt = hex::encode(rand::random::<[u8; 6]>());
    format!(
        "u={}&t={}&s={salt}",
        escape_data_string(user),
        subsonic_token(hidden, &salt)
    )
}

fn joined(head: String, tail: &str) -> String {
    match (head.is_empty(), tail.is_empty()) {
        (true, _) => tail.to_string(),
        (false, true) => head,
        (false, false) => format!("{head}&{tail}"),
    }
}

/// A request's query and body after the rewrite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rewritten {
    pub query: Option<String>,
    /// The new body, when it changed.
    pub body: Option<Bytes>,
}

/// Takes every sign-in key out of the query, a url-encoded body and a JSON body, and signs the
/// request in as `user` with `hidden` (a fresh salt each time). The new sign-in goes where the
/// client put `u`: the body when it sent `u` only there, else the query.
pub fn rewrite(
    query: Option<&str>,
    content_type: Option<&str>,
    body: &[u8],
    user_in_body_only: bool,
    user: &str,
    hidden: &str,
) -> Rewritten {
    let signed = signed_as(user, hidden);
    let kept_query = strip_sign_in(query.unwrap_or(""));
    let mut new_body = None;
    let mut query_out = kept_query.clone();
    if is_url_encoded(content_type) && !body.is_empty() {
        let text = String::from_utf8_lossy(body);
        let kept = strip_sign_in(&text);
        if user_in_body_only {
            new_body = Some(Bytes::from(joined(kept, &signed)));
        } else {
            if kept != text {
                new_body = Some(Bytes::from(kept));
            }
            query_out = joined(kept_query, &signed);
        }
    } else {
        // Read as the parameters read it (a byte order mark, or bytes that are not UTF-8,
        // included), so no sign-in key the parameters would find stays behind.
        if is_json(content_type)
            && let Some(members) = json_object(body)
        {
            let before = members.len();
            let kept: indexmap::IndexMap<String, Box<serde_json::value::RawValue>> = members
                .into_iter()
                .filter(|(key, _)| !is_sign_in_key(key))
                .collect();
            if kept.len() != before {
                new_body = serde_json::to_vec(&kept).ok().map(Bytes::from);
            }
        }
        query_out = joined(kept_query, &signed);
    }
    Rewritten {
        query: (!query_out.is_empty()).then_some(query_out),
        body: new_body,
    }
}

/// Wrong sign-ins allowed per address and member before more are refused unchecked.
pub const MAX_FAILURES: usize = 20;
/// Wrong sign-ins allowed from one peer Octo can vouch for, to any account, in the window. A
/// peer may stand for several people (a proxy Octo does not trust), so its ceiling is higher.
pub const PEER_FAILURES: usize = 100;
/// Wrong sign-ins allowed for one account in the window, from every address that never signed
/// in to it together, and again from every address that did. A flood of guesses from new
/// addresses then never holds back a device at an address it signed in from before, and
/// guessing stays bounded either way.
pub const ACCOUNT_FAILURES: usize = 100;
/// How long an address that signed in to an account stays known for that account.
pub const KNOWN_FOR: TimeDelta = TimeDelta::days(30);
/// How long a wrong sign-in counts.
pub const FAILURE_WINDOW: TimeDelta = TimeDelta::minutes(15);
const MAX_KEYS: usize = 10_000;
/// Tries kept per key at most: twice the highest ceiling.
const KEPT_TRIES: usize = 2 * PEER_FAILURES;

/// How many of a key's tries still count at `now`, the older ones dropped.
fn recent(failures: &mut HashMap<String, VecDeque<DateTime<Utc>>>, key: &str, now: DateTime<Utc>) -> usize {
    let Some(tries) = failures.get_mut(key) else {
        return 0;
    };
    while tries.front().is_some_and(|first| now - *first >= FAILURE_WINDOW) {
        tries.pop_front();
    }
    tries.len()
}

/// An account name as the throttle keys it.
fn account_key(user: &str) -> String {
    user.trim().to_lowercase()
}

/// The keys one sign-in try counts under, each with its ceiling: the client address with the
/// account, the peer Octo can vouch for (whatever hops it names), and the account across every
/// address, counted apart for addresses that signed in to it before. A try is held back when
/// any of them is used up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TryKeys(Vec<(String, usize)>);

impl TryKeys {
    /// The keys for a try at `user` from `address` (behind `peer`), an address that never
    /// signed in to `user`, allowing `per_address` wrong tries from that address.
    pub fn new(address: &IpAddr, peer: &IpAddr, user: &str, per_address: usize) -> Self {
        Self::at(address, peer, user, per_address, false)
    }

    /// [`TryKeys::new`] for an address that is `known` to have signed in to `user` before, or
    /// not: the account's tries from known addresses count apart from everyone else's.
    pub fn at(address: &IpAddr, peer: &IpAddr, user: &str, per_address: usize, known: bool) -> Self {
        let user = account_key(user);
        let account = if known {
            format!("account-known|{user}")
        } else {
            format!("account|{user}")
        };
        TryKeys(vec![
            (format!("{address}|{user}"), per_address),
            (format!("peer|{peer}"), PEER_FAILURES),
            (account, ACCOUNT_FAILURES),
        ])
    }

    /// The keys with their ceilings.
    pub fn keys(&self) -> &[(String, usize)] {
        &self.0
    }

    /// The key of the address with the account, which a right sign-in clears.
    pub fn address_key(&self) -> &str {
        &self.0[0].0
    }
}

/// Counts wrong sign-ins per client address and member, and under the other keys of a
/// [`TryKeys`]: the peer that connected, and the account from everywhere.
pub struct FailureThrottle {
    failures: Mutex<HashMap<String, VecDeque<DateTime<Utc>>>>,
    clock: Clock,
}

impl FailureThrottle {
    pub fn new(clock: Clock) -> Self {
        FailureThrottle {
            failures: Mutex::new(HashMap::new()),
            clock,
        }
    }

    /// The key for one address and member.
    pub fn key(address: &std::net::IpAddr, user: &str) -> String {
        format!("{address}|{}", user.to_lowercase())
    }

    /// Whether this key has used up its tries.
    pub fn blocked(&self, key: &str) -> bool {
        let now = self.clock.now();
        let mut failures = self.failures.lock();
        let Some(tries) = failures.get_mut(key) else {
            return false;
        };
        while tries.front().is_some_and(|first| now - *first >= FAILURE_WINDOW) {
            tries.pop_front();
        }
        tries.len() >= MAX_FAILURES
    }

    /// One more wrong sign-in for this key. Past [`MAX_KEYS`] keys, the ones with nothing recent
    /// go, and then the ones whose last try is oldest, so a flood from many addresses cannot
    /// grow the table without end.
    pub fn failed(&self, key: &str) {
        let now = self.clock.now();
        let mut failures = self.failures.lock();
        if failures.len() >= MAX_KEYS && !failures.contains_key(key) {
            failures.retain(|_, tries| tries.back().is_some_and(|last| now - *last < FAILURE_WINDOW));
            trim_oldest(&mut failures, MAX_KEYS - 1, |tries| tries.back().copied());
        }
        let tries = failures.entry(key.to_string()).or_default();
        tries.push_back(now);
        while tries.len() > KEPT_TRIES {
            tries.pop_front();
        }
    }

    /// Counts one try for this key now, before its sign-in is checked, and answers true; or
    /// answers false, counting nothing, when the key has used up its tries. Checking and
    /// counting happen under one lock, so tries sent all at once cannot all slip under the
    /// limit. A try that turns out right is taken back with [`FailureThrottle::forgive`].
    pub fn attempt(&self, key: &str) -> bool {
        let now = self.clock.now();
        let mut failures = self.failures.lock();
        let tries = failures.entry(key.to_string()).or_default();
        while tries.front().is_some_and(|first| now - *first >= FAILURE_WINDOW) {
            tries.pop_front();
        }
        if tries.len() >= MAX_FAILURES {
            return false;
        }
        tries.push_back(now);
        if failures.len() > MAX_KEYS {
            failures.retain(|_, tries| tries.back().is_some_and(|last| now - *last < FAILURE_WINDOW));
            trim_oldest(&mut failures, MAX_KEYS, |tries| tries.back().copied());
        }
        true
    }

    /// Takes back the newest try of this key: one counted before its sign-in was checked,
    /// which turned out right.
    pub fn forgive(&self, key: &str) {
        let mut failures = self.failures.lock();
        if let Some(tries) = failures.get_mut(key) {
            tries.pop_back();
            if tries.is_empty() {
                failures.remove(key);
            }
        }
    }

    /// Whether any of these keys has used up its tries.
    pub fn blocked_by(&self, keys: &TryKeys) -> bool {
        let now = self.clock.now();
        let mut failures = self.failures.lock();
        keys.keys()
            .iter()
            .any(|(key, ceiling)| recent(&mut failures, key, now) >= *ceiling)
    }

    /// One more wrong sign-in under every one of these keys.
    pub fn failed_by(&self, keys: &TryKeys) {
        for (key, _) in keys.keys() {
            self.failed(key);
        }
    }

    /// Counts one try under every key now, before its sign-in is checked, and answers true; or
    /// answers false, counting nothing, when any key has used up its tries. All under one lock,
    /// as [`FailureThrottle::attempt`].
    pub fn attempt_by(&self, keys: &TryKeys) -> bool {
        let now = self.clock.now();
        let mut failures = self.failures.lock();
        if keys
            .keys()
            .iter()
            .any(|(key, ceiling)| recent(&mut failures, key, now) >= *ceiling)
        {
            return false;
        }
        for (key, _) in keys.keys() {
            failures.entry(key.clone()).or_default().push_back(now);
        }
        if failures.len() > MAX_KEYS {
            failures.retain(|_, tries| tries.back().is_some_and(|last| now - *last < FAILURE_WINDOW));
            trim_oldest(&mut failures, MAX_KEYS, |tries| tries.back().copied());
        }
        true
    }

    /// Takes back the newest try under every key: one counted before its sign-in was checked,
    /// which turned out right or could not be checked at all.
    pub fn forgive_by(&self, keys: &TryKeys) {
        for (key, _) in keys.keys() {
            self.forgive(key);
        }
    }

    /// How many keys the throttle holds now.
    pub fn len(&self) -> usize {
        self.failures.lock().len()
    }

    /// How many tries count for this key now.
    pub fn tries(&self, key: &str) -> usize {
        self.failures.lock().get(key).map_or(0, VecDeque::len)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The addresses each account signed in from lately, at most [`MAX_KEYS`] of them.
pub struct KnownAddresses {
    seen: Mutex<HashMap<String, DateTime<Utc>>>,
    clock: Clock,
}

impl KnownAddresses {
    pub fn new(clock: Clock) -> Self {
        KnownAddresses {
            seen: Mutex::new(HashMap::new()),
            clock,
        }
    }

    fn key(address: &IpAddr, user: &str) -> String {
        format!("{address}|{}", account_key(user))
    }

    /// `user` signed in from `address` just now, with a sign-in that checked out.
    pub fn signed_in(&self, address: &IpAddr, user: &str) {
        let now = self.clock.now();
        let mut seen = self.seen.lock();
        let key = Self::key(address, user);
        if seen.len() >= MAX_KEYS && !seen.contains_key(&key) {
            seen.retain(|_, at| now - *at < KNOWN_FOR);
            trim_oldest(&mut seen, MAX_KEYS - 1, |at| Some(*at));
        }
        seen.insert(key, now);
    }

    /// Whether `user` signed in from `address` within [`KNOWN_FOR`].
    pub fn knows(&self, address: &IpAddr, user: &str) -> bool {
        let now = self.clock.now();
        self.seen
            .lock()
            .get(&Self::key(address, user))
            .is_some_and(|at| now - *at < KNOWN_FOR)
    }

    /// How many addresses are kept now.
    pub fn len(&self) -> usize {
        self.seen.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// How soon a member's hidden password may be set again after a repair.
pub const REPAIR_EVERY: Duration = Duration::from_secs(60);
const LOG_EVERY: Duration = Duration::from_secs(60);

/// The throttle, the addresses accounts signed in from, the repairs under way and the refusals
/// logged lately.
pub struct MemberBridge {
    pub throttle: FailureThrottle,
    pub known: KnownAddresses,
    repairs: Mutex<HashMap<String, Arc<tokio::sync::Mutex<Option<Instant>>>>>,
    logged: Mutex<HashMap<String, Instant>>,
}

impl Default for MemberBridge {
    fn default() -> Self {
        Self::new(Clock::system())
    }
}

impl MemberBridge {
    pub fn new(clock: Clock) -> Self {
        MemberBridge {
            throttle: FailureThrottle::new(clock.clone()),
            known: KnownAddresses::new(clock),
            repairs: Mutex::new(HashMap::new()),
            logged: Mutex::new(HashMap::new()),
        }
    }

    /// The keys a sign-in try at `user` from `address` (behind `peer`) counts under, with the
    /// account's count for known addresses when `user` signed in from `address` before.
    pub fn try_keys(&self, address: &IpAddr, peer: &IpAddr, user: &str, per_address: usize) -> TryKeys {
        TryKeys::at(address, peer, user, per_address, self.known.knows(address, user))
    }

    /// Whether a line about `key` may be logged now (once a minute per key).
    pub fn may_log(&self, key: &str) -> bool {
        let mut logged = self.logged.lock();
        let now = Instant::now();
        if logged
            .get(key)
            .is_some_and(|at| now.duration_since(*at) < LOG_EVERY)
        {
            return false;
        }
        if logged.len() >= MAX_KEYS {
            logged.retain(|_, at| now.duration_since(*at) < LOG_EVERY);
            trim_oldest(&mut logged, MAX_KEYS - 1, |at| Some(*at));
        }
        logged.insert(key.to_string(), now);
        true
    }

    /// How many log keys are kept now.
    pub fn logged_len(&self) -> usize {
        self.logged.lock().len()
    }

    fn repair_lock(&self, user: &str) -> Arc<tokio::sync::Mutex<Option<Instant>>> {
        self.repairs
            .lock()
            .entry(user.to_lowercase())
            .or_default()
            .clone()
    }

    /// Holds off every repair of `user`'s hidden password until the answer is dropped: one
    /// running now is waited for, and one asked for meanwhile waits and then reads the family as
    /// it is by then. A member leaving the family with their account kept is taken out of it
    /// under this hold, so no repair sets a hidden password over the one they chose.
    pub async fn hold_repairs(&self, user: &str) -> tokio::sync::OwnedMutexGuard<Option<Instant>> {
        self.repair_lock(user).lock_owned().await
    }

    /// Navidrome refused a member's request signed with `used`. Answers the hidden password to
    /// try once more with: the current one when it changed meanwhile, or a new one set
    /// through Octo's admin sign-in. None when there is nothing to retry with (a repair ran
    /// for this member less than [`REPAIR_EVERY`] ago, or it failed).
    pub async fn after_refusal(
        &self,
        admin: &NavidromeAdmin,
        store: &FamilyStore,
        vault: &FamilyVault,
        user: &str,
        used: &str,
    ) -> Option<String> {
        let lock = self.repair_lock(user);
        let mut last = lock.lock().await;
        let member = store.member(user)?;
        let current = vault.open_sealed(&member.navidrome_password);
        if let Some(current) = current.as_ref()
            && current != used
        {
            return Some(current.clone());
        }
        if last.is_some_and(|at| at.elapsed() < REPAIR_EVERY) {
            return None;
        }
        *last = Some(Instant::now());
        match repair(admin, store, vault, user).await {
            Ok(hidden) => {
                info!("FAMILY Navidrome refused {user}'s hidden password, so Octo set a new one");
                Some(hidden)
            }
            Err(e) => {
                warn!(
                    "FAMILY Navidrome refused {user}'s hidden password, and Octo could not set a new one: {e}"
                );
                None
            }
        }
    }
}

/// Sets a new hidden password for a member in Navidrome and keeps it (sealed). Answers it.
pub async fn repair(
    admin: &NavidromeAdmin,
    store: &FamilyStore,
    vault: &FamilyVault,
    user: &str,
) -> Result<String, String> {
    let mut member = store
        .member(user)
        .ok_or_else(|| format!("{user} is not in the family"))?;
    if member.navidrome_user_id.trim().is_empty() {
        return Err(format!("{user} has no Navidrome account on record"));
    }
    let hidden = hidden_password();
    let sealed = vault
        .seal(&hidden)
        .ok_or_else(|| "family.key is missing".to_string())?;
    admin
        .set_password(&member.navidrome_user_id, &hidden)
        .await
        .map_err(|e| e.to_string())?;
    member.navidrome_password = sealed;
    store.upsert_member(member);
    Ok(hidden)
}

#[cfg(test)]
mod tests {
    use super::*;
    use octo_subsonic::subsonic_request_parser::parse_query;

    #[test]
    fn a_try_counts_before_it_is_checked_and_a_right_one_is_taken_back() {
        let throttle = FailureThrottle::new(Clock::system());
        let key = FailureThrottle::key(&"203.0.113.9".parse().unwrap(), "Alex");
        for _ in 0..MAX_FAILURES {
            assert!(throttle.attempt(&key));
        }
        assert!(!throttle.attempt(&key), "the next try waits");
        assert_eq!(throttle.tries(&key), MAX_FAILURES, "a refused try is not counted");
        assert!(throttle.blocked(&key));
        throttle.forgive(&key);
        assert_eq!(throttle.tries(&key), MAX_FAILURES - 1);
        assert!(throttle.attempt(&key));
    }

    #[test]
    fn the_throttle_and_the_log_stay_bounded_under_a_flood() {
        let throttle = FailureThrottle::new(Clock::system());
        for n in 0..(MAX_KEYS + 50) {
            throttle.failed(&format!("10.0.{}.{}|alex", n / 256, n % 256));
        }
        assert!(throttle.len() <= MAX_KEYS, "{}", throttle.len());
        for n in 0..(MAX_KEYS + 50) {
            assert!(throttle.attempt(&format!("10.1.{}.{}|alex", n / 256, n % 256)));
        }
        assert!(throttle.len() <= MAX_KEYS, "{}", throttle.len());
        let key = "10.9.9.9|alex";
        for _ in 0..(KEPT_TRIES * 3) {
            throttle.failed(key);
        }
        assert!(throttle.tries(key) <= KEPT_TRIES);
        let bridge = MemberBridge::default();
        for n in 0..(MAX_KEYS + 50) {
            assert!(bridge.may_log(&format!("sign-in|user-{n}")));
        }
        assert!(bridge.logged_len() <= MAX_KEYS, "{}", bridge.logged_len());
    }

    const PASSWORD: &str = "Zx9-secret value";

    fn sign_in(query: &str) -> SignIn {
        SignIn::read(Some(query), None, b"")
    }

    #[test]
    fn the_account_password_verifies_as_a_token() {
        let t = subsonic_token(PASSWORD, "c19b2d");
        assert_eq!(
            verify(Some(PASSWORD), &sign_in(&format!("u=alex&t={t}&s=c19b2d")), false),
            Verdict::Verified
        );
        assert_eq!(
            verify(
                Some(PASSWORD),
                &sign_in(&format!("u=alex&t={}&s=c19b2d", t.to_uppercase())),
                false
            ),
            Verdict::Verified
        );
        assert_eq!(
            verify(Some(PASSWORD), &sign_in("u=alex&t=0000&s=aa"), false),
            Verdict::Refused
        );
    }

    #[test]
    fn plain_and_enc_passwords_verify() {
        let plain = format!("u=alex&p={}", escape_data_string(PASSWORD));
        assert_eq!(verify(Some(PASSWORD), &sign_in(&plain), false), Verdict::Verified);
        let encoded = format!("enc:{}", hex::encode(PASSWORD));
        assert_eq!(
            verify(Some(PASSWORD), &sign_in(&format!("u=alex&p={encoded}")), false),
            Verdict::Verified
        );
        // The password is exact: case matters.
        assert_eq!(
            verify(Some(PASSWORD), &sign_in("u=alex&p=zx9-secret%20value"), false),
            Verdict::Refused
        );
    }

    #[test]
    fn no_password_or_a_wrong_one_is_refused() {
        // A member with no password kept (never signed up, or from before one login).
        let t = subsonic_token(PASSWORD, "aa");
        assert_eq!(
            verify(None, &sign_in(&format!("u=alex&t={t}&s=aa")), false),
            Verdict::Refused
        );
        assert_eq!(
            verify(None, &sign_in("u=alex&p=anything"), false),
            Verdict::Refused
        );
        assert_eq!(
            verify(
                Some("sams password"),
                &sign_in(&format!("u=alex&t={t}&s=aa")),
                false
            ),
            Verdict::Refused
        );
        // Two different passwords, or a token without its salt, are refused too.
        assert_eq!(
            verify(Some(PASSWORD), &sign_in("u=alex&p=x&p=other"), false),
            Verdict::Refused
        );
        assert_eq!(
            verify(Some(PASSWORD), &sign_in("u=alex&t=abc"), false),
            Verdict::Refused
        );
        assert_eq!(
            verify(Some(PASSWORD), &sign_in("u=alex"), false),
            Verdict::Missing
        );
        assert_eq!(
            verify(Some(PASSWORD), &sign_in("u=alex&jwt=x"), false),
            Verdict::Refused
        );
        assert_eq!(
            verify(Some(PASSWORD), &sign_in("u=alex&jwt=x"), true),
            Verdict::Verified
        );
    }

    #[test]
    fn sign_ins_are_read_from_every_place() {
        let read = SignIn::read(
            Some("U=alex&id=1&p=a"),
            Some("application/x-www-form-urlencoded"),
            b"t=tok&s=salt&jwt=j",
        );
        assert_eq!(read.users, ["alex"]);
        assert_eq!(
            (read.passwords.len(), read.tokens.len(), read.jwts.len()),
            (1, 1, 1)
        );
        assert!(!read.user_in_body_only);
        let body_only = SignIn::read(
            None,
            Some("application/x-www-form-urlencoded; charset=utf-8"),
            b"u=alex&p=x",
        );
        assert!(body_only.user_in_body_only);
        let json = SignIn::read(None, Some("application/json"), br#"{"u":"alex","apiKey":"k"}"#);
        assert_eq!(json.api_keys, ["k"]);
        let multipart = SignIn::read(
            None,
            Some("multipart/form-data; boundary=b"),
            b"--b\r\nContent-Disposition: form-data; name=\"p\"\r\n\r\nx\r\n--b--\r\n",
        );
        assert!(multipart.in_multipart);
        let twice = sign_in("u=alex&u=ALEX&u=sam");
        assert_eq!(twice.named_users(), ["alex", "sam"]);
    }

    #[test]
    fn rewrite_keeps_everything_else_as_written() {
        let out = rewrite(
            Some("u=alex&id=A&p=secret&id=B&v=1.16.1&U=x&apikey=k&c=My%20App"),
            None,
            b"",
            false,
            "alex",
            "hidden",
        );
        let query = out.query.expect("a query");
        let (kept, signed) = query.split_at(query.find("&u=").expect("signed in"));
        assert_eq!(kept, "id=A&id=B&v=1.16.1&c=My%20App");
        let signed = parse_query(signed);
        let salt = &signed.get("s").expect("a salt")[0];
        assert_eq!(salt.len(), 12);
        assert_eq!(
            signed.get("t").expect("a token")[0],
            subsonic_token("hidden", salt)
        );
        assert_eq!(signed.get("u").expect("u")[0], "alex");
        assert!(out.body.is_none());
    }

    #[test]
    fn rewrite_follows_a_form_body() {
        let form = Some("application/x-www-form-urlencoded");
        // u only in the body: the sign-in goes back in the body.
        let out = rewrite(
            Some("f=json"),
            form,
            b"u=alex&p=secret&id=1&id=2",
            true,
            "alex",
            "h",
        );
        assert_eq!(out.query.as_deref(), Some("f=json"));
        let body = String::from_utf8(out.body.expect("a body").to_vec()).expect("text");
        assert!(body.starts_with("id=1&id=2&u=alex&t="), "{body}");
        // u in the query: the body only loses its sign-in keys.
        let out = rewrite(Some("u=alex&t=x&s=y"), form, b"id=1&p=z", false, "alex", "h");
        assert_eq!(out.body.as_deref(), Some(&b"id=1"[..]));
        assert!(out.query.expect("a query").starts_with("u=alex&t="));
        // A JSON body loses them too.
        let out = rewrite(
            None,
            Some("application/json"),
            br#"{"p":"x","id":"1"}"#,
            false,
            "alex",
            "h",
        );
        assert_eq!(out.body.as_deref(), Some(&br#"{"id":"1"}"#[..]));
    }

    #[test]
    fn tries_count_per_peer_and_per_account_whatever_address_is_named() {
        let ip = |text: String| -> IpAddr { text.parse().expect("an address") };
        let home = ip("192.168.1.5".into());
        let throttle = FailureThrottle::new(Clock::system());
        for n in 0..PEER_FAILURES {
            let keys = TryKeys::new(
                &ip(format!("203.0.113.{n}")),
                &home,
                &format!("user-{n}"),
                MAX_FAILURES,
            );
            assert!(!throttle.blocked_by(&keys));
            throttle.failed_by(&keys);
        }
        let other = ip("198.51.100.1".into());
        assert!(throttle.blocked_by(&TryKeys::new(&other, &home, "anyone", MAX_FAILURES)));
        assert!(!throttle.blocked_by(&TryKeys::new(&other, &other, "anyone", MAX_FAILURES)));

        let throttle = FailureThrottle::new(Clock::system());
        for n in 0..ACCOUNT_FAILURES {
            let from = ip(format!("10.0.{}.{}", n / 200, n % 200));
            assert!(throttle.attempt_by(&TryKeys::new(&from, &from, "Alex", MAX_FAILURES)));
        }
        let fresh = ip("10.9.9.9".into());
        let keys = TryKeys::new(&fresh, &fresh, "alex", MAX_FAILURES);
        assert!(!throttle.attempt_by(&keys), "the account waits");
        assert!(throttle.attempt_by(&TryKeys::new(&fresh, &fresh, "sam", MAX_FAILURES)));
        // A try taken back frees one place under every key.
        let last = ip(format!(
            "10.0.{}.{}",
            (ACCOUNT_FAILURES - 1) / 200,
            (ACCOUNT_FAILURES - 1) % 200
        ));
        throttle.forgive_by(&TryKeys::new(&last, &last, "alex", MAX_FAILURES));
        assert!(throttle.attempt_by(&keys));
    }

    #[test]
    fn known_addresses_count_apart_from_new_ones() {
        let now = Arc::new(Mutex::new(Utc::now()));
        let reading = now.clone();
        let bridge = MemberBridge::new(Clock::new(move || *reading.lock()));
        let ip = |text: String| -> IpAddr { text.parse().expect("an address") };
        let known = ip("198.51.100.50".into());
        bridge.known.signed_in(&known, "Alex");
        assert!(bridge.known.knows(&known, "alex"));
        assert!(!bridge.known.knows(&known, "sam"));
        for n in 0..ACCOUNT_FAILURES {
            let from = ip(format!("203.0.{}.{}", n / 200, n % 200 + 1));
            let keys = bridge.try_keys(&from, &from, "alex", MAX_FAILURES);
            assert!(!bridge.throttle.blocked_by(&keys));
            bridge.throttle.failed_by(&keys);
        }
        let new = ip("198.51.100.9".into());
        assert!(
            bridge
                .throttle
                .blocked_by(&bridge.try_keys(&new, &new, "alex", MAX_FAILURES))
        );
        let keys = bridge.try_keys(&known, &known, "alex", MAX_FAILURES);
        assert!(!bridge.throttle.blocked_by(&keys));
        // Known addresses together have their own hundred.
        for n in 0..ACCOUNT_FAILURES {
            let from = ip(format!("10.1.{}.{}", n / 200, n % 200 + 1));
            bridge.known.signed_in(&from, "alex");
            bridge
                .throttle
                .failed_by(&bridge.try_keys(&from, &from, "alex", MAX_FAILURES));
        }
        assert!(bridge.throttle.blocked_by(&keys));
        // An address is known for thirty days.
        *now.lock() += KNOWN_FOR;
        assert!(!bridge.known.knows(&known, "alex"));
    }

    #[test]
    fn known_addresses_stay_bounded() {
        let known = KnownAddresses::new(Clock::system());
        for n in 0..(MAX_KEYS + 50) {
            let address: IpAddr = format!("10.2.{}.{}", n / 256, n % 256)
                .parse()
                .expect("an address");
            known.signed_in(&address, "alex");
        }
        assert!(known.len() <= MAX_KEYS, "{}", known.len());
    }

    #[test]
    fn the_throttle_counts_per_address_and_member() {
        let now = Arc::new(Mutex::new(Utc::now()));
        let reading = now.clone();
        let throttle = FailureThrottle::new(Clock::new(move || *reading.lock()));
        let key = FailureThrottle::key(&"203.0.113.9".parse().expect("an address"), "Alex");
        for _ in 0..MAX_FAILURES {
            assert!(!throttle.blocked(&key));
            throttle.failed(&key);
        }
        assert!(throttle.blocked(&key));
        assert!(!throttle.blocked(&FailureThrottle::key(
            &"203.0.113.9".parse().expect("an address"),
            "sam"
        )));
        *now.lock() += FAILURE_WINDOW;
        assert!(!throttle.blocked(&key));
    }
}
