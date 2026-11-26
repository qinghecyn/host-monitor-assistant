//! 登录 token：内存存储，24 小时过期（移植自 server/auth.py）。

use rand::Rng;
use std::collections::HashMap;
use std::sync::Mutex;

use crate::now_ts;

pub struct TokenStore {
    ttl: f64,
    tokens: Mutex<HashMap<String, (String, f64)>>, // token -> (username, expires_at)
}

impl TokenStore {
    pub fn new(ttl_hours: f64) -> Self {
        Self {
            ttl: ttl_hours * 3600.0,
            tokens: Mutex::new(HashMap::new()),
        }
    }

    fn gen_token() -> String {
        // secrets.token_urlsafe(32) 等价：32 字节随机数做 base64url
        let bytes: [u8; 32] = rand::thread_rng().gen();
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }

    pub fn issue(&self, username: &str) -> String {
        let t = Self::gen_token();
        let mut map = self.tokens.lock().unwrap();
        map.insert(t.clone(), (username.to_string(), now_ts() + self.ttl));
        let now = now_ts();
        map.retain(|_, (_, exp)| now <= *exp);
        t
    }

    pub fn verify(&self, token: &str) -> Option<String> {
        if token.is_empty() {
            return None;
        }
        let mut map = self.tokens.lock().unwrap();
        match map.get(token) {
            Some((user, exp)) => {
                if now_ts() > *exp {
                    map.remove(token);
                    None
                } else {
                    Some(user.clone())
                }
            }
            None => None,
        }
    }
}
