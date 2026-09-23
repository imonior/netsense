//! i18n 模块：5 语（zh / en / zh-TW / ja / ko），en 为基准。
//!
//! - 字典以 `include_str!` 在编译期嵌入 `i18n/*.json`，无需运行时读文件、打包即用。
//! - `t(key)` 翻译当前语言；缺失时回退 en；再缺失回退 key 本身（绝不 panic）。
//! - `tf(key, args)` 支持 `{name}` 占位符替换。
//! - `check_parity()` 做 key parity 校验：五语 key 集合须与 en 一致（missing/extra 全 0），
//!   且任一条目不得为空（empty 全 0）。失败仅告警，不阻断启动。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Zh,
    En,
    ZhTw,
    Ja,
    Ko,
}

impl Language {
    pub fn from_code(s: &str) -> Language {
        match s.to_ascii_lowercase().as_str() {
            "zh" | "zh-cn" | "zh_cn" | "chinese" => Language::Zh,
            "en" | "english" => Language::En,
            "zh-tw" | "zh_tw" | "zht" | "chinese-traditional" => Language::ZhTw,
            "ja" | "jp" | "japanese" => Language::Ja,
            "ko" | "kr" | "korean" => Language::Ko,
            _ => Language::En,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Language::Zh => "zh",
            Language::En => "en",
            Language::ZhTw => "zh-TW",
            Language::Ja => "ja",
            Language::Ko => "ko",
        }
    }
}

static DICTS: OnceLock<HashMap<Language, HashMap<String, String>>> = OnceLock::new();
static CURRENT: OnceLock<Mutex<Language>> = OnceLock::new();

fn parse_dict(s: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    if let Value::Object(o) = serde_json::from_str::<Value>(s).unwrap_or(Value::Null) {
        for (k, v) in o {
            if let Value::String(s) = v {
                m.insert(k, s);
            }
        }
    }
    m
}

fn load_dicts() -> HashMap<Language, HashMap<String, String>> {
    let mut m = HashMap::new();
    m.insert(Language::En, parse_dict(include_str!("i18n/en.json")));
    m.insert(Language::Zh, parse_dict(include_str!("i18n/zh.json")));
    m.insert(Language::ZhTw, parse_dict(include_dict_zh_tw()));
    m.insert(Language::Ja, parse_dict(include_str!("i18n/ja.json")));
    m.insert(Language::Ko, parse_dict(include_str!("i18n/ko.json")));
    m
}

// 文件名含连字符，Rust 标识符不允许，用函数包一层 include_str。
#[allow(non_snake_case)]
fn include_dict_zh_tw() -> &'static str {
    include_str!("i18n/zh-TW.json")
}

/// 初始化字典与当前语言（默认 en）。应在 App setup 早期调用一次。
pub fn init() {
    let _ = DICTS.set(load_dicts());
    let _ = CURRENT.set(Mutex::new(Language::En));
}

/// 持久化地切换当前语言。
pub fn set_language(lang: Language) {
    if let Some(c) = CURRENT.get() {
        *c.lock().unwrap_or_else(|e| e.into_inner()) = lang;
    }
}

pub fn current() -> Language {
    CURRENT
        .get()
        .map(|c| *c.lock().unwrap_or_else(|e| e.into_inner()))
        .unwrap_or(Language::En)
}

/// 返回当前语言的完整字典（供前端一次性拉取）。
pub fn all_strings() -> HashMap<String, String> {
    let lang = current();
    DICTS
        .get()
        .and_then(|d| d.get(&lang).cloned())
        .unwrap_or_default()
}

/// 翻译当前语言；缺失回退 en；再缺失回退 key。
pub fn t(key: &str) -> String {
    let lang = current();
    if let Some(dicts) = DICTS.get() {
        if let Some(s) = dicts.get(&lang).and_then(|m| m.get(key)) {
            return s.clone();
        }
        if let Some(s) = dicts.get(&Language::En).and_then(|m| m.get(key)) {
            return s.clone();
        }
    }
    key.to_string()
}

/// 带占位符替换的翻译，例如 `tf("notify.applied", &[("name", "Office")])`。
pub fn tf(key: &str, args: &[(&str, &str)]) -> String {
    let mut s = t(key);
    for (k, v) in args {
        s = s.replace(&format!("{{{}}}", k), v);
    }
    s
}

/// key parity 校验：返回 (missing, extra, empty)。
/// - missing：其他语言相对于 en 缺失的 key（格式 `Lang.key`）
/// - extra：其他语言相对 en 多出的 key
/// - empty：值为空的条目（任意语言）
pub fn check_parity() -> (Vec<String>, Vec<String>, Vec<String>) {
    let dicts = match DICTS.get() {
        Some(d) => d,
        None => return (vec![], vec![], vec![]),
    };
    let en = match dicts.get(&Language::En) {
        Some(e) => e,
        None => return (vec![], vec![], vec![]),
    };
    let mut missing = Vec::new();
    let mut extra = Vec::new();
    let mut empty = Vec::new();

    for lang in [Language::Zh, Language::ZhTw, Language::Ja, Language::Ko] {
        if let Some(m) = dicts.get(&lang) {
            for k in en.keys() {
                if !m.contains_key(k) {
                    missing.push(format!("{:?}.{}", lang, k));
                }
            }
            for k in m.keys() {
                if !en.contains_key(k) {
                    extra.push(format!("{:?}.{}", lang, k));
                }
            }
        }
    }
    for lang in [Language::En, Language::Zh, Language::ZhTw, Language::Ja, Language::Ko] {
        if let Some(m) = dicts.get(&lang) {
            for (k, v) in m {
                if v.trim().is_empty() {
                    empty.push(format!("{:?}.{}", lang, k));
                }
            }
        }
    }
    (missing, extra, empty)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 串行锁：`DICTS` / `CURRENT` 都是进程级共享的 `OnceLock`，
    /// 而 `cargo test` 默认多线程并行跑用例，`set_language` 会互相污染，
    /// 导致断言结果依赖线程调度（在 Windows 上表现为偶发失败）。
    /// 所有用例都必须经 `with_dicts` 进入，保证串行且状态确定。
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn with_dicts(f: impl Fn()) {
        // 锁中毒（前一个用例断言失败）时取回内部值继续，避免连带失败
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // OnceLock 只能初始化一次，用 get_or_init 而非 set，避免第二个用例起变成空操作
        DICTS.get_or_init(load_dicts);
        CURRENT.get_or_init(|| Mutex::new(Language::En));
        set_language(Language::En);
        f();
    }

    #[test]
    fn parity_ok_in_bundle() {
        with_dicts(|| {
            let (missing, extra, empty) = check_parity();
            assert!(missing.is_empty(), "missing keys: {:?}", missing);
            assert!(extra.is_empty(), "extra keys: {:?}", extra);
            assert!(empty.is_empty(), "empty values: {:?}", empty);
        });
    }

    #[test]
    fn fallback_to_en_then_key() {
        with_dicts(|| {
            // 显式声明前置状态，不依赖全局默认值
            set_language(Language::En);
            assert_eq!(t("tray.quit"), "Quit NetSense");
            set_language(Language::Zh);
            assert_eq!(t("tray.quit"), "退出 NetSense");
            // 不存在的 key 回退到自身
            assert_eq!(t("nonexistent.key"), "nonexistent.key");
        });
    }

    #[test]
    fn placeholder_replace() {
        with_dicts(|| {
            set_language(Language::Zh);
            let s = tf("notify.applied", &[("name", "Office")]);
            assert_eq!(s, "已应用配置：Office");
        });
    }
}
