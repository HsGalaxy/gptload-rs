use crate::storage::KeyStore;
use ahash::AHashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

pub const UNLIMITED_BALANCE: i64 = -1;

pub struct BillingStore {
    balances: Arc<RwLock<AHashMap<String, Arc<AtomicI64>>>>,
    persist_tx: Sender<PersistUpdate>,
}

pub enum ReserveResult {
    Reserved,
    Insufficient,
    Missing,
}

pub enum AdjustResult {
    Updated(i64),
    Missing,
    NegativeBalance,
}

enum PersistUpdate {
    Set { key: String, balance: i64 },
    Delete { key: String },
}

impl BillingStore {
    pub fn new(store: &KeyStore) -> anyhow::Result<Self> {
        let tree = store.open_billing_tree()?;
        let balances = Arc::new(RwLock::new(AHashMap::new()));

        {
            let mut map = balances
                .write()
                .map_err(|_| anyhow::anyhow!("billing balances lock poisoned"))?;
            for item in tree.iter() {
                let (k, v) = item?;
                let key = String::from_utf8_lossy(&k).to_string();
                if let Some(balance) = decode_balance(&v) {
                    map.insert(key, Arc::new(AtomicI64::new(balance)));
                }
            }
        }

        let (tx, rx) = mpsc::channel::<PersistUpdate>();
        let persist_tree = tree.clone();
        thread::spawn(move || {
            let mut pending: AHashMap<String, i64> = AHashMap::new();
            let mut last_flush = Instant::now();
            loop {
                match rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(msg) => match msg {
                        PersistUpdate::Set { key, balance } => {
                            pending.insert(key, balance);
                        }
                        PersistUpdate::Delete { key } => {
                            pending.remove(&key);
                            let _ = persist_tree.remove(key.as_bytes());
                            let _ = persist_tree.flush();
                        }
                    },
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }

                if pending.len() >= 1024 || last_flush.elapsed() >= Duration::from_secs(1) {
                    flush_pending(&persist_tree, &mut pending);
                    last_flush = Instant::now();
                }
            }

            if !pending.is_empty() {
                flush_pending(&persist_tree, &mut pending);
            }
        });

