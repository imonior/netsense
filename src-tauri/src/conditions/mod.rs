//! 条件层：把「当前网络」抽象成 [`identity::NetworkSnapshot`]，再按 Profile 的
//! Rules/Conditions 求值出三态结果。
//!
//! 本模块**无副作用**：只读快照、只算匹配，不碰平台、不改网络。Active / Conflict /
//! 执行调度都在 `engine`。

pub mod evaluator;
pub mod identity;

pub use evaluator::{
    eval_profile, evaluate_all, Evaluation, ProfileEvaluation, RuleReport,
};
pub use identity::NetworkSnapshot;
