//! Core Engine 模块入口：匹配 / 健康度 / 自动化。SCAFFOLD（P1/P2/P3 落地）。

pub mod matcher;
pub mod health;
pub mod automation;

pub use matcher::select_profile;
pub use health::HealthMonitor;
pub use automation::run_actions;