        Ok(Self {
            balances,
            persist_tx: tx,
        })
    }

    pub fn create_key(&self, key: String, balance: i64) -> anyhow::Result<bool> {
        if balance < 0 && balance != UNLIMITED_BALANCE {
            anyhow::bail!("balance must be non-negative or -1");
        }
        let mut map = self
            .balances
            .write()
            .map_err(|_| anyhow::anyhow!("billing balances lock poisoned"))?;
        if map.contains_key(&key) {
            return Ok(false);
        }
        map.insert(key.clone(), Arc::new(AtomicI64::new(balance)));
        drop(map);
        let _ = self.persist_tx.send(PersistUpdate::Set { key, balance });
        Ok(true)
    }

    pub fn delete_key(&self, key: &str) -> anyhow::Result<bool> {
        let mut map = self
            .balances
            .write()
            .map_err(|_| anyhow::anyhow!("billing balances lock poisoned"))?;
        if map.remove(key).is_none() {
            return Ok(false);
        }
        drop(map);
        let _ = self.persist_tx.send(PersistUpdate::Delete {
            key: key.to_string(),
        });
        Ok(true)
    }

    pub fn get_balance(&self, key: &str) -> Option<i64> {
        let map = self.balances.read().ok()?;
        map.get(key).map(|v| v.load(Ordering::Relaxed))
    }

    pub fn list_keys(&self) -> Vec<(String, i64)> {
        let map = match self.balances.read() {
            Ok(map) => map,
            Err(_) => return Vec::new(),
        };
        let mut keys: Vec<(String, i64)> = map
            .iter()
            .map(|(key, balance)| (key.clone(), balance.load(Ordering::Relaxed)))
            .collect();
        keys.sort_by(|a, b| a.0.cmp(&b.0));
        keys
    }

    pub fn adjust_balance(&self, key: &str, delta: i64) -> AdjustResult {
        let map = match self.balances.read() {
            Ok(map) => map,
            Err(_) => return AdjustResult::Missing,
        };
        let Some(balance) = map.get(key).cloned() else {
            return AdjustResult::Missing;
        };
        drop(map);

        let mut cur = balance.load(Ordering::Relaxed);
        loop {
            if cur == UNLIMITED_BALANCE {
                return AdjustResult::Updated(UNLIMITED_BALANCE);
            }
            let new_balance = cur.saturating_add(delta);
            if new_balance < 0 {
                return AdjustResult::NegativeBalance;
            }
            match balance.compare_exchange(cur, new_balance, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => {
                    let _ = self.persist_tx.send(PersistUpdate::Set {
                        key: key.to_string(),
                        balance: new_balance,
                    });
                    return AdjustResult::Updated(new_balance);
                }
                Err(v) => cur = v,
            }
        }
    }

    fn adjust_balance_unchecked(&self, key: &str, delta: i64) -> Option<i64> {
        let map = self.balances.read().ok()?;
        let balance = map.get(key)?.clone();
        drop(map);
        let mut cur = balance.load(Ordering::Relaxed);
        loop {
            if cur == UNLIMITED_BALANCE {
                return Some(UNLIMITED_BALANCE);
            }
            let new_balance = cur.saturating_add(delta);
            let new_balance = new_balance.max(0);
            match balance.compare_exchange(cur, new_balance, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => {
                    let _ = self.persist_tx.send(PersistUpdate::Set {
                        key: key.to_string(),
                        balance: new_balance,
                    });
                    return Some(new_balance);
                }
                Err(v) => cur = v,
            }
        }
    }

    pub fn reserve_request(&self, key: &str) -> ReserveResult {
        let map = match self.balances.read() {
            Ok(map) => map,
            Err(_) => return ReserveResult::Missing,
        };
        let Some(balance) = map.get(key).cloned() else {
            return ReserveResult::Missing;
        };
        drop(map);

        let mut cur = balance.load(Ordering::Relaxed);
        loop {
            if cur == UNLIMITED_BALANCE {
                return ReserveResult::Reserved;
            }
            if cur <= 0 {
                return ReserveResult::Insufficient;
            }
            let new_balance = cur.saturating_sub(1);
            match balance.compare_exchange(cur, new_balance, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => {
                    let _ = self.persist_tx.send(PersistUpdate::Set {
                        key: key.to_string(),
                        balance: new_balance,
                    });
                    return ReserveResult::Reserved;
                }
                Err(v) => cur = v,
            }
        }
    }

    pub fn release_reservation(&self, key: &str) -> Option<i64> {
        self.adjust_balance_unchecked(key, 1)
    }

    pub fn settle_reserved_usage(&self, key: &str, total_tokens: u64) -> Option<i64> {
        let delta = i64::try_from(total_tokens).ok()?;
        let adjustment = 1i64.saturating_sub(delta);
        if adjustment == 0 {
            return self.get_balance(key);
        }
        self.adjust_balance_unchecked(key, adjustment)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::KeyStore;
    use std::path::PathBuf;

    fn test_store(name: &str) -> BillingStore {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "gptload-rs-billing-test-{}-{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(&path);
        let store = KeyStore::open(&PathBuf::from(path)).unwrap();
        BillingStore::new(&store).unwrap()
    }

    #[test]
    fn unlimited_balance_is_not_mutated() {
        let billing = test_store("unlimited");
        assert!(billing
            .create_key("bk-unlimited".to_string(), UNLIMITED_BALANCE)
            .unwrap());

        assert!(matches!(
            billing.reserve_request("bk-unlimited"),
            ReserveResult::Reserved
        ));
        assert_eq!(billing.get_balance("bk-unlimited"), Some(UNLIMITED_BALANCE));
        assert_eq!(
            billing.release_reservation("bk-unlimited"),
            Some(UNLIMITED_BALANCE)
        );
        assert_eq!(
            billing.settle_reserved_usage("bk-unlimited", 1_000),
            Some(UNLIMITED_BALANCE)
        );
        assert!(matches!(
            billing.adjust_balance("bk-unlimited", 100),
            AdjustResult::Updated(UNLIMITED_BALANCE)
        ));
        assert_eq!(billing.get_balance("bk-unlimited"), Some(UNLIMITED_BALANCE));
    }

    #[test]
    fn finite_settlement_clamps_at_zero() {
        let billing = test_store("clamp");
        assert!(billing.create_key("bk-finite".to_string(), 2).unwrap());

        assert!(matches!(
            billing.reserve_request("bk-finite"),
            ReserveResult::Reserved
        ));
        assert_eq!(billing.get_balance("bk-finite"), Some(1));
        assert_eq!(billing.settle_reserved_usage("bk-finite", 100), Some(0));
        assert_eq!(billing.get_balance("bk-finite"), Some(0));
    }

    #[test]
    fn create_rejects_negative_balances_except_unlimited() {
        let billing = test_store("negative");
        assert!(billing.create_key("bk-bad".to_string(), -2).is_err());
        assert!(billing
            .create_key("bk-ok".to_string(), UNLIMITED_BALANCE)
            .unwrap());
    }
}

fn decode_balance(bytes: &[u8]) -> Option<i64> {
    if bytes.len() == 8 {
        let mut arr = [0u8; 8];
        arr.copy_from_slice(bytes);
        Some(i64::from_le_bytes(arr))
    } else {
        None
    }
}

fn flush_pending(tree: &sled::Tree, pending: &mut AHashMap<String, i64>) {
    if pending.is_empty() {
        return;
    }
    for (key, balance) in pending.drain() {
        let encoded = balance.to_le_bytes();
        let _ = tree.insert(key.as_bytes(), &encoded);
    }
    let _ = tree.flush();
}
