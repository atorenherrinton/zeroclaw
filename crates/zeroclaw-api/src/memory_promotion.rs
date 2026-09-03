//! Ephemeral provenance carried by the common turn engine, never by a model.
//! Deliberately not Serialize/Debug: original input stays in process memory.

#[derive(Clone)]
pub struct OwnerRecallContext {
    pub agent_alias: String,
    pub turn_id: String,
    pub owner_input: String,
    /// Bound used by the native result collector. Zero denies tool evidence.
    pub tool_output_limit: usize,
}

tokio::task_local! {
    /// None is the fail-closed scope for cron, daemon, peer and embedded turns.
    /// Nested turns replace rather than inherit the parent's authority.
    pub static OWNER_RECALL_CONTEXT: Option<OwnerRecallContext>;
}
